//! The machine's workspace tree, owned by the daemon: the tmux model.
//!
//! # What this replaces, and why
//!
//! The previous design (`core::workspace_store`, since deleted) was an
//! *opaque* record store, where the client owned the schema and the server
//! filed JSON blobs it never read. That shape was right when there was
//! exactly one writer (the GUI)
//! and the server's only job was to make a laptop's layout visible from a
//! desktop. It stops being right the moment two clients — a GUI and a CLI, or
//! two GUIs — write concurrently: whole-record `Put` is last-writer-wins, and
//! a lost update's only symptom is a tab that quietly un-moves itself.
//!
//! So the daemon now owns the tree outright, the way tmux's server owns its
//! sessions: clients send *semantic operations* ("split this pane", "rename
//! that tab"), the daemon validates each against the tree it holds, persists,
//! and broadcasts an incremental [`LayoutDelta`] to every other client. Two
//! clients editing different corners of one workspace both land; a client that
//! falls behind re-pulls the tree it fell behind on.
//!
//! # The shape of the tree
//!
//! ```text
//! Machine
//! ├── workspaces: Vec<Workspace { tabs: Vec<Tab { root: PaneNode }> }>
//! └── panes:      Vec<PaneRecord>          ← the pane registry
//! ```
//!
//! A [`PaneNode::Leaf`] holds a **pane id and nothing else**. Everything that
//! used to ride the client's leaf — cwd, ssh spec, agent identity — is a fact
//! *about the pane*, observed by the daemon itself (OSC 7, the agent hooks,
//! the spawn request), and lives once in the pane registry rather than being a
//! snapshot some client remembered. That is what makes revival sound: after a
//! daemon restart the tree still names its panes, every named pane is known
//! dead (see below), and the pane's own record carries exactly what a client
//! needs to start its successor — the cwd to spawn in, the SSH spec to
//! reconnect, the agent session to `--resume`.
//!
//! # Restart means every pane is dead, and the tree says so
//!
//! PTYs die with the daemon process, so [`load_machine`] force-clears every
//! [`PaneRecord::live`] flag: a freshly-opened store *cannot* claim a live
//! pane, and a leaf whose record answers `live == false` is by construction
//! "awaiting revival". No client-side instance stamps, no id-reuse heuristics
//! — the process that owns the PTYs is the process answering the question, so
//! the answer is a fact rather than a guess.
//!
//! # Paths are `String` here
//!
//! The tree crosses the control wire (replies and [`LayoutDelta`] events), and
//! the dialect's rule is that paths travel as `String` — `PathBuf`'s serde
//! form for a non-UTF-8 path is platform-dependent and unencodable as JSON,
//! and one such cwd must not make a whole workspace unreadable. Lossy
//! conversion happens where the fact is recorded, which is also where the loss
//! is visible in a log.
//!
//! # Concurrency
//!
//! One mutex over the tree *and* the file write, exactly like the store this
//! replaces: the on-disk order is the in-memory order. Deltas are delivered
//! outside the lock, and a subscriber's callback must only enqueue — a peer
//! that stopped reading its socket must not stall another peer's edit.
//!
//! # Two durabilities, because two kinds of change
//!
//! A *structural* edit is persisted before its delta goes out: a change nobody
//! can re-read must be a change nobody was told about ([`Persist::Now`]).
//!
//! An *observation* — a pane's cwd, its agent, its liveness, a workspace's
//! focus stamp — takes [`Persist::Soon`] instead: the delta goes out at once
//! and the file catches up within [`FACT_FLUSH_INTERVAL`]. These arrive from
//! the PTY reader threads, one per OSC 7 report, i.e. once per prompt per pane;
//! writing the whole document (and `fsync`ing it) on each would put a disk
//! stall in the pane's own output path and, because the write happens under
//! `notify_order`, would serialize every other client's edits behind it. What
//! is risked by deferring is at most [`FACT_FLUSH_INTERVAL`] of observations on
//! a `SIGKILL`; the layout itself is never deferred.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::core::cli_agent::CLIAgent;
use crate::core::session::WorkspaceId;
use crate::daemon::protocol::NativeSshSpec;

/// The file's name under the data directory ([`DATA_DIR_ENV`] resolves where
/// that is).
///
/// Deliberately **not** `workspaces.json`: that name belonged to the retired
/// opaque-record store, whose reader quarantined anything it could not parse.
/// A build downgraded across that refactor must find its old file untouched,
/// and this build's tree must not be "repaired" away by the old reader.
pub const MACHINE_FILE: &str = "machine.json";

/// Overrides where the machine's data directory lives. Set by tests and by a
/// second server on a shared box — the same escape hatch
/// [`CONTROL_SOCK_ENV`](crate::host::server::CONTROL_SOCK_ENV) is for the
/// socket.
pub const DATA_DIR_ENV: &str = "TTY7_DATA_DIR";

/// Ceiling on workspaces, carried over from the old store: a client looping on
/// "create workspace" should hit a named error rather than grow the file until
/// the disk fills.
pub const MAX_WORKSPACES: usize = 1024;

/// Ceiling on panes the registry will hold. Panes are bounded by what a machine
/// can actually run, so this only ever catches a client gone wrong.
pub const MAX_PANES: usize = 16 * 1024;

/// How long an observation ([`Persist::Soon`]) may sit in memory before the
/// flusher writes it out.
///
/// Short enough that a crash costs a stale cwd rather than a stale layout, long
/// enough that a shell looping over directories — a `cd` per iteration, per
/// pane — costs one write rather than one per iteration.
#[cfg(not(test))]
pub const FACT_FLUSH_INTERVAL: Duration = Duration::from_secs(2);

/// Out of reach under test, so the assertions about *what defers* are not also
/// assertions about how fast the suite runs: a test that wants the write calls
/// [`MachineStore::flush`], which is the same code path the timer takes.
#[cfg(test)]
pub const FACT_FLUSH_INTERVAL: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Stable identity for one tab, minted by the daemon when the tab is created
/// and carried across restarts.
///
/// Tabs need an identity of their own because operations address them across
/// reorders: "rename tab 2" from a client that has not yet heard about another
/// client's move would rename the wrong tab, while "rename tab `t-…`" cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TabId(uuid::Uuid);

impl TabId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for TabId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TabId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Split orientation. Its own enum rather than a reuse of the client session
/// model's, because this schema is the daemon's to evolve and must not be
/// coupled to a file format that is on its way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// Which child of a [`PaneNode::Split`] a path step descends into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    A,
    B,
}

// ---------------------------------------------------------------------------
// The tree
// ---------------------------------------------------------------------------

/// Everything one machine's daemon knows about its workspaces. The document
/// [`MachineStore`] persists, and the payload a full pull returns.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    /// The pane registry: every pane the tree references, by id. Facts about
    /// panes live here exactly once — see the module header.
    #[serde(default)]
    pub panes: Vec<PaneRecord>,
}

/// Who is currently attached to a workspace.
///
/// **Data only.** The takeover behaviour — push `Preempted { by }` to the old
/// session, close its streams, offer a take-back button — lives in the control
/// server. What is here is the record that machinery needs to exist before it
/// can be written: the random token that tells two connections from the same
/// client apart, and the hostname that fills in "already open on <host>". Both
/// arrive in the [`ControlHello`](crate::daemon::control::ControlHello).
///
/// **Never persisted** (the field carrying it is `#[serde(skip)]`): an
/// attachment describes a live connection; after a server restart there are
/// none, and a stale one on disk would report a takeover against a client
/// that no longer exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// The client's per-session random token, from `ControlHello::client_token`.
    pub token: String,
    /// The client machine's hostname, shown to the user in the preempted
    /// window's status bar.
    pub hostname: String,
    /// Unix seconds when the attach happened.
    pub since: u64,
}

impl Attachment {
    /// An attachment stamped now.
    pub fn new(token: impl Into<String>, hostname: impl Into<String>) -> Attachment {
        Attachment {
            token: token.into(),
            hostname: hostname.into(),
            since: unix_now(),
        }
    }
}

/// One workspace: a named group of tabs. The unit a window shows and a client
/// attaches to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    #[serde(default)]
    pub id: WorkspaceId,
    /// User-set name. `None` lets clients derive one from the tabs' repo/cwd.
    #[serde(default)]
    pub name: Option<String>,
    /// Unix seconds when a client last focused this workspace. 0 == never.
    #[serde(default)]
    pub last_active: u64,
    #[serde(default)]
    pub tabs: Vec<Tab>,
    /// Which tab is active. `None` for a workspace with no tabs (a real state:
    /// the home page), and healed to a real tab whenever one exists.
    #[serde(default)]
    pub active_tab: Option<TabId>,
    /// Who is attached right now. **Runtime only** — an attachment describes a
    /// live connection, and a stale one on disk would report a takeover
    /// against a client that no longer exists.
    #[serde(skip)]
    pub attachment: Option<Attachment>,
}

impl Default for Workspace {
    fn default() -> Self {
        Workspace {
            id: WorkspaceId::new(),
            name: None,
            last_active: unix_now(),
            tabs: Vec::new(),
            active_tab: None,
            attachment: None,
        }
    }
}

/// One tab: a pane tree plus its labels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    #[serde(default)]
    pub id: TabId,
    /// User-set name from "Rename Tab". `None` falls back to a title-derived
    /// label at render time, on the client.
    #[serde(default)]
    pub name: Option<String>,
    /// The tab's sidebar repo group (its repository home), as the client that
    /// resolved it reported. A path in the *machine's* namespace, as a string
    /// for the same reason every other path here is.
    #[serde(default)]
    pub sidebar_group: Option<String>,
    pub root: PaneNode,
}

impl Tab {
    /// A tab holding exactly `pane`.
    pub fn leaf(pane: u64) -> Tab {
        Tab {
            id: TabId::new(),
            name: None,
            sidebar_group: None,
            root: PaneNode::Leaf { pane },
        }
    }
}

/// A tab's split structure. Leaves hold a pane **id and nothing else**; every
/// fact about the pane lives in the registry ([`PaneRecord`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PaneNode {
    Leaf {
        pane: u64,
    },
    Split {
        axis: Axis,
        #[serde(default = "default_ratio")]
        ratio: f32,
        a: Box<PaneNode>,
        b: Box<PaneNode>,
    },
}

fn default_ratio() -> f32 {
    0.5
}

impl PaneNode {
    /// Every pane id under this node, in layout order.
    pub fn pane_ids(&self) -> Vec<u64> {
        let mut out = Vec::new();
        self.collect_panes(&mut out);
        out
    }

    fn collect_panes(&self, out: &mut Vec<u64>) {
        match self {
            PaneNode::Leaf { pane } => out.push(*pane),
            PaneNode::Split { a, b, .. } => {
                a.collect_panes(out);
                b.collect_panes(out);
            }
        }
    }

    /// Whether `pane` appears as a leaf under this node.
    pub fn contains(&self, pane: u64) -> bool {
        match self {
            PaneNode::Leaf { pane: p } => *p == pane,
            PaneNode::Split { a, b, .. } => a.contains(pane) || b.contains(pane),
        }
    }

    /// The node a split path resolves to, if the path is still valid. Public
    /// for the same reason the surgery methods are: a client applying a
    /// [`LayoutDelta::RatioChanged`] resolves the identical path.
    pub fn descend_mut(&mut self, path: &[Side]) -> Option<&mut PaneNode> {
        match path.split_first() {
            None => Some(self),
            Some((side, rest)) => match self {
                PaneNode::Leaf { .. } => None,
                PaneNode::Split { a, b, .. } => match side {
                    Side::A => a.descend_mut(rest),
                    Side::B => b.descend_mut(rest),
                },
            },
        }
    }

    /// Replace the leaf holding `pane` with a split of it and `new`, answering
    /// whether the leaf was found.
    ///
    /// Public (as are [`remove_leaf`](PaneNode::remove_leaf) and
    /// [`replace_leaf`](PaneNode::replace_leaf)) because a client predicting the
    /// outcome of its own operation must run *this* surgery, not a
    /// reimplementation that could disagree with the server's.
    pub fn split_leaf(&mut self, pane: u64, new: u64, axis: Axis, ratio: f32, first: bool) -> bool {
        match self {
            PaneNode::Leaf { pane: p } if *p == pane => {
                let old = PaneNode::Leaf { pane };
                let added = PaneNode::Leaf { pane: new };
                let (a, b) = if first { (added, old) } else { (old, added) };
                *self = PaneNode::Split {
                    axis,
                    ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                };
                true
            }
            PaneNode::Leaf { .. } => false,
            PaneNode::Split { a, b, .. } => {
                a.split_leaf(pane, new, axis, ratio, first)
                    || b.split_leaf(pane, new, axis, ratio, first)
            }
        }
    }

    /// Remove the leaf holding `pane`, collapsing its parent split so the
    /// sibling takes the whole space. `None` when the node *is* that leaf —
    /// the caller then removes the tab. `Some(found)` otherwise.
    pub fn remove_leaf(&mut self, pane: u64) -> Option<bool> {
        match self {
            PaneNode::Leaf { pane: p } => {
                if *p == pane {
                    None
                } else {
                    Some(false)
                }
            }
            PaneNode::Split { a, b, .. } => {
                if matches!(&**a, PaneNode::Leaf { pane: p } if *p == pane) {
                    *self = (**b).clone();
                    return Some(true);
                }
                if matches!(&**b, PaneNode::Leaf { pane: p } if *p == pane) {
                    *self = (**a).clone();
                    return Some(true);
                }
                match a.remove_leaf(pane) {
                    Some(true) => Some(true),
                    Some(false) => b.remove_leaf(pane),
                    // A whole subtree cannot be the leaf; unreachable because
                    // leaf children are handled above, but total anyway.
                    None => Some(false),
                }
            }
        }
    }

    /// Rebind the leaf holding `old` to `new`, answering whether it was found.
    pub fn replace_leaf(&mut self, old: u64, new: u64) -> bool {
        match self {
            PaneNode::Leaf { pane } if *pane == old => {
                *pane = new;
                true
            }
            PaneNode::Leaf { .. } => false,
            PaneNode::Split { a, b, .. } => a.replace_leaf(old, new) || b.replace_leaf(old, new),
        }
    }
}

/// One pane, as the daemon knows it: identity, liveness, and the facts a dead
/// pane's successor is started from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneRecord {
    /// The daemon's pane id — the same number the pane protocol's `Spawn`
    /// answered with. One id space, so a leaf, a `PaneInfo` and this record
    /// can only ever mean the same pane.
    pub id: u64,
    /// Working directory, from OSC 7 (or the spawn request until the first
    /// report). The machine's own namespace.
    #[serde(default)]
    pub cwd: Option<String>,
    // No `title` field, deliberately. The pane's title is a *live* answer (a
    // foreground-process query at `PaneInfo` time), not tracked state the
    // reader loop observes — so a record field for it was never written, and
    // a field that is always empty is a standing invitation to trust it.
    // Revival labels derive from `cwd` and `agent` instead.
    /// The native-SSH spec this pane ran, **secrets stripped**
    /// ([`NativeSshSpec::without_secrets`]). What a revival reconnects with.
    #[serde(default)]
    pub ssh_spec: Option<Box<NativeSshSpec>>,
    /// The coding agent running in this pane, if the hooks reported one.
    #[serde(default)]
    pub agent: Option<AgentFacts>,
    /// Whether a PTY for this pane exists **in this daemon process**.
    ///
    /// Serialized, because clients read it off the wire — `false` on a leaf's
    /// record *is* the "awaiting revival" state a client renders and revives.
    /// But it is a fact about a *process*, so [`load_machine`] force-clears it
    /// on open: PTYs die with the daemon, and whatever the file claims, a
    /// freshly-started process has none. No client-side instance stamp or
    /// id-reuse heuristic is needed, because the process that owns the PTYs is
    /// the one answering.
    #[serde(default)]
    pub live: bool,
}

impl PaneRecord {
    /// A bare record for `id`, with no facts yet.
    pub fn new(id: u64) -> PaneRecord {
        PaneRecord {
            id,
            cwd: None,
            ssh_spec: None,
            agent: None,
            live: false,
        }
    }
}

/// What the daemon knows about the agent a pane runs — enough to resume the
/// conversation in a successor pane after the original dies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentFacts {
    pub agent: CLIAgent,
    /// The agent's own session id, from its `session-start` hook. What
    /// `claude --resume <id>` (and each agent's equivalent) takes.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The argv the agent was launched with, so a resume carries the user's
    /// flags (`--dangerously-skip-permissions`, …) instead of resuming bare.
    #[serde(default)]
    pub launch_argv: Option<Vec<String>>,
    /// Latest coarse status the daemon's sniffer folded from the agent's
    /// hook events. Display only; never load-bearing.
    #[serde(default)]
    pub status: Option<crate::core::cli_agent::AgentStatus>,
}

/// The facts a client hands over when an operation introduces a pane the store
/// has not seen — a new tab's pane, a split's second pane, a revival's
/// replacement. The pane itself was spawned over the pane protocol (that is
/// where PTYs come from); this is its birth certificate for the tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneSeed {
    pub pane: u64,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub ssh_spec: Option<Box<NativeSshSpec>>,
    #[serde(default)]
    pub agent: Option<AgentFacts>,
}

impl PaneSeed {
    /// A seed carrying only the id.
    pub fn bare(pane: u64) -> PaneSeed {
        PaneSeed {
            pane,
            cwd: None,
            ssh_spec: None,
            agent: None,
        }
    }

    fn into_record(self, live: bool) -> PaneRecord {
        PaneRecord {
            id: self.pane,
            cwd: self.cwd,
            ssh_spec: self.ssh_spec.map(|s| Box::new(s.without_secrets())),
            agent: self.agent,
            live,
        }
    }
}

// ---------------------------------------------------------------------------
// Deltas
// ---------------------------------------------------------------------------

/// One incremental change to one workspace's tree, as broadcast to every
/// client but the writer.
///
/// The granularity rule: label changes are carried field-by-field, structural
/// changes carry the whole affected [`Tab`]. A tab is small (a few hundred
/// bytes), and shipping it whole means a client applies structure by
/// *replacement* instead of by re-implementing the server's tree surgery —
/// the class of client/server divergence that cannot happen is the class that
/// was never written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutDelta {
    /// A workspace appeared. Carries it whole (it is newborn, so small).
    WorkspaceCreated {
        workspace: Workspace,
    },
    WorkspaceRenamed {
        name: Option<String>,
    },
    WorkspaceDeleted,
    WorkspaceTouched {
        last_active: u64,
    },
    /// Which tab is active changed — by an explicit set, by a created tab
    /// becoming active, or by the close paths healing a dangling active id.
    /// Emitted for every *implicit* change too, so a mirroring client never
    /// has to re-implement the server's heal rule; the one inexpressible case
    /// (a workspace losing its last tab has no active tab) needs no delta,
    /// because "no tabs → no active tab" is a fact, not surgery.
    ActiveTabChanged {
        tab: TabId,
    },
    /// A tab appeared at `at`. Structural, so it carries the tab whole.
    TabCreated {
        at: usize,
        tab: Tab,
    },
    TabClosed {
        tab: TabId,
    },
    TabRenamed {
        tab: TabId,
        name: Option<String>,
    },
    TabMoved {
        tab: TabId,
        to: usize,
    },
    TabRegrouped {
        tab: TabId,
        group: Option<String>,
    },
    /// A tab's pane structure changed (split, close, revival rebind). The tab
    /// is carried whole — see the enum's granularity rule. `pane` names the
    /// registry record that changed alongside, when one did.
    TabRestructured {
        tab: Tab,
        pane: Option<PaneRecord>,
    },
    /// One split's divider moved. Fine-grained because ratio drags are the
    /// hottest structural edit and the only one where shipping a whole tab
    /// per event would be felt.
    RatioChanged {
        tab: TabId,
        path: Vec<Side>,
        ratio: f32,
    },
    /// A pane's facts changed (cwd, agent, liveness). Not a layout change,
    /// but clients rendering "awaiting revival" or an agent chip need it.
    PaneFacts {
        pane: PaneRecord,
    },
}

/// Identifies one subscriber, so a writer is excluded from its own echo.
/// Same shape as the old store's, for the same reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubscriberId(pub u64);

/// What a subscriber receives: which workspace, and what changed. Runs on the
/// writer's thread — enqueue and return.
pub type Notify = Arc<dyn Fn(&str, &LayoutDelta) + Send + Sync>;

/// A live subscription; dropping it unsubscribes.
pub struct Subscription {
    store: Arc<MachineStore>,
    id: SubscriberId,
}

impl Subscription {
    /// This subscriber's id — pass it as the `origin` of your own writes.
    pub fn id(&self) -> SubscriberId {
        self.id
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.store.unsubscribe(self.id);
    }
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// How the store asks the process serving panes whether an id has a live PTY
/// *right now* — see [`MachineStore::set_liveness_probe`].
pub type LivenessProbe = Arc<dyn Fn(u64) -> bool + Send + Sync>;

/// The daemon's tree, and the one writer to its file.
pub struct MachineStore {
    path: PathBuf,
    state: Mutex<Machine>,
    /// Answers "does this pane have a live PTY right now", installed by the
    /// daemon's pane server. `None` (a store opened by tests, or before the
    /// pane listener is wired) trusts the seed. See
    /// [`set_liveness_probe`](MachineStore::set_liveness_probe).
    liveness: Mutex<Option<LivenessProbe>>,
    /// Serializes each mutation *with its own delivery*. The state lock alone
    /// orders the mutations, but deltas are delivered after it is released —
    /// without this, writer B's deltas could overtake writer A's and every
    /// subscriber would apply the store's history in the wrong order, ending
    /// on the losing state with no error to trigger a re-pull. Cheap to hold
    /// across delivery because a subscriber's callback is enqueue-only by
    /// contract. Always taken before `state`, never inside it.
    notify_order: Mutex<()>,
    subscribers: Mutex<Vec<(SubscriberId, Notify)>>,
    next_subscriber: AtomicU64,
    /// Set by a [`Persist::Soon`] mutation, cleared by every write — the
    /// flusher's whole state. Never a reason to write on its own: a store that
    /// only ever sees structural edits has no flusher at all.
    unwritten: AtomicBool,
    /// Whether the flusher thread has been started, so the first observation
    /// starts it and the rest cost one atomic load.
    flushing: AtomicBool,
}

/// When an operation's change has to be on disk. See the module header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Persist {
    /// Before the deltas go out — every structural edit.
    Now,
    /// Within [`FACT_FLUSH_INTERVAL`] — the machine's own observations.
    Soon,
}

/// The error every invalid operation answers with. `InvalidInput` so the wire
/// layer maps it to a client-visible refusal rather than a server fault.
fn refuse(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

fn not_found(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, msg.into())
}

impl MachineStore {
    /// Open the store at `path`, reading whatever is there.
    ///
    /// Infallible by design: a machine whose tree file is missing or
    /// unreadable must still serve panes and files. A file that does not parse
    /// is copied aside as `machine.json.corrupt` before anything overwrites
    /// it, so "the tree came up empty" is recoverable by hand.
    pub fn open(path: impl Into<PathBuf>) -> Arc<MachineStore> {
        let path = path.into();
        let machine = load_machine(&path);
        Arc::new(MachineStore {
            path,
            state: Mutex::new(machine),
            liveness: Mutex::new(None),
            notify_order: Mutex::new(()),
            subscribers: Mutex::new(Vec::new()),
            next_subscriber: AtomicU64::new(1),
            unwritten: AtomicBool::new(false),
            flushing: AtomicBool::new(false),
        })
    }

    /// Install the pane server's answer to "is this pane alive right now",
    /// consulted whenever a seed introduces a pane to the registry.
    ///
    /// A seed used to enter the registry `live: true` unconditionally — but a
    /// pane that died between its spawn and its adopting operation had its
    /// death observation dropped ([`MachineStore::note_pane_facts`] ignores
    /// panes the tree does not hold), and nothing ever flipped the record back:
    /// the leaf claimed a live pane forever and revival never offered. Asking
    /// the process that owns the PTYs at registration time closes the window.
    pub fn set_liveness_probe(&self, probe: LivenessProbe) {
        *self.liveness.lock().unwrap_or_else(|e| e.into_inner()) = Some(probe);
    }

    /// Whether a seeded pane is alive, per the installed probe. Without one
    /// the seed is trusted (`true`): the seeding client just spawned it.
    fn seed_is_live(&self, pane: u64) -> bool {
        let probe = self
            .liveness
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match probe {
            Some(probe) => probe(pane),
            None => true,
        }
    }

    /// Open the store at its default location under the data directory.
    pub fn shared() -> io::Result<Arc<MachineStore>> {
        Ok(MachineStore::open(default_machine_path()?))
    }

    /// Where this store is persisted.
    pub fn path(&self) -> &Path {
        &self.path
    }

    // ----- reads -----------------------------------------------------------

    /// A snapshot of the whole tree. What a full pull answers with.
    pub fn machine(&self) -> Machine {
        self.locked().clone()
    }

    /// One workspace, whole. `NotFound` when there is no such workspace.
    pub fn workspace(&self, id: WorkspaceId) -> io::Result<Workspace> {
        self.locked()
            .workspaces
            .iter()
            .find(|w| w.id == id)
            .cloned()
            .ok_or_else(|| not_found(format!("no workspace {id} on this machine")))
    }

    /// One pane's record.
    pub fn pane(&self, id: u64) -> Option<PaneRecord> {
        self.locked().panes.iter().find(|p| p.id == id).cloned()
    }

    // ----- workspace operations --------------------------------------------

    /// Create a workspace (empty — its first tab arrives as its own op).
    ///
    /// `id` lets the *client* mint the identity. A window exists before its
    /// first round trip completes — the window registry, the view file and
    /// every queued operation already name the workspace — so making the
    /// daemon the minter would force every client to hold its ops until a
    /// reply carried the "real" id back. Ids are uuids, so a client-minted one
    /// is as unique as a daemon-minted one; a collision with an existing
    /// workspace is refused rather than adopted, because "create" answering an
    /// unrelated workspace's tree would hand one client another's tabs.
    pub fn workspace_create(
        &self,
        id: Option<WorkspaceId>,
        name: Option<String>,
        origin: Option<SubscriberId>,
    ) -> io::Result<Workspace> {
        let created = self.mutate(origin, |m| {
            if m.workspaces.len() >= MAX_WORKSPACES {
                return Err(refuse(format!(
                    "this machine already holds {MAX_WORKSPACES} workspaces"
                )));
            }
            if let Some(id) = id
                && m.workspaces.iter().any(|w| w.id == id)
            {
                return Err(refuse(format!("workspace {id} already exists")));
            }
            let workspace = Workspace {
                id: id.unwrap_or_default(),
                name: name.clone(),
                ..Workspace::default()
            };
            m.workspaces.push(workspace.clone());
            Ok((
                workspace.clone(),
                vec![(
                    workspace.id,
                    LayoutDelta::WorkspaceCreated {
                        workspace: workspace.clone(),
                    },
                )],
            ))
        })?;
        Ok(created)
    }

    /// Set (or clear) a workspace's user-chosen name.
    pub fn workspace_rename(
        &self,
        id: WorkspaceId,
        name: Option<String>,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        self.mutate(origin, |m| {
            let ws = find_workspace(m, id)?;
            ws.name = name.clone();
            Ok(((), vec![(id, LayoutDelta::WorkspaceRenamed { name })]))
        })
    }

    /// Forget a workspace and every pane record only it referenced.
    ///
    /// Answers the ids of the panes that went with it, so the caller can kill
    /// their PTYs — the store never touches a process, only bookkeeping.
    pub fn workspace_delete(
        &self,
        id: WorkspaceId,
        origin: Option<SubscriberId>,
    ) -> io::Result<Vec<u64>> {
        self.mutate(origin, |m| {
            let index = m
                .workspaces
                .iter()
                .position(|w| w.id == id)
                .ok_or_else(|| not_found(format!("no workspace {id} on this machine")))?;
            m.workspaces.remove(index);
            let orphans = collect_orphan_panes(m);
            m.panes.retain(|p| !orphans.contains(&p.id));
            Ok((orphans, vec![(id, LayoutDelta::WorkspaceDeleted)]))
        })
    }

    /// Stamp a workspace as just-focused.
    ///
    /// An observation, not a structural edit ([`Persist::Soon`]): every window
    /// focus change on every client lands one, and a picker's ordering is not
    /// worth a `fsync` per keystroke-of-attention.
    pub fn workspace_touch(
        self: &Arc<Self>,
        id: WorkspaceId,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        self.ensure_flusher();
        self.mutate_with(origin, Persist::Soon, |m| {
            let ws = find_workspace(m, id)?;
            let now = unix_now();
            ws.last_active = now;
            Ok((
                (),
                vec![(id, LayoutDelta::WorkspaceTouched { last_active: now })],
            ))
        })
    }

    /// Change which tab is active.
    pub fn workspace_set_active_tab(
        &self,
        id: WorkspaceId,
        tab: TabId,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        self.mutate(origin, |m| {
            let ws = find_workspace(m, id)?;
            if !ws.tabs.iter().any(|t| t.id == tab) {
                return Err(not_found(format!("workspace {id} has no tab {tab}")));
            }
            ws.active_tab = Some(tab);
            Ok(((), vec![(id, LayoutDelta::ActiveTabChanged { tab })]))
        })
    }

    // ----- tab operations --------------------------------------------------

    /// Create a tab holding `pane`, at `at` (clamped; `None` appends), and make
    /// it active — a created tab is one the user is about to type into.
    ///
    /// `id` is client-mintable for the same reason
    /// [`workspace_create`](MachineStore::workspace_create)'s is: the client's
    /// window holds the tab (and may already have queued operations against it)
    /// before the reply lands, and a uuid minted there is as good as one minted
    /// here. A duplicate is refused, never adopted.
    pub fn tab_create(
        &self,
        workspace: WorkspaceId,
        at: Option<usize>,
        pane: PaneSeed,
        id: Option<TabId>,
        origin: Option<SubscriberId>,
    ) -> io::Result<Tab> {
        let live = self.seed_is_live(pane.pane);
        self.mutate(origin, |m| {
            if let Some(id) = id
                && m.workspaces
                    .iter()
                    .any(|w| w.tabs.iter().any(|t| t.id == id))
            {
                return Err(refuse(format!("tab {id} already exists")));
            }
            register_pane(m, pane.clone(), live)?;
            let ws = find_workspace(m, workspace)?;
            let mut tab = Tab::leaf(pane.pane);
            if let Some(id) = id {
                tab.id = id;
            }
            let tab = tab;
            let at = at.unwrap_or(ws.tabs.len()).min(ws.tabs.len());
            ws.tabs.insert(at, tab.clone());
            ws.active_tab = Some(tab.id);
            let active = tab.id;
            Ok((
                tab.clone(),
                vec![
                    (workspace, LayoutDelta::TabCreated { at, tab }),
                    (workspace, LayoutDelta::ActiveTabChanged { tab: active }),
                ],
            ))
        })
    }

    /// Close a tab, answering the pane ids that left the tree with it (for the
    /// caller to kill — see [`MachineStore::workspace_delete`]).
    pub fn tab_close(
        &self,
        workspace: WorkspaceId,
        tab: TabId,
        origin: Option<SubscriberId>,
    ) -> io::Result<Vec<u64>> {
        self.mutate(origin, |m| {
            let ws = find_workspace(m, workspace)?;
            let index = ws
                .tabs
                .iter()
                .position(|t| t.id == tab)
                .ok_or_else(|| not_found(format!("workspace {workspace} has no tab {tab}")))?;
            ws.tabs.remove(index);
            let mut deltas = vec![(workspace, LayoutDelta::TabClosed { tab })];
            if let Some(active) = heal_active_tab(ws, index) {
                deltas.push((workspace, LayoutDelta::ActiveTabChanged { tab: active }));
            }
            let orphans = collect_orphan_panes(m);
            m.panes.retain(|p| !orphans.contains(&p.id));
            Ok((orphans, deltas))
        })
    }

    /// Set (or clear) a tab's user-chosen name.
    pub fn tab_rename(
        &self,
        workspace: WorkspaceId,
        tab: TabId,
        name: Option<String>,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        self.mutate(origin, |m| {
            let t = find_tab(m, workspace, tab)?;
            t.name = name.clone();
            Ok(((), vec![(workspace, LayoutDelta::TabRenamed { tab, name })]))
        })
    }

    /// Move a tab to position `to` (clamped).
    pub fn tab_move(
        &self,
        workspace: WorkspaceId,
        tab: TabId,
        to: usize,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        self.mutate(origin, |m| {
            let ws = find_workspace(m, workspace)?;
            let from = ws
                .tabs
                .iter()
                .position(|t| t.id == tab)
                .ok_or_else(|| not_found(format!("workspace {workspace} has no tab {tab}")))?;
            let moved = ws.tabs.remove(from);
            let to = to.min(ws.tabs.len());
            ws.tabs.insert(to, moved);
            Ok(((), vec![(workspace, LayoutDelta::TabMoved { tab, to })]))
        })
    }

    /// Record which repo group a tab belongs to in the sidebar.
    pub fn tab_set_group(
        &self,
        workspace: WorkspaceId,
        tab: TabId,
        group: Option<String>,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        self.mutate(origin, |m| {
            let t = find_tab(m, workspace, tab)?;
            t.sidebar_group = group.clone();
            Ok((
                (),
                vec![(workspace, LayoutDelta::TabRegrouped { tab, group })],
            ))
        })
    }

    // ----- pane operations -------------------------------------------------

    /// Split the leaf holding `pane`: the new pane takes the `first` (upper /
    /// left) or second position, at `ratio`.
    pub fn pane_split(
        &self,
        workspace: WorkspaceId,
        pane: u64,
        axis: Axis,
        ratio: f32,
        new: PaneSeed,
        first: bool,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        let ratio = clamp_ratio(ratio)?;
        let live = self.seed_is_live(new.pane);
        self.mutate(origin, |m| {
            register_pane(m, new.clone(), live)?;
            let record = m
                .panes
                .iter()
                .find(|p| p.id == new.pane)
                .cloned()
                .expect("registered above");
            let ws = find_workspace(m, workspace)?;
            let tab = ws
                .tabs
                .iter_mut()
                .find(|t| t.root.contains(pane))
                .ok_or_else(|| {
                    not_found(format!("workspace {workspace} has no pane {pane} to split"))
                })?;
            tab.root.split_leaf(pane, new.pane, axis, ratio, first);
            let delta = LayoutDelta::TabRestructured {
                tab: tab.clone(),
                pane: Some(record),
            };
            Ok(((), vec![(workspace, delta)]))
        })
    }

    /// Remove the leaf holding `pane`. When it was the tab's last pane the tab
    /// closes with it. Answers the pane ids that left the tree.
    pub fn pane_close(
        &self,
        workspace: WorkspaceId,
        pane: u64,
        origin: Option<SubscriberId>,
    ) -> io::Result<Vec<u64>> {
        self.mutate(origin, |m| {
            let ws = find_workspace(m, workspace)?;
            let index = ws
                .tabs
                .iter()
                .position(|t| t.root.contains(pane))
                .ok_or_else(|| not_found(format!("workspace {workspace} has no pane {pane}")))?;
            let mut deltas = Vec::new();
            match ws.tabs[index].root.remove_leaf(pane) {
                // The tab was that one leaf: the tab goes.
                None => {
                    let closed = ws.tabs.remove(index);
                    deltas.push((workspace, LayoutDelta::TabClosed { tab: closed.id }));
                    if let Some(active) = heal_active_tab(ws, index) {
                        deltas.push((workspace, LayoutDelta::ActiveTabChanged { tab: active }));
                    }
                }
                Some(true) => deltas.push((
                    workspace,
                    LayoutDelta::TabRestructured {
                        tab: ws.tabs[index].clone(),
                        pane: None,
                    },
                )),
                Some(false) => unreachable!("the tab was chosen because it contains the pane"),
            };
            let orphans = collect_orphan_panes(m);
            m.panes.retain(|p| !orphans.contains(&p.id));
            Ok((orphans, deltas))
        })
    }

    /// Move a split's divider. `path` addresses the split from the tab root.
    pub fn pane_set_ratio(
        &self,
        workspace: WorkspaceId,
        tab: TabId,
        path: Vec<Side>,
        ratio: f32,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        let ratio = clamp_ratio(ratio)?;
        self.mutate(origin, |m| {
            let t = find_tab(m, workspace, tab)?;
            match t.root.descend_mut(&path) {
                Some(PaneNode::Split { ratio: r, .. }) => *r = ratio,
                _ => {
                    return Err(refuse(format!(
                        "tab {tab} has no split at that path any more"
                    )));
                }
            }
            Ok((
                (),
                vec![(workspace, LayoutDelta::RatioChanged { tab, path, ratio })],
            ))
        })
    }

    /// Move the leaf holding `pane` next to `to`, splitting it along `axis`.
    /// The tmux `move-pane`: remove from where it is (collapsing that split),
    /// then re-split at the destination.
    pub fn pane_move(
        &self,
        workspace: WorkspaceId,
        pane: u64,
        to: u64,
        axis: Axis,
        first: bool,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        if pane == to {
            return Err(refuse("a pane cannot be moved next to itself"));
        }
        self.mutate(origin, |m| {
            let ws = find_workspace(m, workspace)?;
            let from = ws
                .tabs
                .iter()
                .position(|t| t.root.contains(pane))
                .ok_or_else(|| not_found(format!("workspace {workspace} has no pane {pane}")))?;
            let dest = ws
                .tabs
                .iter()
                .position(|t| t.root.contains(to))
                .ok_or_else(|| not_found(format!("workspace {workspace} has no pane {to}")))?;

            let mut deltas: Vec<(WorkspaceId, LayoutDelta)> = Vec::new();
            match ws.tabs[from].root.remove_leaf(pane) {
                None => {
                    // The pane was a whole tab; that tab dissolves into the
                    // destination.
                    if from == dest {
                        return Err(refuse("a pane cannot be moved next to itself".to_string()));
                    }
                    let closed = ws.tabs.remove(from);
                    deltas.push((workspace, LayoutDelta::TabClosed { tab: closed.id }));
                    if let Some(active) = heal_active_tab(ws, from) {
                        deltas.push((workspace, LayoutDelta::ActiveTabChanged { tab: active }));
                    }
                }
                Some(true) => {
                    deltas.push((
                        workspace,
                        LayoutDelta::TabRestructured {
                            tab: ws.tabs[from].clone(),
                            pane: None,
                        },
                    ));
                }
                Some(false) => unreachable!("the tab was chosen because it contains the pane"),
            }
            // Indices may have shifted if a tab was removed above.
            let dest_tab = ws
                .tabs
                .iter_mut()
                .find(|t| t.root.contains(to))
                .expect("the destination tab still exists; only the source tab can close");
            dest_tab.root.split_leaf(to, pane, axis, 0.5, first);
            deltas.push((
                workspace,
                LayoutDelta::TabRestructured {
                    tab: dest_tab.clone(),
                    pane: None,
                },
            ));
            Ok(((), deltas))
        })
    }

    /// Rebind the leaf holding `old` to a freshly-spawned successor — the
    /// revival op. The old record leaves the registry with its facts spent.
    pub fn pane_replace(
        &self,
        workspace: WorkspaceId,
        old: u64,
        new: PaneSeed,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        let live = self.seed_is_live(new.pane);
        self.mutate(origin, |m| {
            register_pane(m, new.clone(), live)?;
            let record = m
                .panes
                .iter()
                .find(|p| p.id == new.pane)
                .cloned()
                .expect("registered above");
            let ws = find_workspace(m, workspace)?;
            let tab = ws
                .tabs
                .iter_mut()
                .find(|t| t.root.contains(old))
                .ok_or_else(|| not_found(format!("workspace {workspace} has no pane {old}")))?;
            tab.root.replace_leaf(old, new.pane);
            let delta = LayoutDelta::TabRestructured {
                tab: tab.clone(),
                pane: Some(record),
            };
            m.panes.retain(|p| p.id != old);
            Ok(((), vec![(workspace, delta)]))
        })
    }

    // ----- pane facts (the daemon's own observations) ----------------------

    /// Record facts the daemon observed about `pane` — OSC 7 cwd, agent hook
    /// events, liveness. Unknown panes are ignored (a pane
    /// the tree never adopted is not the tree's business). The delta is
    /// attributed to no origin: facts come from the machine, so *every*
    /// client hears them.
    ///
    /// Called from the pane reader threads, once per prompt per pane, so the
    /// write is deferred ([`Persist::Soon`]) while the delta is not: what a
    /// client renders stays current, and the disk catches up on the flusher's
    /// tick.
    pub fn note_pane_facts(self: &Arc<Self>, pane: u64, update: impl FnOnce(&mut PaneRecord)) {
        self.ensure_flusher();
        let result: io::Result<()> = self.mutate_with(None, Persist::Soon, |m| {
            let Some(record) = m.panes.iter_mut().find(|p| p.id == pane) else {
                return Ok(((), Vec::new()));
            };
            let before = record.clone();
            update(record);
            record.id = before.id;
            if *record == before {
                return Ok(((), Vec::new()));
            }
            let record = record.clone();
            let workspaces: Vec<WorkspaceId> = m
                .workspaces
                .iter()
                .filter(|w| w.tabs.iter().any(|t| t.root.contains(pane)))
                .map(|w| w.id)
                .collect();
            Ok((
                (),
                workspaces
                    .into_iter()
                    .map(|w| {
                        (
                            w,
                            LayoutDelta::PaneFacts {
                                pane: record.clone(),
                            },
                        )
                    })
                    .collect(),
            ))
        });
        if let Err(e) = result {
            log::warn!("could not record facts about pane {pane}: {e}");
        }
    }

    // ----- attachment (runtime; never persisted) ----------------------------

    /// Record `who` as the workspace's current session and answer whoever held
    /// it before — the data half of the takeover, unchanged in meaning from
    /// the old store's.
    pub fn attach(&self, workspace: WorkspaceId, who: Attachment) -> Option<Attachment> {
        let mut m = self.locked();
        let ws = m.workspaces.iter_mut().find(|w| w.id == workspace)?;
        ws.attachment.replace(who)
    }

    /// Who is attached to `workspace`, if anyone.
    pub fn attachment(&self, workspace: WorkspaceId) -> Option<Attachment> {
        self.locked()
            .workspaces
            .iter()
            .find(|w| w.id == workspace)
            .and_then(|w| w.attachment.clone())
    }

    /// Release `workspace`, but **only if `token` still holds it** — the guard
    /// that keeps a preempted client's teardown from evicting its usurper.
    pub fn detach(&self, workspace: WorkspaceId, token: &str) -> bool {
        let mut m = self.locked();
        let Some(ws) = m.workspaces.iter_mut().find(|w| w.id == workspace) else {
            return false;
        };
        if ws.attachment.as_ref().is_some_and(|a| a.token == token) {
            ws.attachment = None;
            true
        } else {
            false
        }
    }

    // ----- change notification ---------------------------------------------

    /// Be told about every delta. Dropping the [`Subscription`] unsubscribes.
    /// The callback runs on the writer's thread: enqueue and return.
    pub fn subscribe(self: &Arc<Self>, f: Notify) -> Subscription {
        let id = SubscriberId(self.next_subscriber.fetch_add(1, Ordering::Relaxed));
        self.subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, f));
        Subscription {
            store: Arc::clone(self),
            id,
        }
    }

    fn unsubscribe(&self, id: SubscriberId) {
        self.subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(sid, _)| *sid != id);
    }

    // ----- internals -------------------------------------------------------

    fn locked(&self) -> std::sync::MutexGuard<'_, Machine> {
        // A poisoned lock means a panic mid-mutation. Every *fallible* path
        // rolls back before releasing the lock (see `mutate`); the only
        // panics inside an op are `unreachable!`/`expect`s on invariants the
        // same op just established, so a poisoned tree is still the pre- or
        // post-images of some operation. Carrying on beats taking the daemon
        // — and every pane on the machine — down with a bookkeeping panic.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// [`mutate_with`](Self::mutate_with) at [`Persist::Now`] — every
    /// structural operation.
    fn mutate<T>(
        &self,
        origin: Option<SubscriberId>,
        op: impl FnOnce(&mut Machine) -> io::Result<(T, Vec<(WorkspaceId, LayoutDelta)>)>,
    ) -> io::Result<T> {
        self.mutate_with(origin, Persist::Now, op)
    }

    /// Run one operation: mutate under the lock, persist, and — only if the
    /// disk said yes — deliver the deltas outside the state lock.
    ///
    /// A failed persist rolls the tree back to the pre-mutation clone, so the
    /// in-memory state never claims something the file does not, and a change
    /// nobody can re-read is a change nobody is told about. At
    /// [`Persist::Soon`] there is no disk to fail: the change is flagged
    /// unwritten and the flusher carries it, which is sound only because what
    /// takes that path is the machine re-observable rather than the layout —
    /// see the module header.
    ///
    /// `notify_order` is held across the whole thing — see the field — so
    /// subscribers receive deltas in exactly the order the mutations landed.
    fn mutate_with<T>(
        &self,
        origin: Option<SubscriberId>,
        persist: Persist,
        op: impl FnOnce(&mut Machine) -> io::Result<(T, Vec<(WorkspaceId, LayoutDelta)>)>,
    ) -> io::Result<T> {
        let _order = self.notify_order.lock().unwrap_or_else(|e| e.into_inner());
        let deltas;
        let value;
        {
            let mut m = self.locked();
            let before = m.clone();
            match op(&mut m).and_then(|out| {
                if *m != before {
                    match persist {
                        Persist::Now => self.persist(&m)?,
                        // Ordered with every other write by `notify_order`,
                        // which the flusher takes too: the file still moves
                        // through the states the tree moved through.
                        Persist::Soon => self.unwritten.store(true, Ordering::Release),
                    }
                }
                Ok(out)
            }) {
                Ok((v, d)) => {
                    value = v;
                    deltas = d;
                }
                Err(e) => {
                    *m = before;
                    return Err(e);
                }
            }
        }
        if !deltas.is_empty() {
            self.notify_all(&deltas, origin);
        }
        Ok(value)
    }

    /// Write out anything a [`Persist::Soon`] mutation left in memory. A no-op
    /// when there is nothing owed, so it is cheap to call on a timer.
    ///
    /// Public for the daemon's shutdown path: the observations of the last two
    /// seconds are worth one write on the way out.
    pub fn flush(&self) {
        if !self.unwritten.load(Ordering::Acquire) {
            return;
        }
        let _order = self.notify_order.lock().unwrap_or_else(|e| e.into_inner());
        let m = self.locked();
        // Cleared before the write, not after: a fact landing *during* it is
        // owed another write, and losing that flag would strand it until the
        // next one. `persist` failing sets it again below.
        self.unwritten.store(false, Ordering::Release);
        if let Err(e) = self.persist(&m) {
            log::warn!("could not write {}: {e}", self.path.display());
            self.unwritten.store(true, Ordering::Release);
        }
    }

    /// Start the flusher, once, on the first observation that owes a write.
    ///
    /// Weak, so the thread is the store's dependent rather than its owner: a
    /// dropped store (every test that makes one) ends the thread at its next
    /// tick instead of keeping the file — and the file's handle — alive for the
    /// process's life.
    fn ensure_flusher(self: &Arc<Self>) {
        if self.flushing.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("tty7-machine-flush".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(FACT_FLUSH_INTERVAL);
                    let Some(store) = weak.upgrade() else { return };
                    store.flush();
                }
            });
        if let Err(e) = spawned {
            // Fall back to writing observations synchronously: the flag says
            // one is owed, and clearing `flushing` lets the next one retry the
            // spawn. Slow beats silently losing every cwd on the machine.
            log::warn!("could not start the machine-tree flusher ({e}); writing facts inline");
            self.flushing.store(false, Ordering::Release);
            self.flush();
        }
    }

    /// Serialize the whole document and replace the file atomically. The
    /// pretty form, so a human can read and repair it — this file is the
    /// machine's memory of every workspace on it.
    ///
    /// Owner-only: the document names every workspace's directories, the SSH
    /// user and host of every native-SSH pane, and each agent's session id. A
    /// remote box running `tty7-server` is exactly where other logins are
    /// likeliest, so the file must not be created world-readable and fixed up
    /// afterwards — see [`write_atomic_private`](crate::core::config::write_atomic_private).
    fn persist(&self, m: &Machine) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(m).map_err(io::Error::other)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::core::config::write_atomic_private(&self.path, &bytes)
    }

    /// Fan the deltas out, skipping the subscriber that caused them. Called
    /// with no lock held.
    fn notify_all(&self, deltas: &[(WorkspaceId, LayoutDelta)], origin: Option<SubscriberId>) {
        let subscribers: Vec<(SubscriberId, Notify)> = self
            .subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for (workspace, delta) in deltas {
            let key = workspace.to_string();
            for (sid, f) in &subscribers {
                if Some(*sid) != origin {
                    f(&key, delta);
                }
            }
        }
    }
}

/// Find a workspace or answer the `NotFound` every op shares.
fn find_workspace(m: &mut Machine, id: WorkspaceId) -> io::Result<&mut Workspace> {
    m.workspaces
        .iter_mut()
        .find(|w| w.id == id)
        .ok_or_else(|| not_found(format!("no workspace {id} on this machine")))
}

fn find_tab(m: &mut Machine, workspace: WorkspaceId, tab: TabId) -> io::Result<&mut Tab> {
    let ws = find_workspace(m, workspace)?;
    ws.tabs
        .iter_mut()
        .find(|t| t.id == tab)
        .ok_or_else(|| not_found(format!("workspace {workspace} has no tab {tab}")))
}

/// Keep `active_tab` naming a real tab after the tab at `removed` left.
///
/// The replacement is the neighbour that slid into the removed tab's place
/// (or the new last tab), which is what every tab strip does on close.
///
/// Answers the tab that became active when the heal actually re-pointed it,
/// so the caller can broadcast the change — a client mirroring by deltas must
/// not have to re-implement this rule (see [`LayoutDelta::ActiveTabChanged`]).
fn heal_active_tab(ws: &mut Workspace, removed: usize) -> Option<TabId> {
    let named = ws
        .active_tab
        .is_some_and(|active| ws.tabs.iter().any(|t| t.id == active));
    if named || ws.tabs.is_empty() {
        ws.active_tab = ws.active_tab.filter(|_| named);
        return None;
    }
    let active = ws.tabs[removed.min(ws.tabs.len() - 1)].id;
    ws.active_tab = Some(active);
    Some(active)
}

/// Adopt a seed into the registry.
///
/// A pane already shown anywhere in the tree is **refused**: one pane has one
/// stream and one subscriber, so a second leaf on the same id would be two
/// windows silently fighting over one PTY — the exact corruption the old
/// client-side `dedupe_pane_ids` pass existed to mop up after the fact. The
/// daemon owning the tree means it can simply not happen.
///
/// Every registry record is referenced by some leaf (the close paths collect
/// orphans), so "known pane, not in any tree" cannot arise and needs no merge
/// path.
fn register_pane(m: &mut Machine, seed: PaneSeed, live: bool) -> io::Result<()> {
    let shown = m
        .workspaces
        .iter()
        .any(|w| w.tabs.iter().any(|t| t.root.contains(seed.pane)));
    if shown || m.panes.iter().any(|p| p.id == seed.pane) {
        return Err(refuse(format!(
            "pane {} is already part of this machine's tree",
            seed.pane
        )));
    }
    if m.panes.len() >= MAX_PANES {
        return Err(refuse(format!(
            "this machine's tree already references {MAX_PANES} panes"
        )));
    }
    m.panes.push(seed.into_record(live));
    Ok(())
}

/// The pane ids no leaf references any more. Computed over the whole machine
/// because a pane id means one pane — it must not be forgotten while any
/// workspace still shows it.
fn collect_orphan_panes(m: &Machine) -> Vec<u64> {
    m.panes
        .iter()
        .map(|p| p.id)
        .filter(|id| {
            !m.workspaces
                .iter()
                .any(|w| w.tabs.iter().any(|t| t.root.contains(*id)))
        })
        .collect()
}

fn clamp_ratio(ratio: f32) -> io::Result<f32> {
    if !ratio.is_finite() {
        return Err(refuse("a split ratio must be a finite number"));
    }
    Ok(ratio.clamp(0.05, 0.95))
}

/// Read the file, or start empty. A file that cannot be honoured — whether it
/// fails to parse or to *read* — is quarantined first, so the user's tree is
/// recoverable by hand rather than silently overwritten: either way the store
/// proceeds empty, and its first mutation writes the file anew.
fn load_machine(path: &Path) -> Machine {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Machine::default(),
        Err(e) => {
            // Same isolation as the parse failure below, by rename rather
            // than copy: a copy re-reads the very file that just refused to
            // be read, while a rename needs only the directory — which the
            // store can evidently write, since it is about to persist there.
            log::warn!("could not read {}; quarantining it: {e}", path.display());
            quarantine_by_rename(path);
            return Machine::default();
        }
    };
    match serde_json::from_str::<Machine>(crate::core::config::strip_bom(&text)) {
        Ok(mut machine) => {
            // PTYs die with the daemon process, so whatever the file says,
            // nothing is live in a store that was just opened. This line is
            // the whole of the restart semantic: every leaf is now "awaiting
            // revival" simply because its pane's record says so.
            for pane in &mut machine.panes {
                pane.live = false;
            }
            machine
        }
        Err(e) => {
            log::warn!("{} does not parse ({e}); quarantining it", path.display());
            quarantine(path);
            Machine::default()
        }
    }
}

// ---------------------------------------------------------------------------
// The daemon's own observations
// ---------------------------------------------------------------------------

/// The store the running daemon's pane server publishes its observations into.
///
/// A process-wide slot rather than a parameter threaded through `DaemonPane`,
/// for the same reason the control dialect's event observer is one: the
/// observers (every pane's reader thread) and the owner (the control listener
/// the daemon starts) come up independently in code that long predates the
/// tree, and each of the three pane-spawn paths would otherwise have to be
/// taught to carry an `Option<Arc<MachineStore>>` it never reads. Last install
/// wins; `None` — a process serving panes with no tree, or a unit test —
/// simply drops observations.
static OBSERVED: Mutex<Option<Arc<MachineStore>>> = Mutex::new(None);

/// Install `store` as where [`observe_pane`] lands. The daemon calls this once
/// while wiring its control services.
pub fn publish_observations(store: &Arc<MachineStore>) {
    *OBSERVED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(store));
}

/// Record an observation about `pane` — a cwd the shell reported, an agent the
/// sniffer identified, a death — in the installed store, if there is one.
///
/// Facts about panes the tree never adopted are dropped by the store itself
/// (see [`MachineStore::note_pane_facts`]), so callers report unconditionally
/// and pay nothing for a pane that is nobody's business.
pub fn observe_pane(pane: u64, f: impl FnOnce(&mut PaneRecord)) {
    let store = OBSERVED.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(store) = store {
        store.note_pane_facts(pane, f);
    }
}

/// The installed observation store, if any — for daemon-side code (the orphan
/// sweep) that wants to *read* the tree the pane server publishes into.
pub fn observed_store() -> Option<Arc<MachineStore>> {
    OBSERVED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Test-only: clear the slot again, so one test's store cannot swallow the
/// observations of unrelated tests running later in the same binary.
#[cfg(test)]
pub(crate) fn withdraw_observations() {
    *OBSERVED.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Copy a file we are about to stop honouring somewhere the user can find it.
fn quarantine(path: &Path) {
    let aside = quarantine_path(path);
    match std::fs::copy(path, &aside) {
        Ok(_) => log::warn!("the previous contents were kept at {}", aside.display()),
        Err(e) => log::warn!("could not keep a copy at {}: {e}", aside.display()),
    }
}

/// [`quarantine`] for a file that cannot be read: move it aside whole instead
/// of copying (a copy needs the read permission that just failed).
fn quarantine_by_rename(path: &Path) {
    let aside = quarantine_path(path);
    match std::fs::rename(path, &aside) {
        Ok(()) => log::warn!("the previous contents were moved to {}", aside.display()),
        Err(e) => log::warn!("could not move the file to {}: {e}", aside.display()),
    }
}

/// Where a file we are about to stop honouring is kept.
///
/// `machine.json.corrupt` when that name is free, `…corrupt.1`, `…corrupt.2` …
/// when it is not: the second corruption in a machine's life must not overwrite
/// the rescue copy of the first, which is the one with the user's tree in it.
/// After [`MAX_QUARANTINED`] the oldest name is reused — an unbounded fan of
/// files nobody reads is its own kind of mess.
fn quarantine_path(path: &Path) -> PathBuf {
    /// How many quarantined generations to keep before reusing the base name.
    const MAX_QUARANTINED: u32 = 8;

    let base = path.with_extension("json.corrupt");
    if !base.exists() {
        return base;
    }
    (1..MAX_QUARANTINED)
        .map(|n| path.with_extension(format!("json.corrupt.{n}")))
        .find(|candidate| !candidate.exists())
        .unwrap_or(base)
}

/// `<data-dir>/machine.json`.
///
/// | Order | Directory | Why |
/// |---|---|---|
/// | 1 | `$TTY7_DATA_DIR` | Explicit wins; how tests and a second server get their own file |
/// | 2 | `$XDG_DATA_HOME/tty7` | The location the design names, spelled the way XDG spells it |
/// | 3 | `$HOME/.local/share/tty7` | No `XDG_DATA_HOME` — the literal fallback path |
///
/// Deliberately **not** under the config dir. `views.json` there is the
/// *client's* view state, and a box that is both someone's laptop and someone
/// else's remote must keep the two files apart or one role would overwrite the
/// other's idea of which workspaces exist.
pub fn default_machine_path() -> io::Result<PathBuf> {
    Ok(data_dir()?.join(MACHINE_FILE))
}

fn data_dir() -> io::Result<PathBuf> {
    if let Some(explicit) = std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(explicit));
    }
    #[cfg(not(windows))]
    let base = env_dir("XDG_DATA_HOME")
        .or_else(|| env_dir("HOME").map(|h| h.join(".local").join("share")));
    #[cfg(windows)]
    let base = env_dir("LOCALAPPDATA")
        .or_else(|| env_dir("USERPROFILE").map(|h| h.join(".local").join("share")));

    base.map(|b| b.join("tty7")).ok_or_else(|| {
        io::Error::other(format!(
            "no home directory to place {MACHINE_FILE} in; set {DATA_DIR_ENV}"
        ))
    })
}

fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Arc<MachineStore>, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        (MachineStore::open(dir.path().join(MACHINE_FILE)), dir)
    }

    fn seed(pane: u64, cwd: &str) -> PaneSeed {
        PaneSeed {
            pane,
            cwd: Some(cwd.to_string()),
            ssh_spec: None,
            agent: None,
        }
    }

    /// A store, one workspace, one tab on pane 1.
    fn store_with_tab() -> (Arc<MachineStore>, tempfile::TempDir, WorkspaceId, Tab) {
        let (store, dir) = store();
        let ws = store
            .workspace_create(None, Some("api".into()), None)
            .unwrap();
        let tab = store
            .tab_create(ws.id, None, seed(1, "/work"), None, None)
            .unwrap();
        (store, dir, ws.id, tab)
    }

    /// Record every delta a subscriber hears, as `(workspace-key, delta)`.
    fn recorded(
        store: &Arc<MachineStore>,
    ) -> (Subscription, Arc<Mutex<Vec<(String, LayoutDelta)>>>) {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        let sub = store.subscribe(Arc::new(move |ws: &str, delta: &LayoutDelta| {
            sink.lock().unwrap().push((ws.to_string(), delta.clone()));
        }));
        (sub, heard)
    }

    // ── Client-minted identities ───────────────────────────────────────────

    #[test]
    fn a_client_minted_workspace_id_is_kept_and_a_duplicate_is_refused() {
        let (store, _dir) = store();
        let id = WorkspaceId::new();
        let ws = store
            .workspace_create(Some(id), Some("api".into()), None)
            .unwrap();
        assert_eq!(ws.id, id, "the id the client named is the id it gets");

        let refused = store
            .workspace_create(Some(id), None, None)
            .expect_err("a second create on the same id must refuse");
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            store.machine().workspaces.len(),
            1,
            "the refusal changed nothing"
        );
    }

    #[test]
    fn a_client_minted_tab_id_is_kept_and_a_duplicate_is_refused_anywhere() {
        let (store, _dir, ws, _tab) = store_with_tab();
        let id = TabId::new();
        let tab = store
            .tab_create(ws, None, seed(2, "/b"), Some(id), None)
            .unwrap();
        assert_eq!(tab.id, id);

        // Refused even from another workspace: tab ids are one namespace, so a
        // delta about a tab can never be ambiguous about which tab it means.
        let other = store.workspace_create(None, None, None).unwrap();
        let refused = store
            .tab_create(other.id, None, seed(3, "/c"), Some(id), None)
            .expect_err("a taken tab id must refuse");
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
        assert!(
            store.pane(3).is_none(),
            "the refused create adopted no pane either"
        );
    }

    // ── The tree survives the file ─────────────────────────────────────────

    #[test]
    fn the_tree_round_trips_through_the_file() {
        let (store, dir) = store();
        let ws = store
            .workspace_create(None, Some("api".into()), None)
            .unwrap();
        store
            .tab_create(ws.id, None, seed(1, "/work"), None, None)
            .unwrap();
        store
            .pane_split(
                ws.id,
                1,
                Axis::Vertical,
                0.3,
                seed(2, "/work/api"),
                false,
                None,
            )
            .unwrap();

        let reopened = MachineStore::open(dir.path().join(MACHINE_FILE));
        let machine = reopened.machine();
        assert_eq!(machine.workspaces.len(), 1);
        let back = &machine.workspaces[0];
        assert_eq!(back.id, ws.id, "workspace identity survives a restart");
        assert_eq!(back.name.as_deref(), Some("api"));
        assert_eq!(back.tabs.len(), 1);
        assert_eq!(back.tabs[0].root.pane_ids(), vec![1, 2]);
        match &back.tabs[0].root {
            PaneNode::Split { axis, ratio, .. } => {
                assert_eq!(*axis, Axis::Vertical);
                assert!((ratio - 0.3).abs() < 1e-6);
            }
            PaneNode::Leaf { .. } => panic!("the split has to survive"),
        }
        assert_eq!(
            machine.panes.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![1, 2],
            "the pane registry rides the same file"
        );
        assert_eq!(machine.panes[0].cwd.as_deref(), Some("/work"));
    }

    /// **The revival contract.** After a restart every pane the tree names is
    /// dead — PTYs die with the process — and the tree must say so on its own,
    /// with no client-side instance stamp to consult. The leaf stays (the
    /// layout is the thing being revived), the record keeps the facts a
    /// successor is started from, and `live` is false because it cannot be
    /// anything else in a process that spawned nothing yet.
    #[test]
    fn a_reopened_store_marks_every_pane_awaiting_revival() {
        let (store, dir) = store();
        let ws = store.workspace_create(None, None, None).unwrap();
        store
            .tab_create(ws.id, None, seed(7, "/work"), None, None)
            .unwrap();
        assert!(
            store.pane(7).unwrap().live,
            "the pane its own client just seeded is live"
        );

        let restarted = MachineStore::open(dir.path().join(MACHINE_FILE));
        let record = restarted.pane(7).expect("the record survives the restart");
        assert!(!record.live, "a restarted daemon has no live panes");
        assert_eq!(
            record.cwd.as_deref(),
            Some("/work"),
            "the facts a successor spawns from survive"
        );
        assert_eq!(
            restarted.workspace(ws.id).unwrap().tabs[0].root.pane_ids(),
            vec![7],
            "the leaf still names the dead pane: that is the revival slot"
        );
    }

    /// The daemon's registration-time liveness check. A pane that dies
    /// between its spawn and its adopting operation has its death observation
    /// dropped (`note_pane_facts` ignores panes the tree does not hold), so a
    /// seed filed `live: true` unconditionally would claim a live pane for
    /// ever — no revival offered, nothing left to flip the flag. With the
    /// probe installed, the process that owns the PTYs answers at the moment
    /// the record is born.
    #[test]
    fn a_seed_for_an_already_dead_pane_registers_as_awaiting_revival() {
        let (store, _dir) = store();
        store.set_liveness_probe(Arc::new(|id| id == 1));
        let ws = store.workspace_create(None, None, None).unwrap();
        store
            .tab_create(ws.id, None, PaneSeed::bare(1), None, None)
            .unwrap();
        store
            .pane_split(
                ws.id,
                1,
                Axis::Vertical,
                0.5,
                PaneSeed::bare(2),
                false,
                None,
            )
            .unwrap();

        assert!(store.pane(1).unwrap().live, "the probe vouched for pane 1");
        assert!(
            !store.pane(2).unwrap().live,
            "pane 2 died before its adopting op; its record must be born revivable"
        );
    }

    /// The revival itself: a fresh pane takes the leaf over, the spent record
    /// leaves the registry, and everyone else hears the whole tab.
    #[test]
    fn replacing_a_dead_pane_rebinds_the_leaf_and_spends_the_record() {
        let (store, dir) = store();
        let ws = store.workspace_create(None, None, None).unwrap();
        store
            .tab_create(ws.id, None, seed(7, "/work"), None, None)
            .unwrap();

        let restarted = MachineStore::open(dir.path().join(MACHINE_FILE));
        let (_sub, heard) = recorded(&restarted);
        restarted
            .pane_replace(ws.id, 7, seed(42, "/work"), None)
            .unwrap();

        assert_eq!(
            restarted.workspace(ws.id).unwrap().tabs[0].root.pane_ids(),
            vec![42]
        );
        assert!(restarted.pane(7).is_none(), "the old record is spent");
        assert!(restarted.pane(42).unwrap().live);
        let heard = heard.lock().unwrap();
        assert_eq!(heard.len(), 1);
        match &heard[0].1 {
            LayoutDelta::TabRestructured { tab, pane } => {
                assert_eq!(tab.root.pane_ids(), vec![42]);
                assert_eq!(pane.as_ref().map(|p| p.id), Some(42));
            }
            other => panic!("expected TabRestructured, got {other:?}"),
        }
    }

    // ── Workspace ops ──────────────────────────────────────────────────────

    #[test]
    fn workspace_create_rename_touch_delete_land_and_broadcast() {
        let (store, _dir) = store();
        let (_sub, heard) = recorded(&store);

        let ws = store
            .workspace_create(None, Some("api".into()), None)
            .unwrap();
        store
            .workspace_rename(ws.id, Some("web".into()), None)
            .unwrap();
        store.workspace_touch(ws.id, None).unwrap();
        assert_eq!(store.workspace(ws.id).unwrap().name.as_deref(), Some("web"));

        store.workspace_delete(ws.id, None).unwrap();
        assert!(store.workspace(ws.id).is_err());

        let heard = heard.lock().unwrap();
        let kinds: Vec<&LayoutDelta> = heard.iter().map(|(_, d)| d).collect();
        assert!(matches!(kinds[0], LayoutDelta::WorkspaceCreated { .. }));
        assert!(matches!(kinds[1], LayoutDelta::WorkspaceRenamed { name: Some(n) } if n == "web"));
        assert!(matches!(kinds[2], LayoutDelta::WorkspaceTouched { .. }));
        assert!(matches!(kinds[3], LayoutDelta::WorkspaceDeleted));
        assert!(
            heard.iter().all(|(key, _)| key == &ws.id.to_string()),
            "every delta names the workspace it is about"
        );
    }

    #[test]
    fn deleting_a_workspace_forgets_the_panes_only_it_referenced() {
        let (store, _dir, ws, _tab) = store_with_tab();
        let dropped = store.workspace_delete(ws, None).unwrap();
        assert_eq!(dropped, vec![1], "the caller is told which PTYs to kill");
        assert!(store.pane(1).is_none());
    }

    // ── Tab ops ────────────────────────────────────────────────────────────

    #[test]
    fn a_created_tab_lands_at_its_position_and_becomes_active() {
        let (store, _dir, ws, first) = store_with_tab();
        let second = store
            .tab_create(ws, None, seed(2, "/b"), None, None)
            .unwrap();
        let between = store
            .tab_create(ws, Some(1), seed(3, "/c"), None, None)
            .unwrap();

        let workspace = store.workspace(ws).unwrap();
        let order: Vec<TabId> = workspace.tabs.iter().map(|t| t.id).collect();
        assert_eq!(order, vec![first.id, between.id, second.id]);
        assert_eq!(workspace.active_tab, Some(between.id));

        // An out-of-range position clamps rather than refusing: the client's
        // idea of "after the last tab" can be stale by one concurrent close.
        let clamped = store
            .tab_create(ws, Some(99), seed(4, "/d"), None, None)
            .unwrap();
        assert_eq!(
            store.workspace(ws).unwrap().tabs.last().unwrap().id,
            clamped.id
        );
    }

    #[test]
    fn closing_a_tab_forgets_its_panes_and_heals_the_active_tab() {
        let (store, _dir, ws, first) = store_with_tab();
        let second = store
            .tab_create(ws, None, seed(2, "/b"), None, None)
            .unwrap();
        store.workspace_set_active_tab(ws, second.id, None).unwrap();

        let (_sub, heard) = recorded(&store);
        let dropped = store.tab_close(ws, second.id, None).unwrap();
        assert_eq!(dropped, vec![2]);
        let workspace = store.workspace(ws).unwrap();
        assert_eq!(workspace.tabs.len(), 1);
        assert_eq!(
            workspace.active_tab,
            Some(first.id),
            "the active tab may not dangle on a closed id"
        );
        // The heal is broadcast, not left for clients to re-derive: after the
        // `TabClosed` comes an `ActiveTabChanged` naming the survivor.
        assert!(
            matches!(
                heard.lock().unwrap().as_slice(),
                [
                    (_, LayoutDelta::TabClosed { tab }),
                    (_, LayoutDelta::ActiveTabChanged { tab: active })
                ] if *tab == second.id && *active == first.id
            ),
            "heard {:?}",
            heard.lock().unwrap()
        );

        heard.lock().unwrap().clear();
        let dropped = store.tab_close(ws, first.id, None).unwrap();
        assert_eq!(dropped, vec![1]);
        assert_eq!(
            store.workspace(ws).unwrap().active_tab,
            None,
            "a workspace with no tabs has no active one — the home-page state"
        );
        assert_eq!(
            heard.lock().unwrap().len(),
            1,
            "losing the last tab needs no ActiveTabChanged: no tabs, no active tab"
        );
    }

    #[test]
    fn tabs_rename_move_and_regroup_in_place() {
        let (store, _dir, ws, first) = store_with_tab();
        let second = store
            .tab_create(ws, None, seed(2, "/b"), None, None)
            .unwrap();

        store
            .tab_rename(ws, first.id, Some("build".into()), None)
            .unwrap();
        store
            .tab_set_group(ws, first.id, Some("/repo/tty7".into()), None)
            .unwrap();
        store.tab_move(ws, first.id, 1, None).unwrap();

        let workspace = store.workspace(ws).unwrap();
        assert_eq!(workspace.tabs[0].id, second.id);
        assert_eq!(workspace.tabs[1].name.as_deref(), Some("build"));
        assert_eq!(
            workspace.tabs[1].sidebar_group.as_deref(),
            Some("/repo/tty7")
        );
    }

    // ── Pane ops ───────────────────────────────────────────────────────────

    #[test]
    fn splitting_and_closing_panes_reshapes_the_tree() {
        let (store, _dir, ws, tab) = store_with_tab();
        store
            .pane_split(ws, 1, Axis::Horizontal, 0.5, seed(2, "/b"), false, None)
            .unwrap();
        store
            .pane_split(ws, 2, Axis::Vertical, 0.5, seed(3, "/c"), true, None)
            .unwrap();
        assert_eq!(
            store.workspace(ws).unwrap().tabs[0].root.pane_ids(),
            vec![1, 3, 2],
            "`first` puts the new pane on the a side"
        );

        // Closing a middle pane collapses its split; the sibling takes over.
        let dropped = store.pane_close(ws, 3, None).unwrap();
        assert_eq!(dropped, vec![3]);
        assert_eq!(
            store.workspace(ws).unwrap().tabs[0].root.pane_ids(),
            vec![1, 2]
        );

        // Closing down to one pane leaves a plain leaf, not a degenerate split.
        store.pane_close(ws, 2, None).unwrap();
        assert!(matches!(
            store.workspace(ws).unwrap().tabs[0].root,
            PaneNode::Leaf { pane: 1 }
        ));

        // Closing the last pane closes the tab itself.
        let (_sub, heard) = recorded(&store);
        store.pane_close(ws, 1, None).unwrap();
        assert!(store.workspace(ws).unwrap().tabs.is_empty());
        assert!(matches!(
            heard.lock().unwrap()[0].1,
            LayoutDelta::TabClosed { tab: id } if id == tab.id
        ));
    }

    #[test]
    fn a_ratio_change_lands_on_the_split_its_path_names() {
        let (store, _dir, ws, tab) = store_with_tab();
        store
            .pane_split(ws, 1, Axis::Horizontal, 0.5, seed(2, "/b"), false, None)
            .unwrap();
        store
            .pane_split(ws, 2, Axis::Vertical, 0.5, seed(3, "/c"), false, None)
            .unwrap();

        // The nested split lives on the b side of the root.
        store
            .pane_set_ratio(ws, tab.id, vec![Side::B], 0.7, None)
            .unwrap();
        match &store.workspace(ws).unwrap().tabs[0].root {
            PaneNode::Split { a, b, ratio, .. } => {
                assert!((ratio - 0.5).abs() < 1e-6, "the root ratio is untouched");
                assert!(matches!(&**a, PaneNode::Leaf { pane: 1 }));
                match &**b {
                    PaneNode::Split { ratio, .. } => assert!((ratio - 0.7).abs() < 1e-6),
                    PaneNode::Leaf { .. } => panic!("the nested split is gone"),
                }
            }
            PaneNode::Leaf { .. } => panic!("the root split is gone"),
        }

        // A path that no longer names a split refuses rather than guessing —
        // the client falls back to a full re-pull.
        let err = store
            .pane_set_ratio(ws, tab.id, vec![Side::A], 0.6, None)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        // Ratios clamp to sane bounds instead of letting a pane vanish.
        store
            .pane_set_ratio(ws, tab.id, vec![], 0.0001, None)
            .unwrap();
        match &store.workspace(ws).unwrap().tabs[0].root {
            PaneNode::Split { ratio, .. } => assert!(*ratio >= 0.05),
            PaneNode::Leaf { .. } => unreachable!(),
        }
    }

    #[test]
    fn moving_a_pane_between_tabs_dissolves_an_emptied_tab() {
        let (store, _dir, ws, first) = store_with_tab();
        let second = store
            .tab_create(ws, None, seed(2, "/b"), None, None)
            .unwrap();

        store
            .pane_move(ws, 2, 1, Axis::Vertical, false, None)
            .unwrap();
        let workspace = store.workspace(ws).unwrap();
        assert_eq!(workspace.tabs.len(), 1, "the emptied tab dissolved");
        assert_eq!(workspace.tabs[0].id, first.id);
        assert_eq!(workspace.tabs[0].root.pane_ids(), vec![1, 2]);
        assert!(
            !workspace.tabs.iter().any(|t| t.id == second.id),
            "the source tab is gone"
        );
        assert!(store.pane(2).is_some(), "the pane moved; it did not die");

        // Moving a pane next to itself is meaningless and refused.
        let err = store
            .pane_move(ws, 2, 2, Axis::Vertical, false, None)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    // ── Validation is refusal, not corruption ──────────────────────────────

    /// A refused operation leaves the tree byte-for-byte what it was and
    /// tells nobody anything — a delta for a change that did not happen would
    /// desynchronize every listening client at once.
    #[test]
    fn a_refused_operation_changes_nothing_and_notifies_nobody() {
        let (store, _dir, ws, tab) = store_with_tab();
        let before = store.machine();
        let (_sub, heard) = recorded(&store);

        let missing = WorkspaceId::new();
        assert!(store.workspace_rename(missing, None, None).is_err());
        assert!(
            store
                .tab_create(missing, None, seed(9, "/x"), None, None)
                .is_err()
        );
        assert!(store.tab_close(ws, TabId::new(), None).is_err());
        assert!(
            store
                .pane_split(ws, 999, Axis::Vertical, 0.5, seed(9, "/x"), false, None)
                .is_err()
        );
        assert!(store.pane_close(ws, 999, None).is_err());
        assert!(
            store
                .pane_set_ratio(ws, tab.id, vec![Side::A], 0.5, None)
                .is_err()
        );
        assert!(
            store
                .pane_set_ratio(ws, tab.id, vec![], f32::NAN, None)
                .is_err()
        );
        assert!(store.pane_replace(ws, 999, seed(9, "/x"), None).is_err());

        assert_eq!(store.machine(), before);
        assert!(heard.lock().unwrap().is_empty());
        assert!(
            store.pane(9).is_none(),
            "a seed on a refused op must not leak into the registry"
        );
    }

    // ── Origin exclusion ───────────────────────────────────────────────────

    /// The writer does not hear its own echo; everyone else does. This is the
    /// mechanism that lets a client apply its own edit optimistically and
    /// apply everyone else's from deltas without double-applying its own.
    #[test]
    fn a_delta_reaches_every_subscriber_but_its_author() {
        let (store, _dir, ws, _tab) = store_with_tab();
        let (author, heard_by_author) = recorded(&store);
        let (_other, heard_by_other) = recorded(&store);

        store
            .workspace_rename(ws, Some("renamed".into()), Some(author.id()))
            .unwrap();
        assert!(heard_by_author.lock().unwrap().is_empty());
        assert_eq!(heard_by_other.lock().unwrap().len(), 1);

        // A write with no origin reaches all.
        store.workspace_rename(ws, None, None).unwrap();
        assert_eq!(heard_by_author.lock().unwrap().len(), 1);
        assert_eq!(heard_by_other.lock().unwrap().len(), 2);
    }

    #[test]
    fn dropping_a_subscription_stops_the_deltas() {
        let (store, _dir, ws, _tab) = store_with_tab();
        let (sub, heard) = recorded(&store);
        store.workspace_touch(ws, None).unwrap();
        assert_eq!(heard.lock().unwrap().len(), 1);
        drop(sub);
        store.workspace_touch(ws, None).unwrap();
        assert_eq!(heard.lock().unwrap().len(), 1);
    }

    // ── Pane facts ─────────────────────────────────────────────────────────

    /// The daemon's own observations reach every client of every workspace
    /// showing the pane — origin exclusion does not apply, because the machine
    /// is the author and the machine is nobody's echo.
    #[test]
    fn pane_facts_update_the_record_and_reach_every_client() {
        let (store, _dir, ws, _tab) = store_with_tab();
        let (sub, heard) = recorded(&store);

        store.note_pane_facts(1, |p| {
            p.cwd = Some("/work/deeper".into());
        });
        let record = store.pane(1).unwrap();
        assert_eq!(record.cwd.as_deref(), Some("/work/deeper"));
        {
            let heard = heard.lock().unwrap();
            assert_eq!(heard.len(), 1);
            assert_eq!(heard[0].0, ws.to_string());
            assert!(matches!(&heard[0].1, LayoutDelta::PaneFacts { pane } if pane.id == 1));
        }

        // No change, no noise; an unknown pane is nobody's business.
        store.note_pane_facts(1, |_| {});
        store.note_pane_facts(999, |p| p.cwd = Some("/ghost".into()));
        assert_eq!(heard.lock().unwrap().len(), 1);
        drop(sub);
    }

    /// One pane, one leaf. A second adoption of a pane already shown is the
    /// two-windows-one-PTY corruption the old client-side dedupe pass mopped
    /// up after the fact; the daemon owning the tree refuses it up front.
    #[test]
    fn a_pane_already_in_the_tree_cannot_be_adopted_again() {
        let (store, _dir, ws, _tab) = store_with_tab();
        store.note_pane_facts(1, |p| p.cwd = Some("/observed".into()));

        let other = store.workspace_create(None, None, None).unwrap();
        let err = store
            .tab_create(other.id, None, seed(1, "/stale"), None, None)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            store.workspace(other.id).unwrap().tabs.is_empty(),
            "the refused tab must not half-exist"
        );
        assert_eq!(
            store.pane(1).unwrap().cwd.as_deref(),
            Some("/observed"),
            "and the stale seed must not clobber the daemon's own facts"
        );
        let _ = ws;
    }

    /// The pane server's side door: once a store is installed, an observation
    /// lands on the record like any other fact — and before/without one,
    /// observing is a quiet no-op, which is what lets the pane code report
    /// unconditionally.
    #[test]
    fn published_observations_land_in_the_installed_store() {
        observe_pane(1, |p| p.cwd = Some("/nowhere".into()));

        let (store, _dir, _ws, _tab) = store_with_tab();
        publish_observations(&store);
        observe_pane(1, |p| p.cwd = Some("/observed/here".into()));
        assert_eq!(
            store.pane(1).unwrap().cwd.as_deref(),
            Some("/observed/here")
        );
        withdraw_observations();
    }

    // ── Attachment ─────────────────────────────────────────────────────────

    #[test]
    fn attachments_takeover_and_are_never_persisted() {
        let (store, dir, ws, _tab) = store_with_tab();
        assert_eq!(store.attachment(ws), None);

        let laptop = Attachment::new("tok-1", "laptop");
        assert_eq!(store.attach(ws, laptop.clone()), None);
        let desktop = Attachment::new("tok-2", "desktop");
        assert_eq!(store.attach(ws, desktop.clone()), Some(laptop.clone()));

        // The preempted client tidying up must not evict the new owner.
        assert!(!store.detach(ws, &laptop.token));
        assert_eq!(store.attachment(ws).unwrap().hostname, "desktop");
        assert!(store.detach(ws, &desktop.token));
        assert_eq!(store.attachment(ws), None);

        // Attachments describe live connections; a restarted daemon has none.
        store.attach(ws, Attachment::new("secret-token", "laptop"));
        // A structural op that really changes something, to force the write:
        // `workspace_touch` is an observation (deferred to the flusher), and an
        // op that changes nothing does not write at all.
        store
            .workspace_rename(ws, Some("web".into()), None)
            .unwrap();
        let text = std::fs::read_to_string(dir.path().join(MACHINE_FILE)).unwrap();
        assert!(!text.contains("secret-token"), "{text}");
        assert_eq!(
            MachineStore::open(dir.path().join(MACHINE_FILE)).attachment(ws),
            None
        );
    }

    /// An attachment is a field of its workspace, so deleting the workspace
    /// takes it along — there is no table it could go stale in. The retired
    /// record store kept a separate attachment list and had to clear it by
    /// hand; this pins the structural guarantee that replaced that code.
    #[test]
    fn an_attachment_dies_with_its_workspace() {
        let (store, _dir, ws, _tab) = store_with_tab();
        store.attach(ws, Attachment::new("tok", "laptop"));
        assert!(store.attachment(ws).is_some());
        store.workspace_delete(ws, None).unwrap();
        assert_eq!(store.attachment(ws), None);
    }

    /// The default path ends at the documented file under the data directory —
    /// the resolution the retired record store defined and the tree inherited.
    #[test]
    fn the_default_path_ends_at_the_documented_file() {
        match default_machine_path() {
            Ok(path) => assert_eq!(
                path.file_name().and_then(|n| n.to_str()),
                Some(MACHINE_FILE)
            ),
            // No home at all (a bare CI container): the error names the
            // escape hatch rather than being a mystery.
            Err(e) => assert!(e.to_string().contains(DATA_DIR_ENV)),
        }
    }

    // ── Durability ─────────────────────────────────────────────────────────

    /// An observation reaches every client at once but does **not** write the
    /// file: these arrive per prompt per pane from the PTY reader threads, and
    /// a whole-document `fsync` each would put a disk stall in the pane's own
    /// output path and serialize every other client's edit behind it. The
    /// flusher (or the next structural edit, or an explicit `flush`) carries
    /// it to disk.
    #[test]
    fn an_observation_is_broadcast_at_once_and_written_a_little_later() {
        let (store, dir, ws, _tab) = store_with_tab();
        let path = dir.path().join(MACHINE_FILE);
        let (_sub, heard) = recorded(&store);

        store.note_pane_facts(1, |p| p.cwd = Some("/work/deeper".into()));
        assert_eq!(
            heard.lock().unwrap().len(),
            1,
            "the client hears the fact immediately; only the disk waits"
        );
        assert!(
            !std::fs::read_to_string(&path).unwrap().contains("deeper"),
            "an observation must not write the document synchronously"
        );

        store.flush();
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("deeper"),
            "…and the flush is what puts it on disk"
        );
        // Nothing owed, nothing written: the flusher's tick is free on an idle
        // machine.
        let before = std::fs::metadata(&path).unwrap().len();
        store.flush();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), before);

        // A structural edit persists the whole document, deferred facts and
        // all — so an observation can never outlive the layout change after it.
        // (Renamed to something it is not already called: an operation that
        // changes nothing writes nothing, which every path here goes through.)
        store.note_pane_facts(1, |p| p.cwd = Some("/work/deepest".into()));
        store
            .workspace_rename(ws, Some("web".into()), None)
            .unwrap();
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("deepest"),
            "a structural write carries whatever the facts left unwritten"
        );
    }

    /// The layout itself is never deferred: a structural edit is on disk before
    /// its delta goes out, so a client can never be told about a change a
    /// restart would lose.
    #[test]
    fn a_structural_edit_is_on_disk_before_anyone_hears_about_it() {
        let (store, dir) = store();
        let path = dir.path().join(MACHINE_FILE);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let path_in_callback = path.clone();
        let _sub = store.subscribe(Arc::new(move |_ws: &str, _delta: &LayoutDelta| {
            // Read from *inside* the delivery: the file has to already say
            // what this delta is about.
            sink.lock()
                .unwrap()
                .push(std::fs::read_to_string(&path_in_callback).unwrap_or_default());
        }));

        let ws = store
            .workspace_create(None, Some("api".into()), None)
            .unwrap();
        let _ = ws;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(
            seen[0].contains("api"),
            "the delta arrived before the file said so: {}",
            seen[0]
        );
    }

    // ── Corruption ─────────────────────────────────────────────────────────

    #[test]
    fn a_corrupt_file_is_quarantined_rather_than_overwritten() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(MACHINE_FILE);
        std::fs::write(&path, b"{ this is not json").unwrap();

        let store = MachineStore::open(&path);
        assert!(store.machine().workspaces.is_empty());
        store.workspace_create(None, None, None).unwrap();
        let aside = std::fs::read_to_string(path.with_extension("json.corrupt")).unwrap();
        assert_eq!(aside, "{ this is not json");

        // A second corruption gets its own name. Overwriting would spend the
        // rescue copy that has the user's tree in it on one that has garbage.
        std::fs::write(&path, b"corrupt again").unwrap();
        let store = MachineStore::open(&path);
        store.workspace_create(None, None, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(path.with_extension("json.corrupt")).unwrap(),
            "{ this is not json",
            "the first rescue copy is still the first one"
        );
        assert_eq!(
            std::fs::read_to_string(path.with_extension("json.corrupt.1")).unwrap(),
            "corrupt again"
        );
    }

    /// The document names directories, SSH users and hosts, and agent session
    /// ids. On a shared box — which a `tty7-server` machine is likeliest to be
    /// — that is nobody else's business, and it must be private from the first
    /// instant the file exists rather than chmod-ed on the next line.
    #[cfg(unix)]
    #[test]
    fn the_document_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let (store, dir) = store();
        store.workspace_create(None, None, None).unwrap();
        let mode = std::fs::metadata(dir.path().join(MACHINE_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    /// An *unreadable* file gets the same isolation as an unparseable one.
    /// Before this, only the parse path quarantined: a read failure logged,
    /// started empty — and the first mutation then overwrote the very file
    /// that could not be read. Quarantine here is by rename (a copy would
    /// need the read permission that just failed), so the bytes survive.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_moved_aside_rather_than_overwritten() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(MACHINE_FILE);
        std::fs::write(&path, b"{\"workspaces\":[]}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_to_string(&path).is_ok() {
            // Running as root (some CI containers): the permission bits do
            // not bite and the scenario cannot be staged.
            return;
        }

        let store = MachineStore::open(&path);
        store.workspace_create(None, None, None).unwrap();

        let aside = path.with_extension("json.corrupt");
        assert!(aside.exists(), "the unreadable original must be kept");
        std::fs::set_permissions(&aside, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&aside).unwrap(),
            "{\"workspaces\":[]}",
            "moved aside byte-for-byte, ready for a hand repair"
        );
    }

    /// Fields this build has never heard of survive nothing — but fields it
    /// *lacks* must not fail the parse: the schema is `#[serde(default)]`
    /// throughout so the daemon can keep evolving it.
    #[test]
    fn a_sparse_document_decodes_with_defaults() {
        let machine: Machine =
            serde_json::from_str(r#"{"workspaces":[{"tabs":[{"root":{"Leaf":{"pane":3}}}]}]}"#)
                .expect("missing fields default rather than fail");
        assert_eq!(machine.workspaces.len(), 1);
        assert_eq!(machine.workspaces[0].tabs[0].root.pane_ids(), vec![3]);
        assert!(machine.panes.is_empty());
    }
}

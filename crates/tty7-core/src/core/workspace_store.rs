//! The **remote** side of the storage split: the machine's own
//! `~/.local/share/tty7/workspaces.json`, and the one writer to it.
//!
//! # Which half of the split this is
//!
//! | Lives | Holds | Because |
//! |---|---|---|
//! | **Here**, on the machine the panes run on | The workspace list and names, the tab/pane tree, each pane's cwd / `pane_id` / agent, `last_active` | Connect from another laptop and you must see the same thing. This is a fact about the machine |
//! | The **client**'s `session.json` | Which host's which workspaces this client has opened, window geometry, the `open` flag | It is *this client's* view state. Closing a window at the office must not hide the workspace from the laptop at home |
//!
//! [`Workspace::to_remote_json`](crate::core::session::Workspace::to_remote_json)
//! is the client's half of that contract; this module is the server's.
//!
//! # Records are opaque on purpose
//!
//! A record is a [`serde_json::Value`], not a parsed
//! [`Workspace`](crate::core::session::Workspace). The server is a store, not a
//! participant: the client owns the schema, and a client newer than the server
//! it is talking to is the *normal* case (the server is installed once and then
//! left alone for months, auto-install notwithstanding). Parsing here
//! would mean a field the server has never heard of is dropped on the next
//! write — silent data loss whose only symptom is a setting that will not
//! stick.
//!
//! What the store does insist on is the shape it has to index by: a record is a
//! JSON object, and its `id` agrees with the key it was filed under. Those two
//! are what keep the file's array parseable as
//! [`Workspaces`](crate::core::session::Workspaces) by anything that wants the
//! typed view.
//!
//! # Concurrency
//!
//! Several control connections can be writing at once — two of the user's own
//! machines, or one machine reconnecting while the old link has not yet
//! noticed. One mutex covers the record list *and* the file write, so the
//! on-disk order is the in-memory order and no interleaving can produce a file
//! that never existed as a state. The write is atomic
//! ([`write_atomic`](crate::core::config::write_atomic)), so a crash mid-save
//! leaves the old file rather than half of the new one, and a write that fails
//! rolls the memory back rather than leaving the two out of step.
//!
//! Change notifications ([`WorkspaceStore::subscribe`]) are delivered
//! **outside** the lock, and the server's callback only enqueues — a peer that
//! has stopped reading its socket must not be able to stall another peer's
//! `WorkspacePut`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The file's name under the data directory.
pub const STORE_FILE: &str = "workspaces.json";

/// Overrides where the store lives. Set by tests and by a second server on a
/// shared box — the same escape hatch
/// [`CONTROL_SOCK_ENV`](crate::host::server::CONTROL_SOCK_ENV) is for the
/// socket.
pub const DATA_DIR_ENV: &str = "TTY7_DATA_DIR";

/// Ceiling on one record. A workspace with a hundred tabs is a few tens of
/// kilobytes; this is four megabytes, so it only ever catches a client that has
/// gone wrong. Without it a single `WorkspacePut` could pin the file — and the
/// memory holding it — at the 64 MiB frame limit.
pub const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;

/// Ceiling on records. Same reasoning one level up: a user has tens of
/// workspaces, and a client looping on "create workspace" should hit a named
/// error rather than grow the file until the disk fills.
pub const MAX_WORKSPACES: usize = 1024;

/// Ceiling on the whole document, which is what a single `WorkspaceList` reply
/// has to fit into.
///
/// The per-record and per-count ceilings above are independent of each other,
/// and their product is 4 GiB — sixty-four times the frame limit. Seventeen
/// accepted `WorkspacePut`s of a maximal record are enough to put the array
/// past it, and from then on *every* `WorkspaceList` on the machine is a reply
/// that cannot be encoded: every client shows an empty workspace list, and the
/// only repair is editing the file by hand. So the total is bounded where it is
/// actually known — at the save — with room to spare under
/// [`MAX_FRAME`](crate::daemon::protocol::MAX_FRAME), since what is measured
/// here is the pretty-printed form and the wire carries the compact one.
pub const MAX_STORE_BYTES: usize = 32 * 1024 * 1024;

/// Ceiling on a record key, which is a workspace uuid in every non-hostile
/// case.
const MAX_ID_BYTES: usize = 128;

// ---------------------------------------------------------------------------
// Attachment bookkeeping (the data half of M6's takeover)
// ---------------------------------------------------------------------------

/// Who is currently attached to a workspace.
///
/// **Data only.** The takeover — push `Preempted { by }` to the old
/// session, close its streams, offer a [抢回] button — is M6's, and none of it
/// is here. What is here is the record that machinery needs to exist before it
/// can be written: the random token that tells two connections from the same
/// client apart, and the hostname that fills in "已在 <主机名> 上打开". Both
/// arrive in the [`ControlHello`](crate::daemon::control::ControlHello).
///
/// **Never persisted.** An attachment describes a live connection; after a
/// server restart there are none, and a stale one on disk would make M6 report
/// a takeover against a client that no longer exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

/// Identifies one subscriber, so a writer can be told apart from the clients it
/// is notifying.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubscriberId(pub u64);

/// What a subscriber is told: the id of the workspace that changed.
///
/// Deliberately not the new contents. The event is a hint to refetch, so a
/// client that missed three of them is in the same state as one that saw all
/// three — which is what makes dropping a notification safe when a peer is
/// behind.
pub type Notify = Arc<dyn Fn(&str) + Send + Sync>;

/// A live subscription. Dropping it unsubscribes, so a connection's teardown
/// cannot leave a callback pointing at a sink nobody is reading.
pub struct Subscription {
    store: Arc<WorkspaceStore>,
    id: SubscriberId,
}

impl Subscription {
    /// This subscriber's id — pass it as the `origin` of your own writes so you
    /// are not told about changes you made yourself.
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

/// The machine's workspace records, and the file they are persisted to.
pub struct WorkspaceStore {
    path: PathBuf,
    state: Mutex<State>,
    /// Separate from `state` on purpose: attaching is not a change to the
    /// layout, does not write the file, and must not queue behind one.
    attachments: Mutex<Vec<(String, Attachment)>>,
    subscribers: Mutex<Vec<(SubscriberId, Notify)>>,
    next_subscriber: AtomicU64,
}

struct State {
    /// Insertion-ordered `(id, record)`. A `Vec` rather than a `HashMap`
    /// because the file's array order is what a client lists, and a hash map
    /// would reshuffle the picker on every save for no reason.
    records: Vec<(String, Value)>,
    /// `(mtime, len)` of the file as this snapshot last saw it, or `None` when
    /// there was no file.
    ///
    /// This store is not always the only writer. The design's answer is one
    /// server per machine, and `tty7-server --stdio` now starts the daemon
    /// rather than serving in-process for exactly that reason — but an explicit
    /// `--serve`, or a daemon that could not be started, still leaves two
    /// processes over one file. `persist` writes the *whole* document, so
    /// without noticing that the file moved underneath it, the second to save
    /// silently drops everything the first did.
    stamp: Option<(std::time::SystemTime, u64)>,
}

/// The file's identity as far as [`State::stamp`] is concerned.
fn stamp_of(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

impl WorkspaceStore {
    /// Open the store at `path`, reading whatever is there.
    ///
    /// Infallible by design, exactly like
    /// [`Workspaces::load`](crate::core::session::Workspaces::load): a machine
    /// whose workspace file is missing or unreadable must still serve files and
    /// panes. A file that does not parse is copied aside as
    /// `workspaces.json.corrupt` before anything can overwrite it, so "the
    /// store came up empty" is recoverable by hand rather than terminal.
    pub fn open(path: impl Into<PathBuf>) -> Arc<WorkspaceStore> {
        let path = path.into();
        let records = load_records(&path);
        let stamp = stamp_of(&path);
        Arc::new(WorkspaceStore {
            state: Mutex::new(State { records, stamp }),
            path,
            attachments: Mutex::new(Vec::new()),
            subscribers: Mutex::new(Vec::new()),
            next_subscriber: AtomicU64::new(1),
        })
    }

    /// Open the store at [`default_store_path`].
    pub fn shared() -> io::Result<Arc<WorkspaceStore>> {
        Ok(WorkspaceStore::open(default_store_path()?))
    }

    /// Where this store is persisted.
    pub fn path(&self) -> &Path {
        &self.path
    }

    // ----- reads -----------------------------------------------------------

    /// Every record, in file order. Answers
    /// [`WorkspaceList`](crate::daemon::control::ControlRequest::WorkspaceList).
    pub fn list(&self) -> Vec<Value> {
        self.locked()
            .records
            .iter()
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// One record. `None` means no such workspace, which the server turns into
    /// a `NotFound` — distinguishable from a workspace that exists and is
    /// empty, which a `null` payload would not be.
    pub fn get(&self, id: &str) -> Option<Value> {
        self.locked()
            .records
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, v)| v.clone())
    }

    /// How many records are on file.
    pub fn len(&self) -> usize {
        self.locked().records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // ----- writes ----------------------------------------------------------

    /// File `record` under `id`, replacing any record already there, and
    /// persist.
    ///
    /// `origin` is the subscriber that asked for the change, so it is not
    /// notified of its own write; `None` notifies everyone.
    ///
    /// The record's `id` field, if present, must agree with `id` — a mismatch
    /// would put the file's typed view at odds with the store's key, and the
    /// next client to read the array would see a workspace under the wrong
    /// identity. When absent it is filled in, so a client that only sent the
    /// body still produces a well-formed file.
    pub fn put(&self, id: &str, mut record: Value, origin: Option<SubscriberId>) -> io::Result<()> {
        check_id(id)?;
        let obj = record.as_object_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "a workspace record must be a JSON object",
            )
        })?;
        match obj.get("id") {
            Some(Value::String(existing)) if existing == id => {}
            Some(other) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("workspace record carries id {other} but was filed under {id}"),
                ));
            }
            None => {
                obj.insert("id".to_string(), Value::String(id.to_string()));
            }
        }

        let encoded = serde_json::to_vec(&record).map_err(io::Error::other)?;
        if encoded.len() > MAX_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "workspace record is {} bytes; the limit is {MAX_RECORD_BYTES}",
                    encoded.len()
                ),
            ));
        }

        {
            let mut st = self.locked();
            let existing = st.records.iter().position(|(k, _)| k == id);
            if existing.is_none() && st.records.len() >= MAX_WORKSPACES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("this machine already holds {MAX_WORKSPACES} workspaces"),
                ));
            }

            // Mutate, persist, and undo precisely if the disk said no — the
            // in-memory state is what every later read answers from, so it must
            // never claim something the file does not.
            let undo = match existing {
                Some(i) => Undo::Restore(i, std::mem::replace(&mut st.records[i].1, record)),
                None => {
                    st.records.push((id.to_string(), record));
                    Undo::Remove(st.records.len() - 1)
                }
            };
            if let Err(e) = self.persist(&st, true) {
                match undo {
                    Undo::Restore(i, old) => st.records[i].1 = old,
                    Undo::Remove(i) => {
                        st.records.remove(i);
                    }
                }
                return Err(e);
            }
            self.restamp(&mut st);
        }

        self.notify(id, origin);
        Ok(())
    }

    /// Forget a workspace. `false` means there was nothing to forget, which is
    /// still success: a delete that raced another client's delete has got what
    /// it asked for, and reporting an error would make the client retry
    /// something already done.
    pub fn delete(&self, id: &str, origin: Option<SubscriberId>) -> io::Result<bool> {
        check_id(id)?;
        {
            let mut st = self.locked();
            let Some(i) = st.records.iter().position(|(k, _)| k == id) else {
                return Ok(false);
            };
            let removed = st.records.remove(i);
            if let Err(e) = self.persist(&st, false) {
                st.records.insert(i, removed);
                return Err(e);
            }
            self.restamp(&mut st);
        }
        // The attachment goes with it: nothing can be attached to a workspace
        // that no longer exists, and leaving the entry would have M6 report a
        // takeover against a ghost.
        self.attachments_locked().retain(|(k, _)| k != id);
        self.notify(id, origin);
        Ok(true)
    }

    // ----- attachment (M6's data, not M6's behaviour) ----------------------

    /// Record `who` as the workspace's current session and answer whoever held
    /// it before.
    ///
    /// The previous holder is **the thing M6 acts on**: a `Some` return is
    /// exactly the takeover case, and the caller is the one that pushes
    /// `Preempted { by }` and closes the old streams. This function does
    /// neither — it only makes the fact available.
    pub fn attach(&self, workspace: &str, who: Attachment) -> Option<Attachment> {
        let mut slots = self.attachments_locked();
        match slots.iter_mut().find(|(k, _)| k == workspace) {
            Some((_, current)) => Some(std::mem::replace(current, who)),
            None => {
                slots.push((workspace.to_string(), who));
                None
            }
        }
    }

    /// Who is attached to `workspace`, if anyone.
    pub fn attachment(&self, workspace: &str) -> Option<Attachment> {
        self.attachments_locked()
            .iter()
            .find(|(k, _)| k == workspace)
            .map(|(_, a)| a.clone())
    }

    /// Release `workspace`, but **only if `token` still holds it**.
    ///
    /// The token check is the whole point. A preempted client tears its
    /// connection down *after* the new one has attached, and an unconditional
    /// release would have that teardown evict the client that just took over —
    /// leaving the workspace looking free while a live window is on it.
    pub fn detach(&self, workspace: &str, token: &str) -> bool {
        let mut slots = self.attachments_locked();
        let before = slots.len();
        slots.retain(|(k, a)| !(k == workspace && a.token == token));
        slots.len() != before
    }

    /// Every live attachment, for diagnostics.
    pub fn attachments(&self) -> Vec<(String, Attachment)> {
        self.attachments_locked().clone()
    }

    // ----- change notification ---------------------------------------------

    /// Be told when a record changes. Dropping the returned [`Subscription`]
    /// unsubscribes.
    ///
    /// `f` **must not block**: it runs on the thread of whichever connection
    /// made the change, so a callback that waited on a slow peer's socket would
    /// let one stalled client hold up everyone else's writes. The control
    /// server's callback enqueues onto a bounded channel and returns.
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

    /// Fan a change out, skipping the subscriber that caused it.
    ///
    /// Called with no lock held: a callback is other people's code, and holding
    /// the store's mutex across it would make every future write hostage to it.
    fn notify(&self, id: &str, origin: Option<SubscriberId>) {
        let subscribers: Vec<(SubscriberId, Notify)> = self
            .subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for (sid, f) in subscribers {
            if Some(sid) != origin {
                f(id);
            }
        }
    }

    // ----- internals -------------------------------------------------------

    fn locked(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned lock means a panic between a mutation and its write. The
        // in-memory state is still a valid state (the undo path restores it
        // before returning) and the file is either the old or the new one, so
        // carrying on is strictly better than taking the server down.
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());

        // Re-read when the file moved under us. Cheap — one `stat` — and it is
        // what keeps a second writer's changes from being overwritten by this
        // store's whole-document save, since the base we mutate is then theirs
        // rather than a snapshot from before their write. It also lets a read
        // see their changes at all: `notify` reaches subscribers in *this*
        // process only.
        let on_disk = stamp_of(&self.path);
        if on_disk != st.stamp {
            log::debug!(
                "{} changed underneath this store; re-reading",
                self.path.display()
            );
            st.records = load_records(&self.path);
            st.stamp = on_disk;
        }
        st
    }

    fn attachments_locked(&self) -> std::sync::MutexGuard<'_, Vec<(String, Attachment)>> {
        self.attachments.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Serialize the whole file and replace it atomically.
    ///
    /// The document is `{"workspaces": [...]}` — the identical shape
    /// [`Workspaces`](crate::core::session::Workspaces) parses, so this file is
    /// readable by the same code that reads a client's `session.json` and a
    /// human can diff the two.
    /// Write the whole document.
    ///
    /// `bounded` asks for [`MAX_STORE_BYTES`] to be enforced. Set by the paths
    /// that *grow* the file and clear by the ones that shrink it: a store that
    /// came up holding an over-large file — written by an older build, or by
    /// hand — must still be able to delete its way back under the limit rather
    /// than refusing every operation including the repair.
    fn persist(&self, st: &State, bounded: bool) -> io::Result<()> {
        #[derive(Serialize)]
        struct Doc<'a> {
            workspaces: Vec<&'a Value>,
        }
        let doc = Doc {
            workspaces: st.records.iter().map(|(_, v)| v).collect(),
        };
        let bytes = serde_json::to_vec_pretty(&doc).map_err(io::Error::other)?;
        if bounded && bytes.len() > MAX_STORE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "the workspace store would be {} bytes; the limit is {MAX_STORE_BYTES}, \
                     which is what one WorkspaceList reply has to fit into",
                    bytes.len()
                ),
            ));
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::core::config::write_atomic(&self.path, &bytes)
    }

    /// Record the file's identity after this store wrote it, so the next
    /// [`WorkspaceStore::locked`] does not mistake its own save for someone
    /// else's and re-read it.
    fn restamp(&self, st: &mut State) {
        st.stamp = stamp_of(&self.path);
    }
}

/// How to undo a mutation whose write failed.
enum Undo {
    Restore(usize, Value),
    Remove(usize),
}

/// A key has to be something that can key a JSON object and appear in a log
/// line. It is never used to build a path, so this is a sanity check rather
/// than a security boundary.
fn check_id(id: &str) -> io::Result<()> {
    if id.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a workspace id must not be empty",
        ));
    }
    if id.len() > MAX_ID_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("a workspace id must be at most {MAX_ID_BYTES} bytes"),
        ));
    }
    if id.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a workspace id must not contain control characters",
        ));
    }
    Ok(())
}

/// Read the file, keeping whatever is well-formed.
///
/// One unparseable *record* costs that record, not the file: a client that
/// wrote something odd should not make the user's other twelve workspaces
/// disappear. An unparseable *file* is quarantined and the store comes up
/// empty.
fn load_records(path: &Path) -> Vec<(String, Value)> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            log::warn!("could not read {}: {e}; starting empty", path.display());
            return Vec::new();
        }
    };

    let value: Value = match serde_json::from_str(crate::core::config::strip_bom(&text)) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("{} does not parse ({e}); quarantining it", path.display());
            quarantine(path);
            return Vec::new();
        }
    };
    let Some(array) = value.get("workspaces").and_then(Value::as_array) else {
        log::warn!(
            "{} has no `workspaces` array; quarantining it",
            path.display()
        );
        quarantine(path);
        return Vec::new();
    };

    let mut records: Vec<(String, Value)> = Vec::with_capacity(array.len());
    for record in array {
        let Some(id) = record.get("id").and_then(Value::as_str) else {
            log::warn!("dropping a workspace record with no string `id`");
            continue;
        };
        if check_id(id).is_err() {
            log::warn!("dropping a workspace record with an unusable id");
            continue;
        }
        if records.iter().any(|(k, _)| k == id) {
            log::warn!("dropping a duplicate record for workspace {id}");
            continue;
        }
        records.push((id.to_string(), record.clone()));
    }
    records
}

/// Copy a file we are about to stop honouring somewhere the user can find it.
/// Best effort: failing to make the backup is not a reason to refuse to start.
fn quarantine(path: &Path) {
    let aside = path.with_extension("json.corrupt");
    match std::fs::copy(path, &aside) {
        Ok(_) => log::warn!("the previous contents were kept at {}", aside.display()),
        Err(e) => log::warn!("could not keep a copy at {}: {e}", aside.display()),
    }
}

/// `<data-dir>/workspaces.json`.
///
/// | Order | Directory | Why |
/// |---|---|---|
/// | 1 | `$TTY7_DATA_DIR` | Explicit wins; how tests and a second server get their own file |
/// | 2 | `$XDG_DATA_HOME/tty7` | The location the design names, spelled the way XDG spells it |
/// | 3 | `$HOME/.local/share/tty7` | No `XDG_DATA_HOME` — the literal fallback path |
///
/// Deliberately **not** under the config dir. `session.json` there is the
/// *client's* view state, and a box that is both someone's laptop and someone
/// else's remote must keep the two files apart or one role would overwrite the
/// other's idea of which workspaces exist.
pub fn default_store_path() -> io::Result<PathBuf> {
    Ok(data_dir()?.join(STORE_FILE))
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
            "no home directory to place {STORE_FILE} in; set {DATA_DIR_ENV}"
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
    use crate::core::session::{Session, Workspace, WorkspaceId, Workspaces};
    use std::sync::atomic::AtomicUsize;

    fn store() -> (Arc<WorkspaceStore>, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = WorkspaceStore::open(dir.path().join(STORE_FILE));
        (store, dir)
    }

    fn record(id: &str, name: &str) -> Value {
        serde_json::json!({
            "id": id,
            "name": name,
            "session": {"active": 0, "tabs": [
                {"pane": {"Leaf": {"cwd": "/home/me/proj", "pane_id": 7}}}
            ]},
            "last_active": 1_753_600_000u64,
        })
    }

    /// Two stores over one file — an explicit `--serve` alongside a daemon, or
    /// a daemon that could not be started — must not silently undo each other.
    ///
    /// `persist` writes the whole document, so a store that mutates a snapshot
    /// taken before the other's write puts that stale snapshot back. This is
    /// how a workspace rename made on the laptop vanishes the next time the
    /// desktop reorders a tab, with nothing reported to either.
    #[test]
    fn a_second_writer_does_not_get_overwritten_by_a_stale_snapshot() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(STORE_FILE);
        let first = WorkspaceStore::open(&path);
        let second = WorkspaceStore::open(&path);

        first.put("w1", record("w1", "one"), None).unwrap();
        first.put("w2", record("w2", "two"), None).unwrap();

        // `second` last read the file when it was empty. It has to notice.
        second
            .put("w2", record("w2", "two, renamed"), None)
            .unwrap();

        let names: Vec<String> = WorkspaceStore::open(&path)
            .list()
            .iter()
            .map(|r| r["name"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            names,
            ["one", "two, renamed"],
            "the second writer's save dropped what the first had written"
        );
    }

    /// The same, one layer down: a read sees another process's write, because
    /// `notify` only ever reaches subscribers inside this process.
    #[test]
    fn a_read_sees_a_change_another_store_made_to_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(STORE_FILE);
        let reader = WorkspaceStore::open(&path);
        let writer = WorkspaceStore::open(&path);

        assert!(reader.get("w1").is_none());
        writer.put("w1", record("w1", "one"), None).unwrap();
        assert_eq!(
            reader.get("w1").map(|r| r["name"].clone()),
            Some(serde_json::json!("one")),
            "a read answered from a snapshot older than the file"
        );
    }

    /// The per-record and per-count ceilings do not bound their product, so the
    /// document is bounded where it is known — at the save.
    ///
    /// Past `MAX_FRAME` the store is not merely large, it is unreadable: every
    /// `WorkspaceList` becomes a reply that cannot be encoded, so every client
    /// shows an empty list and the only repair is editing the file by hand.
    #[test]
    fn a_put_that_would_outgrow_one_reply_is_refused_and_undone() {
        let (store, _dir) = store();
        // Records big enough that a handful crosses the limit, and small enough
        // that the test stays quick.
        let chunk = "x".repeat(2 * 1024 * 1024);
        let big = |id: &str| {
            let mut r = record(id, "big");
            r["padding"] = Value::String(chunk.clone());
            r
        };

        let mut accepted = 0;
        let refusal = loop {
            let id = format!("w{accepted}");
            match store.put(&id, big(&id), None) {
                Ok(()) => accepted += 1,
                Err(e) => break e,
            }
            assert!(accepted < 64, "the total was never bounded");
        };
        assert_eq!(refusal.kind(), io::ErrorKind::InvalidInput);
        assert!(
            refusal.to_string().contains("WorkspaceList"),
            "the refusal has to say what the limit is for: {refusal}"
        );

        // Refused, not half-applied: the record that did not fit is not in the
        // store and is not in the file.
        assert_eq!(store.len(), accepted);
        assert!(store.get(&format!("w{accepted}")).is_none());
        assert_eq!(WorkspaceStore::open(store.path()).len(), accepted);

        // And a delete still works, so a store that came up over the limit can
        // be repaired rather than being wedged.
        assert!(store.delete("w0", None).unwrap());
    }

    // ── The basics ──────────────────────────────────────────────────────────

    #[test]
    fn a_missing_file_is_an_empty_store_not_an_error() {
        let (store, _dir) = store();
        assert!(store.is_empty());
        assert!(store.list().is_empty());
        assert_eq!(store.get("nope"), None);
        // And deleting nothing is success, not an error.
        assert!(!store.delete("nope", None).unwrap());
    }

    #[test]
    fn put_get_list_delete_round_trip_through_the_file() {
        let (store, dir) = store();
        store.put("a", record("a", "api"), None).unwrap();
        store.put("b", record("b", "web"), None).unwrap();
        assert_eq!(store.len(), 2);
        assert_eq!(store.get("a").unwrap()["name"], "api");

        // A second store over the same path sees it: the file is the authority,
        // which is the entire reason this lives on the remote.
        let reopened = WorkspaceStore::open(dir.path().join(STORE_FILE));
        assert_eq!(reopened.len(), 2);
        assert_eq!(reopened.get("b").unwrap()["name"], "web");
        // File order is list order.
        let names: Vec<String> = reopened
            .list()
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["api", "web"]);

        assert!(store.delete("a", None).unwrap());
        assert_eq!(
            WorkspaceStore::open(dir.path().join(STORE_FILE)).len(),
            1,
            "a delete has to reach the disk, not just the map"
        );
    }

    #[test]
    fn replacing_a_record_keeps_its_place_in_the_list() {
        let (store, _dir) = store();
        for id in ["a", "b", "c"] {
            store.put(id, record(id, id), None).unwrap();
        }
        store.put("a", record("a", "renamed"), None).unwrap();
        let ids: Vec<String> = store
            .list()
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"], "a rename must not reshuffle");
        assert_eq!(store.get("a").unwrap()["name"], "renamed");
    }

    /// The file the store writes is the one `Workspaces` parses. That is what
    /// "换台电脑连过来要看到同一份" means concretely — the record a client puts
    /// comes back as the same `Workspace` on the next machine.
    #[test]
    fn the_file_is_a_workspaces_document() {
        let (store, dir) = store();
        let mut ws = Workspace::from_session(Session::default());
        ws.name = Some("api".into());
        ws.last_active = 1_753_600_000;
        let id = ws.id.to_string();
        store.put(&id, ws.to_remote_json(), None).unwrap();

        let text = std::fs::read_to_string(dir.path().join(STORE_FILE)).unwrap();
        let parsed = Workspaces::decode(&text).expect("the store's file is a Workspaces document");
        assert_eq!(parsed.workspaces.len(), 1);
        assert_eq!(parsed.workspaces[0].id, ws.id, "the identity survives");
        assert_eq!(parsed.workspaces[0].name.as_deref(), Some("api"));
        // The client's view state was never sent, so the remote's copy has the
        // defaults rather than another machine's window geometry.
        assert!(parsed.workspaces[0].window.is_none());
        assert!(!parsed.workspaces[0].is_remote());
    }

    // ── Validation ──────────────────────────────────────────────────────────

    #[test]
    fn a_record_must_be_an_object_whose_id_agrees_with_its_key() {
        let (store, _dir) = store();
        let kinds = |e: io::Error| e.kind();
        assert_eq!(
            store
                .put("a", serde_json::json!([1, 2, 3]), None)
                .map_err(kinds),
            Err(io::ErrorKind::InvalidInput)
        );
        assert_eq!(
            store
                .put("a", serde_json::json!({"id": "b"}), None)
                .map_err(kinds),
            Err(io::ErrorKind::InvalidInput),
            "filing b's record under a would put the key and the file at odds"
        );
        assert_eq!(
            store.put("", record("", "x"), None).map_err(kinds),
            Err(io::ErrorKind::InvalidInput)
        );
        assert!(store.is_empty(), "a rejected put must not be half-applied");

        // A body with no id is completed rather than refused: the key is the
        // authority and the file still ends up well-formed.
        store
            .put("a", serde_json::json!({"name": "api"}), None)
            .unwrap();
        assert_eq!(store.get("a").unwrap()["id"], "a");
    }

    #[test]
    fn oversized_and_overnumerous_records_are_refused_by_name() {
        let (store, _dir) = store();
        let huge = serde_json::json!({"name": "x".repeat(MAX_RECORD_BYTES + 16)});
        assert_eq!(
            store.put("a", huge, None).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(store.is_empty());
    }

    // ── Corruption ──────────────────────────────────────────────────────────

    #[test]
    fn a_corrupt_file_is_quarantined_rather_than_overwritten() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(STORE_FILE);
        std::fs::write(&path, b"{ this is not json").unwrap();

        let store = WorkspaceStore::open(&path);
        assert!(
            store.is_empty(),
            "an unparseable file yields an empty store"
        );
        // The user's bytes are still recoverable after the store overwrites the
        // original.
        store.put("a", record("a", "api"), None).unwrap();
        let aside = std::fs::read_to_string(path.with_extension("json.corrupt")).unwrap();
        assert_eq!(aside, "{ this is not json");
    }

    #[test]
    fn one_bad_record_does_not_cost_the_others() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(STORE_FILE);
        std::fs::write(
            &path,
            br#"{"workspaces":[
                 {"id":"a","name":"api"},
                 {"name":"no id at all"},
                 {"id":42},
                 {"id":"a","name":"duplicate"},
                 {"id":"b","name":"web"}
               ]}"#,
        )
        .unwrap();
        let store = WorkspaceStore::open(&path);
        assert_eq!(store.len(), 2);
        assert_eq!(store.get("a").unwrap()["name"], "api", "the first wins");
        assert_eq!(store.get("b").unwrap()["name"], "web");
    }

    #[test]
    fn a_utf8_bom_does_not_empty_the_store() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(STORE_FILE);
        std::fs::write(&path, "\u{FEFF}{\"workspaces\":[{\"id\":\"a\"}]}").unwrap();
        assert_eq!(WorkspaceStore::open(&path).len(), 1);
    }

    // ── Notification ────────────────────────────────────────────────────────

    #[test]
    fn a_change_notifies_every_subscriber_but_its_author() {
        let (store, _dir) = store();
        let heard_by_a = Arc::new(Mutex::new(Vec::<String>::new()));
        let heard_by_b = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = |log: &Arc<Mutex<Vec<String>>>| {
            let log = Arc::clone(log);
            Arc::new(move |id: &str| log.lock().unwrap().push(id.to_string())) as Notify
        };
        let a = store.subscribe(sink(&heard_by_a));
        let _b = store.subscribe(sink(&heard_by_b));

        // A writes: B hears about it, A does not hear its own change.
        store.put("w1", record("w1", "one"), Some(a.id())).unwrap();
        assert!(heard_by_a.lock().unwrap().is_empty());
        assert_eq!(&*heard_by_b.lock().unwrap(), &["w1".to_string()]);

        // A delete is a change too, and a write with no origin reaches all.
        store.delete("w1", Some(a.id())).unwrap();
        store.put("w2", record("w2", "two"), None).unwrap();
        assert_eq!(&*heard_by_a.lock().unwrap(), &["w2".to_string()]);
        assert_eq!(
            &*heard_by_b.lock().unwrap(),
            &["w1".to_string(), "w1".to_string(), "w2".to_string()]
        );

        // Deleting nothing changed nothing, so it says nothing.
        let before = heard_by_b.lock().unwrap().len();
        assert!(!store.delete("gone", None).unwrap());
        assert_eq!(heard_by_b.lock().unwrap().len(), before);
    }

    #[test]
    fn dropping_a_subscription_stops_the_notifications() {
        let (store, _dir) = store();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let sub = store.subscribe(Arc::new(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        store.put("a", record("a", "x"), None).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(sub);
        store.put("b", record("b", "y"), None).unwrap();
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "a torn-down connection must not still be written to"
        );
    }

    /// A rejected put changed nothing, so it must not claim otherwise.
    #[test]
    fn a_failed_put_notifies_nobody() {
        let (store, _dir) = store();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        let _sub = store.subscribe(Arc::new(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        }));
        store
            .put("a", serde_json::json!("not an object"), None)
            .ok();
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    // ── Concurrency ─────────────────────────────────────────────────────────

    /// Several connections writing at once is the normal case, not the
    /// pathological one. Every write must land, and the file must end up as a
    /// state that actually existed — not a half-written interleaving.
    #[test]
    fn concurrent_writers_all_land_and_the_file_stays_whole() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(STORE_FILE);
        let store = WorkspaceStore::open(&path);

        let threads: Vec<_> = (0..8)
            .map(|t| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    for i in 0..25 {
                        let id = format!("w{t}-{i}");
                        store.put(&id, record(&id, "x"), None).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }

        assert_eq!(store.len(), 200);
        // And the file on disk agrees, which is the part a torn write would
        // fail: it would not parse at all.
        let reopened = WorkspaceStore::open(&path);
        assert_eq!(reopened.len(), 200);
        for t in 0..8 {
            assert!(reopened.get(&format!("w{t}-24")).is_some());
        }
    }

    /// The same workspace written from two connections: last writer wins, and
    /// the loser's record is gone rather than merged into a hybrid neither
    /// client asked for.
    #[test]
    fn concurrent_writes_to_one_record_are_last_writer_wins() {
        let (store, _dir) = store();
        let a = Arc::clone(&store);
        let b = Arc::clone(&store);
        let ta = std::thread::spawn(move || {
            for _ in 0..200 {
                a.put("w", record("w", "from-a"), None).unwrap();
            }
        });
        let tb = std::thread::spawn(move || {
            for _ in 0..200 {
                b.put("w", record("w", "from-b"), None).unwrap();
            }
        });
        ta.join().unwrap();
        tb.join().unwrap();
        assert_eq!(store.len(), 1);
        let name = store.get("w").unwrap()["name"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(name == "from-a" || name == "from-b", "{name}");
    }

    // ── Attachment (M6's data) ──────────────────────────────────────────────

    #[test]
    fn attaching_reports_the_session_it_displaced() {
        let (store, _dir) = store();
        assert_eq!(store.attachment("w"), None);

        let laptop = Attachment::new("tok-1", "laptop");
        assert_eq!(
            store.attach("w", laptop.clone()),
            None,
            "nothing to preempt"
        );
        assert_eq!(store.attachment("w"), Some(laptop.clone()));

        // The second client's attach hands back the first — the exact fact M6's
        // takeover acts on.
        let desktop = Attachment::new("tok-2", "desktop");
        assert_eq!(store.attach("w", desktop.clone()), Some(laptop.clone()));
        assert_eq!(store.attachment("w"), Some(desktop));

        // The preempted client tearing down afterwards must not evict the new
        // owner: its token no longer holds the workspace.
        assert!(!store.detach("w", &laptop.token));
        assert_eq!(store.attachment("w").unwrap().hostname, "desktop");
        assert!(store.detach("w", "tok-2"));
        assert_eq!(store.attachment("w"), None);
    }

    #[test]
    fn attachments_are_scoped_to_a_workspace_and_die_with_it() {
        let (store, _dir) = store();
        store.put("w", record("w", "one"), None).unwrap();
        store.attach("w", Attachment::new("tok", "laptop"));
        store.attach("other", Attachment::new("tok", "laptop"));
        assert_eq!(store.attachments().len(), 2);

        store.delete("w", None).unwrap();
        assert_eq!(store.attachment("w"), None);
        assert!(store.attachment("other").is_some());
    }

    /// Attachments describe live connections, so they must not outlive the
    /// process that held them.
    #[test]
    fn attachments_are_never_written_to_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(STORE_FILE);
        let store = WorkspaceStore::open(&path);
        store.put("w", record("w", "one"), None).unwrap();
        store.attach("w", Attachment::new("secret-token", "laptop"));

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("secret-token"), "{text}");
        assert!(!text.contains("laptop"), "{text}");
        assert_eq!(
            WorkspaceStore::open(&path).attachment("w"),
            None,
            "a restarted server has no attached clients"
        );
    }

    // ── Path resolution ─────────────────────────────────────────────────────

    #[test]
    fn the_store_path_ends_at_the_documented_file() {
        // `TTY7_DATA_DIR` is process-global, so this only asserts the shape the
        // resolution produces rather than setting the variable under other
        // tests running beside it.
        let p = WorkspaceStore::open(PathBuf::from("/srv/data/tty7").join(STORE_FILE));
        assert!(p.path().ends_with("tty7/workspaces.json"));
    }

    #[test]
    fn a_workspace_id_is_a_usable_store_key() {
        let (store, _dir) = store();
        let id = WorkspaceId::new().to_string();
        store.put(&id, record(&id, "api"), None).unwrap();
        assert!(store.get(&id).is_some());
    }
}

//! [`HostOps`] — the only place in the GUI that is allowed to touch a
//! [`Host`].
//!
//! Every [`Host`] method blocks (`tty7_core::host` explains why), and a blocked
//! UI thread is a frozen window. On a local host that is a stutter nobody
//! notices; on a remote one it is a quarter-second freeze per directory
//! expanded. So the rule is absolute: **no view calls a host directly.** Views
//! hand the work to `HostOps`, which runs it on the background executor and
//! lands the result back on the UI thread.
//!
//! That makes this module a chokepoint on purpose. Being the single door is
//! what lets in-flight de-duplication, staleness checks, error notification and
//! the "don't ask a disconnected host" rule live in one place instead of being
//! re-derived, differently, at every call site. It is also what makes the CI
//! grep enforceable: `src/ui/` and `src/terminal/` may not contain `std::fs::`,
//! `Command::new("git")`, `.canonicalize()` or `.is_absolute()` anywhere but
//! here.
//!
//! # The shape every call takes
//!
//! ```ignore
//! HostOps::run(
//!     host,
//!     cx,
//!     move |h| h.read_dir(&dir, Some(&root)),      // background: blocking
//!     move |view, listed, cx| { /* UI thread */ }, // land it
//! );
//! ```
//!
//! The closure gets `&dyn Host` rather than the `Arc`, so it cannot stash the
//! host somewhere that outlives the request. The landing closure runs on the UI
//! thread with the view borrowed mutably — and, because it may run arbitrarily
//! later, is where the staleness check belongs.
//!
//! # Four things a landing closure has to get right
//!
//! | Concern | What to do |
//! |---|---|
//! | **De-duplication** | Keep an [`InFlight`] per request kind. Render is called every frame and will re-ask for the same directory until an answer lands. |
//! | **Staleness** | Re-check the precondition before landing: the text the completion was for, the directory the listing was of. `InFlight::finish` answers this for keyed requests. |
//! | **Errors** | [`HostOps::notify_err`] unless the pre-existing behaviour was deliberately silent (the git probes are). Silence is how a failed save becomes a lost file. |
//! | **Disconnection** | `!host.is_connected()` means don't ask — keep showing the last good answer rather than replacing it with an error. |

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use gpui::{App, Context, Window};
use gpui_component::WindowExt as _;

// The host vocabulary, re-exported so a view imports everything it needs from
// the one module it is allowed to import hosts from. `allow`, because in a
// binary crate a re-export nothing has consumed *yet* reads as unused.
#[allow(unused_imports)]
pub use tty7_core::host::{
    Entry, Host, HostId, MTime, Meta, Output, SearchHit, SharedHost, WatchSub,
};

/// Where blocking [`Host`] calls actually run.
///
/// **Not gpui's background executor.** On Linux that is a fixed pool of
/// `available_parallelism().max(2)` worker threads with no separate blocking
/// tier, so N stalled host calls on an N-core client occupy every worker there
/// is. Everything else that uses `background_executor` then queues behind them
/// — including the reconnect in `remote_workspace::launch_attempt`, which is
/// the one thing that would clear the stall. Expanding a subtree on a link that
/// has gone silent is enough: one call per directory, each parked for its
/// deadline (5s for a `ReadDir`, 30s for a `ReadFile`). macOS is far less
/// exposed, since libdispatch grows its global queues when their threads block,
/// which is why this does not show up in development.
///
/// Elastic and its own: a thread per concurrent call, reused while warm and
/// retired after [`LINGER`], capped at [`MAX_THREADS`]. A `Host` call is
/// user-driven — a directory expanded, a file opened — not per-frame, so the
/// steady state is one or two threads.
mod blocking {
    use std::collections::VecDeque;
    use std::sync::{Arc, Condvar, Mutex, OnceLock};
    use std::time::Duration;

    type Job = Box<dyn FnOnce() + Send + 'static>;

    /// Ceiling on threads. Deliberately well above any core count: these are
    /// parked on a socket rather than competing for CPU, and what has to fit is
    /// the number of host calls in flight — one per expanded directory in a
    /// burst, plus whatever the editor and the git probes are doing.
    const MAX_THREADS: usize = 64;

    /// How long an idle worker waits for more work before retiring.
    const LINGER: Duration = Duration::from_secs(30);

    struct Inner {
        state: Mutex<State>,
        wake: Condvar,
    }

    struct State {
        jobs: VecDeque<Job>,
        threads: usize,
        /// Workers parked in `wait_timeout`, counted from before they park
        /// until after they have re-acquired the lock on the way out.
        idle: usize,
    }

    impl State {
        /// Whether a job just queued needs a thread spawned for it.
        ///
        /// Compares the backlog against the parked workers rather than asking
        /// whether *any* worker is parked: `idle` still counts a worker that
        /// has been handed a job but has not yet woken, and the job meant for
        /// it is still in `jobs`, so counting both sides cancels the window out.
        fn wants_another_thread(&self) -> bool {
            self.jobs.len() > self.idle && self.threads < MAX_THREADS
        }
    }

    fn pool() -> &'static Arc<Inner> {
        static POOL: OnceLock<Arc<Inner>> = OnceLock::new();
        POOL.get_or_init(|| {
            Arc::new(Inner {
                state: Mutex::new(State {
                    jobs: VecDeque::new(),
                    threads: 0,
                    idle: 0,
                }),
                wake: Condvar::new(),
            })
        })
    }

    /// Queue `job`. Never refuses: a dropped job is a `Host` call whose caller
    /// waits forever, and at this cap the backlog is a better failure than that.
    pub(super) fn submit(job: impl FnOnce() + Send + 'static) {
        let inner = pool();
        let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
        st.jobs.push_back(Box::new(job));
        if st.wants_another_thread() {
            st.threads += 1;
            let spawned = Arc::clone(inner);
            match std::thread::Builder::new()
                .name("tty7-host-op".into())
                .spawn(move || worker(spawned))
            {
                Ok(_) => return,
                Err(e) => {
                    // Out of threads: leave the job for whoever is already
                    // running. With nobody at all there is no one to run it, so
                    // run it here — blocking this caller, which is the lesser
                    // harm against never answering.
                    st.threads -= 1;
                    log::warn!("could not start a host-op thread: {e}");
                    if st.threads == 0
                        && let Some(job) = st.jobs.pop_back()
                    {
                        drop(st);
                        job();
                        return;
                    }
                }
            }
        }
        drop(st);
        inner.wake.notify_one();
    }

    fn worker(inner: Arc<Inner>) {
        loop {
            let job = {
                let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(job) = st.jobs.pop_front() {
                        break job;
                    }
                    st.idle += 1;
                    let (guard, timeout) = inner
                        .wake
                        .wait_timeout(st, LINGER)
                        .unwrap_or_else(|e| e.into_inner());
                    st = guard;
                    st.idle -= 1;
                    if timeout.timed_out() && st.jobs.is_empty() {
                        st.threads -= 1;
                        return;
                    }
                }
            };
            job();
        }
    }
}

/// Run `f` on the blocking pool and await its result.
///
/// `None` means the job was dropped without running, which happens only when
/// the process is going down — the caller lands nothing rather than inventing
/// an answer.
async fn off_thread<T, F>(f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = smol::channel::bounded(1);
    blocking::submit(move || {
        let _ = tx.send_blocking(f());
    });
    rx.recv().await.ok()
}

/// The GPUI-facing facade over [`Host`].
///
/// A unit struct rather than a value: there is no per-instance state, and
/// making it a namespace keeps every call site reading `HostOps::run(host, …)`,
/// which is what the CI grep and code review look for.
pub struct HostOps;

impl HostOps {
    /// Run `f` against `host` on the background executor, then hand its result
    /// to `land` on the UI thread.
    ///
    /// The task detaches: if the view is gone by the time the answer arrives,
    /// `land` simply never runs. Nothing here can fail in a way the caller
    /// needs to handle — a `Host` method's own failure is part of `T`, usually
    /// as an `io::Result`.
    pub fn run<T, E, F, L>(host: SharedHost, cx: &mut Context<E>, f: F, land: L)
    where
        E: 'static,
        T: Send + 'static,
        F: FnOnce(&dyn Host) -> T + Send + 'static,
        L: FnOnce(&mut E, T, &mut Context<E>) + 'static,
    {
        // Whoever is calling this is, by construction, on the UI thread — which
        // makes this the natural place to teach the core's debug guard what the
        // UI thread is. Idempotent, so paying for it per call is a store the
        // optimizer sees through after the first.
        tty7_core::host::register_ui_thread();
        cx.spawn(async move |this, cx| {
            let Some(out) = off_thread(move || f(&*host)).await else {
                return;
            };
            let _ = this.update(cx, |view, cx| land(view, out, cx));
        })
        .detach();
    }

    /// [`HostOps::run`] for work whose result belongs to the *app*, not to the
    /// view that asked for it — so it lands even if that view is gone.
    ///
    /// [`run`](HostOps::run) drops its landing closure when the entity dies,
    /// which is right for anything that updates the view and wrong for anything
    /// that releases a shared claim. The git probe is the latter: its in-flight
    /// entry in the process-wide `GitStatusCache` is keyed by `(host, cwd)`, not
    /// by pane, so a pane closed mid-probe that never released its claim would
    /// wedge the branch line of every *other* pane in that directory —
    /// permanently, since nothing else ever clears it.
    ///
    /// The landing closure gets `&mut App` and no view. When it needs one
    /// anyway, it captures a `WeakEntity` and updates through it, which makes
    /// "the shared part always happens, the view part happens if there is a
    /// view" explicit instead of accidental.
    pub fn run_detached<T, E, F, L>(host: SharedHost, cx: &mut Context<E>, f: F, land: L)
    where
        E: 'static,
        T: Send + 'static,
        F: FnOnce(&dyn Host) -> T + Send + 'static,
        L: FnOnce(&mut App, T) + 'static,
    {
        tty7_core::host::register_ui_thread();
        cx.spawn(async move |_this, cx| {
            let Some(out) = off_thread(move || f(&*host)).await else {
                return;
            };
            cx.update(|cx| land(cx, out));
        })
        .detach();
    }

    /// [`HostOps::run`] for work that lands with a window — anything that
    /// notifies, focuses, or opens a dialog on completion.
    pub fn run_in<T, E, F, L>(host: SharedHost, window: &Window, cx: &mut Context<E>, f: F, land: L)
    where
        E: 'static,
        T: Send + 'static,
        F: FnOnce(&dyn Host) -> T + Send + 'static,
        L: FnOnce(&mut E, T, &mut Window, &mut Context<E>) + 'static,
    {
        tty7_core::host::register_ui_thread();
        cx.spawn_in(window, async move |this, cx| {
            let Some(out) = off_thread(move || f(&*host)).await else {
                return;
            };
            let _ = this.update_in(cx, |view, window, cx| land(view, out, window, cx));
        })
        .detach();
    }

    /// Land an `io::Result`, notifying on failure and calling `land` on success.
    ///
    /// The overwhelmingly common shape for a *write*: create, rename, delete,
    /// save. Failing silently is how a rejected save turns into a file the user
    /// believes they wrote, so the error path is not optional here — a call
    /// site that genuinely wants silence uses [`HostOps::run`] and says so.
    ///
    /// `context` prefixes the message ("Delete failed", "Could not rename").
    pub fn run_or_notify<T, E, F, L>(
        host: SharedHost,
        window: &Window,
        cx: &mut Context<E>,
        context: impl Into<String>,
        f: F,
        land: L,
    ) where
        E: 'static,
        T: Send + 'static,
        F: FnOnce(&dyn Host) -> std::io::Result<T> + Send + 'static,
        L: FnOnce(&mut E, T, &mut Window, &mut Context<E>) + 'static,
    {
        let context = context.into();
        Self::run_in(
            host,
            window,
            cx,
            f,
            move |view, result, window, cx| match result {
                Ok(value) => land(view, value, window, cx),
                Err(e) => HostOps::notify_err(window, cx, &context, &e),
            },
        );
    }

    /// Show a host failure to the user.
    ///
    /// One funnel so the wording stays consistent and so there is a single place
    /// to special-case a disconnected host later (a reconnect banner reads
    /// better than twelve "Connection reset" toasts).
    pub fn notify_err(window: &mut Window, cx: &mut App, context: &str, err: &std::io::Error) {
        window.push_notification(format!("{context}: {err}"), cx);
    }
}

/// Which requests are out, and which of those were superseded while they flew.
///
/// Two problems, one structure. Render re-asks for the same missing directory
/// every frame until an answer lands, so without `in_flight` a slow listing
/// becomes a listing per frame. And a filesystem change arriving mid-request
/// would otherwise let the pre-change answer win the race, so `stale` makes
/// that answer discard itself and the next render ask again.
///
/// Lifted out of the file tree, where it was `Loads`, because every consumer of
/// a remote host needs it: a round trip is long enough that *every* request kind
/// can be superseded before it lands.
pub struct InFlight<K: Eq + Hash + Clone> {
    in_flight: HashSet<K>,
    stale: HashSet<K>,
}

impl<K: Eq + Hash + Clone> Default for InFlight<K> {
    fn default() -> Self {
        InFlight {
            in_flight: HashSet::new(),
            stale: HashSet::new(),
        }
    }
}

impl<K: Eq + Hash + Clone> InFlight<K> {
    /// `true` when the caller should spawn — nothing is out for `key` yet.
    pub fn begin(&mut self, key: K) -> bool {
        self.in_flight.insert(key)
    }

    /// Record that something superseded whatever is in flight for `key`. A
    /// no-op when nothing is.
    pub fn invalidate(&mut self, key: &K) {
        if self.in_flight.contains(key) {
            self.stale.insert(key.clone());
        }
    }

    /// Every outstanding answer is now stale — for when a whole cache goes.
    pub fn invalidate_all(&mut self) {
        self.stale.extend(self.in_flight.iter().cloned());
    }

    /// Retire the request for `key`: `true` when its answer is still current and
    /// may be used, `false` when it must be thrown away.
    pub fn finish(&mut self, key: &K) -> bool {
        self.in_flight.remove(key);
        !self.stale.remove(key)
    }

    /// Whether a request for `key` is outstanding.
    pub fn is_pending(&self, key: &K) -> bool {
        self.in_flight.contains(key)
    }

    /// The keys with a request outstanding, for callers that have to reason
    /// about the set rather than about one key.
    pub fn pending_keys(&self) -> impl Iterator<Item = &K> {
        self.in_flight.iter()
    }

    /// How many requests are outstanding.
    pub fn len(&self) -> usize {
        self.in_flight.len()
    }

    /// Whether nothing is outstanding.
    pub fn is_empty(&self) -> bool {
        self.in_flight.is_empty()
    }
}

/// A per-host cache keyed by whatever the consumer needs.
///
/// The reason this exists rather than a bare `HashMap<PathBuf, V>`: two
/// workspaces on two machines can hold the identical path (`/home/me/proj` is
/// on both), and a map keyed by path alone would serve one machine's listing
/// for the other's directory. Keying by host as well makes that impossible by
/// construction rather than by everyone remembering.
///
/// # Why a map of maps rather than one map keyed by `(HostId, K)`
///
/// Because a tuple key cannot be *borrowed*. `HashMap<(HostId, PathBuf), V>`
/// can only be probed with a real `(HostId, PathBuf)`, so every read would have
/// to clone the `PathBuf` just to ask a question — and these caches are read
/// from `render`: the sidebar asks for every tab's branch line and grouping key
/// on every frame, which turned into thousands of throwaway allocations a
/// second. Nesting keeps the inner map a plain `HashMap<K, V>`, so lookups go
/// through [`Borrow`] and cost nothing: `cache.get(host, path)` takes a
/// `&Path` against a `PathBuf` key.
///
/// It also makes `clear_host` a single `remove` instead of a full-table
/// `retain`.
pub struct ByHost<K: Eq + Hash, V> {
    map: HashMap<HostId, HashMap<K, V>>,
}

impl<K: Eq + Hash, V> Default for ByHost<K, V> {
    fn default() -> Self {
        ByHost {
            map: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash, V> ByHost<K, V> {
    /// Look up `key` on `host`.
    ///
    /// Generic over the borrowed form of the key, like [`HashMap::get`] itself,
    /// so a `ByHost<PathBuf, _>` can be probed with a `&Path`.
    pub fn get<Q>(&self, host: HostId, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.map.get(&host)?.get(key)
    }

    /// Insert `value` for `key` on `host`.
    pub fn insert(&mut self, host: HostId, key: K, value: V) -> Option<V> {
        self.map.entry(host).or_default().insert(key, value)
    }

    /// Remove `key` on `host`.
    pub fn remove<Q>(&mut self, host: HostId, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.map.get_mut(&host)?.remove(key)
    }

    /// Every `(host, key)` currently cached.
    ///
    /// For the invalidations that reach the whole cache without emptying it:
    /// marking each entry for a refresh needs the keys, and dropping them
    /// instead is what makes a remote tree blink.
    pub fn keys(&self) -> impl Iterator<Item = (HostId, &K)> {
        self.map
            .iter()
            .flat_map(|(host, inner)| inner.keys().map(move |k| (*host, k)))
    }

    /// Drop everything belonging to `host` — what a disconnect or a closed
    /// workspace does, without disturbing the other machines' entries.
    pub fn clear_host(&mut self, host: HostId) {
        self.map.remove(&host);
    }

    /// Drop everything.
    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// How many entries are cached across all hosts.
    pub fn len(&self) -> usize {
        self.map.values().map(HashMap::len).sum()
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.map.values().all(HashMap::is_empty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// The three-state dance the file tree's loads have always done: one
    /// request out per key, an invalidation mid-flight makes the answer
    /// discardable, and a clean finish keeps it.
    #[test]
    fn in_flight_tracks_supersession() {
        let mut loads: InFlight<PathBuf> = InFlight::default();
        let a = PathBuf::from("/a");
        let b = PathBuf::from("/b");

        assert!(loads.begin(a.clone()), "first request spawns");
        assert!(!loads.begin(a.clone()), "a repeat frame does not");
        assert!(loads.is_pending(&a));
        assert_eq!(loads.len(), 1);

        // Nothing superseded it: the answer is good.
        assert!(loads.finish(&a));
        assert!(!loads.is_pending(&a));
        assert!(loads.is_empty());

        // Superseded while in flight: the answer must be dropped, and the key
        // is clean again afterwards so the next request is not poisoned.
        assert!(loads.begin(a.clone()));
        loads.invalidate(&a);
        assert!(!loads.finish(&a));
        assert!(loads.begin(a.clone()));
        assert!(loads.finish(&a));

        // Invalidating something that is not out is a no-op, not a booby trap
        // for the next request.
        loads.invalidate(&b);
        assert!(loads.begin(b.clone()));
        assert!(loads.finish(&b));
    }

    /// A whole-cache invalidation reaches every outstanding request, and only
    /// the outstanding ones.
    #[test]
    fn invalidate_all_covers_everything_in_flight() {
        let mut loads: InFlight<u32> = InFlight::default();
        loads.begin(1);
        loads.begin(2);
        loads.invalidate_all();
        assert!(!loads.finish(&1));
        assert!(!loads.finish(&2));
        assert!(loads.is_empty());

        // And a request started *after* the invalidation is not stale.
        loads.begin(3);
        assert!(loads.finish(&3));
    }

    /// The same path on two machines is two entries. This is the bug the type
    /// exists to make unrepresentable: `/home/me/proj` is a real path on the
    /// laptop *and* on the remote box.
    #[test]
    fn by_host_keys_by_machine_as_well_as_path() {
        let remote = HostId::from_connection_key("ssh-direct:me@box:22");
        let mut cache: ByHost<PathBuf, &str> = ByHost::default();
        let p = PathBuf::from("/home/me/proj");

        cache.insert(HostId::LOCAL, p.clone(), "local listing");
        cache.insert(remote, p.clone(), "remote listing");
        assert_eq!(cache.get(HostId::LOCAL, &p), Some(&"local listing"));
        assert_eq!(cache.get(remote, &p), Some(&"remote listing"));
        assert_eq!(cache.len(), 2);

        // A disconnect drops one machine's entries and leaves the other's.
        cache.clear_host(remote);
        assert_eq!(cache.get(remote, &p), None);
        assert_eq!(cache.get(HostId::LOCAL, &p), Some(&"local listing"));

        cache.remove(HostId::LOCAL, &p);
        assert!(cache.is_empty());
    }

    /// A `ByHost<PathBuf, _>` is probed with a borrowed `&Path`, no allocation.
    ///
    /// Not a micro-optimisation: `GitStatusCache::status_for` and
    /// `known_repo_for` are read from `render`, once per tab per frame, so a
    /// key that had to be cloned to be asked about meant thousands of throwaway
    /// `PathBuf`s a second. This pins the borrowed lookup so a future
    /// "simplification" back to a `(HostId, K)` tuple key fails here.
    #[test]
    fn lookups_borrow_the_key_rather_than_cloning_it() {
        let mut cache: ByHost<PathBuf, u32> = ByHost::default();
        cache.insert(HostId::LOCAL, PathBuf::from("/a/b"), 1);
        // `&Path`, not `&PathBuf` — this is the line that would stop compiling.
        assert_eq!(cache.get(HostId::LOCAL, Path::new("/a/b")), Some(&1));
        assert_eq!(cache.remove(HostId::LOCAL, Path::new("/a/b")), Some(1));
        assert!(cache.is_empty());
    }
}

/// The one `HostOps` guarantee that cannot be checked by reading the code: that
/// a `run_detached` result still lands after the view that asked for it is
/// gone.
#[cfg(test)]
mod gpui_tests {
    use crate::ui::host_ops::{Host, HostOps};
    use gpui::{App, AppContext, Context, Entity, TestAppContext};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Stand-in for a pane: something to own the request and then stop existing.
    struct Pane;

    /// `run_detached` exists for exactly this: the git probe releases its
    /// in-flight claim in the landing closure, and that claim is keyed by
    /// `(host, cwd)` rather than by pane. If closing a pane mid-probe could
    /// swallow the landing, the claim would never be released and the branch
    /// line of every *other* pane in that directory would stop updating —
    /// permanently, since nothing else clears it.
    ///
    /// The contrast with `run` is the point of having both, so both are
    /// asserted here: `run` is view-scoped and *should* drop its landing.
    #[gpui::test]
    fn a_detached_result_lands_after_its_view_is_dropped(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let detached: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let view_scoped: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));

        let pane: Entity<Pane> = cx.new(|_cx: &mut Context<Pane>| Pane);
        let _: () = pane.update(cx, |_pane: &mut Pane, cx: &mut Context<Pane>| {
            let d = Arc::clone(&detached);
            HostOps::run_detached(
                tty7_core::host::local::LocalHost::new(),
                cx,
                |_h: &dyn Host| 7usize,
                move |_app: &mut App, n: usize| {
                    d.fetch_add(n, Ordering::SeqCst);
                },
            );
            let v = Arc::clone(&view_scoped);
            HostOps::run(
                tty7_core::host::local::LocalHost::new(),
                cx,
                |_h: &dyn Host| 7usize,
                move |_pane: &mut Pane, n: usize, _cx: &mut Context<Pane>| {
                    v.fetch_add(n, Ordering::SeqCst);
                },
            );
        });

        // The pane goes away before either answer can land — a tab closed
        // while its probe was in flight.
        drop(pane);

        // Pumped rather than parked once: a `Host` call runs on `HostOps`' own
        // thread pool, not on gpui's executor, so `run_until_parked` has
        // nothing to wait for until the answer has already crossed back. The
        // deadline is generous because what is being timed is a closure that
        // returns a constant.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while detached.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            cx.background_executor.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        // Both were submitted together, so the detached one landing means the
        // view-scoped one has had its chance too. A few more turns, in case it
        // is merely slower.
        for _ in 0..20 {
            cx.background_executor.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        assert_eq!(
            detached.load(Ordering::SeqCst),
            7,
            "run_detached must land: it is what releases the shared claim"
        );
        assert_eq!(
            view_scoped.load(Ordering::SeqCst),
            0,
            "run is view-scoped and must not run against a dead view"
        );
    }
}

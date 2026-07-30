use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use gpui::{App, Context, Window};
use gpui_component::WindowExt as _;

#[allow(unused_imports)]
pub use tty7_core::host::{
    Entry, Host, HostId, MTime, Meta, Output, SearchHit, SharedHost, WatchSub,
};

mod blocking {
    use std::collections::VecDeque;
    use std::sync::{Arc, Condvar, Mutex, OnceLock};
    use std::time::Duration;

    type Job = Box<dyn FnOnce() + Send + 'static>;

    const MAX_THREADS: usize = 64;

    const LINGER: Duration = Duration::from_secs(30);

    struct Inner {
        state: Mutex<State>,
        wake: Condvar,
    }

    struct State {
        jobs: VecDeque<Job>,
        threads: usize,
        idle: usize,
    }

    impl State {
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

pub struct HostOps;

impl HostOps {
    pub fn run<T, E, F, L>(host: SharedHost, cx: &mut Context<E>, f: F, land: L)
    where
        E: 'static,
        T: Send + 'static,
        F: FnOnce(&dyn Host) -> T + Send + 'static,
        L: FnOnce(&mut E, T, &mut Context<E>) + 'static,
    {
        tty7_core::host::register_ui_thread();
        cx.spawn(async move |this, cx| {
            let Some(out) = off_thread(move || f(&*host)).await else {
                return;
            };
            let _ = this.update(cx, |view, cx| land(view, out, cx));
        })
        .detach();
    }

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

    pub fn notify_err(window: &mut Window, cx: &mut App, context: &str, err: &std::io::Error) {
        window.push_notification(format!("{context}: {err}"), cx);
    }
}

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
    pub fn begin(&mut self, key: K) -> bool {
        self.in_flight.insert(key)
    }

    pub fn invalidate(&mut self, key: &K) {
        if self.in_flight.contains(key) {
            self.stale.insert(key.clone());
        }
    }

    pub fn invalidate_all(&mut self) {
        self.stale.extend(self.in_flight.iter().cloned());
    }

    pub fn finish(&mut self, key: &K) -> bool {
        self.in_flight.remove(key);
        !self.stale.remove(key)
    }

    pub fn is_pending(&self, key: &K) -> bool {
        self.in_flight.contains(key)
    }

    pub fn pending_keys(&self) -> impl Iterator<Item = &K> {
        self.in_flight.iter()
    }

    pub fn len(&self) -> usize {
        self.in_flight.len()
    }

    pub fn is_empty(&self) -> bool {
        self.in_flight.is_empty()
    }
}

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
    pub fn get<Q>(&self, host: HostId, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.map.get(&host)?.get(key)
    }

    pub fn insert(&mut self, host: HostId, key: K, value: V) -> Option<V> {
        self.map.entry(host).or_default().insert(key, value)
    }

    pub fn remove<Q>(&mut self, host: HostId, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.map.get_mut(&host)?.remove(key)
    }

    pub fn keys(&self) -> impl Iterator<Item = (HostId, &K)> {
        self.map
            .iter()
            .flat_map(|(host, inner)| inner.keys().map(move |k| (*host, k)))
    }

    pub fn clear_host(&mut self, host: HostId) {
        self.map.remove(&host);
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn len(&self) -> usize {
        self.map.values().map(HashMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.map.values().all(HashMap::is_empty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn in_flight_tracks_supersession() {
        let mut loads: InFlight<PathBuf> = InFlight::default();
        let a = PathBuf::from("/a");
        let b = PathBuf::from("/b");

        assert!(loads.begin(a.clone()), "first request spawns");
        assert!(!loads.begin(a.clone()), "a repeat frame does not");
        assert!(loads.is_pending(&a));
        assert_eq!(loads.len(), 1);

        assert!(loads.finish(&a));
        assert!(!loads.is_pending(&a));
        assert!(loads.is_empty());

        assert!(loads.begin(a.clone()));
        loads.invalidate(&a);
        assert!(!loads.finish(&a));
        assert!(loads.begin(a.clone()));
        assert!(loads.finish(&a));

        loads.invalidate(&b);
        assert!(loads.begin(b.clone()));
        assert!(loads.finish(&b));
    }

    #[test]
    fn invalidate_all_covers_everything_in_flight() {
        let mut loads: InFlight<u32> = InFlight::default();
        loads.begin(1);
        loads.begin(2);
        loads.invalidate_all();
        assert!(!loads.finish(&1));
        assert!(!loads.finish(&2));
        assert!(loads.is_empty());

        loads.begin(3);
        assert!(loads.finish(&3));
    }

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

        cache.clear_host(remote);
        assert_eq!(cache.get(remote, &p), None);
        assert_eq!(cache.get(HostId::LOCAL, &p), Some(&"local listing"));

        cache.remove(HostId::LOCAL, &p);
        assert!(cache.is_empty());
    }

    #[test]
    fn lookups_borrow_the_key_rather_than_cloning_it() {
        let mut cache: ByHost<PathBuf, u32> = ByHost::default();
        cache.insert(HostId::LOCAL, PathBuf::from("/a/b"), 1);
        assert_eq!(cache.get(HostId::LOCAL, Path::new("/a/b")), Some(&1));
        assert_eq!(cache.remove(HostId::LOCAL, Path::new("/a/b")), Some(1));
        assert!(cache.is_empty());
    }
}

#[cfg(test)]
mod gpui_tests {
    use crate::ui::host_ops::{Host, HostOps};
    use gpui::{App, AppContext, Context, Entity, TestAppContext};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Pane;

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

        drop(pane);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while detached.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            cx.background_executor.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
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

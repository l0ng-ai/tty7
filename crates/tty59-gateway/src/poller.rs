//! Reading several machines at once without waiting on the slowest.
//!
//! Each linked machine is read over its own SSH link, and one slow link must
//! not hold up the tree: not the local machine's, not the other links'. So
//! every machine is read on its own thread, a caller waits only a short
//! budget for them, and a machine that has not answered by then is reported
//! as it last was. Its read carries on, and lands for the next caller. There is
//! never more than one read in flight per machine, however many phones ask.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A read of one machine: what it produced, or why it failed.
pub type Fetched<T> = Result<T, String>;

pub struct Poller<T> {
    shared: Arc<(Mutex<HashMap<String, Slot<T>>>, Condvar)>,
    next_generation: std::sync::atomic::AtomicU64,
}

struct Slot<T> {
    last: Option<Fetched<T>>,
    in_flight: bool,
    /// Which life of this machine's slot a read belongs to. A machine that is
    /// forgotten and comes back gets a new one, so a read from before cannot
    /// land in it.
    generation: u64,
}

impl<T> Default for Poller<T> {
    fn default() -> Self {
        Poller {
            shared: Arc::new((Mutex::new(HashMap::new()), Condvar::new())),
            next_generation: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl<T: Clone + Send + 'static> Poller<T> {
    /// Starts a read of every machine in `reads` that has none running, waits
    /// up to `budget` for the running ones, and returns the latest result per
    /// machine: `None` for one never read yet. Machines not in `reads` are
    /// forgotten.
    pub fn poll(
        &self,
        reads: Vec<(String, Box<dyn FnOnce() -> Fetched<T> + Send>)>,
        budget: Duration,
    ) -> HashMap<String, Option<Fetched<T>>> {
        let (lock, done) = &*self.shared;
        let keys: Vec<String> = reads.iter().map(|(k, _)| k.clone()).collect();
        {
            let mut slots = lock.lock().unwrap_or_else(|e| e.into_inner());
            slots.retain(|key, _| keys.contains(key));
            for (key, read) in reads {
                let slot = slots.entry(key.clone()).or_insert_with(|| Slot {
                    last: None,
                    in_flight: false,
                    generation: self
                        .next_generation
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                });
                if slot.in_flight {
                    continue;
                }
                slot.in_flight = true;
                let generation = slot.generation;
                let shared = self.shared.clone();
                let spawned = std::thread::Builder::new()
                    .name(format!("gateway-remote-{key}"))
                    .spawn(move || {
                        let result = read();
                        let (lock, done) = &*shared;
                        let mut slots = lock.lock().unwrap_or_else(|e| e.into_inner());
                        // Forgotten while it ran: the link is gone, and so is
                        // the answer.
                        if let Some(slot) = slots
                            .get_mut(&key)
                            .filter(|slot| slot.generation == generation)
                        {
                            slot.last = Some(result);
                            slot.in_flight = false;
                        }
                        done.notify_all();
                    });
                if spawned.is_err() {
                    slot.in_flight = false;
                    slot.last = Some(Err("could not start a thread to read it".into()));
                }
            }
        }

        let deadline = Instant::now() + budget;
        let mut slots = lock.lock().unwrap_or_else(|e| e.into_inner());
        while keys
            .iter()
            .any(|k| slots.get(k).is_some_and(|s| s.in_flight))
        {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            slots = done
                .wait_timeout(slots, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        keys.into_iter()
            .map(|k| {
                let last = slots.get(&k).and_then(|s| s.last.clone());
                (k, last)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    type Read = Box<dyn FnOnce() -> Fetched<u32> + Send>;

    fn instant(v: u32) -> Read {
        Box::new(move || Ok(v))
    }

    /// A read that finishes only when the test lets it.
    fn held(v: u32) -> (Read, mpsc::Sender<()>) {
        let (tx, rx) = mpsc::channel::<()>();
        (
            Box::new(move || {
                let _ = rx.recv();
                Ok(v)
            }),
            tx,
        )
    }

    const BUDGET: Duration = Duration::from_millis(200);

    #[test]
    fn a_slow_machine_does_not_hold_up_the_fast_ones() {
        let poller = Poller::default();
        let (slow, release) = held(2);
        let t0 = Instant::now();
        let got = poller.poll(
            vec![("fast".into(), instant(1)), ("slow".into(), slow)],
            BUDGET,
        );
        assert!(t0.elapsed() < BUDGET * 3, "waited {:?}", t0.elapsed());
        assert_eq!(got["fast"], Some(Ok(1)));
        assert_eq!(got["slow"], None, "never read yet");

        // Once it answers, the next caller has it — whether that caller
        // catches the read finishing, or finds it done and starts another that
        // has not come back yet.
        release.send(()).unwrap();
        let (next, hold) = held(9);
        let got = poller.poll(
            vec![("fast".into(), instant(1)), ("slow".into(), next)],
            BUDGET,
        );
        assert_eq!(got["slow"], Some(Ok(2)));
        drop(hold);
    }

    #[test]
    fn a_machine_is_read_once_at_a_time() {
        let poller = Poller::default();
        let reads = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel::<()>();
        let rx = Arc::new(Mutex::new(rx));
        let counted = || -> Read {
            let (reads, rx) = (reads.clone(), rx.clone());
            Box::new(move || {
                reads.fetch_add(1, Ordering::SeqCst);
                let _ = rx.lock().unwrap().recv();
                Ok(0)
            })
        };
        poller.poll(vec![("m".into(), counted())], Duration::from_millis(20));
        poller.poll(vec![("m".into(), counted())], Duration::from_millis(20));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        tx.send(()).unwrap();
    }

    #[test]
    fn a_failed_read_is_reported_and_retried() {
        let poller: Poller<u32> = Poller::default();
        let got = poller.poll(vec![("m".into(), Box::new(|| Err("no".into())))], BUDGET);
        assert_eq!(got["m"], Some(Err("no".into())));
        let got = poller.poll(vec![("m".into(), instant(3))], BUDGET);
        assert_eq!(got["m"], Some(Ok(3)));
    }

    #[test]
    fn a_machine_no_longer_asked_about_is_forgotten() {
        let poller = Poller::default();
        poller.poll(vec![("m".into(), instant(1))], BUDGET);
        let (slow, release) = held(5);
        let got = poller.poll(vec![("m".into(), slow)], Duration::ZERO);
        assert_eq!(got["m"], Some(Ok(1)), "the last answer stands in");
        poller.poll(Vec::new(), BUDGET);
        release.send(()).unwrap();
        let got = poller.poll(vec![("m".into(), instant(7))], BUDGET);
        assert_eq!(got["m"], Some(Ok(7)), "read afresh, not the stale one");
    }
}

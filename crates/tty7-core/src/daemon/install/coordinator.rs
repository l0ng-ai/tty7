//! A preparation batch, not a permanent assertion that a remote is healthy.
//!
//! Concurrent callers hold the same batch through the install-key weak map.
//! They share a preparation outcome only for the same SSH incarnation.
//! When the batch drains, the next route probes again. Maintenance takes the
//! same gate and invalidates the result even when maintenance fails.

use std::io;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

const CANCEL_POLL: Duration = Duration::from_millis(100);

pub(super) struct PreparationBatch<T> {
    state: Mutex<State<T>>,
    changed: Condvar,
}

struct State<T> {
    busy: bool,
    completed: Option<(uuid::Uuid, BatchResult<T>)>,
}

type BatchResult<T> = Result<T, (io::ErrorKind, String)>;

impl<T> Default for PreparationBatch<T> {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                busy: false,
                completed: None,
            }),
            changed: Condvar::new(),
        }
    }
}

impl<T: Clone> PreparationBatch<T> {
    pub(super) fn prepare(
        &self,
        generation: uuid::Uuid,
        cancelled: &dyn Fn() -> bool,
        run: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        let _running = self.acquire(cancelled)?;
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((known, result)) = &state.completed
                && *known == generation
            {
                return result
                    .clone()
                    .map_err(|(kind, message)| io::Error::new(kind, message));
            }
            state.completed = None;
        }
        let result = run();
        // An absent/declining consent owner must not decide for a different
        // window. Other failures belong to the batch: ten panes should not
        // repeat the same failing network/install operation ten times.
        let completed = match &result {
            Ok(value) => Some(Ok(value.clone())),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::PermissionDenied
                ) =>
            {
                None
            }
            Err(error) => Some(Err((error.kind(), error.to_string()))),
        };
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .completed = completed.map(|outcome| (generation, outcome));
        result
    }

    pub(super) fn maintain<R>(
        &self,
        cancelled: &dyn Fn() -> bool,
        run: impl FnOnce() -> io::Result<R>,
    ) -> io::Result<R> {
        let _running = self.acquire(cancelled)?;
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .completed = None;
        run()
    }
}

impl<T> PreparationBatch<T> {
    fn acquire(&self, cancelled: &dyn Fn() -> bool) -> io::Result<Running<'_, T>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "remote preparation cancelled",
                ));
            }
            if !state.busy {
                state.busy = true;
                return Ok(Running(self));
            }
            (state, _) = self
                .changed
                .wait_timeout(state, CANCEL_POLL)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

struct Running<'a, T>(&'a PreparationBatch<T>);

impl<T> Drop for Running<'_, T> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner()).busy = false;
        self.0.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    };

    #[test]
    fn ten_concurrent_routes_prepare_once() {
        let batch = PreparationBatch::default();
        let gate = Barrier::new(10);
        let calls = AtomicUsize::new(0);
        let generation = uuid::Uuid::new_v4();
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..10)
                .map(|_| {
                    scope.spawn(|| {
                        gate.wait();
                        batch
                            .prepare(generation, &|| false, || {
                                calls.fetch_add(1, Ordering::SeqCst);
                                Ok("server".to_string())
                            })
                            .unwrap()
                    })
                })
                .collect();
            for worker in workers {
                assert_eq!(worker.join().unwrap(), "server");
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_new_ssh_generation_cannot_reuse_the_old_result() {
        let batch = PreparationBatch::default();
        let first = uuid::Uuid::new_v4();
        assert_eq!(batch.prepare(first, &|| false, || Ok(1)).unwrap(), 1);
        assert_eq!(
            batch
                .prepare(uuid::Uuid::new_v4(), &|| false, || Ok(2))
                .unwrap(),
            2
        );
    }

    #[test]
    fn failed_maintenance_invalidates_a_successful_preparation() {
        let batch = PreparationBatch::default();
        let generation = uuid::Uuid::new_v4();
        batch.prepare(generation, &|| false, || Ok(1)).unwrap();
        let result: io::Result<()> = batch.maintain(&|| false, || Err(io::Error::other("failed")));
        assert!(result.is_err());
        assert_eq!(batch.prepare(generation, &|| false, || Ok(2)).unwrap(), 2);
    }

    #[test]
    fn a_failed_or_cancelled_preparer_does_not_poison_other_consumers() {
        let batch = PreparationBatch::default();
        let generation = uuid::Uuid::new_v4();
        let result: io::Result<u32> = batch.prepare(generation, &|| false, || {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "first route left",
            ))
        });
        assert!(result.is_err());
        assert_eq!(batch.prepare(generation, &|| false, || Ok(2)).unwrap(), 2);
        let result = batch.prepare(generation, &|| true, || Ok(3));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn a_failed_batch_reports_one_failure_without_repeating_remote_work() {
        let batch = PreparationBatch::<u32>::default();
        let generation = uuid::Uuid::new_v4();
        let calls = AtomicUsize::new(0);
        let gate = Barrier::new(10);
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..10)
                .map(|_| {
                    scope.spawn(|| {
                        gate.wait();
                        let error = batch
                            .prepare(generation, &|| false, || {
                                calls.fetch_add(1, Ordering::SeqCst);
                                Err(io::Error::new(
                                    io::ErrorKind::TimedOut,
                                    "startup did not answer",
                                ))
                            })
                            .unwrap_err();
                        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
                        assert_eq!(error.to_string(), "startup did not answer");
                    })
                })
                .collect();
            for worker in workers {
                worker.join().unwrap();
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_declined_prompt_does_not_decline_for_a_different_window() {
        let batch = PreparationBatch::default();
        let generation = uuid::Uuid::new_v4();
        let error = batch
            .prepare(generation, &|| false, || -> io::Result<u32> {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "consent declined",
                ))
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(batch.prepare(generation, &|| false, || Ok(2)).unwrap(), 2);
    }

    #[test]
    fn a_cancelled_waiter_leaves_without_waiting_for_or_cancelling_the_leader() {
        let batch = Arc::new(PreparationBatch::default());
        let running = batch.acquire(&|| false).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (queued, waiting) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let waiter_batch = batch.clone();
        let flag = cancelled.clone();
        let waiter = std::thread::spawn(move || {
            let result = waiter_batch.prepare(
                uuid::Uuid::new_v4(),
                &|| {
                    let _ = queued.send(());
                    flag.load(Ordering::Acquire)
                },
                || -> io::Result<u32> { panic!("a cancelled waiter must not start preparation") },
            );
            done.send(result.unwrap_err().kind()).unwrap();
        });
        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
        cancelled.store(true, Ordering::Release);
        assert_eq!(
            finished.recv_timeout(Duration::from_secs(2)).unwrap(),
            io::ErrorKind::Interrupted
        );
        assert!(
            batch.state.lock().unwrap().busy,
            "the leader remains independent"
        );
        drop(running);
        waiter.join().unwrap();
    }

    #[test]
    fn maintenance_waits_for_preparation_to_finish() {
        let batch = PreparationBatch::<u32>::default();
        let running = batch.acquire(&|| false).unwrap();
        let (queued, waiting) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                batch.maintain(
                    &|| {
                        queued.send(()).unwrap();
                        false
                    },
                    || {
                        done.send(()).unwrap();
                        Ok(())
                    },
                )
            });
            waiting.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(finished.try_recv().is_err());
            drop(running);
            worker.join().unwrap().unwrap();
            finished.recv_timeout(Duration::from_secs(2)).unwrap();
        });
    }
}

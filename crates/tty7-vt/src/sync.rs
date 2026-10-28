//! The fair mutex the GUI's engine is shared through.
//!
//! Copied verbatim from `alacritty_terminal`'s `sync.rs` (Apache-2.0, like
//! this crate's other engine code) rather than re-exported: the seam's
//! sharing contract — one engine, a reader thread and a UI thread, fairness
//! so a painter never queues behind a reader's whole batch — outlives the
//! engine itself, and the alacritty dependency is deleted in Phase 6
//! (docs/libghostty-migration.md). Semantics are exactly alacritty's:
//! `lock()` takes a lease and then the data lock, so a waiter is served
//! before anyone can re-lock, while `lock_unfair`/`try_lock_unfair` skip
//! the lease for the callers that must never queue (the paint path).

use parking_lot::{Mutex, MutexGuard};

/// A fair mutex.
///
/// Uses an extra lock to ensure that if one thread is waiting that it will get
/// the lock before a single thread can re-lock it.
pub struct FairMutex<T> {
    /// Data.
    data: Mutex<T>,
    /// Next-to-access.
    next: Mutex<()>,
}

impl<T> FairMutex<T> {
    /// Create a new fair mutex.
    pub fn new(data: T) -> FairMutex<T> {
        FairMutex { data: Mutex::new(data), next: Mutex::new(()) }
    }

    /// Acquire a lease to reserve the mutex lock.
    ///
    /// This will prevent others from acquiring a terminal lock, but block if anyone else is
    /// already holding a lease.
    pub fn lease(&self) -> MutexGuard<'_, ()> {
        self.next.lock()
    }

    /// Lock the mutex.
    pub fn lock(&self) -> MutexGuard<'_, T> {
        // Must bind to a temporary or the lock will be freed before going
        // into data.lock().
        let _next = self.next.lock();
        self.data.lock()
    }

    /// Unfairly lock the mutex.
    pub fn lock_unfair(&self) -> MutexGuard<'_, T> {
        self.data.lock()
    }

    /// Unfairly try to lock the mutex.
    pub fn try_lock_unfair(&self) -> Option<MutexGuard<'_, T>> {
        self.data.try_lock()
    }
}

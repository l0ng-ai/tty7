use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use tty7_core::daemon::cancel::RouteCancellation;

#[derive(Clone)]
pub(crate) struct PromptValidity {
    pub(super) id: uuid::Uuid,
    active: Arc<AtomicBool>,
    cancellation: RouteCancellation,
}

impl Default for PromptValidity {
    fn default() -> Self {
        Self::new(RouteCancellation::default())
    }
}

impl PromptValidity {
    pub(super) fn new(cancellation: RouteCancellation) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            active: Arc::new(AtomicBool::new(true)),
            cancellation,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire) && self.cancellation.is_active()
    }

    pub(super) fn wait<T>(&self, receiver: mpsc::Receiver<T>, budget: Duration) -> Option<T> {
        struct Expire<'a>(&'a AtomicBool);
        impl Drop for Expire<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _expire = Expire(&self.active);
        let deadline = Instant::now() + budget;
        while self.is_active() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match receiver.recv_timeout(remaining.min(Duration::from_millis(100))) {
                Ok(answer) => return self.is_active().then_some(answer),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_wakes_a_waiter_even_while_the_ui_keeps_the_question() {
        let cancellation = RouteCancellation::default();
        let validity = PromptValidity::new(cancellation.clone());
        let ui = validity.clone();
        let (sender, receiver) = mpsc::sync_channel::<()>(1);
        let (finished, done) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            finished
                .send(validity.wait(receiver, Duration::from_secs(180)))
                .unwrap();
        });
        cancellation.cancel();
        assert!(done.recv_timeout(Duration::from_secs(2)).unwrap().is_none());
        assert!(!ui.is_active());
        assert!(sender.send(()).is_err());
        waiter.join().unwrap();
    }

    #[test]
    fn a_timed_out_question_expires_without_an_attempt_cancellation() {
        let validity = PromptValidity::default();
        let (_sender, receiver) = mpsc::sync_channel::<()>(1);
        assert!(validity.wait(receiver, Duration::ZERO).is_none());
        assert!(!validity.is_active());
    }
}

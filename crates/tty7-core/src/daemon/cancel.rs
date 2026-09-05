//! Cancellation of one routed connection, never of its shared SSH transport.

use std::io;
use std::sync::{Arc, Mutex};

use super::control::LinkShutdown;

#[derive(Clone, Default)]
pub struct RouteCancellation(Arc<Mutex<State>>);

#[derive(Default)]
enum State {
    #[default]
    Waiting,
    Routed(Arc<dyn LinkShutdown>),
    Cancelled,
    Accepted,
}

impl RouteCancellation {
    pub fn is_active(&self) -> bool {
        matches!(
            *self.0.lock().unwrap_or_else(|e| e.into_inner()),
            State::Waiting | State::Routed(_)
        )
    }

    pub fn check(&self) -> io::Result<()> {
        if self.is_active() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "remote connection attempt ended",
            ))
        }
    }

    /// Register before sending any route header. Registration racing a cancel
    /// closes the late socket instead of letting a cancelled attempt revive.
    pub fn register(&self, route: Arc<dyn LinkShutdown>) -> io::Result<()> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*state, State::Waiting) {
            *state = State::Routed(route);
            return Ok(());
        }
        drop(state);
        let _ = route.shutdown_link();
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "remote connection attempt is no longer waiting",
        ))
    }

    pub fn cancel(&self) {
        let previous = {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(*state, State::Cancelled | State::Accepted) {
                return;
            }
            std::mem::replace(&mut *state, State::Cancelled)
        };
        if let State::Routed(route) = previous {
            let _ = route.shutdown_link();
        }
    }

    /// The UI has accepted this exact attempt's result. Drop our socket clone
    /// without shutdown; the established host now owns the route's lifetime.
    pub fn accept(&self) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*state, State::Cancelled | State::Accepted) {
            return false;
        }
        *state = State::Accepted;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Shutdowns(AtomicUsize);
    impl LinkShutdown for Shutdowns {
        fn shutdown_link(&self) -> io::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn cancellation_is_sticky_and_closes_only_its_registered_route() {
        let a = RouteCancellation::default();
        let b = RouteCancellation::default();
        let a_link = Arc::new(Shutdowns::default());
        let b_link = Arc::new(Shutdowns::default());
        a.register(a_link.clone()).unwrap();
        b.register(b_link.clone()).unwrap();
        a.cancel();
        a.cancel();
        assert_eq!(a_link.0.load(Ordering::SeqCst), 1);
        assert_eq!(b_link.0.load(Ordering::SeqCst), 0);
        assert!(!a.accept());
        assert!(b.is_active());
    }

    #[test]
    fn cancellation_before_socket_creation_closes_a_late_socket() {
        let cancellation = RouteCancellation::default();
        cancellation.cancel();
        let route = Arc::new(Shutdowns::default());
        assert!(cancellation.register(route.clone()).is_err());
        assert_eq!(route.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn accepting_hands_off_the_route_without_shutdown() {
        let cancellation = RouteCancellation::default();
        let route = Arc::new(Shutdowns::default());
        cancellation.register(route.clone()).unwrap();
        assert!(cancellation.accept());
        cancellation.cancel();
        assert_eq!(route.0.load(Ordering::SeqCst), 0);
        assert_eq!(Arc::strong_count(&route), 1);
    }

    #[test]
    fn concurrent_registration_and_cancellation_always_close_the_socket() {
        for _ in 0..32 {
            let cancellation = RouteCancellation::default();
            let route = Arc::new(Shutdowns::default());
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    let _ = cancellation.register(route.clone());
                });
                scope.spawn(|| cancellation.cancel());
            });
            assert_eq!(route.0.load(Ordering::SeqCst), 1);
            assert!(!cancellation.is_active());
        }
    }
}

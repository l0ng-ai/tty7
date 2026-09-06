use std::sync::{Arc, Mutex};

use gpui::{App, Global};
use tty7_core::daemon::cancel::RouteCancellation;
use tty7_core::host::HostId;

/// UI-owned lifetime, deliberately not Clone. Background work gets only the
/// cancellation handle; dropping/replacing a window's attempt cancels it.
pub struct ConnectAttempt {
    pub id: uuid::Uuid,
    cancellation: RouteCancellation,
    registry: Entries,
}

type Entries = Arc<Mutex<Vec<(uuid::Uuid, HostId, RouteCancellation)>>>;

#[derive(Default)]
struct AttemptRegistry(Entries);

impl Global for AttemptRegistry {}

impl ConnectAttempt {
    pub fn new(host: HostId, cx: &mut App) -> Self {
        cx.default_global::<AttemptRegistry>().start(host)
    }

    #[cfg(test)]
    pub(crate) fn with_id(id: uuid::Uuid) -> Self {
        Self {
            id,
            cancellation: RouteCancellation::default(),
            registry: Entries::default(),
        }
    }

    pub fn cancellation(&self) -> RouteCancellation {
        self.cancellation.clone()
    }

    pub fn is_active(&self) -> bool {
        self.cancellation.is_active()
    }

    pub fn accept(&self) -> bool {
        self.cancellation.accept()
    }

    pub fn active_on(host: HostId, cx: &App) -> bool {
        cx.try_global::<AttemptRegistry>()
            .is_some_and(|registry| registry.active_on(host))
    }

    /// Explicit disconnect is host-wide, including attempts in other windows.
    /// Closing just one window drops just its own ConnectAttempt instead.
    pub fn cancel_host(host: HostId, cx: &App) {
        if let Some(registry) = cx.try_global::<AttemptRegistry>() {
            registry.cancel_host(host);
        }
    }
}

impl AttemptRegistry {
    fn start(&self, host: HostId) -> ConnectAttempt {
        let attempt = ConnectAttempt {
            id: uuid::Uuid::new_v4(),
            cancellation: RouteCancellation::default(),
            registry: self.0.clone(),
        };
        self.0.lock().unwrap_or_else(|e| e.into_inner()).push((
            attempt.id,
            host,
            attempt.cancellation.clone(),
        ));
        attempt
    }

    fn active_on(&self, host: HostId) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(_, known, cancellation)| *known == host && cancellation.is_active())
    }

    fn cancel_host(&self, host: HostId) {
        let cancelled: Vec<_> = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, known, _)| *known == host)
            .map(|(_, _, cancellation)| cancellation.clone())
            .collect();
        for cancellation in cancelled {
            cancellation.cancel();
        }
    }
}

impl Drop for ConnectAttempt {
    fn drop(&mut self) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(id, _, _)| *id != self.id);
        self.cancellation.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_window_leaving_does_not_cancel_another_windows_attempt() {
        let host = HostId::from_connection_key("test:attempt-window-close");
        let registry = AttemptRegistry::default();
        let a = registry.start(host);
        let b = registry.start(host);
        let cancelled = a.cancellation();
        drop(a);
        assert!(!cancelled.is_active());
        assert!(b.is_active());
        assert!(registry.active_on(host));
        drop(b);
        assert!(!registry.active_on(host));
    }

    #[test]
    fn disconnect_cancels_all_attempts_for_only_the_selected_host() {
        let host = HostId::from_connection_key("test:attempt-disconnect");
        let registry = AttemptRegistry::default();
        let a = registry.start(host);
        let b = registry.start(host);
        let other = registry.start(HostId::from_connection_key("test:attempt-other-host"));
        registry.cancel_host(host);
        assert!(!a.is_active());
        assert!(!b.is_active());
        assert!(other.is_active());
    }

    #[test]
    fn independent_app_registries_do_not_interfere() {
        let host = HostId::from_connection_key("test:attempt-same-host");
        let first = AttemptRegistry::default();
        let second = AttemptRegistry::default();
        let a = first.start(host);
        assert!(!second.active_on(host));
        let b = second.start(host);
        first.cancel_host(host);
        assert!(!a.is_active());
        assert!(b.is_active());
    }
}

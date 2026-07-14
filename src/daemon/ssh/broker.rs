//! The interactive prompt broker: how the async russh auth/host-key flow reaches
//! the GUI and blocks for an answer.
//!
//! During a native-SSH spawn the connect task needs decisions only the user can
//! make (a password, a key passphrase, keyboard-interactive answers, a host-key
//! confirmation). It emits a `DaemonMsg::AuthPrompt` over the pane's own
//! connection and `.await`s a `oneshot` that `run_stream` fulfils when the
//! matching `ClientMsg::AuthResponse` arrives (routed here through
//! `DaemonPane::deliver_auth_response`). Status/banner frames are fire-and-forget.
//!
//! The broker is constructed by `DaemonPane` (which owns the subscriber the frames
//! must reach) and handed an `emit` closure; keeping the type here puts it beside
//! the auth code that drives it. Secrets returned in `AuthResponse` are never
//! logged (its `Debug` redacts) and live only for the auth attempt.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;

use crate::daemon::protocol::{AuthPromptKind, AuthResponse, DaemonMsg, SshPhase};

/// How long an auth step waits for the user before failing cleanly.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
/// How long we keep re-offering a prompt frame while waiting for a subscriber to
/// attach (the spawn's socket may not have finished attaching the instant the
/// first prompt is ready). The frame is only actually sent once a subscriber
/// exists, so this never duplicates a prompt in the GUI.
const DELIVERY_WINDOW: Duration = Duration::from_secs(15);
const DELIVERY_POLL: Duration = Duration::from_millis(100);

pub struct PromptBroker {
    /// Sends a `DaemonMsg` to the pane's *current* subscriber, returning whether
    /// one was present (and thus whether the frame actually went out). Provided by
    /// `DaemonPane`, which owns the subscriber behind its state lock.
    emit: Box<dyn Fn(DaemonMsg) -> bool + Send + Sync>,
    pending: Mutex<HashMap<u64, oneshot::Sender<AuthResponse>>>,
    next_id: AtomicU64,
}

impl PromptBroker {
    pub fn new(emit: Box<dyn Fn(DaemonMsg) -> bool + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            emit,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        })
    }

    /// Whether an interactive prompt is currently awaiting the user's reply.
    /// The connect watchdog reads this to stop billing the connect timeout
    /// while the user is thinking (e.g. reading a host-key fingerprint).
    pub fn has_pending(&self) -> bool {
        !self.pending.lock().unwrap().is_empty()
    }

    /// Send an interactive prompt to the GUI and block (async) for its reply.
    /// Returns [`AuthResponse::Cancelled`] on user cancel, timeout, or if no GUI
    /// ever attaches to receive it — every one of which fails the auth step
    /// cleanly rather than hanging the connection.
    pub async fn prompt(&self, kind: AuthPromptKind) -> AuthResponse {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        // Deliver the frame, retrying only while no subscriber is attached yet.
        let frame = DaemonMsg::AuthPrompt {
            request_id: id,
            prompt: kind,
        };
        if !self.deliver_with_retry(frame).await {
            self.pending.lock().unwrap().remove(&id);
            return AuthResponse::Cancelled;
        }

        match tokio::time::timeout(PROMPT_TIMEOUT, rx).await {
            Ok(Ok(resp)) => resp,
            _ => {
                self.pending.lock().unwrap().remove(&id);
                AuthResponse::Cancelled
            }
        }
    }

    async fn deliver_with_retry(&self, frame: DaemonMsg) -> bool {
        let deadline = tokio::time::Instant::now() + DELIVERY_WINDOW;
        loop {
            if (self.emit)(frame.clone()) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(DELIVERY_POLL).await;
        }
    }

    /// Fire-and-forget server banner. No response is awaited.
    pub fn banner(&self, text: String) {
        let _ = (self.emit)(DaemonMsg::AuthPrompt {
            request_id: 0,
            prompt: AuthPromptKind::Banner { text },
        });
    }

    /// Fire-and-forget spawn-progress update.
    pub fn status(&self, phase: SshPhase) {
        let _ = (self.emit)(DaemonMsg::SshStatus { phase });
    }

    /// Fulfil a pending prompt with the GUI's reply. Unknown ids are ignored (a
    /// late reply to a step that already timed out).
    pub fn deliver(&self, request_id: u64, response: AuthResponse) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&request_id) {
            let _ = tx.send(response);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_returns_delivered_response() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        // A always-succeeding emit sink; we ignore the frame and reply out of band.
        let broker = PromptBroker::new(Box::new(|_| true));
        rt.block_on(async {
            let b = broker.clone();
            let fut = b.prompt(AuthPromptKind::Password {
                user: "u".into(),
                host: "h".into(),
            });
            // Reply to request id 1 (the first allocated) concurrently.
            let b2 = broker.clone();
            let replier = async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                b2.deliver(1, AuthResponse::Secret("pw".into()));
            };
            let (resp, _) = tokio::join!(fut, replier);
            assert!(matches!(resp, AuthResponse::Secret(_)));
        });
    }

    #[test]
    fn prompt_cancels_when_no_subscriber_ever_attaches() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        // An emit sink that never has a subscriber → never delivers.
        let broker = PromptBroker::new(Box::new(|_| false));
        rt.block_on(async {
            let resp = broker
                .prompt(AuthPromptKind::Password {
                    user: "u".into(),
                    host: "h".into(),
                })
                .await;
            assert!(matches!(resp, AuthResponse::Cancelled));
        });
    }
}

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;

use crate::daemon::protocol::{AuthPromptKind, AuthResponse, DaemonMsg, SshPhase};

const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
const DELIVERY_WINDOW: Duration = Duration::from_secs(15);
const DELIVERY_POLL: Duration = Duration::from_millis(100);

pub struct PromptBroker {
    emit: Box<dyn Fn(DaemonMsg) -> bool + Send + Sync>,
    pending: Mutex<HashMap<u64, oneshot::Sender<AuthResponse>>>,
    next_id: AtomicU64,
    cancelled: AtomicBool,
}

impl PromptBroker {
    pub fn new(emit: Box<dyn Fn(DaemonMsg) -> bool + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            emit,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            cancelled: AtomicBool::new(false),
        })
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.lock().unwrap().is_empty()
    }

    pub fn cancel(&self) {
        let mut pending = self.pending.lock().unwrap();
        self.cancelled.store(true, Ordering::Release);
        pending.clear();
    }

    pub async fn prompt(&self, kind: AuthPromptKind) -> AuthResponse {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if self.cancelled.load(Ordering::Acquire) {
                return AuthResponse::Cancelled;
            }
            pending.insert(id, tx);
        }
        // Authentication can be dropped while awaiting delivery or a reply.
        // Otherwise the stale sender keeps has_pending() true indefinitely,
        // suspending network deadlines on subsequent attempts.
        let _pending = PendingPrompt { broker: self, id };

        let frame = DaemonMsg::AuthPrompt {
            request_id: id,
            prompt: kind,
        };
        if !self.deliver_with_retry(frame).await {
            return AuthResponse::Cancelled;
        }

        match tokio::time::timeout(PROMPT_TIMEOUT, rx).await {
            Ok(Ok(resp)) => resp,
            _ => AuthResponse::Cancelled,
        }
    }

    async fn deliver_with_retry(&self, frame: DaemonMsg) -> bool {
        let deadline = tokio::time::Instant::now() + DELIVERY_WINDOW;
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return false;
            }
            if (self.emit)(frame.clone()) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(DELIVERY_POLL).await;
        }
    }

    pub fn banner(&self, text: String) {
        let _ = (self.emit)(DaemonMsg::AuthPrompt {
            request_id: 0,
            prompt: AuthPromptKind::Banner { text },
        });
    }

    pub fn status(&self, phase: SshPhase) {
        let _ = (self.emit)(DaemonMsg::SshStatus { phase });
    }

    pub fn deliver(&self, request_id: u64, response: AuthResponse) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&request_id) {
            let _ = tx.send(response);
        }
    }
}

struct PendingPrompt<'a> {
    broker: &'a PromptBroker,
    id: u64,
}

impl Drop for PendingPrompt<'_> {
    fn drop(&mut self) {
        self.broker.pending.lock().unwrap().remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn abandoning_a_prompt_cleans_up_during_delivery_and_reply_waits() {
        for delivered in [false, true] {
            let broker = PromptBroker::new(Box::new(move |_| delivered));
            let asking = broker.clone();
            let task = tokio::spawn(async move {
                asking
                    .prompt(AuthPromptKind::Password {
                        user: "u".into(),
                        host: "h".into(),
                    })
                    .await
            });
            tokio::task::yield_now().await;
            assert!(broker.has_pending());
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert!(!broker.has_pending());
            broker.deliver(1, AuthResponse::Secret("late reply".into()));
            assert!(!broker.has_pending());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_a_route_wakes_prompts_owned_by_the_ssh_handler_too() {
        for delivered in [false, true] {
            let broker = PromptBroker::new(Box::new(move |_| delivered));
            let asking = broker.clone();
            let task = tokio::spawn(async move {
                asking
                    .prompt(AuthPromptKind::Password {
                        user: "u".into(),
                        host: "h".into(),
                    })
                    .await
            });
            tokio::task::yield_now().await;
            assert!(broker.has_pending());
            broker.cancel();
            // The handler task is not aborted: cancellation must reach the
            // prompt even when russh, not the route future, owns that task.
            let response = tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(response, AuthResponse::Cancelled));
            assert!(!broker.has_pending());
            let response = broker
                .prompt(AuthPromptKind::Password {
                    user: "u".into(),
                    host: "h".into(),
                })
                .await;
            assert!(matches!(response, AuthResponse::Cancelled));
        }
    }

    #[test]
    fn prompt_returns_delivered_response() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let broker = PromptBroker::new(Box::new(|_| true));
        rt.block_on(async {
            let b = broker.clone();
            let fut = b.prompt(AuthPromptKind::Password {
                user: "u".into(),
                host: "h".into(),
            });
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

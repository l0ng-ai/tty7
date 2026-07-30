use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
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
}

impl PromptBroker {
    pub fn new(emit: Box<dyn Fn(DaemonMsg) -> bool + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            emit,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        })
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.lock().unwrap().is_empty()
    }

    pub async fn prompt(&self, kind: AuthPromptKind) -> AuthResponse {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

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

#[cfg(test)]
mod tests {
    use super::*;

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

use std::sync::Arc;

use gpui::{App, Global};
use tty7_core::daemon::control::ControlClient;

use crate::ui::remote_workspace::Backoff;

#[derive(Default)]
pub struct LocalLink {
    client: Option<Arc<ControlClient>>,
    backoff: Backoff,
    next_attempt: Option<std::time::Instant>,
    attempting: bool,
    pumping: bool,
}

impl Global for LocalLink {}

impl LocalLink {
    pub fn install(cx: &mut App) {
        crate::ui::remote_workspace::install_event_observer();
        let link = cx.default_global::<LocalLink>();
        if link.pumping {
            return;
        }
        link.pumping = true;
        cx.spawn(async move |cx| {
            loop {
                cx.update(|cx| {
                    Self::tick(cx);
                    crate::ui::remote_workspace::drain_events(cx);
                });
                cx.background_executor()
                    .timer(crate::ui::remote_workspace::PUMP_TICK)
                    .await;
            }
        })
        .detach();
    }

    pub fn client(cx: &mut App) -> Option<Arc<ControlClient>> {
        let link = cx.default_global::<LocalLink>();
        link.client.as_ref().filter(|c| c.is_connected()).cloned()
    }

    /// Drops the cached client without waiting for its reader to notice.
    ///
    /// `ControlClient::is_connected` only flips once the reader sees EOF, so
    /// for a moment after we kill the daemon ourselves the dead link still
    /// hands itself out and every call on it fails. Callers that know the far
    /// end is gone say so here, and the next tick reconnects.
    pub fn invalidate(cx: &mut App) {
        let link = cx.default_global::<LocalLink>();
        if link.client.take().is_some() {
            log::info!("dropped the control link to the local daemon; it was restarted");
        }
        link.backoff.reset();
        link.next_attempt = None;
    }

    fn tick(cx: &mut App) {
        let now = std::time::Instant::now();
        let link = cx.default_global::<LocalLink>();
        if link.attempting {
            return;
        }
        if let Some(client) = &link.client {
            if client.is_connected() {
                return;
            }
            log::info!("lost the control link to the local daemon; reconnecting");
            link.client = None;
        }
        match link.next_attempt {
            None if link.backoff.attempt() == 0 => {}
            None => {
                link.next_attempt = Some(now + link.backoff.delay());
                return;
            }
            Some(at) if at > now => return,
            Some(_) => {}
        }
        link.next_attempt = None;
        link.attempting = true;
        let _ = link.backoff.advance();

        cx.spawn(async move |cx| {
            let connected = cx
                .background_executor()
                .spawn(async move { connect_blocking() })
                .await;
            cx.update(|cx| {
                let link = cx.default_global::<LocalLink>();
                link.attempting = false;
                match connected {
                    Ok(client) => {
                        log::info!("control link to the local daemon is up");
                        link.client = Some(client);
                        link.backoff.reset();
                        link.next_attempt = None;
                        crate::ui::machine_mirror::MachineMirrors::refresh(
                            cx,
                            tty7_core::host::HostId::LOCAL,
                        );
                        crate::ui::tree_sync::on_link_up(cx, tty7_core::host::HostId::LOCAL);
                    }
                    Err(e) => {
                        log::debug!("local control link attempt failed: {e}");
                    }
                }
            });
        })
        .detach();
    }
}

fn connect_blocking() -> std::io::Result<Arc<ControlClient>> {
    use tty7_core::daemon::control::ControlHello;

    crate::daemon::spawn::ensure_running().map_err(std::io::Error::other)?;
    let hello = ControlHello::host_rpc(uuid::Uuid::new_v4().to_string(), "this computer");
    let sink: tty7_core::daemon::control::EventSink = Box::new(local_event_sink);
    #[cfg(unix)]
    let client = ControlClient::over_unix(
        std::os::unix::net::UnixStream::connect(tty7_core::host::server::control_socket_path()?)?,
        &hello,
        sink,
    )?;
    #[cfg(windows)]
    let client =
        ControlClient::over_tcp(tty7_core::host::server::connect_control()?, &hello, sink)?;
    Ok(Arc::new(client))
}

fn local_event_sink(event: tty7_core::daemon::control::ControlEvent) {
    tty7_core::daemon::control::observe_event(tty7_core::host::HostId::LOCAL, event);
}

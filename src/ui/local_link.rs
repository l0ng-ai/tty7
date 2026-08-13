use std::sync::Arc;

use gpui::{App, Global};
use tty7_core::daemon::control::ControlClient;

use crate::ui::remote_workspace::Backoff;

#[derive(Default)]
pub struct LocalLink {
    client: Option<Arc<ControlClient>>,
    /// The daemon process the current link is talking to, by its hello
    /// instance — empty until the first connect records one. Survives
    /// `invalidate` on purpose: forgetting it there would make every
    /// reconnect a first sighting, and a daemon that came back as a
    /// *different* process would never be noticed (#553).
    instance: String,
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
    ///
    /// The remembered hello `instance` deliberately survives: the reconnect
    /// compares against it to tell "same daemon, link hiccuped" from "new
    /// daemon process" (#553), and forgetting it here — the restart path's
    /// own first move — would blind exactly that comparison.
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
                        // A daemon that died and came back is a *new process*
                        // whose registry knows nothing about the panes this
                        // window is showing — and from the client's side a
                        // killed daemon is indistinguishable from one whose
                        // shells all exited at once (its DeathReporter says
                        // nothing while it shuts down, and a kill says nothing
                        // ever), so the panes on screen are probably lying
                        // about being alive. The instance id in the hello is
                        // the only way to tell "same daemon, link hiccuped"
                        // from "new daemon": compare before syncing, or
                        // `on_link_up` would push the window of dead panes up
                        // as the new daemon's truth (#553).
                        let restarted = {
                            let link = cx.default_global::<LocalLink>();
                            crate::ui::tree_sync::note_instance(
                                &mut link.instance,
                                &client.hello().instance,
                            )
                        };
                        let link = cx.default_global::<LocalLink>();
                        link.client = Some(client);
                        link.backoff.reset();
                        link.next_attempt = None;
                        crate::ui::machine_mirror::MachineMirrors::refresh(
                            cx,
                            tty7_core::host::HostId::LOCAL,
                        );
                        if restarted {
                            // The link installed just above is this new
                            // daemon's own and answers right now, so the pull
                            // goes out on it. Dropping it first — which is what
                            // a caller that killed the daemon itself has to do —
                            // would leave every window waiting out another
                            // connect, and a pull that runs out its fifteen
                            // seconds waiting owes a `Replace` a window with
                            // tabs on screen never claims back.
                            crate::ui::tree_sync::resync_local_windows_from_tree(cx);
                        } else {
                            crate::ui::tree_sync::on_link_up(cx, tty7_core::host::HostId::LOCAL);
                        }
                    }
                    Err(e) => match dialect_refusal(&e) {
                        // Retrying will not talk this server round, and the
                        // window already on screen has no tabs to show. Arm the
                        // prompt so the next window built offers the restart.
                        Some(refusal) => {
                            log::warn!(
                                "the local server refused this build's control dialect: {e}"
                            );
                            crate::daemon::spawn::note_daemon_mismatch(
                                crate::daemon::spawn::DaemonMismatch::Dialect(refusal),
                            );
                        }
                        None => log::debug!("local control link attempt failed: {e}"),
                    },
                }
            });
        })
        .detach();
    }
}

fn connect_blocking() -> std::io::Result<Arc<ControlClient>> {
    use tty7_core::daemon::control::ControlHello;

    crate::daemon::spawn::ensure_running().map_err(std::io::Error::other)?;
    let hello = ControlHello::gui(uuid::Uuid::new_v4().to_string(), "this computer");
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

/// The dialect mismatch behind a failed connect, if that is what it was.
///
/// It arrives as the handshake's own wording rather than a typed error, so this
/// is where the string becomes the fact again.
fn dialect_refusal(e: &std::io::Error) -> Option<tty7_core::daemon::control::DialectRefusal> {
    tty7_core::daemon::control::parse_dialect_refusal(&e.to_string())
}

fn local_event_sink(event: tty7_core::daemon::control::ControlEvent) {
    tty7_core::daemon::control::observe_event(tty7_core::host::HostId::LOCAL, event);
}

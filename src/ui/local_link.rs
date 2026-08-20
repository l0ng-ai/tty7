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
        match due(
            link.next_attempt,
            link.backoff.attempt(),
            link.backoff.delay(),
            now,
        ) {
            Due::Now => {}
            Due::Wait => return,
            Due::ScheduleAt(at) => {
                link.next_attempt = Some(at);
                return;
            }
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

/// What a tick should do about reconnecting.
#[derive(Debug, PartialEq, Eq)]
enum Due {
    /// Try now.
    Now,
    /// Something is already scheduled and is not due yet.
    Wait,
    /// Nothing was scheduled; put the next attempt here and come back.
    ScheduleAt(std::time::Instant),
}

/// The reconnect schedule, with the clock and the link's state passed in.
///
/// The very first attempt goes out immediately — at startup the daemon is
/// usually seconds from being up, and making the window wait a backoff for the
/// first try would be a visible stall — and only from the second does the
/// backoff get a say.
///
/// Nothing here reads a global or the wall clock, which is what lets it be
/// tested. The structurally identical scheduler in `remote_workspace` is
/// covered through a `TestAppContext`; this one, which every user depends on at
/// launch, was covered not at all.
fn due(
    scheduled: Option<std::time::Instant>,
    attempts_so_far: u32,
    delay: std::time::Duration,
    now: std::time::Instant,
) -> Due {
    match scheduled {
        None if attempts_so_far == 0 => Due::Now,
        None => Due::ScheduleAt(now + delay),
        Some(at) if at > now => Due::Wait,
        Some(_) => Due::Now,
    }
}

fn connect_blocking() -> std::io::Result<Arc<ControlClient>> {
    use tty7_core::daemon::control::ControlHello;

    crate::daemon::spawn::ensure_running().map_err(std::io::Error::other)?;
    let hello = ControlHello::gui(uuid::Uuid::new_v4().to_string(), "this computer");
    let sink: tty7_core::daemon::control::EventSink = Box::new(local_event_sink);
    #[cfg(unix)]
    let client = {
        let stream = std::os::unix::net::UnixStream::connect(
            tty7_core::host::server::control_socket_path()?,
        )?;
        // Every other client socket goes through `tune` on its way up — 256 KiB
        // buffers on Unix, nodelay on Windows — because it is `connect_endpoint`
        // that calls it, and this is the one connect that does not go through
        // there. It is also the busiest: the control link carries every event
        // the window redraws from.
        tty7_core::daemon::transport::tune(&stream);
        ControlClient::over_unix(stream, &hello, sink)?
    };
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

#[cfg(test)]
mod schedule_tests {
    use super::*;
    use std::time::{Duration, Instant};

    const D: Duration = Duration::from_secs(2);

    #[test]
    fn the_very_first_attempt_goes_out_immediately() {
        // No backoff before anything has failed: at launch the daemon is
        // seconds from being up and a delay here is a visible stall.
        assert_eq!(due(None, 0, D, Instant::now()), Due::Now);
    }

    #[test]
    fn a_later_attempt_with_nothing_scheduled_gets_scheduled() {
        let now = Instant::now();
        assert_eq!(due(None, 1, D, now), Due::ScheduleAt(now + D));
        assert_eq!(
            due(None, 7, Duration::from_secs(30), now),
            Due::ScheduleAt(now + Duration::from_secs(30)),
            "the delay is the backoff's to decide, not this function's"
        );
    }

    #[test]
    fn a_scheduled_attempt_in_the_future_waits() {
        let now = Instant::now();
        assert_eq!(due(Some(now + D), 3, D, now), Due::Wait);
    }

    #[test]
    fn a_scheduled_attempt_that_has_come_due_fires() {
        let now = Instant::now();
        assert_eq!(due(Some(now - D), 3, D, now), Due::Now);
        assert_eq!(
            due(Some(now), 3, D, now),
            Due::Now,
            "exactly due counts as due, or a tick landing on the instant waits a whole round"
        );
    }

    /// Scheduling happens once. A tick that arrives while an attempt is
    /// pending must not push the deadline further out, or a busy window would
    /// starve the reconnect forever.
    #[test]
    fn ticking_repeatedly_does_not_move_a_pending_deadline() {
        let start = Instant::now();
        let at = start + D;
        for step in [0u64, 1, 100, 500] {
            let now = start + Duration::from_millis(step);
            assert_eq!(due(Some(at), 3, D, now), Due::Wait, "at +{step}ms");
        }
        assert_eq!(due(Some(at), 3, D, start + D), Due::Now);
    }
}

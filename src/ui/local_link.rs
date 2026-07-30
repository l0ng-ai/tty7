//! The GUI's control link to **this machine's own daemon**.
//!
//! Local and remote machines are the same thing seen from different distances:
//! one machine, one daemon, one workspace tree, one control link. The remote
//! machines' links live in [`crate::ui::remote_connect::HostLinks`]; this
//! module is the local machine's — the link over which the GUI receives the
//! local daemon's pushes (`ControlEvent::Layout` deltas, `Preempted`) and
//! sends its semantic tree operations.
//!
//! # Not a `HostLinks` entry
//!
//! `HostLinks` doubles as the [`crate::ui::host_registry::HostRegistry`]
//! feeder: inserting there would register a *wire-backed* `Host` for this
//! machine, while the local file tree and git must keep going through the
//! in-process [`LocalHost`](tty7_core::host::local::LocalHost) — a socket
//! round trip per `stat` on the machine you are sitting at would be absurd. It
//! also keeps the `HostId::LOCAL`-never-holds-a-control-connection invariant
//! untouched: this link lives in its own global, not in any host table.
//!
//! # Not routed
//!
//! A remote control connection dials the local daemon's *pane* socket and asks
//! it to route (the GUI never speaks SSH). This machine needs no routing — the
//! daemon's control endpoint is right here, so the link is a plain connect plus
//! a `ControlHello`.
//!
//! # Its own pump
//!
//! The remote supervisor's pump deliberately stops when the last remote
//! workspace closes; this link must outlive that — a purely local session is
//! the *common* case — so [`LocalLink::install`] runs its own forever loop at
//! the same cadence. Each turn supervises the connection (reconnecting on the
//! same 1/2/4/…/30 s backoff a remote machine gets — the daemon may be
//! restarting or upgrading, and the GUI auto-spawns it, so "down" is always
//! transient) and drains the shared event queue, so local pushes are delivered
//! even when the remote pump is parked. Events land in that queue under
//! [`HostId::LOCAL`](tty7_core::host::HostId::LOCAL): the pump drains one
//! queue and machines differ only by id, which is the same-shape-everywhere
//! the whole design is after.
//!
//! # Both platforms
//!
//! The dial is the one part that differs, and only in its first line: a Unix
//! socket where there are Unix sockets, and the same token-checked loopback
//! endpoint the pane dialect uses on Windows (see
//! [`tty7_core::daemon::transport`]). Everything above `connect_blocking` —
//! supervision, backoff, the event queue, the tree sync that rides this link —
//! is one code path, because a machine's tree is what a window's layout *is*
//! and a platform without it is a platform where tabs do not come back.

use std::sync::Arc;

use gpui::{App, Global};
use tty7_core::daemon::control::ControlClient;

use crate::ui::remote_workspace::Backoff;

/// The link, and the schedule for getting it back.
#[derive(Default)]
pub struct LocalLink {
    client: Option<Arc<ControlClient>>,
    backoff: Backoff,
    /// When the next attempt is due. `None` while the link is up or an
    /// attempt is in flight.
    next_attempt: Option<std::time::Instant>,
    attempting: bool,
    /// Whether the forever loop is already running, so `install` is idempotent.
    pumping: bool,
}

impl Global for LocalLink {}

impl LocalLink {
    /// Start supervising the local link. Called once at startup; safe to call
    /// again (the loop is a singleton).
    pub fn install(cx: &mut App) {
        // Local pushes need the same somewhere-to-go the remote ones have,
        // and this loop may be the only one draining it.
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

    /// The live control client for this machine's daemon, if there is one.
    ///
    /// `None` is always transient — the supervisor is already reconnecting —
    /// so callers treat it exactly like an unreachable remote: skip the
    /// operation, or queue nothing and rely on the full pull that follows a
    /// reconnect.
    pub fn client(cx: &mut App) -> Option<Arc<ControlClient>> {
        let link = cx.default_global::<LocalLink>();
        link.client.as_ref().filter(|c| c.is_connected()).cloned()
    }

    /// One supervision step: notice a dead link, drop it, and schedule or
    /// launch the next attempt on the backoff.
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
            // Never attempted at all: due now. The daemon is normally already
            // up (main spawns it before the first window), so the first tick
            // should connect, not start a schedule.
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
                        // Every fresh link starts with a full pull — deltas
                        // only advance a mirror that has a base to advance.
                        crate::ui::machine_mirror::MachineMirrors::refresh(
                            cx,
                            tty7_core::host::HostId::LOCAL,
                        );
                        // …and re-runs every local window's sync: a window
                        // built while this link was still dialing is parked
                        // `Unprimed { dirty }` with nothing else scheduled to
                        // wake it (see `tree_sync::on_link_up`).
                        crate::ui::tree_sync::on_link_up(cx, tty7_core::host::HostId::LOCAL);
                    }
                    Err(e) => {
                        // The next tick schedules the following attempt off
                        // the already-advanced backoff.
                        log::debug!("local control link attempt failed: {e}");
                    }
                }
            });
        })
        .detach();
    }
}

/// Dial the local daemon's control endpoint and shake hands. **Blocking**; runs
/// on the background executor.
///
/// `ensure_running` first, because the daemon is the GUI's own child in the
/// common case: on a cold start this races the daemon binding its listener,
/// and the backoff absorbs the one or two attempts that lose the race.
fn connect_blocking() -> std::io::Result<Arc<ControlClient>> {
    use tty7_core::daemon::control::ControlHello;

    crate::daemon::spawn::ensure_running().map_err(std::io::Error::other)?;
    let hello = ControlHello::host_rpc(
        uuid::Uuid::new_v4().to_string(),
        // Its own label rather than this machine's hostname: if this session
        // is ever preempted, "this computer" is the useful thing to show —
        // the hostname would name the machine the user is already at.
        "this computer",
    );
    let sink: tty7_core::daemon::control::EventSink = Box::new(local_event_sink);
    // The one platform difference: which kind of stream carries the dialect.
    #[cfg(unix)]
    let client = ControlClient::over_unix(
        std::os::unix::net::UnixStream::connect(tty7_core::host::server::control_socket_path()?)?,
        &hello,
        sink,
    )?;
    // Loopback TCP with the daemon's token as a preamble — the access boundary
    // Windows has instead of socket permissions; `connect_control` presents it.
    #[cfg(windows)]
    let client =
        ControlClient::over_tcp(tty7_core::host::server::connect_control()?, &hello, sink)?;
    Ok(Arc::new(client))
}

/// Local daemon pushes land in the same process-wide observer as every remote
/// machine's, attributed to [`HostId::LOCAL`](tty7_core::host::HostId::LOCAL).
fn local_event_sink(event: tty7_core::daemon::control::ControlEvent) {
    tty7_core::daemon::control::observe_event(tty7_core::host::HostId::LOCAL, event);
}

//! The gateway as a service a process can start and stop: the `tty7-gateway
//! serve` command runs one until it is killed, and the desktop's daemon runs
//! one for as long as mobile access is switched on.
//!
//! Whoever runs it, its state is the files in `<config dir>/mobile/`, so the
//! GUI can make pairing codes and list phones without talking to it — and it
//! reports how it is doing in `status.json` there for the GUI to show.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context as _, Result};
use iroh::endpoint::{BindOpts, presets};
use iroh::{Endpoint, SecretKey};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use tokio::sync::oneshot;
use tty7_mobile_proto::{ALPN, MDNS_SERVICE, PairCode};

use crate::daemon::{Daemon, hostname};
use crate::serve::{self, Backend as _};
use crate::state::{Reachable, State, Status, failed, running};

/// A gateway running on its own thread. Dropping it stops it.
pub struct Running {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Running {
    /// Stops accepting phones, closes every connection, and waits for the
    /// thread to finish.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Starts the gateway on a thread of its own and returns once it is listening
/// — or with why it could not: another gateway holds this config dir, or no
/// socket could be bound.
pub fn start(state: State) -> Result<Running> {
    let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("mobile-gateway".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("mobile-gateway-rt")
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.into()));
                    return;
                }
            };
            runtime.block_on(run(state, ready_tx, stop_rx));
        })
        .context("starting the gateway thread")?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(Running {
            stop: Some(stop_tx),
            thread: Some(thread),
        }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            anyhow::bail!("the gateway thread ended before it was listening")
        }
    }
}

async fn run(state: State, ready: std_mpsc::Sender<Result<()>>, stop: oneshot::Receiver<()>) {
    // Another gateway already serving this config dir owns `status.json`; one
    // that could not take the lock must not overwrite what it says.
    let lock = match state.lock_serve() {
        Ok(lock) => lock,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let started = async {
        let endpoint = bind(state.secret_key()?, state.port()).await?;
        if let Some(port) = endpoint
            .bound_sockets()
            .iter()
            .map(SocketAddr::port)
            .find(|&p| p != 0)
            && state.port() != Some(port)
        {
            state.set_port(port).context("remembering the port")?;
        }
        anyhow::Ok(endpoint)
    };
    let endpoint = match started.await {
        Ok(up) => up,
        Err(e) => {
            let _ = state.set_status(&failed(&e));
            let _ = ready.send(Err(e));
            return;
        }
    };
    let id = endpoint.id().to_string();
    log::info!("mobile gateway listening as {id}");
    let _ = state.set_status(&running(&id));
    let _ = ready.send(Ok(()));

    let daemon = Arc::new(Daemon::default());
    // Not fatal: tty7 may well be opened after the gateway, and phones are
    // told the same thing until it is.
    if let Err(e) = daemon.snapshot() {
        log::warn!("{}", serve::server_down(&daemon.hostname(), &e));
    }

    // Keep the addresses a pairing code carries current. The local ones are
    // known at once and are all a phone on the same network needs; the relay
    // only arrives once the endpoint gets online, which on a network that
    // blocks the relays is never — a code must not wait for it.
    let addrs = tokio::spawn({
        let (endpoint, state) = (endpoint.clone(), state.clone());
        async move {
            loop {
                let addr = endpoint.addr();
                let reachable = Reachable {
                    relay: addr.relay_urls().next().map(|u| u.to_string()),
                    addrs: addr.ip_addrs().map(|a| a.to_string()).collect(),
                };
                if state.reachable() != reachable {
                    let _ = state.set_reachable(&reachable);
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    });

    tokio::select! {
        () = serve::run(endpoint.clone(), state.clone(), daemon) => {}
        _ = stop => {}
    }
    addrs.abort();
    endpoint.close().await;
    let _ = state.set_status(&Status::Stopped);
    drop(lock);
    log::info!("mobile gateway stopped");
}

/// Binds the gateway's endpoint on the port it had last time, so the
/// addresses in phones' pairing codes still reach it, and advertises it on the
/// local network. Each of those is given up, with a note, rather than let it
/// keep the gateway from starting: the port may be taken, and multicast may be
/// off.
async fn bind(key: SecretKey, port: Option<u16>) -> Result<Endpoint> {
    let mut attempts = Vec::new();
    if let Some(port) = port {
        attempts.extend([(port, true), (port, false)]);
    }
    attempts.extend([(0, true), (0, false)]);

    let mut failures = Vec::new();
    for (port, mdns) in attempts {
        let builder = Endpoint::builder(presets::N0)
            .secret_key(key.clone())
            .alpns(vec![ALPN.to_vec()])
            .clear_ip_transports()
            .bind_addr(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))?
            // IPv6 is a bonus, as it is in iroh's own defaults.
            .bind_addr_with_opts(
                SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)),
                BindOpts::default().set_is_required(false),
            )?;
        let builder = match mdns {
            true => builder.address_lookup(MdnsAddressLookup::builder().service_name(MDNS_SERVICE)),
            false => builder,
        };
        match builder.bind().await {
            Ok(endpoint) => {
                for failure in failures {
                    log::warn!("mobile gateway: {failure} — carrying on without it");
                }
                return Ok(endpoint);
            }
            Err(e) => {
                let what = match (port, mdns) {
                    (0, true) => "local network discovery".to_string(),
                    (0, false) => "the connection".to_string(),
                    (port, _) => format!("port {port}"),
                };
                failures.push(format!("{what}: {e}"));
            }
        }
    }
    anyhow::bail!("could not start: {}", failures.join("; "))
}

/// Serves until this process's stdin closes — the way a daemon runs its
/// gateway as a child: the pipe it holds closes when it ends, however it ends,
/// and the gateway goes with it.
pub fn serve_until_stdin_closes(state: State) -> Result<()> {
    let running = start(state)?;
    let mut sink = [0u8; 256];
    let mut stdin = std::io::stdin().lock();
    while matches!(std::io::Read::read(&mut stdin, &mut sink), Ok(n) if n > 0) {}
    running.stop();
    Ok(())
}

/// Opens a pairing offer valid for `ttl`, and returns the code a phone scans
/// or pastes for it. The addresses in it are the ones the running gateway
/// last wrote down; with none running the code still pairs once one starts.
pub fn pair_code(state: &State, ttl: Duration) -> Result<String> {
    let reachable = state.reachable();
    Ok(PairCode {
        host_id: state.secret_key()?.public().to_string(),
        host_name: hostname(),
        relay: reachable.relay,
        addrs: reachable.addrs,
        secret: state.open_pairing(ttl.as_secs())?,
    }
    .encode())
}

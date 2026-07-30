use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

use crate::daemon::pane::DaemonPane;
use crate::daemon::protocol::{ClientMsg, DaemonMsg, DaemonVersion, RemoteKind};
use crate::daemon::ssh::SshConnection;
use crate::daemon::transport::{self, Stream};

struct Registry {
    panes: Mutex<HashMap<u64, Arc<DaemonPane>>>,
    next_id: AtomicU64,
}

impl Registry {
    fn new() -> Self {
        Self {
            panes: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn seed_ids_past(&self, machine: &crate::core::machine::Machine) {
        let max = machine
            .panes
            .iter()
            .map(|p| p.id)
            .chain(
                machine
                    .workspaces
                    .iter()
                    .flat_map(|w| w.tabs.iter())
                    .flat_map(|t| t.root.pane_ids()),
            )
            .max()
            .unwrap_or(0);
        let next = max.saturating_add(1);
        let before = self.next_id.fetch_max(next, Ordering::Relaxed);
        if next > before {
            log::info!("pane ids start at {next} (the tree names panes up to {max})");
        }
    }

    fn insert(&self, pane: Arc<DaemonPane>) {
        self.panes.lock().unwrap().insert(pane.id, pane);
    }

    fn get(&self, id: u64) -> Option<Arc<DaemonPane>> {
        self.panes.lock().unwrap().get(&id).cloned()
    }

    fn remove(&self, id: u64) -> Option<Arc<DaemonPane>> {
        self.panes.lock().unwrap().remove(&id)
    }

    fn drain_and_kill(&self) {
        let panes: Vec<Arc<DaemonPane>> = {
            let mut guard = self.panes.lock().unwrap();
            guard.drain().map(|(_, p)| p).collect()
        };
        for pane in panes {
            pane.kill();
        }
    }

    fn list(&self) -> Vec<crate::daemon::protocol::PaneInfo> {
        self.panes
            .lock()
            .unwrap()
            .values()
            .map(|p| p.info())
            .collect()
    }
}

const ORPHAN_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);

fn spawn_orphan_sweep(registry: Arc<Registry>) {
    let spawned = std::thread::Builder::new()
        .name("tty7-orphan-sweep".into())
        .spawn(move || {
            let mut previous: std::collections::HashSet<u64> = std::collections::HashSet::new();
            loop {
                std::thread::sleep(ORPHAN_SWEEP_INTERVAL);
                let Some(store) = crate::core::machine::observed_store() else {
                    continue;
                };
                let machine = store.machine();
                let referenced: std::collections::HashSet<u64> = machine
                    .workspaces
                    .iter()
                    .flat_map(|w| w.tabs.iter())
                    .flat_map(|t| t.root.pane_ids())
                    .collect();
                let orphans: std::collections::HashSet<u64> = registry
                    .list()
                    .into_iter()
                    .filter(|p| p.alive && !referenced.contains(&p.pane_id))
                    .map(|p| p.pane_id)
                    .collect();
                for id in orphans.intersection(&previous) {
                    log::info!(
                        "pane {id} is running but no workspace tree references it \
                         (kept; the sweep only reports — see spawn_orphan_sweep)"
                    );
                }
                previous = orphans;
            }
        });
    if let Err(e) = spawned {
        log::warn!("could not start the orphan-pane sweep: {e}");
    }
}

fn ssh_connection_for(
    registry: &Registry,
    pane_id: u64,
) -> Result<Arc<crate::daemon::ssh::SshConnection>, String> {
    let pane = registry
        .get(pane_id)
        .ok_or_else(|| format!("no such pane {pane_id}"))?;
    pane.ssh_connection().ok_or_else(|| {
        "pane has no native SSH connection (SFTP needs a native-SSH pane)".to_string()
    })
}

pub fn run_daemon() -> anyhow::Result<()> {
    #[cfg(any(unix, windows))]
    match crate::host::server::spawn_control_listener_with(
        crate::host::local::LocalHost::shared(),
        control_services(),
    ) {
        Ok(path) => eprintln!("tty7-server: control socket at {}", path.display()),
        Err(e) => eprintln!("tty7-server: control listener unavailable: {e}"),
    }
    #[cfg(not(any(unix, windows)))]
    log::info!("no control listener on this platform; serving panes only");

    run()
}

pub fn control_services() -> crate::host::server::Services {
    use crate::core::machine::MachineStore;
    match MachineStore::shared() {
        Ok(machine) => {
            eprintln!("machine tree at {}", machine.path().display());
            crate::core::machine::publish_observations(&machine);
            crate::host::server::Services::with_machine(machine)
        }
        Err(e) => {
            eprintln!("no machine tree ({e}); serving files and panes only");
            crate::host::server::Services::none()
        }
    }
}

pub fn run() -> anyhow::Result<()> {
    if transport::endpoint_exists() {
        match transport::connect() {
            Ok(_) => {
                anyhow::bail!(
                    "daemon already running at {}",
                    transport::endpoint_display()
                );
            }
            Err(_) => {
                transport::remove_stale_endpoint();
            }
        }
    }

    let listener = transport::bind()?;
    log::info!("daemon listening on {}", transport::endpoint_display());

    crate::daemon::pidfile::write_current();

    let registry = Arc::new(Registry::new());

    #[cfg(unix)]
    serve_sigterm(registry.clone());

    if let Some(store) = crate::core::machine::observed_store() {
        registry.seed_ids_past(&store.machine());
        let probe = registry.clone();
        store.set_liveness_probe(Arc::new(move |id| {
            probe.get(id).is_some_and(|pane| pane.info().alive)
        }));
    }

    spawn_orphan_sweep(registry.clone());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                transport::tune(&stream);
                let registry = registry.clone();
                std::thread::Builder::new()
                    .name("tty7-daemon-conn".to_string())
                    .spawn(move || {
                        crate::core::threads::promote_to_user_interactive();
                        if let Err(e) = handle_conn(stream, registry) {
                            log::debug!("connection ended: {e}");
                        }
                    })
                    .ok();
            }
            Err(e) => log::warn!("accept failed: {e}"),
        }
    }

    Ok(())
}

#[cfg(unix)]
fn serve_sigterm(registry: Arc<Registry>) {
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            log::warn!("could not block SIGTERM; keeping default termination");
            return;
        }
        set
    };
    std::thread::Builder::new()
        .name("tty7-daemon-sigterm".to_string())
        .spawn(move || {
            let mut sig: libc::c_int = 0;
            if unsafe { libc::sigwait(&set, &mut sig) } == 0 {
                log::info!("daemon shutting down on SIGTERM");
                registry.drain_and_kill();
                on_shutdown();
                std::process::exit(0);
            }
        })
        .ok();
}

fn on_shutdown() {
    if let Some(store) = crate::core::machine::observed_store() {
        store.flush();
    }
    transport::remove_stale_endpoint();
    #[cfg(windows)]
    crate::host::server::remove_control_endpoint();
    crate::daemon::pidfile::remove();
}

fn handle_conn(stream: Stream, registry: Arc<Registry>) -> anyhow::Result<()> {
    let mut read_stream = stream;

    transport::authenticate(&mut read_stream)?;

    let write_stream = read_stream.try_clone()?;

    let (first_kind, first_payload) = crate::daemon::protocol::read_frame(&mut read_stream)?;
    if first_kind == crate::daemon::router::ROUTE_KIND {
        drop(write_stream);
        let header = crate::daemon::router::RouteHeader::decode(&first_payload)?;
        return Ok(crate::daemon::router::RemoteRouter::route(
            read_stream,
            &header,
        )?);
    }

    let first = ClientMsg::from_frame(first_kind, first_payload)?;
    match first {
        ClientMsg::Spawn {
            cwd,
            size,
            shell,
            owner,
        } => {
            let id = registry.alloc_id();
            let on_dead = {
                let registry = registry.clone();
                move || {
                    std::thread::Builder::new()
                        .name("tty7-daemon-pane-reap".to_string())
                        .spawn(move || {
                            registry.remove(id);
                        })
                        .ok();
                }
            };
            let pane = match DaemonPane::spawn(id, cwd, size, shell, owner, on_dead) {
                Ok(p) => p,
                Err(e) => {
                    let mut w = write_stream;
                    let _ = DaemonMsg::Error(format!("spawn failed: {e}")).encode(&mut w);
                    return Err(e);
                }
            };
            registry.insert(pane.clone());

            {
                let mut w = &write_stream;
                DaemonMsg::Spawned { pane_id: id }.encode(&mut w)?;
            }
            stream_pane(pane, id, read_stream, write_stream, registry)
        }

        ClientMsg::SpawnNativeSsh { cwd: _, size, spec } => {
            let id = registry.alloc_id();
            let on_dead = {
                let registry = registry.clone();
                move || {
                    std::thread::Builder::new()
                        .name("tty7-daemon-pane-reap".to_string())
                        .spawn(move || {
                            registry.remove(id);
                        })
                        .ok();
                }
            };
            let pane = match DaemonPane::spawn_native_ssh(id, size, spec, on_dead) {
                Ok(p) => p,
                Err(e) => {
                    let mut w = write_stream;
                    let _ =
                        DaemonMsg::Error(format!("native ssh spawn failed: {e}")).encode(&mut w);
                    return Err(e);
                }
            };
            registry.insert(pane.clone());
            {
                let mut w = &write_stream;
                DaemonMsg::Spawned { pane_id: id }.encode(&mut w)?;
            }
            stream_pane(pane, id, read_stream, write_stream, registry)
        }

        ClientMsg::Attach { pane_id, size: _ } => match registry.get(pane_id) {
            Some(pane) => {
                stream_pane_with_attach(pane, pane_id, read_stream, write_stream, registry)
            }
            None => {
                let mut w = write_stream;
                DaemonMsg::Error(format!("no such pane {pane_id}")).encode(&mut w)?;
                Ok(())
            }
        },

        ClientMsg::List => {
            let mut w = write_stream;
            DaemonMsg::PaneList(registry.list()).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::Version => {
            let mut w = write_stream;
            DaemonMsg::Version(DaemonVersion::current()).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::Shutdown => {
            log::info!("daemon shutting down on client request");
            registry.drain_and_kill();
            on_shutdown();
            std::process::exit(0);
        }

        ClientMsg::Kill { pane_id } => {
            if let Some(pane) = registry.remove(pane_id) {
                pane.kill();
            }
            Ok(())
        }

        ClientMsg::EnsureLoopbackForward(req) => {
            let mut w = write_stream;
            let Some(pane) = registry.get(req.pane_id) else {
                DaemonMsg::Error(format!("no such pane {}", req.pane_id)).encode(&mut w)?;
                return Ok(());
            };
            let Some(remote) = pane.remote_context() else {
                DaemonMsg::Error("pane has no ssh remote context".to_string()).encode(&mut w)?;
                return Ok(());
            };
            let result = if remote.kind == RemoteKind::NativeSsh {
                match pane.ssh_connection() {
                    Some(conn) => crate::daemon::ssh::SshManager::global()
                        .ensure_loopback_forward(
                            req.pane_id,
                            conn,
                            &remote.target,
                            &req.remote_host,
                            req.remote_port,
                        )
                        .map_err(|e| e.to_string()),
                    None => Err("native ssh connection is not ready".to_string()),
                }
            } else {
                Err("pane is not a native ssh session".to_string())
            };
            match result {
                Ok(forward) => DaemonMsg::LoopbackForward(forward).encode(&mut w)?,
                Err(e) => DaemonMsg::Error(format!("forward failed: {e}")).encode(&mut w)?,
            }
            Ok(())
        }

        ClientMsg::ListLoopbackForwards => {
            let mut w = write_stream;
            let list = crate::daemon::ssh::SshManager::global().list_loopback_forwards();
            DaemonMsg::LoopbackForwardList(list).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::CloseLoopbackForward(id) => {
            let mut w = write_stream;
            crate::daemon::ssh::SshManager::global().close_loopback_forward(&id);
            let list = crate::daemon::ssh::SshManager::global().list_loopback_forwards();
            DaemonMsg::LoopbackForwardList(list).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::ListKnownHosts => {
            let mut w = write_stream;
            let list = crate::daemon::ssh::known_hosts::list();
            DaemonMsg::KnownHostsList(list).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::DeleteKnownHost(id) => {
            let mut w = write_stream;
            let _ = crate::daemon::ssh::known_hosts::delete(&id);
            let list = crate::daemon::ssh::known_hosts::list();
            DaemonMsg::KnownHostsList(list).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::SftpList { pane_id, path } => {
            let mut w = write_stream;
            match ssh_connection_for(&registry, pane_id) {
                Ok(conn) => {
                    match crate::daemon::ssh::sftp::SftpManager::global().list(&conn, &path) {
                        Ok(entries) => DaemonMsg::SftpEntries(entries).encode(&mut w)?,
                        Err(e) => DaemonMsg::Error(e).encode(&mut w)?,
                    }
                }
                Err(e) => DaemonMsg::Error(e).encode(&mut w)?,
            }
            Ok(())
        }

        ClientMsg::AddForward { pane_id, rule } => {
            let mut w = write_stream;
            match forward_pane_connection(&registry, pane_id) {
                Ok(conn) => {
                    let list =
                        crate::daemon::ssh::SshManager::global().add_forward(pane_id, conn, &rule);
                    DaemonMsg::ForwardList(list).encode(&mut w)?;
                }
                Err(e) => DaemonMsg::Error(e).encode(&mut w)?,
            }
            Ok(())
        }

        ClientMsg::SftpOp { pane_id, op } => {
            let mut w = write_stream;
            match ssh_connection_for(&registry, pane_id) {
                Ok(conn) => {
                    let result = crate::daemon::ssh::sftp::SftpManager::global().op(&conn, &op);
                    DaemonMsg::SftpOpResult(result).encode(&mut w)?;
                }
                Err(e) => DaemonMsg::Error(e).encode(&mut w)?,
            }
            Ok(())
        }

        ClientMsg::SftpTransferStart(spec) => {
            let mut w = write_stream;
            match ssh_connection_for(&registry, spec.pane_id) {
                Ok(conn) => {
                    match crate::daemon::ssh::sftp::SftpManager::global()
                        .start_transfer(&conn, spec)
                    {
                        Ok(job_id) => DaemonMsg::SftpTransferStarted { job_id }.encode(&mut w)?,
                        Err(e) => DaemonMsg::Error(e).encode(&mut w)?,
                    }
                }
                Err(e) => DaemonMsg::Error(e).encode(&mut w)?,
            }
            Ok(())
        }

        ClientMsg::SftpTransferCancel { job_id } => {
            let mut w = write_stream;
            let jobs = crate::daemon::ssh::sftp::SftpManager::global().cancel(job_id);
            DaemonMsg::SftpTransferProgress(jobs).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::SftpTransferList { pane_id } => {
            let mut w = write_stream;
            let jobs = crate::daemon::ssh::sftp::SftpManager::global().list_jobs(pane_id);
            DaemonMsg::SftpTransferProgress(jobs).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::RemoveForward {
            pane_id,
            forward_id,
        } => {
            let mut w = write_stream;
            let list = crate::daemon::ssh::SshManager::global().remove_forward(pane_id, forward_id);
            DaemonMsg::ForwardList(list).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::QueryProcs { pane_id } => {
            let mut w = write_stream;
            let procs = registry.get(pane_id).map(|p| p.procs()).unwrap_or_default();
            DaemonMsg::Procs(procs).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::ListForwards { pane_id } => {
            let mut w = write_stream;
            let list = crate::daemon::ssh::SshManager::global().list_forwards(pane_id);
            DaemonMsg::ForwardList(list).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::OnWorkspace(req) => {
            let mut w = write_stream;
            crate::daemon::ssh::workspace::handle(&req).encode(&mut w)?;
            Ok(())
        }

        other => {
            log::debug!("unexpected opening message: {other:?}");
            Ok(())
        }
    }
}

fn forward_pane_connection(
    registry: &Registry,
    pane_id: u64,
) -> Result<Arc<SshConnection>, String> {
    let pane = registry
        .get(pane_id)
        .ok_or_else(|| format!("no such pane {pane_id}"))?;
    pane.ssh_connection()
        .ok_or_else(|| "pane is not a ready native-ssh session".to_string())
}

fn stream_pane_with_attach(
    pane: Arc<DaemonPane>,
    id: u64,
    read_stream: Stream,
    write_stream: Stream,
    registry: Arc<Registry>,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<DaemonMsg>();
    let epoch = pane.attach(tx);
    run_stream(pane, id, epoch, rx, read_stream, write_stream, registry)
}

fn stream_pane(
    pane: Arc<DaemonPane>,
    id: u64,
    read_stream: Stream,
    write_stream: Stream,
    registry: Arc<Registry>,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<DaemonMsg>();
    let epoch = pane.attach(tx);
    run_stream(pane, id, epoch, rx, read_stream, write_stream, registry)
}

fn run_stream(
    pane: Arc<DaemonPane>,
    id: u64,
    epoch: u64,
    rx: Receiver<DaemonMsg>,
    mut read_stream: Stream,
    write_stream: Stream,
    registry: Arc<Registry>,
) -> anyhow::Result<()> {
    let writer = spawn_writer(rx, write_stream, pane.gate());

    let mut killed = false;
    loop {
        match ClientMsg::read(&mut read_stream) {
            Ok(ClientMsg::Input(bytes)) => pane.write_input(&bytes),
            Ok(ClientMsg::Resize(size)) => pane.resize(size),
            Ok(ClientMsg::AuthResponse {
                request_id,
                response,
            }) => pane.deliver_auth_response(request_id, response),
            Ok(ClientMsg::Detach) => break,
            Ok(ClientMsg::Kill { pane_id }) => {
                if pane_id == id {
                    killed = true;
                    break;
                } else if let Some(other) = registry.remove(pane_id) {
                    other.kill();
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    let reclaimable = pane.detach(epoch);
    let _ = writer.join();

    if killed {
        if let Some(p) = registry.remove(id) {
            p.kill();
        }
    } else if reclaimable {
        registry.remove(id);
    }
    Ok(())
}

const OUTPUT_COALESCE_CAP: usize = 256 * 1024;

fn spawn_writer(
    rx: Receiver<DaemonMsg>,
    mut write_stream: Stream,
    gate: Arc<crate::daemon::pane::OutputGate>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("tty7-daemon-writer".to_string())
        .spawn(move || {
            crate::core::threads::promote_to_user_interactive();
            let mut carried: Option<DaemonMsg> = None;
            loop {
                let msg = match carried.take() {
                    Some(m) => m,
                    None => match rx.recv() {
                        Ok(m) => m,
                        Err(_) => break,
                    },
                };
                let msg = if let DaemonMsg::Output(mut buf) = msg {
                    while buf.len() < OUTPUT_COALESCE_CAP {
                        match rx.try_recv() {
                            Ok(DaemonMsg::Output(more)) => buf.extend_from_slice(&more),
                            Ok(other) => {
                                carried = Some(other);
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    DaemonMsg::Output(buf)
                } else {
                    msg
                };
                let drained = match &msg {
                    DaemonMsg::Output(b) => b.len(),
                    _ => 0,
                };
                let write_ok = msg.encode(&mut write_stream).is_ok();
                if drained > 0 {
                    gate.sub(drained);
                }
                if !write_ok {
                    break;
                }
                let _ = write_stream.flush();
            }
        })
        .expect("spawn daemon writer thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_id_is_monotonic_from_one() {
        let reg = Registry::new();
        assert_eq!(reg.alloc_id(), 1);
        assert_eq!(reg.alloc_id(), 2);
        assert_eq!(reg.alloc_id(), 3);
    }

    #[test]
    fn pane_ids_never_alias_what_the_persisted_tree_references() {
        use crate::core::machine::{MachineStore, PaneSeed};
        let dir = tempfile::TempDir::new().unwrap();
        let store = MachineStore::open(dir.path().join("machine.json"));
        let ws = store.workspace_create(None, None, None).unwrap();
        store
            .tab_create(ws.id, None, PaneSeed::bare(7), None, None)
            .unwrap();

        let reg = Registry::new();
        reg.seed_ids_past(&store.machine());
        assert_eq!(reg.alloc_id(), 8, "past the highest id the tree names");

        reg.seed_ids_past(&store.machine());
        assert_eq!(reg.alloc_id(), 9);
    }

    #[test]
    fn a_tree_naming_the_maximum_pane_id_does_not_panic_the_seed() {
        use crate::core::machine::{Machine, PaneRecord};
        let reg = Registry::new();
        reg.seed_ids_past(&Machine {
            workspaces: Vec::new(),
            panes: vec![PaneRecord::new(u64::MAX)],
        });
        assert_eq!(reg.alloc_id(), u64::MAX);
    }

    #[test]
    fn empty_registry_get_remove_list_are_empty() {
        let reg = Registry::new();
        assert!(reg.get(1).is_none());
        assert!(reg.remove(1).is_none());
        assert!(reg.list().is_empty());
    }

    #[cfg(unix)]
    mod conn {
        use super::super::{OUTPUT_COALESCE_CAP, Registry, handle_conn, spawn_writer};
        use crate::daemon::protocol::{ClientMsg, DaemonMsg, WinSize};
        use std::os::unix::net::UnixStream;
        use std::sync::{Arc, mpsc};
        use std::thread;

        const SIZE: WinSize = WinSize {
            cols: 80,
            rows: 24,
            cell_w: 8,
            cell_h: 17,
        };

        fn serve() -> (UnixStream, thread::JoinHandle<()>) {
            let (client, server) = UnixStream::pair().unwrap();
            let reg = Arc::new(Registry::new());
            let h = thread::spawn(move || {
                let _ = handle_conn(server, reg);
            });
            (client, h)
        }

        #[test]
        fn list_on_empty_registry_replies_with_empty_pane_list() {
            let (mut client, h) = serve();
            ClientMsg::List.encode(&mut client).unwrap();
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::PaneList(vec![])
            );
            h.join().unwrap();
        }

        #[test]
        fn attach_to_missing_pane_reports_error() {
            let (mut client, h) = serve();
            ClientMsg::Attach {
                pane_id: 999,
                size: SIZE,
            }
            .encode(&mut client)
            .unwrap();
            match DaemonMsg::read(&mut client).unwrap() {
                DaemonMsg::Error(msg) => assert!(msg.contains("999"), "error names the id"),
                other => panic!("expected Error, got {other:?}"),
            }
            h.join().unwrap();
        }

        #[test]
        fn kill_unknown_pane_closes_without_reply() {
            let (mut client, h) = serve();
            ClientMsg::Kill { pane_id: 123 }
                .encode(&mut client)
                .unwrap();
            assert!(DaemonMsg::read(&mut client).is_err());
            h.join().unwrap();
        }

        #[test]
        fn unexpected_opening_message_is_ignored_and_closed() {
            let (mut client, h) = serve();
            ClientMsg::Resize(SIZE).encode(&mut client).unwrap();
            assert!(DaemonMsg::read(&mut client).is_err());
            h.join().unwrap();
        }

        #[test]
        fn spawn_writer_frames_messages_then_exits_on_channel_close() {
            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            let (mut client, server) = UnixStream::pair().unwrap();
            let writer = spawn_writer(rx, server, Arc::new(crate::daemon::pane::OutputGate::new()));

            tx.send(DaemonMsg::Output(b"hi".to_vec())).unwrap();
            tx.send(DaemonMsg::Exited { code: Some(0) }).unwrap();
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Output(b"hi".to_vec())
            );
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Exited { code: Some(0) }
            );

            drop(tx);
            writer.join().unwrap();
        }

        #[test]
        fn spawn_writer_coalesces_queued_outputs_into_one_frame() {
            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            tx.send(DaemonMsg::Output(b"one".to_vec())).unwrap();
            tx.send(DaemonMsg::Output(b"two".to_vec())).unwrap();
            tx.send(DaemonMsg::Output(b"three".to_vec())).unwrap();
            drop(tx);

            let (mut client, server) = UnixStream::pair().unwrap();
            let writer = spawn_writer(rx, server, Arc::new(crate::daemon::pane::OutputGate::new()));

            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Output(b"onetwothree".to_vec()),
                "queued Outputs must merge into one frame, bytes in send order"
            );
            assert!(DaemonMsg::read(&mut client).is_err());
            writer.join().unwrap();
        }

        #[test]
        fn spawn_writer_splits_output_backlog_at_the_coalesce_cap() {
            const CHUNK: usize = 64 * 1024;
            let chunks: Vec<Vec<u8>> = (0u8..6).map(|i| vec![i; CHUNK]).collect();
            let expected: Vec<u8> = chunks.concat();

            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            for chunk in &chunks {
                tx.send(DaemonMsg::Output(chunk.clone())).unwrap();
            }
            drop(tx);

            let (mut client, server) = UnixStream::pair().unwrap();
            let writer = spawn_writer(rx, server, Arc::new(crate::daemon::pane::OutputGate::new()));

            let mut frames: Vec<Vec<u8>> = Vec::new();
            loop {
                match DaemonMsg::read(&mut client) {
                    Ok(DaemonMsg::Output(bytes)) => frames.push(bytes),
                    Ok(other) => panic!("expected only Output frames, got {other:?}"),
                    Err(_) => break,
                }
            }
            writer.join().unwrap();

            assert!(
                frames.len() >= 2,
                "a backlog over the cap must be split into multiple frames, got {}",
                frames.len()
            );
            for frame in &frames {
                assert!(
                    frame.len() <= OUTPUT_COALESCE_CAP + CHUNK,
                    "frame of {} bytes exceeds the cap by more than one message",
                    frame.len()
                );
            }
            assert_eq!(frames.concat(), expected, "no bytes lost or reordered");
        }

        #[test]
        fn spawn_writer_does_not_coalesce_outputs_across_a_non_output_message() {
            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            tx.send(DaemonMsg::Output(b"before".to_vec())).unwrap();
            tx.send(DaemonMsg::Exited { code: Some(0) }).unwrap();
            tx.send(DaemonMsg::Output(b"after".to_vec())).unwrap();
            drop(tx);

            let (mut client, server) = UnixStream::pair().unwrap();
            let writer = spawn_writer(rx, server, Arc::new(crate::daemon::pane::OutputGate::new()));

            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Output(b"before".to_vec()),
                "the first Output must not absorb bytes from beyond the Exited"
            );
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Exited { code: Some(0) },
                "the interrupting message keeps its place in the sequence"
            );
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Output(b"after".to_vec())
            );
            assert!(DaemonMsg::read(&mut client).is_err());
            writer.join().unwrap();
        }

        #[test]
        fn spawn_writer_exits_on_write_failure_while_sender_is_alive() {
            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            let (client, server) = UnixStream::pair().unwrap();
            let writer = spawn_writer(rx, server, Arc::new(crate::daemon::pane::OutputGate::new()));

            drop(client);

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !writer.is_finished() && std::time::Instant::now() < deadline {
                let _ = tx.send(DaemonMsg::Output(b"into the void".to_vec()));
                thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(
                writer.is_finished(),
                "writer must exit once the socket write fails, without waiting for channel close"
            );
            writer.join().unwrap();
            drop(tx);
        }
    }
}

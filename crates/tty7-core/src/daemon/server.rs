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

    /// Resume naming panes where the previous image left off.
    ///
    /// Reusing a number would be worse than skipping one: a client that
    /// reconnects after a handoff is still holding the old ids, and an `Attach`
    /// for one of them has to find the pane it means or nothing at all.
    fn claim_ids_past(&self, next: u64) {
        self.next_id.fetch_max(next, Ordering::Relaxed);
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

    fn all(&self) -> Vec<Arc<DaemonPane>> {
        self.panes.lock().unwrap().values().cloned().collect()
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

impl crate::host::server::PaneDirectory for Registry {
    fn pane_count(&self) -> u64 {
        self.panes.lock().unwrap().len() as u64
    }

    fn panes(&self) -> Vec<crate::daemon::protocol::PaneInfo> {
        self.list()
    }

    fn agent_states(&self) -> Vec<crate::daemon::control::PaneAgentState> {
        let panes: Vec<Arc<DaemonPane>> = self.panes.lock().unwrap().values().cloned().collect();
        let mut states: Vec<_> = panes.iter().filter_map(|p| p.agent_state()).collect();
        states.sort_by_key(|s| s.pane_id);
        states
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

/// Pane ids that something can still ask to see again: every pane a workspace
/// tree names, plus every pane this daemon is running.
///
/// The registry half matters for a pane spawned since the tree was last
/// written — without it a sweep landing in that window would delete the
/// snapshot of a pane that is very much alive, and the writer would put it
/// back seconds later.
fn restorable_pane_ids(registry: &Registry) -> std::collections::HashSet<u64> {
    let mut ids: std::collections::HashSet<u64> =
        registry.list().into_iter().map(|p| p.pane_id).collect();
    if let Some(store) = crate::core::machine::observed_store() {
        ids.extend(
            store
                .machine()
                .workspaces
                .iter()
                .flat_map(|w| w.tabs.iter())
                .flat_map(|t| t.root.pane_ids()),
        );
    }
    ids
}

/// Keep each pane's stored screen roughly current.
///
/// Only panes whose ring has moved are written, so an idle machine does no IO
/// at all, and the busy pane that most needs a fresh copy is the one that gets
/// it. Turning the setting off mid-run is honoured here too: the next tick
/// clears the directory rather than leaving terminal output on disk that the
/// user has just said they do not want stored.
fn spawn_scrollback_writer(registry: Arc<Registry>) {
    let spawned = std::thread::Builder::new()
        .name("tty7-scrollback".into())
        .spawn(move || {
            let mut marks: HashMap<u64, u64> = HashMap::new();
            let mut storing = crate::daemon::scrollback::enabled();
            loop {
                std::thread::sleep(crate::daemon::scrollback::SNAPSHOT_INTERVAL);
                let enabled = crate::daemon::scrollback::enabled();
                if !enabled {
                    if storing {
                        log::info!("scrollback persistence turned off; dropping what was stored");
                        crate::daemon::scrollback::sweep(&std::collections::HashSet::new());
                        marks.clear();
                        storing = false;
                    }
                    continue;
                }
                storing = true;
                for pane in registry.all() {
                    if marks.get(&pane.id) == Some(&pane.scrollback_mark()) {
                        continue;
                    }
                    let (segments, mark) = pane.scrollback_snapshot();
                    crate::daemon::scrollback::save(pane.id, &segments);
                    marks.insert(pane.id, mark);
                }
                crate::daemon::scrollback::sweep(&restorable_pane_ids(&registry));
                marks.retain(|id, _| registry.get(*id).is_some());
            }
        });
    if let Err(e) = spawned {
        log::warn!("could not start the scrollback writer: {e}");
    }
}

/// Write every pane's screen one last time, on the way out of a shutdown this
/// process was told about.
///
/// The periodic writer covers the deaths nobody gets to prepare for; this
/// covers the ones we do, and makes the copy exact rather than up to
/// [`SNAPSHOT_INTERVAL`](crate::daemon::scrollback::SNAPSHOT_INTERVAL) stale.
fn store_scrollback_now(registry: &Registry) {
    if !crate::daemon::scrollback::enabled() {
        return;
    }
    for pane in registry.all() {
        let (segments, _) = pane.scrollback_snapshot();
        crate::daemon::scrollback::save(pane.id, &segments);
    }
}

/// Turn a client's restore request into the screen its new pane opens with.
///
/// The snapshot is dropped as it is handed out. It has been folded into a live
/// pane's ring, which is where it will be persisted from now on — under that
/// pane's own id — and a copy left behind could only ever be used to restore
/// the same screen into a second pane.
fn restored_screen(
    request: crate::daemon::protocol::RestoreFrom,
) -> Option<crate::daemon::pane::Restore> {
    if !crate::daemon::scrollback::enabled() {
        return None;
    }
    let segments = crate::daemon::scrollback::load(request.pane_id)?;
    crate::daemon::scrollback::forget(request.pane_id);
    if segments.is_empty() {
        return None;
    }
    log::info!(
        "pane {} is gone; its last screen is restored into a fresh pane",
        request.pane_id
    );
    Some(crate::daemon::pane::Restore {
        segments,
        banner: request.banner,
    })
}

/// Close a pane for good: stop it, drop it from the registry, and drop the copy
/// of its screen.
///
/// A stored screen exists so a pane can survive a death nobody chose. A pane
/// the user closed is not that; keeping its output on disk afterwards would
/// only be a way for it to turn up in some later restore.
fn kill_pane(registry: &Registry, pane_id: u64) {
    if let Some(pane) = registry.remove(pane_id) {
        pane.kill();
    }
    crate::daemon::scrollback::forget(pane_id);
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

/// `eprintln!` for a process that may have no standard error.
///
/// A daemon started through the Windows clean-parent path (see
/// `daemon::spawn::windows`) has NULL standard handles: naming a logical parent
/// makes handle inheritance follow *that* process, so tty7 cannot hand the
/// child a `NUL` handle the way the ordinary `Stdio::null()` path does. On
/// Windows a failed write to stderr makes `eprintln!` panic, which would kill
/// the daemon before it serves anything. These notes are diagnostics for
/// someone running the daemon in a terminal; dropping them is the right
/// outcome when nobody is there to read them.
macro_rules! startup_note {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

pub fn run_daemon() -> anyhow::Result<()> {
    #[cfg(unix)]
    if let Some(inheritance) = crate::daemon::handoff::requested() {
        return run_adopting(inheritance);
    }

    // Before either endpoint, and before the machine tree is opened. Whoever
    // holds this is the server; everyone else stands down while it lives.
    //
    // The endpoints cannot decide this between themselves. They are bound in
    // sequence, so a second server can take one of them in the window before
    // the first takes the other, and the two then serve half a machine each —
    // panes on one socket, an empty pane registry answering the machine tree on
    // the other. That is how a workspace switch came to rebuild live sessions:
    // no error anywhere, just every pane reading as dead. See `singleton`.
    let _seat = match crate::daemon::singleton::claim() {
        crate::daemon::singleton::Claim::Held(seat) => Some(seat),
        crate::daemon::singleton::Claim::Taken => {
            startup_note!("tty7-server: another server already serves this config dir; exiting");
            return Ok(());
        }
        crate::daemon::singleton::Claim::Unavailable(why) => {
            startup_note!("tty7-server: starting without the single-server lock ({why})");
            None
        }
    };

    let registry = Arc::new(Registry::new());

    #[cfg(any(unix, windows))]
    {
        let mut services = control_services();
        services.panes = Some(registry.clone());
        match crate::host::server::spawn_control_listener_with(
            crate::host::local::LocalHost::shared(),
            services,
        ) {
            Ok(path) => startup_note!("tty7-server: control socket at {}", path.display()),
            Err(e) => startup_note!("tty7-server: control listener unavailable: {e}"),
        }
    }
    #[cfg(not(any(unix, windows)))]
    log::info!("no control listener on this platform; serving panes only");

    run_with(registry)
}

/// Come up as the far side of a handoff: the panes are already running, and
/// this image only has to recognise them.
///
/// The seat is adopted rather than claimed — the lock is held by this process,
/// which is the process that held it before the exec. Claiming would ask the
/// kernel for a lock our own descriptor already has, be refused, and take the
/// "another server is already serving" exit, which would leave the machine with
/// no daemon and a set of orphaned shells nobody can reach.
#[cfg(unix)]
fn run_adopting(inheritance: crate::daemon::handoff::Inheritance) -> anyhow::Result<()> {
    let _seat = inheritance
        .seat_fd
        .map(|fd| unsafe { crate::daemon::singleton::adopt(fd) });
    if _seat.is_none() {
        startup_note!("tty7-server: handed over without a seat descriptor; serving unprotected");
    }

    let registry = Arc::new(Registry::new());

    match crate::daemon::handoff::adopt(inheritance.blob_fd) {
        Some(adopted) => {
            registry.claim_ids_past(adopted.next_pane_id);
            let mut kept = 0usize;
            for carried in adopted.panes {
                let id = carried.id;
                let on_dead = reaper(registry.clone(), id);
                match crate::daemon::pane::DaemonPane::adopt(carried, on_dead) {
                    Ok(pane) => {
                        registry.insert(pane);
                        kept += 1;
                    }
                    // The shell is alive and this image cannot speak to its pty.
                    // Saying so is all that can be done: the descriptor closes
                    // when this process eventually exits, and the shell gets the
                    // hangup it would have got from an ordinary restart.
                    Err(e) => log::error!("pane {id} could not be adopted: {e}"),
                }
            }
            startup_note!("tty7-server: adopted {kept} pane(s) from the previous build");
        }
        // The blob is a file this process wrote and unlinked moments ago, so
        // this is a bug rather than an accident. The shells are the cost: their
        // ptys are still open here, held by descriptors nothing can now name,
        // which leaves them running with no reader — #42's stranded sessions,
        // arrived at from the other direction. They are released when this
        // daemon exits, so a restart clears it; there is nothing safe to close
        // in the meantime, because which descriptors they were is exactly what
        // was lost.
        None => startup_note!(
            "tty7-server: the handoff blob was unreadable; the previous build's panes are lost"
        ),
    }

    {
        let mut services = control_services();
        services.panes = Some(registry.clone());
        match crate::host::server::spawn_control_listener_with(
            crate::host::local::LocalHost::shared(),
            services,
        ) {
            Ok(path) => startup_note!("tty7-server: control socket at {}", path.display()),
            Err(e) => startup_note!("tty7-server: control listener unavailable: {e}"),
        }
    }

    run_with(registry)
}

/// Become `exe` in place, keeping every pane that can survive the crossing.
///
/// Returns the reason it did not happen; on success there is no return, because
/// by then this program has been replaced by the one it was asked to become.
#[cfg(unix)]
fn hand_over(registry: &Registry, exe: &std::path::Path) -> anyhow::Error {
    // Before anything is given up: the native-SSH panes below are hung up on
    // the promise that this process is about to stop existing, and an exec
    // that was never going to work must not collect on it. `execve` can still
    // fail after this — a wrong architecture, a permission the metadata does
    // not show — but the everyday failures, a path that is not there or not a
    // program, are caught while everything is still intact.
    match std::fs::metadata(exe) {
        Err(e) => return anyhow::anyhow!("cannot become {}: {e}", exe.display()),
        Ok(meta) => {
            use std::os::unix::fs::PermissionsExt as _;
            if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
                return anyhow::anyhow!("cannot become {}: not an executable file", exe.display());
            }
        }
    }

    let mut carried = Vec::new();
    for pane in registry.all() {
        match pane.carry() {
            Some(c) => carried.push(c),
            None => {
                // A native-SSH pane's session is cipher state in this process's
                // memory; the socket would cross and nothing able to speak on
                // it would. Hanging it up here means the far end sees a close
                // rather than a connection that has stopped answering.
                log::info!("pane {} cannot cross a handoff; closing it", pane.id);
                kill_pane(registry, pane.id);
            }
        }
    }

    // The tree is what the window rebuilds itself from when it reconnects, and
    // the reconnect happens milliseconds from now.
    if let Some(store) = crate::core::machine::observed_store() {
        store.flush();
    }

    crate::daemon::handoff::take_over(
        exe,
        carried,
        registry.alloc_id(),
        crate::daemon::singleton::held_fd(),
    )
}

#[cfg(not(unix))]
fn hand_over(_registry: &Registry, _exe: &std::path::Path) -> anyhow::Error {
    anyhow::anyhow!(
        "this platform has no way to replace a running program while keeping its \
         open consoles, so the daemon has to be stopped and started"
    )
}

/// What a pane does to the registry when its shell dies.
fn reaper(registry: Arc<Registry>, id: u64) -> impl FnOnce() + Send + 'static {
    move || {
        std::thread::Builder::new()
            .name("tty7-daemon-pane-reap".to_string())
            .spawn(move || {
                registry.remove(id);
            })
            .ok();
    }
}

pub fn control_services() -> crate::host::server::Services {
    use crate::core::machine::MachineStore;
    match MachineStore::shared() {
        Ok(machine) => {
            startup_note!("machine tree at {}", machine.path().display());
            crate::core::machine::publish_observations(&machine);
            crate::host::server::Services::with_machine(machine)
        }
        Err(e) => {
            startup_note!("no machine tree ({e}); serving files and panes only");
            crate::host::server::Services::none()
        }
    }
}

/// Say which pseudoconsole this daemon's panes will run on.
///
/// `portable-pty` loads a sideloaded `conpty.dll` if one sits beside the
/// running executable and silently uses `kernel32`'s otherwise. The difference
/// is invisible until someone asks why a pane swallowed an OSC 11 query (#345),
/// so the choice belongs in the log rather than in a bug report.
#[cfg(windows)]
fn report_conpty_host() {
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("conpty.dll")));
    match bundled {
        Some(path) if path.is_file() => {
            log::info!("panes use the bundled ConPTY at {}", path.display())
        }
        _ => log::warn!(
            "no conpty.dll beside this executable; panes fall back to the in-box conhost, \
             which does not answer terminal color queries"
        ),
    }
}

fn run_with(registry: Arc<Registry>) -> anyhow::Result<()> {
    crate::daemon::control::server_started();

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

    #[cfg(windows)]
    report_conpty_host();

    crate::daemon::pidfile::write_current();

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
    // Before the writer starts: a daemon that has just come up owns no panes,
    // so everything on disk belongs to panes the machine tree either still
    // names — those are the ones a window is about to ask to restore — or has
    // forgotten, and the latter are nobody's to restore any more.
    let restorable = restorable_pane_ids(&registry);
    if crate::daemon::scrollback::enabled() {
        crate::daemon::scrollback::sweep(&restorable);
    } else {
        crate::daemon::scrollback::sweep(&std::collections::HashSet::new());
    }
    // A daemon that was killed outright never retired anything, so the files of
    // panes that died with it are still here. Their commands cannot be
    // recovered — the mark saying which were new belongs to a shell that is
    // gone — so what is left is not to hoard them.
    crate::daemon::history::sweep(&restorable);
    spawn_scrollback_writer(registry.clone());

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
                // Ahead of the kill: `drain_and_kill` hangs up every pty, and a
                // pane whose shell has already been reaped has nothing left to
                // photograph.
                store_scrollback_now(&registry);
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
            workspace,
            restore,
        } => {
            let id = registry.alloc_id();
            if let Some(dead) = restore.as_ref().map(|r| r.pane_id) {
                // Before the spawn, because the spawn is what hands the shell
                // the name of its history file — and this is what puts the
                // predecessor's commands behind that name.
                crate::daemon::history::carry(dead, id);
            }
            let restore = restore.and_then(restored_screen);
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
            let pane =
                match DaemonPane::spawn(id, cwd, size, shell, owner, workspace, restore, on_dead) {
                    Ok(p) => p,
                    Err(e) => {
                        let mut w = write_stream;
                        // The daemon's own error is already a sentence; a second
                        // "spawn failed:" in front of it only pads the one the
                        // window ends up showing.
                        let _ = DaemonMsg::Error(format!("{e}")).encode(&mut w);
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

        // The size is accepted but deliberately not applied: an observer is
        // read-only, and resizing the pty for it would reflow the grid under
        // the controller. The replay tells the observer the pane's real size;
        // the field stays on the wire so a future observer-side viewport does
        // not need another message kind.
        ClientMsg::Observe { pane_id, size: _ } => match registry.get(pane_id) {
            Some(pane) => stream_observer(pane, read_stream, write_stream),
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
            // This is the path a restart takes, so it is the one that decides
            // whether panes come back showing anything. Ahead of the kill:
            // `drain_and_kill` hangs up every pty, and a pane whose shell has
            // been reaped has nothing left to photograph.
            store_scrollback_now(&registry);
            registry.drain_and_kill();
            // The ConPTY hosts (OpenConsole.exe) are this process's children,
            // not the shells', so the per-pane kill never reaches them — and
            // exiting right away would leave them holding the installed
            // OpenConsole.exe image open while an updater tries to replace it.
            // Reap everything still below us and wait for the images to be
            // released before the endpoint disappears, because the endpoint
            // going away is what tells `spawn::stop` the shutdown is complete.
            #[cfg(windows)]
            crate::daemon::winproc::reap_descendants_of(
                std::process::id(),
                std::time::Duration::from_secs(3),
            );
            on_shutdown();
            std::process::exit(0);
        }

        ClientMsg::Kill { pane_id } => {
            kill_pane(&registry, pane_id);
            Ok(())
        }

        ClientMsg::Handoff { exe } => {
            let mut w = write_stream;
            // Only ever returns having failed: a handoff that works replaces
            // this program mid-call, and there is nobody left to write a reply.
            let failure = hand_over(&registry, &exe);
            log::error!("handoff to {} did not happen: {failure}", exe.display());
            DaemonMsg::Error(failure.to_string()).encode(&mut w)?;
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

        ClientMsg::SendInput { pane_id, bytes } => {
            let mut w = write_stream;
            match registry.get(pane_id) {
                Some(pane) if pane.alive() => {
                    pane.write_input(&bytes);
                    DaemonMsg::InputAck { pane_id }.encode(&mut w)?;
                }
                Some(_) => {
                    DaemonMsg::Error(format!("pane {pane_id} is not running")).encode(&mut w)?
                }
                None => DaemonMsg::Error(format!("no such pane {pane_id}")).encode(&mut w)?,
            }
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

fn stream_observer(
    pane: Arc<DaemonPane>,
    mut read_stream: Stream,
    write_stream: Stream,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<DaemonMsg>();
    let refusals = tx.clone();
    let gate = Arc::new(crate::daemon::pane::OutputGate::new());
    let observer_id = pane.observe(tx, gate.clone());
    let writer = spawn_writer(rx, write_stream, gate);

    observe_loop(&mut read_stream, &refusals);

    pane.unobserve(observer_id);
    drop(refusals);
    let _ = writer.join();
    Ok(())
}

fn observe_loop<R: std::io::Read>(read_stream: &mut R, refusals: &mpsc::Sender<DaemonMsg>) {
    loop {
        match ClientMsg::read(read_stream) {
            Ok(ClientMsg::Input(_)) | Ok(ClientMsg::Resize(_)) => {
                let refused = refusals.send(DaemonMsg::Error(
                    "this connection is a read-only observer; attach to write".to_string(),
                ));
                if refused.is_err() {
                    break;
                }
            }
            Ok(ClientMsg::Detach) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
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
    use std::io::Read as _;

    // Blocking reads: when this controller is displaced its writer's channel
    // closes, and the writer shuts our read side down on its way out (see
    // spawn_writer), so the read below returns instead of hanging.
    let writer = spawn_writer(rx, write_stream, pane.gate());

    let mut killed = false;
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 65536];
    'conn: loop {
        loop {
            let (kind, payload) = match crate::daemon::protocol::take_frame(&mut pending) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(_) => break 'conn,
            };
            let Ok(msg) = ClientMsg::from_frame(kind, payload) else {
                break 'conn;
            };
            match msg {
                ClientMsg::Input(bytes) => {
                    if !pane.controls(epoch) {
                        break 'conn;
                    }
                    pane.write_input(&bytes);
                }
                ClientMsg::Resize(size) => {
                    if !pane.controls(epoch) {
                        break 'conn;
                    }
                    pane.resize(size);
                }
                ClientMsg::AuthResponse {
                    request_id,
                    response,
                } => pane.deliver_auth_response(request_id, response),
                ClientMsg::Detach => break 'conn,
                ClientMsg::Kill { pane_id } => {
                    if pane_id == id {
                        killed = true;
                        break 'conn;
                    }
                    kill_pane(&registry, pane_id);
                }
                _ => {}
            }
        }
        match read_stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => pending.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    // Asked after the loop, before detach clears the seat: whoever no longer
    // holds the epoch was preempted rather than having hung up on their own.
    let displaced = !pane.controls(epoch);
    let reclaimable = pane.detach(epoch);
    if displaced {
        let _ = read_stream.shutdown(std::net::Shutdown::Both);
    }
    let _ = writer.join();

    if killed {
        kill_pane(&registry, id);
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
                    // Image frames are lifted from the same PTY read the gate
                    // credits, so they must debit it too or the reader stays
                    // throttled against bytes that already left the queue.
                    DaemonMsg::Image(b) => b.len(),
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
            // This half and the connection's read half are try_clone'd views of
            // one socket, so shutting the read side down here unblocks whoever
            // is parked in read() on the other handle. The writer's channel
            // closing IS the handover signal — a new controller replaces the
            // subscriber, the old Sender drops, rx.recv() fails, and we land
            // here — so the displaced reader wakes at once and nobody has to
            // poll for it.
            let _ = write_stream.shutdown(std::net::Shutdown::Read);
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

    #[test]
    fn an_empty_registry_serves_empty_aggregates() {
        use crate::host::server::PaneDirectory as _;
        let reg = Registry::new();
        assert_eq!(reg.pane_count(), 0);
        assert!(reg.agent_states().is_empty());
    }

    #[test]
    fn the_observer_loop_refuses_writes_and_honors_detach() {
        let size = crate::daemon::protocol::WinSize {
            cols: 80,
            rows: 24,
            cell_w: 8,
            cell_h: 17,
        };
        let mut wire = Vec::new();
        ClientMsg::Input(b"echo hijack\r".to_vec())
            .encode(&mut wire)
            .unwrap();
        ClientMsg::Resize(size).encode(&mut wire).unwrap();
        ClientMsg::QueryProcs { pane_id: 1 }
            .encode(&mut wire)
            .unwrap();
        ClientMsg::Detach.encode(&mut wire).unwrap();
        ClientMsg::Input(b"too late".to_vec())
            .encode(&mut wire)
            .unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        observe_loop(&mut std::io::Cursor::new(wire), &tx);
        drop(tx);

        assert!(
            matches!(rx.try_recv(), Ok(DaemonMsg::Error(m)) if m.contains("read-only")),
            "an observer's Input must be answered with an Error, not forwarded"
        );
        assert!(
            matches!(rx.try_recv(), Ok(DaemonMsg::Error(m)) if m.contains("read-only")),
            "an observer's Resize must be answered with an Error, not applied"
        );
        assert!(
            rx.try_recv().is_err(),
            "Detach ends the loop; frames after it are never read"
        );
    }

    #[test]
    fn the_observer_loop_ends_at_stream_eof() {
        let (tx, rx) = std::sync::mpsc::channel();
        observe_loop(&mut std::io::Cursor::new(Vec::<u8>::new()), &tx);
        drop(tx);
        assert!(rx.try_recv().is_err());
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
        fn observe_of_a_missing_pane_reports_error() {
            let (mut client, h) = serve();
            ClientMsg::Observe {
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

        #[test]
        fn a_closed_writer_channel_wakes_the_controller_parked_in_read() {
            let (client, server) = UnixStream::pair().unwrap();
            let mut reader = server.try_clone().expect("clone the read half");
            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            let writer = spawn_writer(rx, server, Arc::new(crate::daemon::pane::OutputGate::new()));

            // Exactly where run_stream sits between frames.
            let parked = thread::spawn(move || {
                let mut buf = [0u8; 64];
                std::io::Read::read(&mut reader, &mut buf).ok()
            });

            // A handover: the pane seats a new controller and drops this one's
            // Sender. Nothing else happens — no poll, no timeout.
            drop(tx);

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !parked.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(
                parked.is_finished(),
                "a displaced controller must be woken by the writer's shutdown; \
                 if this hangs, run_stream is back to polling for the handover"
            );
            assert_eq!(
                parked.join().unwrap(),
                Some(0),
                "the woken read must report EOF, not a partial frame"
            );
            writer.join().unwrap();
            drop(client);
        }
    }
}

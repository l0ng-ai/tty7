use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::core::machine::{self, Attachment, MachineStore};
use crate::daemon::control::{
    CONTROL_VERSION, ControlClientMsg, ControlEvent, ControlHello, ControlHelloOk, ControlReply,
    ControlRequest, ControlServerMsg, GIT_STREAM_CHUNK, GIT_STREAM_CHUNK_MAX, LinkShutdown,
    MAX_CONCURRENT_GIT_STREAMS, PaneAgentState, ReplyOk, ServerStatus, WATCH_BURST_CAP, WireError,
    WireErrorKind, WorkspaceId, feature, server_started,
};
use crate::daemon::duplex::{Duplex, Halves};
use crate::daemon::protocol::PaneInfo;
use crate::host::{Host, SearchHit, SharedHost, WatchSub};

pub const MAX_WORKERS: usize = 64;

pub const WORKER_LINGER: Duration = Duration::from_secs(10);

pub const MAX_QUEUED: usize = 1024;

pub const LAYOUT_EVENT_QUEUE: usize = 1024;

pub trait PaneDirectory: Send + Sync {
    fn pane_count(&self) -> u64;
    fn panes(&self) -> Vec<PaneInfo>;
    fn agent_states(&self) -> Vec<PaneAgentState>;
}

#[derive(Clone, Default)]
pub struct Services {
    pub machine: Option<Arc<MachineStore>>,
    pub attachments: Arc<AttachRegistry>,
    pub panes: Option<Arc<dyn PaneDirectory>>,
}

impl Services {
    pub fn none() -> Services {
        Services::default()
    }

    pub fn with_machine(store: Arc<MachineStore>) -> Services {
        Services {
            machine: Some(store),
            attachments: Arc::new(AttachRegistry::default()),
            panes: None,
        }
    }
}

#[derive(Default)]
pub struct AttachRegistry {
    live: Mutex<Vec<Live>>,
    guis: Mutex<Vec<GuiLive>>,
    handover: Mutex<()>,
}

struct Live {
    workspace: String,
    conn: u64,
    token: String,
    hostname: String,
    sink: Arc<Sink>,
    shutdown: Arc<dyn LinkShutdown>,
    dedicated: bool,
}

struct Evicted {
    hostname: String,
    sink: Arc<Sink>,
    shutdown: Arc<dyn LinkShutdown>,
    dedicated: bool,
}

struct GuiLive {
    conn: u64,
    sink: Arc<Sink>,
}

impl AttachRegistry {
    fn handover(&self) -> std::sync::MutexGuard<'_, ()> {
        self.handover.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn holder(&self, workspace: &str) -> Option<(String, String)> {
        self.locked()
            .iter()
            .find(|l| l.workspace == workspace)
            .map(|l| (l.token.clone(), l.hostname.clone()))
    }

    pub fn len(&self) -> usize {
        self.locked().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn register_gui(&self, conn: u64, sink: Arc<Sink>) {
        let mut guis = self.guis.lock().unwrap_or_else(|e| e.into_inner());
        guis.retain(|gui| gui.conn != conn);
        guis.push(GuiLive { conn, sink });
    }

    fn unregister_gui(&self, conn: u64) {
        self.guis
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|gui| gui.conn != conn);
    }

    fn open_gui(&self, path: Option<String>, workspace: Option<WorkspaceId>) -> bool {
        // The newest GUI connection belongs to the most recently started app
        // process. Window recency is resolved inside that process, where GPUI
        // owns the authoritative focus state.
        loop {
            let target = self
                .guis
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last()
                .map(|gui| (gui.conn, Arc::clone(&gui.sink)));
            let Some((conn, sink)) = target else {
                return false;
            };
            let event = ControlServerMsg::Event(ControlEvent::GuiOpen {
                path: path.clone(),
                workspace,
            });
            if sink.send(&event).is_ok() {
                return true;
            }
            self.unregister_gui(conn);
        }
    }

    fn claim(
        &self,
        workspace: &str,
        conn: u64,
        holder: &Holder,
        dedicated: bool,
    ) -> Option<Evicted> {
        let mut live = self.locked();
        let evicted = match live.iter().position(|l| l.workspace == workspace) {
            Some(i) if live[i].conn == conn => {
                live[i].token = holder.token.clone();
                None
            }
            Some(i) => {
                let old = live.remove(i);
                Some(Evicted {
                    hostname: old.hostname,
                    sink: old.sink,
                    shutdown: old.shutdown,
                    dedicated: old.dedicated,
                })
            }
            None => None,
        };
        if !live.iter().any(|l| l.workspace == workspace) {
            live.push(Live {
                workspace: workspace.to_string(),
                conn,
                token: holder.token.clone(),
                hostname: holder.hostname.clone(),
                sink: Arc::clone(&holder.sink),
                shutdown: Arc::clone(&holder.shutdown),
                dedicated,
            });
        }
        evicted
    }

    fn forget_workspace(&self, workspace: &str) {
        self.locked().retain(|l| l.workspace != workspace);
    }

    fn release(&self, workspace: &str, conn: u64) -> bool {
        let mut live = self.locked();
        let before = live.len();
        live.retain(|l| !(l.workspace == workspace && l.conn == conn));
        live.len() != before
    }

    fn release_conn(&self, conn: u64) -> Vec<String> {
        let mut live = self.locked();
        let mut released = Vec::new();
        live.retain(|l| {
            if l.conn == conn {
                released.push(l.workspace.clone());
                false
            } else {
                true
            }
        });
        released
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Vec<Live>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }
}

struct Holder {
    token: String,
    hostname: String,
    sink: Arc<Sink>,
    shutdown: Arc<dyn LinkShutdown>,
}

pub fn serve<D: Duplex>(link: D, host: SharedHost) -> io::Result<()> {
    serve_with(link, host, Services::none())
}

pub fn serve_with<D: Duplex>(link: D, host: SharedHost, services: Services) -> io::Result<()> {
    let label = link.kind_label();
    let Halves {
        read,
        write,
        shutdown,
    } = link.split()?;
    serve_halves_with(read, write, shutdown, host, services, label)
}

pub fn serve_halves<R, W>(
    r: R,
    w: W,
    shutdown: Arc<dyn LinkShutdown>,
    host: SharedHost,
    label: &'static str,
) -> io::Result<()>
where
    R: Read,
    W: Write + Send + 'static,
{
    serve_halves_with(r, w, shutdown, host, Services::none(), label)
}

pub fn serve_halves_with<R, W>(
    mut r: R,
    w: W,
    shutdown: Arc<dyn LinkShutdown>,
    host: SharedHost,
    services: Services,
    label: &'static str,
) -> io::Result<()>
where
    R: Read,
    W: Write + Send + 'static,
{
    let sink = Arc::new(Sink::new(w));

    let hello = match handshake(&mut r, &sink, &*host, &services) {
        Ok(Some(hello)) => hello,
        Ok(None) => {
            let _ = shutdown.shutdown_link();
            return Ok(());
        }
        Err(e) => {
            let _ = shutdown.shutdown_link();
            return if is_disconnect(&e) {
                log::debug!("control peer ({label}) left during the handshake: {e}");
                Ok(())
            } else {
                Err(e)
            };
        }
    };

    let machine_sub = subscribe_machine(&services, &sink);

    let conn = Arc::new(Conn {
        host,
        sink: Arc::clone(&sink),
        inflight: Mutex::new(HashMap::new()),
        watches: Mutex::new(HashMap::new()),
        deferred_watches: Mutex::new(HashMap::new()),
        next_watch: AtomicU64::new(1),
        git_streams: Arc::new(AtomicUsize::new(0)),
        pool: Pool::new(),
        machine: services.machine.clone(),
        machine_origin: machine_sub.as_ref().map(machine::Subscription::id),
        attachments: Arc::clone(&services.attachments),
        panes: services.panes.clone(),
        id: NEXT_CONN.fetch_add(1, Ordering::Relaxed),
        holder: Holder {
            token: hello.client_token.clone(),
            hostname: hello.client_hostname.clone(),
            sink: Arc::clone(&sink),
            shutdown: Arc::clone(&shutdown),
        },
    });

    if hello.gui {
        // GUI registration starts only after a successful version handshake,
        // so an incompatible app can never be reported as a live receiver.
        services
            .attachments
            .register_gui(conn.id, Arc::clone(&sink));
    }

    if let Some(workspace) = hello.workspace.as_deref()
        && let Err(e) = attach_workspace(&conn, workspace, true)
    {
        log::warn!("control peer ({label}) could not attach to workspace {workspace}: {e}");
    }

    let outcome = read_loop(&mut r, &conn);

    conn.pool.close();
    conn.watches
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    conn.release_all_workspaces();
    // Every exit path converges here, including EOF and protocol errors.
    conn.attachments.unregister_gui(conn.id);
    drop(machine_sub);
    sink.retire();
    let _ = shutdown.shutdown_link();

    match outcome {
        Ok(()) => {
            log::debug!("control connection ({label}) closed by peer");
            Ok(())
        }
        Err(e) if is_disconnect(&e) => {
            log::debug!("control connection ({label}) ended: {e}");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn is_disconnect(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
    )
}

fn handshake<R: Read>(
    r: &mut R,
    sink: &Sink,
    host: &dyn Host,
    services: &Services,
) -> io::Result<Option<ControlHello>> {
    let hello = match ControlClientMsg::read(r)? {
        ControlClientMsg::Hello(h) => h,
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("control peer opened with {other:?} instead of HELLO"),
            ));
        }
    };

    let mut features = vec![
        feature::CONTROL.to_string(),
        feature::HOST_RPC.to_string(),
        feature::STDIO_BRIDGE.to_string(),
    ];
    if services.machine.is_some() {
        features.push(feature::MACHINE_TREE.to_string());
    }

    sink.send(&ControlServerMsg::HelloOk(ControlHelloOk {
        control_version: CONTROL_VERSION,
        protocol_version: crate::daemon::protocol::PROTOCOL_VERSION,
        build: env!("CARGO_PKG_VERSION").to_string(),
        separator: host.separator(),
        home: home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        features,
        instance: crate::daemon::control::server_instance().to_string(),
    }))?;

    if hello.control_version != CONTROL_VERSION {
        log::warn!(
            "control peer speaks v{}, this build speaks v{CONTROL_VERSION}; closing",
            hello.control_version
        );
        return Ok(None);
    }
    Ok(Some(hello))
}

static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

fn attach_workspace(
    conn: &Arc<Conn>,
    workspace: &str,
    dedicated: bool,
) -> io::Result<Option<String>> {
    if conn.machine.is_none() {
        return Err(io::Error::other(
            "this server does not serve the machine tree",
        ));
    }
    let tree_id: Option<crate::core::session::WorkspaceId> = workspace.parse().ok();
    let (displaced, evicted) = {
        let _handover = conn.attachments.handover();
        let attachment = Attachment::new(conn.holder.token.clone(), conn.holder.hostname.clone());
        let displaced = match (&conn.machine, tree_id) {
            (Some(machine), Some(id)) => machine.attach(id, attachment),
            _ => None,
        };
        let evicted = conn
            .attachments
            .claim(workspace, conn.id, &conn.holder, dedicated);
        (displaced, evicted)
    };

    if let Some(evicted) = evicted {
        log::info!(
            "workspace {workspace} taken over by {} from {}",
            conn.holder.hostname,
            evicted.hostname
        );
        let notice = ControlEvent::Preempted {
            workspace: workspace.to_string(),
            by: conn.holder.hostname.clone(),
        };
        if let Err(e) = evicted.sink.send(&ControlServerMsg::Event(notice)) {
            log::debug!("could not tell the displaced session about {workspace}: {e}");
        }
        if evicted.dedicated {
            let _ = evicted.shutdown.shutdown_link();
        }
    }

    Ok(displaced
        .filter(|a| a.token != conn.holder.token)
        .map(|a| a.hostname))
}

fn detach_workspace(conn: &Arc<Conn>, workspace: &str) -> io::Result<bool> {
    if conn.machine.is_none() {
        return Err(io::Error::other(
            "this server does not serve the machine tree",
        ));
    }
    let _handover = conn.attachments.handover();
    let released = conn.attachments.release(workspace, conn.id);
    let forgotten = match (&conn.machine, workspace.parse().ok()) {
        (Some(machine), Some(id)) => machine.detach(id, &conn.holder.token),
        _ => false,
    };
    Ok(released || forgotten)
}

fn home_dir() -> Option<PathBuf> {
    let pick = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    pick("HOME").or_else(|| pick("USERPROFILE"))
}

fn read_loop<R: Read>(r: &mut R, conn: &Arc<Conn>) -> io::Result<()> {
    loop {
        let msg = ControlClientMsg::read(r)?;
        match msg {
            ControlClientMsg::Hello(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "control peer sent a second HELLO on an established connection",
                ));
            }
            ControlClientMsg::Request { req_id, req } => submit(conn, req_id, req, Vec::new()),
            ControlClientMsg::RequestBlob { req_id, req, blob } => submit(conn, req_id, req, blob),
            ControlClientMsg::Cancel { req_id } => conn.cancel(req_id),
        }
    }
}

fn submit(conn: &Arc<Conn>, req_id: u64, req: ControlRequest, blob: Vec<u8>) {
    conn.inflight
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(req_id, false);

    let job_conn = Arc::clone(conn);
    let queued = conn
        .pool
        .submit(move || run_job(&job_conn, req_id, req, blob));
    if !queued {
        conn.finish(
            req_id,
            ControlReply::Err(WireError::new(
                WireErrorKind::Other,
                "the control server has more requests queued than it will hold",
            )),
            Vec::new(),
            false,
        );
    }
}

fn run_job(conn: &Arc<Conn>, req_id: u64, req: ControlRequest, blob: Vec<u8>) {
    if conn.is_cancelled(req_id) {
        conn.forget(req_id);
        return;
    }

    let wants_blob = req.returns_blob();
    let (reply, out_blob) = match run_request(conn, req_id, req, blob) {
        Ok((ok, bytes)) => (ControlReply::Ok(ok), bytes),
        Err(e) => (ControlReply::Err(WireError::from_io(&e)), Vec::new()),
    };
    conn.finish(req_id, reply, out_blob, wants_blob);
}

fn drop_unsendable_hits(hits: &mut Vec<SearchHit>) {
    let before = hits.len();
    hits.retain(|hit| hit.path.to_str().is_some());
    if hits.len() != before {
        log::debug!(
            "search dropped {} hit(s) whose paths are not UTF-8",
            before - hits.len()
        );
    }
}

fn machine_with_live_panes(conn: &Conn) -> io::Result<machine::Machine> {
    let mut machine = conn.machine()?.machine();
    let Some(panes) = conn.panes.as_ref().map(|p| p.panes()) else {
        return Ok(machine);
    };
    for info in panes {
        let record = match machine.panes.iter_mut().find(|p| p.id == info.pane_id) {
            Some(record) => record,
            None => {
                machine.panes.push(machine::PaneRecord::new(info.pane_id));
                machine.panes.last_mut().expect("record was just inserted")
            }
        };
        record.cwd = info.cwd.map(|p| p.to_string_lossy().into_owned());
        record.title = info.title;
        record.live = info.alive;
    }
    Ok(machine)
}

fn run_request(
    conn: &Arc<Conn>,
    req_id: u64,
    req: ControlRequest,
    blob: Vec<u8>,
) -> io::Result<(ReplyOk, Vec<u8>)> {
    let h: &dyn Host = &*conn.host;
    let p = |s: &str| PathBuf::from(s);

    Ok(match req {
        ControlRequest::Ping => (ReplyOk::Pong, Vec::new()),

        ControlRequest::ReadDir { dir, root } => {
            let root = root.map(|r| p(&r));
            let entries = h.read_dir(&p(&dir), root.as_deref())?;
            (ReplyOk::Entries(entries), Vec::new())
        }
        ControlRequest::Stat { path } => (ReplyOk::Meta(h.stat(&p(&path))?), Vec::new()),
        ControlRequest::Exists { path } => (ReplyOk::Bool(h.exists(&p(&path))), Vec::new()),
        ControlRequest::Canonicalize { path } => {
            let canon = h.canonicalize(&p(&path))?;
            (
                ReplyOk::Path(canon.to_string_lossy().into_owned()),
                Vec::new(),
            )
        }
        ControlRequest::ReadFile { path, max_bytes } => {
            let path = p(&path);
            let bytes = h.read_file(&path, max_bytes)?;
            let meta = h.stat(&path)?;
            (ReplyOk::FileMeta { meta }, bytes)
        }
        ControlRequest::Search {
            roots,
            query,
            limit,
            max_dirs,
            show_hidden,
        } => {
            let roots: Vec<PathBuf> = roots.iter().map(|r| p(r)).collect();
            let mut hits = h.search(
                &roots,
                &query,
                clamp_usize(limit),
                clamp_usize(max_dirs),
                show_hidden,
            )?;
            drop_unsendable_hits(&mut hits);
            (ReplyOk::Hits(hits), Vec::new())
        }

        ControlRequest::WriteFile { path } => {
            (ReplyOk::Meta(h.write_file(&p(&path), &blob)?), Vec::new())
        }
        ControlRequest::CreateFileNew { path } => {
            h.create_file_new(&p(&path))?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::CreateDir { path, recursive } => {
            h.create_dir(&p(&path), recursive)?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::Rename { from, to } => {
            h.rename(&p(&from), &p(&to))?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::Remove { path, recursive } => {
            h.remove(&p(&path), recursive)?;
            (ReplyOk::Unit, Vec::new())
        }

        ControlRequest::RepoRoot { path } => {
            let root = h.repo_root(&p(&path))?;
            (
                ReplyOk::OptPath(root.map(|r| r.to_string_lossy().into_owned())),
                Vec::new(),
            )
        }
        ControlRequest::Git { cwd, args } => {
            let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
            (ReplyOk::Output(h.git(&p(&cwd), &borrowed)?), Vec::new())
        }
        ControlRequest::GitStream { id, cwd, args } => {
            conn.start_git_stream(id, p(&cwd), args);
            (ReplyOk::Unit, Vec::new())
        }

        ControlRequest::Shells => (ReplyOk::Shells(h.shells()?), Vec::new()),

        ControlRequest::WatchOpen { dirs } => {
            let id = conn.open_watch(req_id, &paths(&dirs))?;
            (ReplyOk::WatchId(id), Vec::new())
        }
        ControlRequest::WatchSet { id, dirs } => {
            conn.set_watch_dirs(id, &paths(&dirs))?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::WatchClose { id } => {
            conn.close_watch(id);
            (ReplyOk::Unit, Vec::new())
        }

        ControlRequest::WorkspaceAttach { id } => (
            ReplyOk::Attached {
                took_over_from: attach_workspace(conn, &id, false)?,
            },
            Vec::new(),
        ),
        ControlRequest::WorkspaceDetach { id } => {
            detach_workspace(conn, &id)?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::GuiOpen { path, workspace } => (
            ReplyOk::Bool(conn.attachments.open_gui(path, workspace)),
            Vec::new(),
        ),

        ControlRequest::MachineGet => (
            ReplyOk::MachineTree(Box::new(machine_with_live_panes(conn)?)),
            Vec::new(),
        ),
        ControlRequest::WorkspaceTree { workspace } => (
            ReplyOk::WorkspaceTree(Box::new(conn.machine()?.workspace(workspace)?)),
            Vec::new(),
        ),
        ControlRequest::WorkspaceCreate { name, workspace } => (
            ReplyOk::WorkspaceTree(Box::new(conn.machine()?.workspace_create(
                workspace,
                name,
                conn.machine_origin,
            )?)),
            Vec::new(),
        ),
        ControlRequest::WorkspaceRename { workspace, name } => {
            conn.machine()?
                .workspace_rename(workspace, name, conn.machine_origin)?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::WorkspaceRemove { workspace } => {
            let store = conn.machine()?;
            let panes = {
                let _handover = conn.attachments.handover();
                let panes = store.workspace_delete(workspace, conn.machine_origin)?;
                conn.attachments.forget_workspace(&workspace.to_string());
                panes
            };
            (ReplyOk::Panes(panes), Vec::new())
        }
        ControlRequest::WorkspaceTouch { workspace } => {
            conn.machine()?
                .workspace_touch(workspace, conn.machine_origin)?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::WorkspaceSetActiveTab { workspace, tab } => {
            conn.machine()?
                .workspace_set_active_tab(workspace, tab, conn.machine_origin)?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::TabCreate {
            workspace,
            at,
            pane,
            tab,
        } => (
            ReplyOk::TabTree(Box::new(conn.machine()?.tab_create(
                workspace,
                at.map(clamp_usize),
                pane,
                tab,
                conn.machine_origin,
            )?)),
            Vec::new(),
        ),
        ControlRequest::TabClose { workspace, tab } => (
            ReplyOk::Panes(
                conn.machine()?
                    .tab_close(workspace, tab, conn.machine_origin)?,
            ),
            Vec::new(),
        ),
        ControlRequest::TabRename {
            workspace,
            tab,
            name,
        } => {
            conn.machine()?
                .tab_rename(workspace, tab, name, conn.machine_origin)?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::TabMove { workspace, tab, to } => {
            conn.machine()?
                .tab_move(workspace, tab, clamp_usize(to), conn.machine_origin)?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::TabSetGroup {
            workspace,
            tab,
            group,
        } => {
            conn.machine()?
                .tab_set_group(workspace, tab, group, conn.machine_origin)?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::PaneSplit {
            workspace,
            pane,
            axis,
            ratio,
            new,
            first,
        } => {
            conn.machine()?.pane_split(
                workspace,
                pane,
                axis,
                ratio,
                new,
                first,
                conn.machine_origin,
            )?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::PaneClose { workspace, pane } => (
            ReplyOk::Panes(
                conn.machine()?
                    .pane_close(workspace, pane, conn.machine_origin)?,
            ),
            Vec::new(),
        ),
        ControlRequest::PaneSetRatio {
            workspace,
            tab,
            path,
            ratio,
        } => {
            conn.machine()?
                .pane_set_ratio(workspace, tab, path, ratio, conn.machine_origin)?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::PaneMove {
            workspace,
            pane,
            to,
            axis,
            first,
        } => {
            conn.machine()?
                .pane_move(workspace, pane, to, axis, first, conn.machine_origin)?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::PaneReplace {
            workspace,
            old,
            new,
        } => {
            conn.machine()?
                .pane_replace(workspace, old, new, conn.machine_origin)?;
            (ReplyOk::Unit, Vec::new())
        }

        ControlRequest::AgentStates => (
            ReplyOk::AgentStates(
                conn.panes
                    .as_ref()
                    .map(|p| p.agent_states())
                    .unwrap_or_default(),
            ),
            Vec::new(),
        ),
        ControlRequest::Routes => (
            ReplyOk::Routes(crate::daemon::ssh::SshManager::global().routes()),
            Vec::new(),
        ),
        ControlRequest::Status => (
            ReplyOk::Status(ServerStatus {
                pid: std::process::id(),
                uptime_secs: server_started().elapsed().as_secs(),
                panes: conn.panes.as_ref().map(|p| p.pane_count()).unwrap_or(0),
                control_version: CONTROL_VERSION,
                protocol_version: crate::daemon::protocol::PROTOCOL_VERSION,
                build: env!("CARGO_PKG_VERSION").to_string(),
                socket: control_endpoint_display(),
            }),
            Vec::new(),
        ),
    })
}

#[cfg(unix)]
fn control_endpoint_display() -> String {
    control_socket_path()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

#[cfg(windows)]
fn control_endpoint_display() -> String {
    control_endpoint_path()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

#[cfg(not(any(unix, windows)))]
fn control_endpoint_display() -> String {
    String::new()
}

fn paths(v: &[String]) -> Vec<PathBuf> {
    v.iter().map(PathBuf::from).collect()
}

fn clamp_usize(v: u64) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

struct Conn {
    host: SharedHost,
    sink: Arc<Sink>,
    inflight: Mutex<HashMap<u64, bool>>,
    watches: Mutex<HashMap<u64, WatchSub>>,
    deferred_watches: Mutex<HashMap<u64, (u64, smol::channel::Receiver<Vec<PathBuf>>)>>,
    next_watch: AtomicU64,
    git_streams: Arc<AtomicUsize>,
    pool: Pool,
    machine: Option<Arc<MachineStore>>,
    machine_origin: Option<machine::SubscriberId>,
    attachments: Arc<AttachRegistry>,
    panes: Option<Arc<dyn PaneDirectory>>,
    id: u64,
    holder: Holder,
}

impl Conn {
    fn machine(&self) -> io::Result<&Arc<MachineStore>> {
        self.machine
            .as_ref()
            .ok_or_else(|| io::Error::other("this server does not serve the machine tree"))
    }

    fn release_all_workspaces(&self) {
        let _handover = self.attachments.handover();
        let released = self.attachments.release_conn(self.id);
        for workspace in released {
            if let (Some(machine), Some(id)) = (&self.machine, workspace.parse().ok()) {
                machine.detach(id, &self.holder.token);
            }
        }
    }

    fn is_cancelled(&self, req_id: u64) -> bool {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&req_id)
            .copied()
            .unwrap_or(false)
    }

    fn forget(&self, req_id: u64) {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&req_id);
    }

    fn cancel(&self, req_id: u64) {
        let mut f = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(flag) = f.get_mut(&req_id) {
            *flag = true;
        }
    }

    fn finish(&self, req_id: u64, reply: ControlReply, blob: Vec<u8>, wants_blob: bool) {
        let cancelled = self
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&req_id)
            .unwrap_or(false);
        if cancelled {
            log::trace!("control request {req_id} was cancelled; dropping its reply");
            self.start_deferred_watch(req_id, false);
            return;
        }

        let msg = if wants_blob && matches!(reply, ControlReply::Ok(_)) {
            ControlServerMsg::ResponseBlob {
                req_id,
                reply,
                blob,
            }
        } else {
            ControlServerMsg::Response { req_id, reply }
        };
        let delivered = match msg.to_frame() {
            Ok((k, payload)) => match self.sink.send_frame(k, &payload) {
                Ok(()) => true,
                Err(e) => {
                    log::debug!("control reply {req_id} could not be written: {e}");
                    false
                }
            },
            Err(e) => {
                log::warn!("control reply {req_id} could not be encoded: {e}");
                let excuse = ControlServerMsg::Response {
                    req_id,
                    reply: ControlReply::Err(WireError::from_io(&e)),
                };
                if let Err(e) = self.sink.send(&excuse) {
                    log::debug!("control error reply {req_id} could not be written: {e}");
                }
                false
            }
        };

        self.start_deferred_watch(req_id, delivered);
    }

    fn open_watch(&self, req_id: u64, dirs: &[PathBuf]) -> io::Result<u64> {
        let sub = self.host.watch(dirs)?;
        let id = self.next_watch.fetch_add(1, Ordering::Relaxed);
        let rx = sub.events().clone();
        self.watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, sub);
        self.deferred_watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(req_id, (id, rx));
        Ok(id)
    }

    fn start_deferred_watch(&self, req_id: u64, deliver: bool) {
        let parked = self
            .deferred_watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&req_id);
        let Some((id, rx)) = parked else {
            return;
        };
        if !deliver {
            self.watches
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return;
        }
        spawn_watch_forwarder(id, rx, Arc::clone(&self.sink));
    }

    fn start_git_stream(&self, id: u64, cwd: PathBuf, args: Vec<String>) {
        let host = Arc::clone(&self.host);
        let sink = Arc::clone(&self.sink);
        let slot = StreamSlot(Arc::clone(&self.git_streams));
        if slot.0.fetch_add(1, Ordering::AcqRel) >= MAX_CONCURRENT_GIT_STREAMS {
            drop(slot);
            let _ = self
                .sink
                .send(&ControlServerMsg::Event(ControlEvent::GitEnd {
                    id,
                    code: None,
                    failed: true,
                }));
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("tty7-control-git-stream".into())
            .spawn(move || {
                let _slot = slot;
                let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
                let mut batch: Vec<u8> = Vec::with_capacity(GIT_STREAM_CHUNK);
                let mut stopped: Option<StreamStop> = None;
                let flush = |batch: &mut Vec<u8>, stopped: &mut Option<StreamStop>| {
                    if stopped.is_some() {
                        batch.clear();
                        return;
                    }
                    if batch.is_empty() {
                        return;
                    }
                    let bytes = std::mem::take(batch);
                    for piece in bytes.chunks(GIT_STREAM_CHUNK_MAX) {
                        let event = ControlServerMsg::Event(ControlEvent::GitChunk {
                            id,
                            bytes: piece.to_vec(),
                        });
                        let Ok((kind, payload)) = event.to_frame() else {
                            *stopped = Some(StreamStop::Unencodable);
                            return;
                        };
                        if sink.send_frame(kind, &payload).is_err() {
                            *stopped = Some(StreamStop::LinkGone);
                            return;
                        }
                    }
                };
                let result = host.git_lines(&cwd, &borrowed, &mut |line| {
                    if stopped.is_some() {
                        return;
                    }
                    batch.extend_from_slice(line.as_bytes());
                    batch.push(b'\n');
                    if batch.len() >= GIT_STREAM_CHUNK {
                        flush(&mut batch, &mut stopped);
                    }
                });
                flush(&mut batch, &mut stopped);
                if matches!(stopped, Some(StreamStop::LinkGone)) {
                    return;
                }
                let (code, failed) = match (stopped, result) {
                    (Some(StreamStop::Unencodable), _) => (None, true),
                    (_, Ok(code)) => (code, false),
                    (_, Err(_)) => (None, true),
                };
                let _ = sink.send(&ControlServerMsg::Event(ControlEvent::GitEnd {
                    id,
                    code,
                    failed,
                }));
            });
        if spawned.is_err() {
            let _ = self
                .sink
                .send(&ControlServerMsg::Event(ControlEvent::GitEnd {
                    id,
                    code: None,
                    failed: true,
                }));
        }
    }

    fn set_watch_dirs(&self, id: u64, dirs: &[PathBuf]) -> io::Result<()> {
        let watches = self.watches.lock().unwrap_or_else(|e| e.into_inner());
        let sub = watches.get(&id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("no such watch subscription {id}"),
            )
        })?;
        sub.set_dirs(dirs)
    }

    fn close_watch(&self, id: u64) {
        self.watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
}

fn subscribe_machine(services: &Services, sink: &Arc<Sink>) -> Option<machine::Subscription> {
    let store = services.machine.as_ref()?;
    let (tx, rx) = smol::channel::bounded::<(String, machine::LayoutDelta)>(LAYOUT_EVENT_QUEUE);
    let lagged = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw_drop = Arc::clone(&lagged);
    let subscription = store.subscribe(Arc::new(
        move |workspace: &str, delta: &machine::LayoutDelta| {
            if tx.try_send((workspace.to_string(), delta.clone())).is_err() {
                saw_drop.store(true, Ordering::Release);
                log::warn!(
                    "dropping a layout delta for a peer {LAYOUT_EVENT_QUEUE} deltas behind; \
                     it will be told to resync"
                );
            }
        },
    ));
    spawn_layout_forwarder(rx, Arc::clone(sink), lagged);
    Some(subscription)
}

fn spawn_layout_forwarder(
    rx: smol::channel::Receiver<(String, machine::LayoutDelta)>,
    sink: Arc<Sink>,
    lagged: Arc<std::sync::atomic::AtomicBool>,
) {
    let spawned = std::thread::Builder::new()
        .name("tty7-control-layout".into())
        .spawn(move || {
            while let Ok((workspace, delta)) = rx.recv_blocking() {
                if lagged.swap(false, Ordering::AcqRel) {
                    let mut superseded = 1;
                    while rx.try_recv().is_ok() {
                        superseded += 1;
                    }
                    log::info!(
                        "dropping {superseded} superseded layout delta(s) and asking the peer \
                         to resync"
                    );
                    if sink
                        .send(&ControlServerMsg::Event(ControlEvent::LayoutResync))
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                let event = ControlEvent::Layout { workspace, delta };
                if sink.send(&ControlServerMsg::Event(event)).is_err() {
                    return;
                }
            }
        });
    if let Err(e) = spawned {
        log::warn!("could not start the layout forwarder: {e}");
    }
}

fn spawn_watch_forwarder(id: u64, rx: smol::channel::Receiver<Vec<PathBuf>>, sink: Arc<Sink>) {
    let spawned = std::thread::Builder::new()
        .name("tty7-control-watch".into())
        .spawn(move || {
            while let Ok(batch) = rx.recv_blocking() {
                let event = if batch.len() > WATCH_BURST_CAP {
                    ControlEvent::WatchOverflow { id }
                } else {
                    ControlEvent::Watch {
                        id,
                        paths: batch
                            .iter()
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect(),
                    }
                };
                if sink.send(&ControlServerMsg::Event(event)).is_err() {
                    return;
                }
            }
        });
    if let Err(e) = spawned {
        log::warn!("could not start the watch forwarder for subscription {id}: {e}");
    }
}

struct StreamSlot(Arc<AtomicUsize>);

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StreamStop {
    LinkGone,
    Unencodable,
}

struct Sink {
    out: Mutex<Option<Box<dyn Write + Send>>>,
}

impl Sink {
    fn new<W: Write + Send + 'static>(w: W) -> Sink {
        Sink {
            out: Mutex::new(Some(Box::new(w))),
        }
    }

    fn send(&self, msg: &ControlServerMsg) -> io::Result<()> {
        let (k, payload) = msg.to_frame()?;
        self.send_frame(k, &payload)
    }

    fn send_frame(&self, k: u8, payload: &[u8]) -> io::Result<()> {
        let mut slot = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let w = slot.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "control connection is closed")
        })?;
        crate::daemon::protocol::write_frame(&mut *w, k, payload).and_then(|()| w.flush())
    }

    fn retire(&self) {
        *self.out.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

type Job = Box<dyn FnOnce() + Send + 'static>;

struct Pool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    state: Mutex<PoolState>,
    wake: Condvar,
}

struct PoolState {
    jobs: VecDeque<Job>,
    workers: usize,
    idle: usize,
    closed: bool,
}

impl PoolState {
    fn wants_another_worker(&self) -> bool {
        self.jobs.len() > self.idle && self.workers < MAX_WORKERS
    }
}

impl Pool {
    fn new() -> Pool {
        Pool {
            inner: Arc::new(PoolInner {
                state: Mutex::new(PoolState {
                    jobs: VecDeque::new(),
                    workers: 0,
                    idle: 0,
                    closed: false,
                }),
                wake: Condvar::new(),
            }),
        }
    }

    fn submit(&self, job: impl FnOnce() + Send + 'static) -> bool {
        let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.closed || st.jobs.len() >= MAX_QUEUED {
            return false;
        }
        st.jobs.push_back(Box::new(job));

        if st.wants_another_worker() {
            st.workers += 1;
            let inner = Arc::clone(&self.inner);
            match std::thread::Builder::new()
                .name("tty7-control-worker".into())
                .spawn(move || worker(inner))
            {
                Ok(_) => return true,
                Err(e) => {
                    st.workers -= 1;
                    log::warn!("could not start a control worker: {e}");
                    if st.workers == 0 {
                        st.jobs.pop_back();
                        return false;
                    }
                }
            }
        }
        drop(st);
        self.inner.wake.notify_one();
        true
    }

    fn close(&self) {
        {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.closed = true;
            st.jobs.clear();
        }
        self.inner.wake.notify_all();
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.close();
    }
}

fn worker(inner: Arc<PoolInner>) {
    loop {
        let job = {
            let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(job) = st.jobs.pop_front() {
                    break job;
                }
                if st.closed {
                    st.workers -= 1;
                    return;
                }
                st.idle += 1;
                let (guard, timeout) = inner
                    .wake
                    .wait_timeout(st, WORKER_LINGER)
                    .unwrap_or_else(|e| e.into_inner());
                st = guard;
                st.idle -= 1;
                if timeout.timed_out() && st.jobs.is_empty() {
                    st.workers -= 1;
                    return;
                }
            }
        };
        job();
    }
}

pub const CONTROL_SOCK_ENV: &str = "TTY7_CONTROL_SOCK";

#[cfg(unix)]
mod sock {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::net::{UnixListener, UnixStream};

    const MAX_SOCKET_PATH_BYTES: usize = 100;

    /// The control endpoint, derived from the config dir exactly the way the
    /// pane endpoint is (`transport::socket_path_for`).
    ///
    /// It used to sit in `$XDG_RUNTIME_DIR/tty7` or `~/.local/share/tty7`,
    /// which meant it did not follow `--config-dir` while the pane socket did.
    /// Two consequences, both real: a `--config-dir` instance published the
    /// *default* control socket to the shells it spawned, so a CLI inside it
    /// talked to a different server entirely; and because both endpoints were
    /// named `daemon.sock`, telling them apart depended on their directories
    /// differing — so deriving one from the other by filename silently yielded
    /// the wrong socket. They are siblings now, distinguished by name, which is
    /// what the Windows arm has always done (`control.port`/`daemon.port`).
    pub fn control_socket_path() -> io::Result<PathBuf> {
        if let Some(explicit) = std::env::var_os(CONTROL_SOCK_ENV).filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(explicit));
        }

        let dir = crate::core::config::config_dir_path().ok_or_else(|| {
            io::Error::other("no config directory to place the control socket in")
        })?;

        let fallbacks: Vec<PathBuf> = [runtime_dir(), Some(std::env::temp_dir())]
            .into_iter()
            .flatten()
            .collect();
        socket_path_in(&dir, &fallbacks)
    }

    pub(super) const CONTROL_SOCK_FILE: &str = "control.sock";

    pub(crate) fn socket_path_in(dir: &Path, fallbacks: &[PathBuf]) -> io::Result<PathBuf> {
        let inline = dir.join(CONTROL_SOCK_FILE);
        if fits(&inline) {
            return Ok(inline);
        }

        use std::os::unix::ffi::OsStrExt as _;
        // `-control` keeps this clear of the pane socket's fallback name, which
        // hashes the same directory and would otherwise land on the same file.
        let name = format!(
            "tty7-{:016x}-control.sock",
            crate::host::fnv1a64(dir.as_os_str().as_bytes())
        );
        for base in fallbacks {
            let candidate = base.join(&name);
            if fits(&candidate) {
                return Ok(candidate);
            }
        }
        Err(io::Error::other(format!(
            "no directory short enough for a control socket ({MAX_SOCKET_PATH_BYTES}-byte \
             sun_path limit); set {CONTROL_SOCK_ENV} to one that is"
        )))
    }

    fn runtime_dir() -> Option<PathBuf> {
        std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
    }

    fn fits(p: &Path) -> bool {
        use std::os::unix::ffi::OsStrExt as _;
        p.as_os_str().as_bytes().len() <= MAX_SOCKET_PATH_BYTES
    }

    pub fn bind_control_socket(path: &Path) -> io::Result<UnixListener> {
        let parent = path.parent().unwrap_or(Path::new("."));
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }

        if path.exists() {
            match UnixStream::connect(path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!(
                            "a control server is already listening on {}",
                            path.display()
                        ),
                    ));
                }
                Err(_) => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }

        let listener = bind_private(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }

    pub(super) fn bind_private(path: &Path) -> io::Result<UnixListener> {
        static UMASK: Mutex<()> = Mutex::new(());
        let _held = UMASK.lock().unwrap_or_else(|e| e.into_inner());

        let previous = unsafe { libc::umask(0o077) };
        let bound = UnixListener::bind(path);
        unsafe { libc::umask(previous) };
        bound
    }

    pub fn serve_listener(listener: UnixListener, host: SharedHost) {
        serve_listener_with(listener, host, Services::none())
    }

    pub fn serve_listener_with(listener: UnixListener, host: SharedHost, services: Services) {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let host = Arc::clone(&host);
                    let services = services.clone();
                    let spawned = std::thread::Builder::new()
                        .name("tty7-control-conn".into())
                        .spawn(move || {
                            if let Err(e) = serve_with(stream, host, services) {
                                log::warn!("control connection failed: {e}");
                            }
                        });
                    if let Err(e) = spawned {
                        log::warn!("could not start a control connection thread: {e}");
                    }
                }
                Err(e) => log::warn!("control accept failed: {e}"),
            }
        }
    }

    pub fn spawn_control_listener(host: SharedHost) -> io::Result<PathBuf> {
        spawn_control_listener_with(host, Services::none())
    }

    pub fn spawn_control_listener_with(
        host: SharedHost,
        services: Services,
    ) -> io::Result<PathBuf> {
        // Anchor uptime here, not at the first Status request. The GUI can host
        // the control listener too, and there `run_with` never runs — so
        // without this the OnceLock is first set by whoever asks, and every
        // answer reports a server that just started.
        super::server_started();
        let path = control_socket_path()?;
        let listener = bind_control_socket(&path)?;
        std::thread::Builder::new()
            .name("tty7-control-listener".into())
            .spawn(move || serve_listener_with(listener, host, services))?;
        Ok(path)
    }
}

#[cfg(all(unix, test))]
pub(crate) use sock::socket_path_in;
#[cfg(unix)]
pub use sock::{
    bind_control_socket, control_socket_path, serve_listener, serve_listener_with,
    spawn_control_listener, spawn_control_listener_with,
};

#[cfg(windows)]
mod wsock {
    use super::*;
    use crate::daemon::transport;
    use std::net::TcpListener;

    pub const CONTROL_PORT_FILE: &str = "control.port";

    pub fn control_endpoint_path() -> io::Result<PathBuf> {
        transport::port_path_named(CONTROL_PORT_FILE).ok_or_else(|| {
            io::Error::other("no config directory to record the control endpoint in")
        })
    }

    pub fn spawn_control_listener_with(
        host: SharedHost,
        services: Services,
    ) -> io::Result<PathBuf> {
        // See the unix arm: uptime is anchored where the listener opens, so a
        // GUI-hosted control server does not report itself as freshly started.
        super::server_started();
        let path = control_endpoint_path()?;
        if let Ok(live) = transport::connect_endpoint(CONTROL_PORT_FILE) {
            drop(live);
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "a control server is already listening at {}",
                    transport::endpoint_display_named(CONTROL_PORT_FILE)
                ),
            ));
        }
        let (listener, token) =
            transport::bind_endpoint(CONTROL_PORT_FILE).map_err(io::Error::other)?;
        std::thread::Builder::new()
            .name("tty7-control-listener".into())
            .spawn(move || serve_listener_with(listener, token, host, services))?;
        Ok(path)
    }

    pub fn spawn_control_listener(host: SharedHost) -> io::Result<PathBuf> {
        spawn_control_listener_with(host, Services::none())
    }

    pub fn serve_listener_with(
        listener: TcpListener,
        token: transport::Token,
        host: SharedHost,
        services: Services,
    ) {
        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    transport::tune(&stream);
                    let host = Arc::clone(&host);
                    let services = services.clone();
                    let spawned = std::thread::Builder::new()
                        .name("tty7-control-conn".into())
                        .spawn(move || {
                            if let Err(e) = transport::check_endpoint_token(&mut stream, &token) {
                                log::warn!("control connection rejected: {e}");
                                return;
                            }
                            if let Err(e) = serve_with(stream, host, services) {
                                log::warn!("control connection failed: {e}");
                            }
                        });
                    if let Err(e) = spawned {
                        log::warn!("could not start a control connection thread: {e}");
                    }
                }
                Err(e) => log::warn!("control accept failed: {e}"),
            }
        }
    }

    pub fn connect_control() -> io::Result<std::net::TcpStream> {
        transport::connect_endpoint(CONTROL_PORT_FILE)
    }

    pub fn remove_control_endpoint() {
        transport::remove_endpoint(CONTROL_PORT_FILE);
    }
}

#[cfg(windows)]
pub use wsock::{
    CONTROL_PORT_FILE, connect_control, control_endpoint_path, remove_control_endpoint,
    spawn_control_listener, spawn_control_listener_with,
};

#[cfg(test)]
mod aggregate_tests {
    use super::*;
    use crate::core::cli_agent::{AgentSessionState, AgentStatus, CLIAgent};
    use crate::daemon::control::ControlClient;
    use crate::host::local::LocalHost;
    use std::net::{TcpListener, TcpStream};

    struct ThreePanesOneAgent {
        panes: Vec<PaneInfo>,
    }

    impl PaneDirectory for ThreePanesOneAgent {
        fn pane_count(&self) -> u64 {
            3
        }

        fn panes(&self) -> Vec<PaneInfo> {
            self.panes.clone()
        }

        fn agent_states(&self) -> Vec<PaneAgentState> {
            vec![PaneAgentState {
                pane_id: 7,
                agent: Some(CLIAgent::Claude),
                state: AgentSessionState {
                    status: AgentStatus::Working,
                    session_id: Some("sess-7".into()),
                    ..Default::default()
                },
            }]
        }
    }

    fn client_with(services: Services) -> ControlClient {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = serve_with(stream, LocalHost::new(), services);
        });
        let sock = TcpStream::connect(addr).unwrap();
        ControlClient::over_tcp(
            sock,
            &ControlHello::host_rpc("tok", "test-host"),
            Box::new(|_| {}),
        )
        .unwrap()
    }

    #[test]
    fn status_answers_with_this_servers_facts() {
        let services = Services {
            panes: Some(Arc::new(ThreePanesOneAgent { panes: Vec::new() })),
            ..Services::none()
        };
        let client = client_with(services);

        let ReplyOk::Status(status) = client.call(ControlRequest::Status).unwrap() else {
            panic!("Status must answer with ReplyOk::Status");
        };
        assert_eq!(status.pid, std::process::id());
        assert_eq!(status.panes, 3);
        assert_eq!(status.control_version, CONTROL_VERSION);
        assert_eq!(
            status.protocol_version,
            crate::daemon::protocol::PROTOCOL_VERSION
        );
        assert_eq!(status.build, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            status.socket,
            control_endpoint_display(),
            "the CLI dials whatever path Status names"
        );
        assert!(status.uptime_secs <= server_started().elapsed().as_secs());
    }

    #[test]
    fn agent_states_are_the_pane_directorys_snapshot() {
        let services = Services {
            panes: Some(Arc::new(ThreePanesOneAgent { panes: Vec::new() })),
            ..Services::none()
        };
        let client = client_with(services);

        let ReplyOk::AgentStates(states) = client.call(ControlRequest::AgentStates).unwrap() else {
            panic!("AgentStates must answer with ReplyOk::AgentStates");
        };
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].pane_id, 7);
        assert_eq!(states[0].agent, Some(CLIAgent::Claude));
        assert_eq!(states[0].state.status, AgentStatus::Working);
        assert_eq!(states[0].state.session_id.as_deref(), Some("sess-7"));
    }

    #[test]
    fn machine_get_overlays_live_pane_titles() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = MachineStore::open(dir.path().join(machine::MACHINE_FILE));
        let ws = store.workspace_create(None, None, None).unwrap();
        store
            .tab_create(
                ws.id,
                None,
                machine::PaneSeed {
                    pane: 7,
                    cwd: Some("/repo/tty7".into()),
                    ssh_spec: None,
                    agent: None,
                },
                None,
                None,
            )
            .unwrap();

        let services = Services {
            machine: Some(store),
            attachments: Arc::new(AttachRegistry::default()),
            panes: Some(Arc::new(ThreePanesOneAgent {
                panes: vec![PaneInfo {
                    pane_id: 7,
                    cwd: Some(PathBuf::from("/repo/tty7")),
                    title: "nvim".into(),
                    alive: true,
                    owner: None,
                }],
            })),
        };
        let client = client_with(services);
        let ReplyOk::MachineTree(machine) = client.call(ControlRequest::MachineGet).unwrap() else {
            panic!("MachineGet must answer with a machine tree");
        };
        let pane = machine.panes.iter().find(|p| p.id == 7).unwrap();
        assert_eq!(pane.title, "nvim");
        assert!(pane.live);
    }

    #[test]
    fn aggregates_still_answer_when_this_process_serves_no_panes() {
        let client = client_with(Services::none());

        let ReplyOk::AgentStates(states) = client.call(ControlRequest::AgentStates).unwrap() else {
            panic!("AgentStates must answer with ReplyOk::AgentStates");
        };
        assert!(states.is_empty());

        let ReplyOk::Status(status) = client.call(ControlRequest::Status).unwrap() else {
            panic!("Status must answer with ReplyOk::Status");
        };
        assert_eq!(status.panes, 0);

        let ReplyOk::Routes(routes) = client.call(ControlRequest::Routes).unwrap() else {
            panic!("Routes must answer with ReplyOk::Routes");
        };
        assert!(
            routes.iter().all(|r| !r.key.is_empty()),
            "whatever links exist are named; none are blank"
        );
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    #[test]
    fn the_pool_reuses_a_warm_worker() {
        let pool = Pool::new();

        let settled = || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                let st = pool.inner.state.lock().unwrap();
                if st.idle == st.workers {
                    return;
                }
                drop(st);
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("the pool never went idle");
        };

        for _ in 0..50 {
            let (tx, rx) = std::sync::mpsc::channel();
            assert!(pool.submit(move || {
                let _ = tx.send(());
            }));
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
            settled();
        }
        let st = pool.inner.state.lock().unwrap();
        assert_eq!(
            st.workers, 1,
            "50 sequential jobs should not want 50 threads"
        );
    }

    #[test]
    fn a_worker_is_spawned_when_the_backlog_outruns_the_parked_workers() {
        let state = |jobs: usize, workers: usize, idle: usize| PoolState {
            jobs: (0..jobs).map(|_| Box::new(|| ()) as Job).collect(),
            workers,
            idle,
            closed: false,
        };

        assert!(!state(1, 1, 1).wants_another_worker());

        assert!(state(2, 1, 1).wants_another_worker());

        assert!(state(1, 1, 0).wants_another_worker());

        assert!(!state(64, MAX_WORKERS, 0).wants_another_worker());
    }

    #[test]
    fn the_pool_grows_for_concurrent_work() {
        let pool = Pool::new();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        for _ in 0..8 {
            let gate = Arc::clone(&gate);
            let done_tx = done_tx.clone();
            assert!(pool.submit(move || {
                let (lock, cv) = &*gate;
                let mut open = lock.lock().unwrap();
                let _ = done_tx.send(());
                while !*open {
                    open = cv.wait(open).unwrap();
                }
            }));
        }
        for _ in 0..8 {
            done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert_eq!(pool.inner.state.lock().unwrap().workers, 8);

        let (lock, cv) = &*gate;
        *lock.lock().unwrap() = true;
        cv.notify_all();
        pool.close();
    }

    #[test]
    fn closing_the_pool_drops_queued_work() {
        let pool = Pool::new();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let ran = Arc::new(AtomicBool::new(false));

        let blocker = Arc::clone(&gate);
        assert!(pool.submit(move || {
            let (lock, cv) = &*blocker;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
        }));
        std::thread::sleep(Duration::from_millis(100));
        let ran2 = Arc::clone(&ran);
        pool.submit(move || ran2.store(true, Ordering::SeqCst));

        pool.close();
        let (lock, cv) = &*gate;
        *lock.lock().unwrap() = true;
        cv.notify_all();
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !ran.load(Ordering::SeqCst),
            "a job queued at close still ran"
        );
        assert!(!pool.submit(|| {}), "a closed pool must refuse work");
    }
}

#[cfg(test)]
mod gui_registry_tests {
    use super::*;
    use std::io::Cursor;

    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn gui_open_is_delivered_only_while_a_gui_is_registered() {
        let registry = AttachRegistry::default();
        assert!(!registry.open_gui(Some("/work".into()), None));

        let bytes = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(Sink::new(SharedWriter(Arc::clone(&bytes))));
        registry.register_gui(7, sink);
        assert!(registry.open_gui(Some("/work".into()), None));

        let frame = bytes.lock().unwrap().clone();
        assert_eq!(
            ControlServerMsg::read(&mut Cursor::new(frame)).unwrap(),
            ControlServerMsg::Event(ControlEvent::GuiOpen {
                path: Some("/work".into()),
                workspace: None,
            })
        );

        registry.unregister_gui(7);
        assert!(!registry.open_gui(None, None));
    }

    #[test]
    fn gui_open_carries_the_workspace_the_caller_named() {
        let registry = AttachRegistry::default();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(Sink::new(SharedWriter(Arc::clone(&bytes))));
        registry.register_gui(7, sink);

        let made = WorkspaceId::new();
        assert!(registry.open_gui(None, Some(made)));

        let frame = bytes.lock().unwrap().clone();
        assert_eq!(
            ControlServerMsg::read(&mut Cursor::new(frame)).unwrap(),
            ControlServerMsg::Event(ControlEvent::GuiOpen {
                path: None,
                workspace: Some(made),
            }),
            "the GUI has to be told which workspace, not left to guess a window"
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::daemon::control::{ControlHello, MTime, feature};
    use crate::host::local::LocalHost;
    use crate::host::remote::RemoteHost;
    use crate::host::{Entry, HostId, Meta, Output};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    struct Pair {
        host: Arc<RemoteHost>,
    }

    fn pair_with(server_host: SharedHost) -> Pair {
        let (server, client) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let _ = serve(server, server_host);
        });
        let hello = ControlHello::host_rpc("test-token", "test-host");
        let host = RemoteHost::over_unix(client, "test:pair", &hello).unwrap();
        Pair { host }
    }

    fn pair() -> Pair {
        pair_with(LocalHost::new())
    }

    fn raw() -> (UnixStream, ControlHelloOk) {
        raw_with(Services::none())
    }

    fn raw_with(services: Services) -> (UnixStream, ControlHelloOk) {
        raw_hello(services, ControlHello::host_rpc("t", "h")).0
    }

    fn raw_hello(
        services: Services,
        hello: ControlHello,
    ) -> ((UnixStream, ControlHelloOk), std::thread::JoinHandle<()>) {
        let (server, mut client) = UnixStream::pair().unwrap();
        let served = std::thread::spawn(move || {
            let _ = serve_with(server, LocalHost::new(), services);
        });
        ControlClientMsg::Hello(hello).encode(&mut client).unwrap();
        client.flush().unwrap();
        let ok = match ControlServerMsg::read(&mut client).unwrap() {
            ControlServerMsg::HelloOk(ok) => ok,
            other => panic!("{other:?}"),
        };
        ((client, ok), served)
    }

    fn hello_for(workspace: &str, token: &str, hostname: &str) -> ControlHello {
        ControlHello {
            control_version: CONTROL_VERSION,
            workspace: Some(workspace.to_string()),
            client_token: token.to_string(),
            client_hostname: hostname.to_string(),
            gui: false,
        }
    }

    fn round_trip(
        sock: &mut UnixStream,
        req_id: u64,
        req: ControlRequest,
    ) -> (ControlReply, Vec<ControlEvent>) {
        ControlClientMsg::Request { req_id, req }
            .encode(sock)
            .unwrap();
        sock.flush().unwrap();
        let mut events = Vec::new();
        loop {
            match ControlServerMsg::read(sock).unwrap() {
                ControlServerMsg::Response { req_id: got, reply } if got == req_id => {
                    return (reply, events);
                }
                ControlServerMsg::Event(e) => events.push(e),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    fn await_preempted(sock: &mut UnixStream) -> Option<ControlEvent> {
        loop {
            match ControlServerMsg::read(sock) {
                Ok(ControlServerMsg::Event(e @ ControlEvent::Preempted { .. })) => return Some(e),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }

    fn await_holder(registry: &AttachRegistry, workspace: &str, hostname: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while registry.holder(workspace).map(|(_, h)| h).as_deref() != Some(hostname) {
            assert!(
                Instant::now() < deadline,
                "{workspace} is held by {:?}, not {hostname}",
                registry.holder(workspace)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn workspace_services() -> (Services, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = MachineStore::open(dir.path().join(machine::MACHINE_FILE));
        (Services::with_machine(store), dir)
    }

    fn tree_workspace(services: &Services) -> String {
        services
            .machine
            .as_ref()
            .expect("workspace_services always carries a tree")
            .workspace_create(None, None, None)
            .expect("an empty tree accepts a workspace")
            .id
            .to_string()
    }

    struct SlowGit {
        inner: SharedHost,
        delay: Duration,
        running: Arc<AtomicBool>,
    }

    impl Host for SlowGit {
        fn id(&self) -> HostId {
            self.inner.id()
        }
        fn separator(&self) -> char {
            self.inner.separator()
        }
        fn is_absolute(&self, p: &Path) -> bool {
            self.inner.is_absolute(p)
        }
        fn read_dir(&self, dir: &Path, root: Option<&Path>) -> io::Result<Vec<Entry>> {
            self.inner.read_dir(dir, root)
        }
        fn stat(&self, p: &Path) -> io::Result<Meta> {
            self.inner.stat(p)
        }
        fn read_file(&self, p: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
            self.inner.read_file(p, max_bytes)
        }
        fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
            self.inner.canonicalize(p)
        }
        fn search(
            &self,
            roots: &[PathBuf],
            query: &str,
            limit: usize,
            max_dirs: usize,
            show_hidden: bool,
        ) -> io::Result<Vec<SearchHit>> {
            self.inner
                .search(roots, query, limit, max_dirs, show_hidden)
        }
        fn write_file(&self, p: &Path, bytes: &[u8]) -> io::Result<Meta> {
            self.inner.write_file(p, bytes)
        }
        fn create_file_new(&self, p: &Path) -> io::Result<()> {
            self.inner.create_file_new(p)
        }
        fn create_dir(&self, p: &Path, recursive: bool) -> io::Result<()> {
            self.inner.create_dir(p, recursive)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename(from, to)
        }
        fn remove(&self, p: &Path, recursive: bool) -> io::Result<()> {
            self.inner.remove(p, recursive)
        }
        fn repo_root(&self, p: &Path) -> io::Result<Option<PathBuf>> {
            self.inner.repo_root(p)
        }
        fn git(&self, _cwd: &Path, _args: &[&str]) -> io::Result<Output> {
            self.running.store(true, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            Ok(Output {
                status: Some(0),
                stdout: b"slow".to_vec(),
                stderr: Vec::new(),
            })
        }
        fn shells(&self) -> io::Result<crate::host::ShellInventory> {
            self.inner.shells()
        }
        fn watch(&self, dirs: &[PathBuf]) -> io::Result<WatchSub> {
            self.inner.watch(dirs)
        }
    }

    #[test]
    fn a_slow_request_does_not_block_a_fast_one() {
        let running = Arc::new(AtomicBool::new(false));
        let p = pair_with(Arc::new(SlowGit {
            inner: LocalHost::new(),
            delay: Duration::from_millis(1500),
            running: Arc::clone(&running),
        }));

        let tmp = tempfile::TempDir::new().unwrap();
        let slow_host = Arc::clone(&p.host);
        let dir = tmp.path().to_path_buf();
        let slow = std::thread::spawn(move || slow_host.git(&dir, &["status"]));

        let deadline = Instant::now() + Duration::from_secs(5);
        while !running.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(running.load(Ordering::SeqCst), "the slow request never ran");

        let started = Instant::now();
        p.host.stat(tmp.path()).unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(600),
            "a stat behind an in-flight git took {elapsed:?} — the server is serializing"
        );

        let out = slow.join().unwrap().unwrap();
        assert_eq!(out.stdout, b"slow");
    }

    #[test]
    fn many_slow_requests_still_leave_the_server_answering() {
        let running = Arc::new(AtomicBool::new(false));
        let p = pair_with(Arc::new(SlowGit {
            inner: LocalHost::new(),
            delay: Duration::from_millis(1200),
            running: Arc::clone(&running),
        }));
        let tmp = tempfile::TempDir::new().unwrap();

        let mut slow = Vec::new();
        for _ in 0..24 {
            let host = Arc::clone(&p.host);
            let dir = tmp.path().to_path_buf();
            slow.push(std::thread::spawn(move || {
                let _ = host.git(&dir, &["status"]);
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !running.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }

        let started = Instant::now();
        assert!(p.host.exists(tmp.path()));
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "24 in-flight gits stalled a trivial request"
        );
        for t in slow {
            let _ = t.join();
        }
    }

    #[test]
    fn replies_come_back_out_of_order() {
        let running = Arc::new(AtomicBool::new(false));
        let p = pair_with(Arc::new(SlowGit {
            inner: LocalHost::new(),
            delay: Duration::from_millis(800),
            running: Arc::clone(&running),
        }));
        let tmp = tempfile::TempDir::new().unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let host = Arc::clone(&p.host);
        let dir = tmp.path().to_path_buf();
        let slow_tx = tx.clone();
        let slow = std::thread::spawn(move || {
            let _ = host.git(&dir, &["status"]);
            let _ = slow_tx.send("git");
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !running.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        p.host.stat(tmp.path()).unwrap();
        tx.send("stat").unwrap();

        assert_eq!(
            rx.recv().unwrap(),
            "stat",
            "the later request answered first"
        );
        assert_eq!(rx.recv().unwrap(), "git");
        slow.join().unwrap();
    }

    #[test]
    fn the_handshake_describes_this_server() {
        let p = pair();
        let peer = p.host.peer();
        assert_eq!(peer.control_version, CONTROL_VERSION);
        assert_eq!(
            peer.protocol_version,
            crate::daemon::protocol::PROTOCOL_VERSION
        );
        assert_eq!(peer.separator, std::path::MAIN_SEPARATOR);
        assert!(peer.has_feature(feature::CONTROL));
        assert!(peer.has_feature(feature::HOST_RPC));
        assert!(
            !peer.home.is_empty(),
            "the server's $HOME backs `new workspace in ~`"
        );
    }

    #[test]
    fn a_version_mismatch_is_answered_then_closed() {
        let (server, mut client) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let _ = serve(server, LocalHost::new());
        });

        let mut hello = ControlHello::host_rpc("t", "h");
        hello.control_version = CONTROL_VERSION + 7;
        ControlClientMsg::Hello(hello).encode(&mut client).unwrap();
        client.flush().unwrap();

        match ControlServerMsg::read(&mut client).unwrap() {
            ControlServerMsg::HelloOk(ok) => assert_eq!(ok.control_version, CONTROL_VERSION),
            other => panic!("{other:?}"),
        }
        let mut buf = [0u8; 1];
        assert_eq!(
            client.read(&mut buf).unwrap(),
            0,
            "the server should hang up"
        );
    }

    #[test]
    fn the_client_refuses_a_mismatched_peer() {
        let (server, client) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let mut s = server;
            let _ = ControlClientMsg::read(&mut s);
            let _ = ControlServerMsg::HelloOk(ControlHelloOk {
                control_version: CONTROL_VERSION + 1,
                protocol_version: 3,
                build: "other".into(),
                separator: '/',
                home: "/root".into(),
                features: vec![],
                instance: "other-instance".into(),
            })
            .encode(&mut s);
        });
        let hello = ControlHello::host_rpc("t", "h");
        let err = RemoteHost::over_unix(client, "test:skew", &hello).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported, "{err}");
    }

    #[test]
    fn an_oversized_read_ships_no_bytes() {
        let (mut client, _) = raw();
        let tmp = tempfile::TempDir::new().unwrap();
        let fat = tmp.path().join("fat.bin");
        std::fs::write(&fat, vec![b'x'; 4096]).unwrap();

        ControlClientMsg::Request {
            req_id: 1,
            req: ControlRequest::ReadFile {
                path: fat.to_string_lossy().into_owned(),
                max_bytes: 16,
            },
        }
        .encode(&mut client)
        .unwrap();
        client.flush().unwrap();

        match ControlServerMsg::read(&mut client).unwrap() {
            ControlServerMsg::Response { req_id, reply } => {
                assert_eq!(req_id, 1);
                match reply {
                    ControlReply::Err(e) => assert_eq!(e.kind, WireErrorKind::FileTooLarge),
                    other => panic!("{other:?}"),
                }
            }
            ControlServerMsg::ResponseBlob { blob, .. } => {
                panic!("the refusal carried {} bytes of the file", blob.len())
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_empty_read_still_uses_the_blob_kind() {
        let (mut client, _) = raw();
        let tmp = tempfile::TempDir::new().unwrap();
        let empty = tmp.path().join("empty.txt");
        std::fs::write(&empty, b"").unwrap();

        ControlClientMsg::Request {
            req_id: 1,
            req: ControlRequest::ReadFile {
                path: empty.to_string_lossy().into_owned(),
                max_bytes: 1024,
            },
        }
        .encode(&mut client)
        .unwrap();
        client.flush().unwrap();

        match ControlServerMsg::read(&mut client).unwrap() {
            ControlServerMsg::ResponseBlob { reply, blob, .. } => {
                assert!(blob.is_empty());
                assert!(matches!(reply, ControlReply::Ok(ReplyOk::FileMeta { .. })));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn error_kinds_survive_the_round_trip() {
        let p = pair();
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();

        let cases: Vec<(io::ErrorKind, io::Error)> = vec![
            (
                io::ErrorKind::NotFound,
                p.host.stat(&root.join("nope")).unwrap_err(),
            ),
            (io::ErrorKind::AlreadyExists, {
                let f = root.join("taken.txt");
                p.host.write_file(&f, b"x").unwrap();
                p.host.create_file_new(&f).unwrap_err()
            }),
            (io::ErrorKind::FileTooLarge, {
                let f = root.join("fat.txt");
                p.host.write_file(&f, &vec![b'x'; 512]).unwrap();
                p.host.read_file(&f, 16).unwrap_err()
            }),
            (io::ErrorKind::IsADirectory, {
                let d = root.join("adir");
                p.host.create_dir(&d, false).unwrap();
                p.host.read_file(&d, 1024).unwrap_err()
            }),
            (io::ErrorKind::DirectoryNotEmpty, {
                let d = root.join("full");
                p.host.create_dir(&d, false).unwrap();
                p.host.write_file(&d.join("f"), b"x").unwrap();
                p.host.remove(&d, false).unwrap_err()
            }),
        ];
        for (want, got) in cases {
            assert_eq!(got.kind(), want, "{got}");
            assert!(!got.to_string().is_empty());
        }

        for kind in [
            WireErrorKind::NotFound,
            WireErrorKind::PermissionDenied,
            WireErrorKind::AlreadyExists,
            WireErrorKind::InvalidInput,
            WireErrorKind::NotADirectory,
            WireErrorKind::IsADirectory,
            WireErrorKind::DirectoryNotEmpty,
            WireErrorKind::FileTooLarge,
            WireErrorKind::TimedOut,
            WireErrorKind::ConnectionReset,
            WireErrorKind::Other,
        ] {
            let io_err = io::Error::new(kind.to_io_kind(), "x");
            assert_eq!(
                WireError::from_io(&io_err).kind,
                kind,
                "{kind:?} is not its own round trip"
            );
        }
    }

    #[test]
    fn a_failing_git_is_ok_across_the_wire() {
        let p = pair();
        let tmp = tempfile::TempDir::new().unwrap();
        let Ok(out) = p.host.git(tmp.path(), &["rev-parse", "--show-toplevel"]) else {
            return;
        };
        if out.success() {
            return;
        }
        assert!(out.status.is_some());
        assert!(
            !out.stderr.is_empty(),
            "stderr crosses the wire, not dropped"
        );
    }

    #[test]
    fn an_unknown_frame_kind_ends_the_connection() {
        let (mut client, _) = raw();
        crate::daemon::protocol::write_frame(&mut client, 99, b"junk").unwrap();
        client.flush().unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(
            client.read(&mut buf).unwrap(),
            0,
            "the server should hang up"
        );
    }

    #[test]
    fn a_cancelled_request_is_answered_with_silence() {
        let (mut client, _) = raw();
        let tmp = tempfile::TempDir::new().unwrap();

        ControlClientMsg::Cancel { req_id: 1 }
            .encode(&mut client)
            .unwrap();
        ControlClientMsg::Request {
            req_id: 2,
            req: ControlRequest::Stat {
                path: tmp.path().to_string_lossy().into_owned(),
            },
        }
        .encode(&mut client)
        .unwrap();
        client.flush().unwrap();

        match ControlServerMsg::read(&mut client).unwrap() {
            ControlServerMsg::Response { req_id, reply } => {
                assert_eq!(req_id, 2);
                assert!(matches!(reply, ControlReply::Ok(ReplyOk::Meta(_))));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_machine_tree_is_declined_not_faked() {
        let p = pair();
        let err = p
            .host
            .client()
            .call(ControlRequest::MachineGet)
            .unwrap_err();
        assert!(err.to_string().contains("machine tree"), "{err}");
    }

    #[test]
    fn watch_batches_reach_the_client() {
        let p = pair();
        let tmp = tempfile::TempDir::new().unwrap();
        let sub = p.host.watch(&[tmp.path().to_path_buf()]).unwrap();
        while sub.events().try_recv().is_ok() {}

        let deadline = Instant::now() + Duration::from_secs(8);
        let mut seen = false;
        while Instant::now() < deadline && !seen {
            std::fs::write(tmp.path().join("hello.txt"), b"x").unwrap();
            std::thread::sleep(Duration::from_millis(200));
            while let Ok(batch) = sub.events().try_recv() {
                if batch
                    .iter()
                    .any(|q| q.file_name().is_some_and(|n| n == "hello.txt"))
                {
                    seen = true;
                }
            }
        }
        assert!(seen, "no watch event crossed the connection");
    }

    #[test]
    fn a_watch_id_reaches_the_client_before_any_batch_for_it() {
        let (mut client, _ok) = raw();
        let tmp = tempfile::TempDir::new().unwrap();

        ControlClientMsg::Request {
            req_id: 1,
            req: ControlRequest::WatchOpen {
                dirs: vec![tmp.path().to_string_lossy().into_owned()],
            },
        }
        .encode(&mut client)
        .unwrap();
        client.flush().unwrap();

        let churn = tmp.path().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let churning = Arc::clone(&stop);
        let churner = std::thread::spawn(move || {
            let mut n = 0u32;
            while !churning.load(Ordering::SeqCst) {
                let _ = std::fs::write(churn.join(format!("f{n}")), b"x");
                n = n.wrapping_add(1);
                std::thread::sleep(Duration::from_millis(1));
            }
        });

        let first = ControlServerMsg::read(&mut client).unwrap();
        stop.store(true, Ordering::SeqCst);
        churner.join().unwrap();

        match first {
            ControlServerMsg::Response {
                req_id: 1,
                reply: ControlReply::Ok(ReplyOk::WatchId(_)),
            } => {}
            other => panic!("a batch overtook the WatchId reply: {other:?}"),
        }
    }

    #[test]
    fn closing_a_watch_releases_it_on_the_server() {
        let (server, client) = UnixStream::pair().unwrap();
        let host = LocalHost::new();
        std::thread::spawn(move || {
            let _ = serve(server, host);
        });
        let hello = ControlHello::host_rpc("t", "h");
        let remote = RemoteHost::over_unix(client, "test:watch", &hello).unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let sub = remote.watch(&[tmp.path().to_path_buf()]).unwrap();
        let rx = sub.events().clone();
        drop(sub);

        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(tmp.path().join("after.txt"), b"x").unwrap();
        std::thread::sleep(Duration::from_millis(800));
        while let Ok(batch) = rx.try_recv() {
            assert!(
                !batch
                    .iter()
                    .any(|q| q.file_name().is_some_and(|n| n == "after.txt")),
                "the server kept watching after the subscription closed"
            );
        }
    }

    #[test]
    fn a_gitignore_edit_reaches_a_remote_listing() {
        let p = pair();
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        p.host
            .write_file(&root.join(".gitignore"), b"*.log\n")
            .unwrap();
        p.host.write_file(&root.join("a.log"), b"").unwrap();

        let listed = p.host.read_dir(&root, Some(&root)).unwrap();
        assert!(
            listed.iter().any(|e| e.name == "a.log" && e.ignored),
            "the fixture should start out ignored"
        );

        let sub = p.host.watch(std::slice::from_ref(&root)).unwrap();

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut cleared = false;
        while Instant::now() < deadline {
            p.host
                .write_file(&root.join(".gitignore"), b"# nothing\n")
                .unwrap();
            std::thread::sleep(Duration::from_millis(250));
            while sub.events().try_recv().is_ok() {}
            if p.host
                .read_dir(&root, Some(&root))
                .unwrap()
                .iter()
                .any(|e| e.name == "a.log" && !e.ignored)
            {
                cleared = true;
                break;
            }
        }
        assert!(
            cleared,
            "a `.gitignore` change on the server must reach the remote client's listing"
        );
    }

    #[test]
    fn a_write_answers_with_the_new_metadata() {
        let (mut client, _) = raw();
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("w.txt");

        ControlClientMsg::RequestBlob {
            req_id: 1,
            req: ControlRequest::WriteFile {
                path: f.to_string_lossy().into_owned(),
            },
            blob: b"hello".to_vec(),
        }
        .encode(&mut client)
        .unwrap();
        client.flush().unwrap();

        match ControlServerMsg::read(&mut client).unwrap() {
            ControlServerMsg::Response {
                reply: ControlReply::Ok(ReplyOk::Meta(m)),
                ..
            } => {
                assert_eq!(m.len, 5);
                assert!(
                    m.mtime.is_some(),
                    "the editor keys its echo detection on this"
                );
                let on_disk = std::fs::metadata(&f).unwrap();
                assert_eq!(
                    MTime::from_system_time(on_disk.modified().unwrap()),
                    m.mtime.unwrap()
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_socket_path_honours_its_override() {
        let dir = tempfile::TempDir::new().unwrap();
        let sock = dir.path().join("explicit.sock");
        let listener = bind_control_socket(&sock).unwrap();
        assert!(sock.exists());
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777,
            0o600,
            "a control socket is the access boundary; it must not be group- or world-reachable"
        );
        drop(listener);
    }

    #[test]
    fn binding_does_not_re_permission_a_directory_it_did_not_create() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::TempDir::new().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();

        let listener = bind_control_socket(&shared.join("s.sock")).unwrap();
        assert_eq!(
            std::fs::metadata(&shared).unwrap().permissions().mode() & 0o7777,
            0o1777,
            "a directory tty7 did not create must keep its own permissions"
        );
        drop(listener);
    }

    #[test]
    fn a_bind_is_owner_only_under_a_permissive_umask() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("permissive.sock");

        let previous = unsafe { libc::umask(0) };
        let bound = sock::bind_private(&path);
        unsafe { libc::umask(previous) };

        let listener = bound.unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
            0,
            "bind left a window in which another user could connect"
        );
        drop(listener);
    }

    #[test]
    fn a_filename_that_is_not_utf8_costs_only_its_own_hit() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let hit = |name: &str, path: &Path| SearchHit {
            name: name.to_string(),
            path: path.to_path_buf(),
            is_dir: false,
            ignored: false,
        };
        let mut hits = vec![
            hit("plain-needle.rs", Path::new("/home/me/plain-needle.rs")),
            hit(
                "caf\u{fffd}-needle.rs",
                Path::new(OsStr::from_bytes(b"/home/me/caf\xe9-needle.rs")),
            ),
            hit("other-needle.rs", Path::new("/home/me/other-needle.rs")),
        ];

        drop_unsendable_hits(&mut hits);
        let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(
            names,
            ["plain-needle.rs", "other-needle.rs"],
            "the representable hits must survive their neighbour"
        );
    }

    #[test]
    fn a_socket_connection_serves_requests() {
        let dir = tempfile::TempDir::new().unwrap();
        let sock = dir.path().join("s.sock");
        let listener = bind_control_socket(&sock).unwrap();
        std::thread::spawn(move || serve_listener(listener, LocalHost::new()));

        let stream = UnixStream::connect(&sock).unwrap();
        let hello = ControlHello::host_rpc("t", "h");
        let host = RemoteHost::over_unix(stream, "test:sock", &hello).unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        host.write_file(&tmp.path().join("x.txt"), b"content")
            .unwrap();
        assert_eq!(
            host.read_file(&tmp.path().join("x.txt"), 1024).unwrap(),
            b"content"
        );
        let entries = host.read_dir(tmp.path(), None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "x.txt");
    }

    #[test]
    fn binding_refuses_a_live_server() {
        let dir = tempfile::TempDir::new().unwrap();
        let sock = dir.path().join("s.sock");
        let _first = bind_control_socket(&sock).unwrap();
        assert_eq!(
            bind_control_socket(&sock).unwrap_err().kind(),
            io::ErrorKind::AddrInUse
        );
    }

    #[test]
    fn a_too_long_socket_path_falls_back_to_one_that_fits() {
        let short = PathBuf::from("/tmp/rt");
        let long = PathBuf::from(format!("/tmp/{}", "d".repeat(120)));

        assert_eq!(
            sock::socket_path_in(&short, &[]).unwrap(),
            PathBuf::from("/tmp/rt/control.sock")
        );

        let picked = sock::socket_path_in(&long, &[long.clone(), short.clone()]).unwrap();
        assert!(picked.starts_with(&short), "{}", picked.display());
        assert!(picked.to_string_lossy().ends_with(".sock"));

        let other = PathBuf::from(format!("/tmp/{}", "e".repeat(120)));
        assert_ne!(
            picked,
            sock::socket_path_in(&other, std::slice::from_ref(&short)).unwrap()
        );

        assert_eq!(
            picked,
            sock::socket_path_in(&long, &[long.clone(), short.clone()]).unwrap()
        );

        let err = sock::socket_path_in(&long, std::slice::from_ref(&long)).unwrap_err();
        assert!(err.to_string().contains(CONTROL_SOCK_ENV), "{err}");
    }

    #[test]
    fn binding_clears_a_socket_a_crash_left_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        let sock = dir.path().join("s.sock");
        let first = bind_control_socket(&sock).unwrap();
        drop(first);
        assert!(sock.exists());

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match bind_control_socket(&sock) {
                Ok(listener) => {
                    drop(listener);
                    return;
                }
                Err(e) if Instant::now() >= deadline => {
                    panic!("a stale control socket was never cleared: {e}")
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    #[test]
    fn a_stream_slot_comes_back_however_it_leaves() {
        let count = Arc::new(AtomicUsize::new(0));

        {
            let slot = StreamSlot(Arc::clone(&count));
            slot.0.fetch_add(1, Ordering::AcqRel);
            assert_eq!(count.load(Ordering::Acquire), 1);
        }
        assert_eq!(count.load(Ordering::Acquire), 0, "released on drop");

        let refused = StreamSlot(Arc::clone(&count));
        refused.0.fetch_add(1, Ordering::AcqRel);
        drop(refused);
        assert_eq!(count.load(Ordering::Acquire), 0);

        let panicking = Arc::clone(&count);
        let _ = std::thread::spawn(move || {
            let slot = StreamSlot(panicking);
            slot.0.fetch_add(1, Ordering::AcqRel);
            panic!("the read blew up");
        })
        .join();
        assert_eq!(
            count.load(Ordering::Acquire),
            0,
            "a panicking stream must not take its slot with it"
        );
    }

    #[test]
    fn git_stream_delivers_the_same_lines_as_the_buffered_read() {
        let (mut client, _hello) = raw();
        let here = env!("CARGO_MANIFEST_DIR");
        let args: Vec<String> = ["log", "--oneline", "-n", "30"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let buffered = match ask(
            &mut client,
            1,
            ControlRequest::Git {
                cwd: here.to_string(),
                args: args.clone(),
            },
        ) {
            ControlReply::Ok(ReplyOk::Output(o)) => o,
            other => panic!("expected output, got {other:?}"),
        };
        if buffered.status != Some(0) {
            return;
        }
        let expected: Vec<String> = String::from_utf8_lossy(&buffered.stdout)
            .lines()
            .map(str::to_string)
            .collect();

        ControlClientMsg::Request {
            req_id: 2,
            req: ControlRequest::GitStream {
                id: 77,
                cwd: here.to_string(),
                args,
            },
        }
        .encode(&mut client)
        .unwrap();
        client.flush().unwrap();

        let mut split = crate::core::git::LineSplitter::default();
        let mut got: Vec<String> = Vec::new();
        let mut accepted = false;
        let code = loop {
            match ControlServerMsg::read(&mut client).unwrap() {
                ControlServerMsg::Response { req_id: 2, reply } => {
                    assert!(
                        matches!(reply, ControlReply::Ok(ReplyOk::Unit)),
                        "{reply:?}"
                    );
                    accepted = true;
                }
                ControlServerMsg::Event(ControlEvent::GitChunk { id, bytes }) => {
                    assert_eq!(id, 77, "chunks carry the id the client chose");
                    split.push(&bytes, |l| got.push(l.to_string()));
                }
                ControlServerMsg::Event(ControlEvent::GitEnd { id, code, failed }) => {
                    assert_eq!(id, 77);
                    assert!(!failed, "git ran");
                    break code;
                }
                other => panic!("unexpected {other:?}"),
            }
        };
        split.finish(|l| got.push(l.to_string()));
        assert!(accepted, "the request was answered");
        assert_eq!(code, Some(0));
        assert_eq!(got, expected, "streamed lines match the buffered read");
        assert!(!got.is_empty(), "this repo has commits");
    }

    fn ask(client: &mut UnixStream, req_id: u64, req: ControlRequest) -> ControlReply {
        ControlClientMsg::Request { req_id, req }
            .encode(&mut *client)
            .unwrap();
        client.flush().unwrap();
        loop {
            match ControlServerMsg::read(&mut *client).unwrap() {
                ControlServerMsg::Response { req_id: got, reply } if got == req_id => {
                    return reply;
                }
                ControlServerMsg::Event(_) => continue,
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    }

    #[test]
    fn a_closed_connection_stops_being_a_subscriber() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = MachineStore::open(dir.path().join(machine::MACHINE_FILE));
        let served = {
            let store = Arc::clone(&store);
            let (server, client) = UnixStream::pair().unwrap();
            let handle = std::thread::spawn(move || {
                let _ = serve_with(server, LocalHost::new(), Services::with_machine(store));
            });
            let mut client = client;
            ControlClientMsg::Hello(ControlHello::host_rpc("t", "h"))
                .encode(&mut client)
                .unwrap();
            client.flush().unwrap();
            let _ = ControlServerMsg::read(&mut client).unwrap();
            drop(client);
            handle
        };
        served.join().unwrap();

        let ws = store
            .workspace_create(None, Some("api".into()), None)
            .unwrap();
        assert_eq!(store.machine().workspaces.len(), 1);
        assert_eq!(store.workspace(ws.id).unwrap().name.as_deref(), Some("api"));
    }

    #[test]
    fn concurrent_connections_can_all_write_the_tree() {
        let (services, _dir) = workspace_services();
        let store = Arc::clone(services.machine.as_ref().unwrap());

        let writers: Vec<_> = (0..6)
            .map(|_| {
                let services = services.clone();
                std::thread::spawn(move || {
                    let (mut client, _) = raw_with(services);
                    for i in 0..10 {
                        let reply = ask(
                            &mut client,
                            i as u64 + 1,
                            ControlRequest::WorkspaceCreate {
                                name: Some(format!("w-{i}")),
                                workspace: None,
                            },
                        );
                        assert!(
                            matches!(reply, ControlReply::Ok(ReplyOk::WorkspaceTree(_))),
                            "{reply:?}"
                        );
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        assert_eq!(
            store.machine().workspaces.len(),
            60,
            "every operation landed exactly once"
        );
    }

    #[test]
    fn a_lagged_connection_hears_a_resync_instead_of_the_superseded_backlog() {
        let (server_end, mut client_end) = UnixStream::pair().unwrap();
        let sink = Arc::new(Sink::new(server_end));
        let (tx, rx) = smol::channel::bounded::<(String, machine::LayoutDelta)>(8);
        let lagged = Arc::new(AtomicBool::new(false));

        for _ in 0..4 {
            tx.send_blocking(("ws-1".to_string(), machine::LayoutDelta::WorkspaceDeleted))
                .unwrap();
        }
        lagged.store(true, Ordering::Release);
        spawn_layout_forwarder(rx, sink, Arc::clone(&lagged));

        assert_eq!(
            ControlServerMsg::read(&mut client_end).unwrap(),
            ControlServerMsg::Event(ControlEvent::LayoutResync)
        );
        assert!(
            !lagged.load(Ordering::Acquire),
            "the flag is consumed: one gap, one resync"
        );

        tx.send_blocking(("ws-2".to_string(), machine::LayoutDelta::WorkspaceDeleted))
            .unwrap();
        match ControlServerMsg::read(&mut client_end).unwrap() {
            ControlServerMsg::Event(ControlEvent::Layout { workspace, delta }) => {
                assert_eq!(workspace, "ws-2", "the stale backlog was delivered anyway");
                assert_eq!(delta, machine::LayoutDelta::WorkspaceDeleted);
            }
            other => panic!("expected the post-resync delta, got {other:?}"),
        }
    }

    #[test]
    fn a_second_client_takes_the_workspace_and_the_first_is_told() {
        let (services, _dir) = workspace_services();
        let registry = Arc::clone(&services.attachments);
        let w = tree_workspace(&services);

        let ((mut laptop, _), _laptop_served) =
            raw_hello(services.clone(), hello_for(&w, "tok-laptop", "laptop"));
        await_holder(&registry, &w, "laptop");

        let ((mut desktop, _), _desktop_served) =
            raw_hello(services.clone(), hello_for(&w, "tok-desktop", "desktop"));

        assert_eq!(
            await_preempted(&mut laptop),
            Some(ControlEvent::Preempted {
                workspace: w.clone(),
                by: "desktop".to_string(),
            })
        );
        assert_eq!(registry.holder(&w).map(|(_, h)| h), Some("desktop".into()));
        assert_eq!(
            services
                .machine
                .as_ref()
                .unwrap()
                .attachment(w.parse().unwrap())
                .unwrap()
                .hostname,
            "desktop",
            "the tree's record moves with the live handles"
        );

        let (reply, _) = round_trip(
            &mut desktop,
            1,
            ControlRequest::WorkspaceAttach { id: w.clone() },
        );
        assert_eq!(
            reply,
            ControlReply::Ok(ReplyOk::Attached {
                took_over_from: None
            }),
            "re-attaching from the connection that already holds it is not a takeover"
        );
    }

    #[test]
    fn a_dedicated_connection_is_closed_when_its_workspace_is_taken() {
        let (services, _dir) = workspace_services();
        let w = tree_workspace(&services);
        let ((mut laptop, _), _l) =
            raw_hello(services.clone(), hello_for(&w, "tok-laptop", "laptop"));
        await_holder(&services.attachments, &w, "laptop");
        let ((_desktop, _), _d) =
            raw_hello(services.clone(), hello_for(&w, "tok-desktop", "desktop"));

        assert!(await_preempted(&mut laptop).is_some());
        let mut sink = Vec::new();
        let _ = std::io::Read::read_to_end(&mut laptop, &mut sink);
        assert!(
            ControlServerMsg::read(&mut laptop).is_err(),
            "a dedicated connection is closed once its workspace is gone"
        );
    }

    #[test]
    fn removing_a_workspace_clears_both_attachment_tables() {
        let (services, _dir) = workspace_services();
        let registry = Arc::clone(&services.attachments);
        let machine = services.machine.clone().unwrap();
        let w = tree_workspace(&services);
        let id: crate::core::session::WorkspaceId = w.parse().unwrap();

        let ((mut laptop, _), _l) =
            raw_hello(services.clone(), hello_for(&w, "tok-laptop", "laptop"));
        await_holder(&registry, &w, "laptop");
        assert!(machine.attachment(id).is_some());

        let reply = ask(
            &mut laptop,
            1,
            ControlRequest::WorkspaceRemove { workspace: id },
        );
        assert!(
            matches!(reply, ControlReply::Ok(ReplyOk::Panes(_))),
            "{reply:?}"
        );

        assert!(
            machine.attachment(id).is_none(),
            "the tree still names a holder for a workspace that is gone"
        );
        assert!(
            registry.holder(&w).is_none(),
            "the registry still holds a workspace that is gone"
        );
    }

    #[test]
    fn an_attach_moves_both_tables_under_one_lock() {
        let (services, _dir) = workspace_services();
        let registry = Arc::clone(&services.attachments);
        let machine = services.machine.clone().unwrap();
        let w = tree_workspace(&services);
        let id: crate::core::session::WorkspaceId = w.parse().unwrap();

        let held = registry.handover();
        let ((_laptop, _ok), _served) =
            raw_hello(services.clone(), hello_for(&w, "tok-laptop", "laptop"));

        std::thread::sleep(Duration::from_millis(150));
        assert!(
            registry.holder(&w).is_none(),
            "the registry was moved while a handover was in flight"
        );
        assert!(
            machine.attachment(id).is_none(),
            "the tree was moved while a handover was in flight"
        );

        drop(held);
        await_holder(&registry, &w, "laptop");
        assert_eq!(
            machine.attachment(id).map(|a| a.token).as_deref(),
            Some("tok-laptop"),
            "both tables have to name the same session once the handover is done"
        );
    }

    #[test]
    fn a_shared_connection_survives_losing_one_of_its_workspaces() {
        let (services, _dir) = workspace_services();
        let registry = Arc::clone(&services.attachments);
        let w1 = tree_workspace(&services);
        let w2 = tree_workspace(&services);

        let ((mut laptop, _), _l) = raw_hello(
            services.clone(),
            ControlHello::host_rpc("tok-laptop", "laptop"),
        );
        for (i, id) in [&w1, &w2].iter().enumerate() {
            let (reply, _) = round_trip(
                &mut laptop,
                i as u64 + 1,
                ControlRequest::WorkspaceAttach { id: (*id).clone() },
            );
            assert_eq!(
                reply,
                ControlReply::Ok(ReplyOk::Attached {
                    took_over_from: None
                })
            );
        }
        assert_eq!(registry.len(), 2);

        let ((_desktop, _), _d) =
            raw_hello(services.clone(), hello_for(&w1, "tok-desktop", "desktop"));
        assert_eq!(
            await_preempted(&mut laptop),
            Some(ControlEvent::Preempted {
                workspace: w1.clone(),
                by: "desktop".to_string(),
            })
        );

        let (reply, _) = round_trip(&mut laptop, 9, ControlRequest::Ping);
        assert_eq!(reply, ControlReply::Ok(ReplyOk::Pong));
        assert_eq!(
            registry.holder(&w2).map(|(t, _)| t),
            Some("tok-laptop".into())
        );
        assert_eq!(
            registry.holder(&w1).map(|(t, _)| t),
            Some("tok-desktop".into())
        );
    }

    #[test]
    fn a_displaced_session_tidying_up_does_not_evict_the_new_owner() {
        let (services, _dir) = workspace_services();
        let registry = Arc::clone(&services.attachments);
        let machine = Arc::clone(services.machine.as_ref().unwrap());
        let w = tree_workspace(&services);
        let other = tree_workspace(&services);

        let ((mut laptop, _), _l) = raw_hello(
            services.clone(),
            ControlHello::host_rpc("tok-laptop", "laptop"),
        );
        for (i, id) in [&w, &other].iter().enumerate() {
            round_trip(
                &mut laptop,
                i as u64 + 1,
                ControlRequest::WorkspaceAttach { id: (*id).clone() },
            );
        }
        let ((_desktop, _), _d) =
            raw_hello(services.clone(), hello_for(&w, "tok-desktop", "desktop"));
        assert!(await_preempted(&mut laptop).is_some());

        let (reply, _) = round_trip(
            &mut laptop,
            3,
            ControlRequest::WorkspaceDetach { id: w.clone() },
        );
        assert_eq!(reply, ControlReply::Ok(ReplyOk::Unit));
        assert_eq!(
            registry.holder(&w).map(|(t, _)| t),
            Some("tok-desktop".into()),
            "the displaced session must not release what it no longer holds"
        );
        assert_eq!(
            machine.attachment(w.parse().unwrap()).unwrap().token,
            "tok-desktop"
        );
    }

    #[test]
    fn a_closed_connection_releases_what_it_held() {
        let (services, _dir) = workspace_services();
        let registry = Arc::clone(&services.attachments);
        let machine = Arc::clone(services.machine.as_ref().unwrap());
        let w = tree_workspace(&services);
        {
            let ((client, _), served) = raw_hello(services.clone(), hello_for(&w, "tok", "laptop"));
            await_holder(&registry, &w, "laptop");
            drop(client);
            served.join().unwrap();
        }
        assert!(registry.is_empty(), "the registry outlived the connection");
        assert_eq!(machine.attachment(w.parse().unwrap()), None);
    }

    #[test]
    fn attaching_to_a_server_without_a_tree_is_an_error() {
        let (mut client, _) = raw_with(Services::none());
        let (reply, _) = round_trip(
            &mut client,
            1,
            ControlRequest::WorkspaceAttach { id: "w".into() },
        );
        assert!(matches!(reply, ControlReply::Err(_)), "{reply:?}");
    }
}

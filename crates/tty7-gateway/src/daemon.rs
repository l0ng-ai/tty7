//! The real [`Backend`]: this machine's tty7 server, over its local sockets —
//! and through it, every machine it holds an SSH link to.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::projects;
use tty7_core::client::{ControlClient, PaneClient, PaneInput, PaneOutput};
use tty7_core::core::group_key::AutoKey;
use tty7_core::core::machine::{Machine, TabId};
use tty7_core::daemon::control::{
    ControlHello, ControlRequest, PaneAgentState, PaneSeed, ReplyOk, RouteInfo, WorkspaceId,
};
use tty7_core::daemon::protocol::{DaemonMsg, FEATURE_SIZE_LEASE, LeaseRequest, WinSize};
use tty7_core::daemon::router::RouteTarget;
use tty7_mobile_proto::{Folder, Folders, GridSize, ProjectOpened, TabCreated};

use crate::poller::Poller;
use crate::serve::{Backend, PaneFeed, PaneLeases, Remote};

/// How long a tree read waits on linked machines before reporting the slow
/// ones as they last were. A healthy link answers well inside it.
const REMOTE_BUDGET: Duration = Duration::from_millis(250);

type Snapshot = (Machine, Vec<PaneAgentState>);
/// One linked machine's routed control connection. Each has its own lock, so
/// a request stuck on one link never queues work for another.
type RoutedSlot = Arc<Mutex<Option<ControlClient>>>;

/// The size the gateway claims when it observes a pane. The daemon ignores it
/// for an observer — the pane keeps the size its window gave it — but the
/// message carries one.
const OBSERVE_SIZE: WinSize = WinSize {
    cols: 80,
    rows: 24,
    cell_w: 8,
    cell_h: 16,
};

/// The grid a tab started from the phone gets when the phone did not say. The
/// same the `tty7` CLI starts its shells at.
const NEW_TAB_SIZE: WinSize = WinSize {
    cols: 120,
    rows: 30,
    cell_w: 8,
    cell_h: 16,
};

pub struct Daemon {
    host: String,
    control: Mutex<Option<ControlClient>>,
    /// A control connection per linked machine, routed through the local
    /// server, by link key.
    remote: Mutex<HashMap<String, RoutedSlot>>,
    trees: Poller<Snapshot>,
    project_launch: Mutex<()>,
}

impl Default for Daemon {
    fn default() -> Daemon {
        Daemon {
            host: crate::identity::name(),
            control: Mutex::new(None),
            remote: Mutex::new(HashMap::new()),
            trees: Poller::default(),
            project_launch: Mutex::new(()),
        }
    }
}

impl Daemon {
    fn hello(&self) -> ControlHello {
        ControlHello::host_rpc(
            format!("tty7-gateway-{}", std::process::id()),
            self.host.clone(),
        )
    }

    fn request(&self, req: ControlRequest) -> io::Result<ReplyOk> {
        let mut slot = self.control.lock().unwrap_or_else(|e| e.into_inner());
        if !slot.as_ref().is_some_and(ControlClient::is_connected) {
            *slot = Some(ControlClient::connect(&self.hello())?);
        }
        let client = slot.as_ref().expect("connected just above");
        let reply = client.request(req);
        if reply.is_err() {
            *slot = None;
        }
        reply
    }

    fn routes(&self) -> io::Result<Vec<RouteInfo>> {
        match self.request(ControlRequest::Routes)? {
            ReplyOk::Routes(routes) => Ok(routes),
            other => Err(unexpected("Routes", &other)),
        }
    }

    /// Where to route for a linked machine — only over a link the desktop
    /// already holds. Routing to a down one would have the server dial it
    /// afresh, guessing at credentials the phone does not have.
    fn target(&self, key: &str) -> io::Result<RouteTarget> {
        let routes = self.routes()?;
        let route = routes
            .iter()
            .find(|r| r.key == key)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no machine {key}")))?;
        if !route.connected {
            return Err(link_down(key));
        }
        route.target().map_err(io::Error::other)
    }

    fn slot(&self, key: &str) -> RoutedSlot {
        self.remote
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.to_string())
            .or_default()
            .clone()
    }

    fn request_on(&self, machine: Option<&str>, req: ControlRequest) -> io::Result<ReplyOk> {
        match machine {
            None => self.request(req),
            Some(key) => routed_request(&self.slot(key), || self.target(key), &self.hello(), req),
        }
    }

    fn panes_on(&self, machine: Option<&str>) -> io::Result<PaneClient> {
        match machine {
            None => Ok(PaneClient::local()),
            Some(key) => Ok(PaneClient::routed(self.target(key)?)),
        }
    }

    fn open_tab(
        &self,
        machine: Option<&str>,
        workspace_id: &str,
        cwd: Option<String>,
        size: Option<GridSize>,
        tab: Option<TabId>,
    ) -> io::Result<TabCreated> {
        let workspace: WorkspaceId = workspace_id.parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("no workspace {workspace_id}"),
            )
        })?;
        let for_phone = size.is_some();
        let size = size.map_or(NEW_TAB_SIZE, |s| WinSize {
            cols: s.cols.clamp(20, 500),
            rows: s.rows.clamp(5, 300),
            ..NEW_TAB_SIZE
        });
        let owner = workspace.to_string();
        // A tab asked for with nowhere in mind starts at home, as a new
        // window's does, rather than the directory the server started in.
        // A remote's own server knows its home; this one only knows this.
        let start = match (cwd.as_deref(), machine) {
            (Some(dir), _) => Some(PathBuf::from(dir)),
            (None, None) => std::env::home_dir(),
            (None, Some(_)) => None,
        };
        let session =
            self.panes_on(machine)?
                .spawn(start, size, None, Some(owner.clone()), Some(owner))?;
        let pane = session.pane_id();
        session.detach()?;
        // The tab is about to reach the desktop, which lays every tab out at
        // its window's size; the shell's first screen, a banner or prompt,
        // would be drawn that wide and then squeezed onto the phone. Held
        // at the phone's size from before the desktop hears of it, until
        // the phone's own view of it takes over.
        if for_phone {
            match self.observe(machine, pane) {
                Ok(feed) => hold_for_phone(feed, size),
                Err(e) => log::debug!("mobile gateway: holding new pane {pane}: {e}"),
            }
        }
        let seed = PaneSeed {
            pane,
            cwd,
            ssh_spec: None,
            agent: None,
            shell: None,
        };
        let create = ControlRequest::TabCreate {
            workspace,
            at: None,
            pane: seed,
            tab,
        };
        let reply = match self.request_on(machine, create) {
            Ok(reply) => reply,
            Err(error) => {
                // A response can be lost after the tab was recorded. Keep
                // that pane, and return its real id instead of killing it.
                if let Ok(tree) = self.machine_on(machine) {
                    if let Some(tab) = tree
                        .workspaces
                        .iter()
                        .flat_map(|ws| &ws.tabs)
                        .find(|tab| tab.root.contains(pane))
                    {
                        return Ok(TabCreated {
                            tab_id: tab.id.to_string(),
                            pane_id: pane,
                        });
                    }
                    let _ = self.panes_on(machine).and_then(|client| client.kill(pane));
                }
                return Err(error);
            }
        };
        match reply {
            ReplyOk::TabTree(tab) => Ok(TabCreated {
                tab_id: tab.id.to_string(),
                pane_id: pane,
            }),
            other => Err(unexpected("TabCreate", &other)),
        }
    }

    fn location(&self, machine: Option<&str>) -> io::Result<(String, char)> {
        self.request_on(machine, ControlRequest::Ping)?;
        let read = |slot: &Mutex<Option<ControlClient>>| {
            let slot = slot.lock().unwrap_or_else(|e| e.into_inner());
            let hello = slot
                .as_ref()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotConnected, "The computer disconnected")
                })?
                .hello();
            Ok((hello.home.clone(), hello.separator))
        };
        match machine {
            None => read(&self.control),
            Some(key) => read(&self.slot(key)),
        }
    }

    fn directory(
        &self,
        machine: Option<&str>,
        path: &str,
        home: &str,
        separator: char,
    ) -> io::Result<String> {
        let path = projects::expand(path, home, separator)?;
        let path = match self.request_on(machine, ControlRequest::Canonicalize { path })? {
            ReplyOk::Path(path) => path,
            other => return Err(unexpected("Canonicalize", &other)),
        };
        match self.request_on(machine, ControlRequest::Stat { path: path.clone() })? {
            ReplyOk::Meta(meta) if meta.is_dir => Ok(path),
            ReplyOk::Meta(_) => Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "The selected path is not a directory",
            )),
            other => Err(unexpected("Stat", &other)),
        }
    }

    fn machine_on(&self, machine: Option<&str>) -> io::Result<Machine> {
        match self.request_on(machine, ControlRequest::MachineGet)? {
            ReplyOk::MachineTree(tree) => Ok(*tree),
            other => Err(unexpected("MachineGet", &other)),
        }
    }

    fn project_roots(
        &self,
        machine: Option<&str>,
        tree: &Machine,
        separator: char,
    ) -> HashMap<String, String> {
        let mut roots = HashMap::new();
        // The desktop already stores repository and pinned-folder hints.
        // Use them before doing filesystem round trips on a remote link.
        for ws in &tree.workspaces {
            for folder in ws
                .groups
                .pinned
                .iter()
                .filter_map(|group| group.folder.as_deref())
            {
                roots
                    .entry(projects::path_key(folder, separator))
                    .or_insert_with(|| ws.id.to_string());
            }
            for tab in &ws.tabs {
                if let Some(AutoKey::Repo(path)) = &tab.last_auto {
                    roots
                        .entry(projects::path_key(&path.to_string_lossy(), separator))
                        .or_insert_with(|| ws.id.to_string());
                }
            }
        }
        let mut queried = std::collections::HashSet::new();
        for pane in &tree.panes {
            if pane.ssh_spec.is_some() {
                continue;
            }
            let Some(cwd) = pane.cwd.as_deref() else {
                continue;
            };
            let Some(ws) = tree
                .workspaces
                .iter()
                .find(|ws| ws.tabs.iter().any(|tab| tab.root.contains(pane.id)))
            else {
                continue;
            };
            let key = projects::path_key(cwd, separator);
            roots
                .entry(key.clone())
                .or_insert_with(|| ws.id.to_string());
            if queried.len() >= 64 || !queried.insert(key) {
                continue;
            }
            // Tabs with a saved repository hint need no Git lookup here.
            if ws.tabs.iter().any(|tab| {
                tab.root.contains(pane.id) && matches!(tab.last_auto, Some(AutoKey::Repo(_)))
            }) {
                continue;
            }
            let root = match self.request_on(
                machine,
                ControlRequest::RepoRoot {
                    path: cwd.to_string(),
                },
            ) {
                Ok(ReplyOk::OptPath(Some(path))) => path,
                _ => cwd.to_string(),
            };
            roots
                .entry(projects::path_key(&root, separator))
                .or_insert_with(|| ws.id.to_string());
        }
        roots
    }
}

/// A request over one linked machine's routed connection, dialing the route
/// (never the machine: the link is the desktop's) when there is none yet.
fn routed_request(
    slot: &Mutex<Option<ControlClient>>,
    target: impl FnOnce() -> io::Result<RouteTarget>,
    hello: &ControlHello,
    req: ControlRequest,
) -> io::Result<ReplyOk> {
    let mut client = slot.lock().unwrap_or_else(|e| e.into_inner());
    if !client.as_ref().is_some_and(ControlClient::is_connected) {
        *client = Some(ControlClient::routed(target()?, hello)?);
    }
    let reply = client.as_ref().expect("connected just above").request(req);
    if reply.is_err() {
        *client = None;
    }
    reply
}

fn read_tree(
    mut request: impl FnMut(ControlRequest) -> io::Result<ReplyOk>,
) -> io::Result<Snapshot> {
    let tree = match request(ControlRequest::MachineGet)? {
        ReplyOk::MachineTree(tree) => *tree,
        other => return Err(unexpected("MachineGet", &other)),
    };
    let agents = match request(ControlRequest::AgentStates)? {
        ReplyOk::AgentStates(agents) => agents,
        other => return Err(unexpected("AgentStates", &other)),
    };
    Ok((tree, agents))
}

fn link_down(key: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        format!("the desktop's link to {key} is down — reconnect it from tty7 on the computer"),
    )
}

impl Backend for Daemon {
    fn hostname(&self) -> String {
        self.host.clone()
    }

    fn snapshot(&self) -> io::Result<Snapshot> {
        read_tree(|req| self.request(req))
    }

    fn folders(&self, machine: Option<&str>, dir: Option<String>) -> io::Result<Folders> {
        let (home, separator) = self.location(machine)?;
        let tree = self.machine_on(machine)?;
        let roots = self.project_roots(machine, &tree, separator);
        let start = dir.unwrap_or_else(|| {
            roots
                .keys()
                .min()
                .and_then(|p| projects::parent(p, separator))
                .unwrap_or_else(|| home.clone())
        });
        let dir = self.directory(machine, &start, &home, separator)?;
        let entries = match self.request_on(
            machine,
            ControlRequest::ReadDir {
                dir: dir.clone(),
                root: None,
            },
        )? {
            ReplyOk::Entries(entries) => entries,
            other => return Err(unexpected("ReadDir", &other)),
        };
        let mut entries: Vec<_> = entries
            .into_iter()
            .filter(|e| e.is_dir && !e.name.starts_with('.'))
            .collect();
        entries.sort_by_key(|e| (e.name.to_lowercase(), e.name.clone()));
        let truncated = entries.len() > 256;
        let folders = entries
            .into_iter()
            .take(256)
            .map(|e| {
                let path = projects::join(&dir, &e.name, separator);
                let id = projects::workspace_id(&path, separator);
                let workspace_id = tree
                    .workspaces
                    .iter()
                    .find(|ws| ws.id == id)
                    .map(|ws| ws.id.to_string())
                    .or_else(|| roots.get(&projects::path_key(&path, separator)).cloned());
                Folder {
                    name: e.name,
                    path,
                    workspace_id,
                }
            })
            .collect();
        Ok(Folders {
            parent: projects::parent(&dir, separator),
            dir,
            home,
            folders,
            truncated,
        })
    }

    fn open_project(
        &self,
        machine: Option<&str>,
        cwd: String,
        launch_id: String,
        size: Option<GridSize>,
    ) -> io::Result<ProjectOpened> {
        // Two devices opening the same project must not create two workspaces.
        let _launch = self
            .project_launch
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tab_id: TabId =
            serde_json::from_value(serde_json::Value::String(launch_id)).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "Invalid project launch id")
            })?;
        let (home, separator) = self.location(machine)?;
        let cwd = self.directory(machine, &cwd, &home, separator)?;
        let tree = self.machine_on(machine)?;
        let id = projects::workspace_id(&cwd, separator);
        let roots = self.project_roots(machine, &tree, separator);
        let existing = roots.get(&projects::path_key(&cwd, separator));
        let found = tree.workspaces.iter().find(|ws| ws.id == id).or_else(|| {
            tree.workspaces
                .iter()
                .find(|ws| existing.is_some_and(|e| *e == ws.id.to_string()))
        });
        let created_workspace = found.is_none();
        let workspace = match found {
            Some(ws) => ws.clone(),
            None => match self.request_on(
                machine,
                ControlRequest::WorkspaceCreate {
                    name: Some(projects::name(&cwd, separator)),
                    workspace: Some(id),
                },
            ) {
                Ok(ReplyOk::WorkspaceTree(ws)) => *ws,
                Ok(other) => return Err(unexpected("WorkspaceCreate", &other)),
                // A creation whose response was lost, or another gateway's
                // simultaneous request, can still have made this exact id.
                Err(error) => self
                    .machine_on(machine)?
                    .workspaces
                    .into_iter()
                    .find(|ws| ws.id == id)
                    .ok_or(error)?,
            },
        };
        let opened = if let Some(tab) = workspace.tabs.iter().find(|tab| tab.id == tab_id) {
            TabCreated {
                tab_id: tab.id.to_string(),
                pane_id: *tab
                    .root
                    .pane_ids()
                    .first()
                    .ok_or_else(|| io::Error::other("The previously opened tab has no pane"))?,
            }
        } else {
            match self.open_tab(
                machine,
                &workspace.id.to_string(),
                Some(cwd.clone()),
                size,
                Some(tab_id),
            ) {
                Ok(tab) => tab,
                Err(error) => {
                    // Keep the project workspace for retry. Another client
                    // may have started using it while this request ran.
                    return Err(error);
                }
            }
        };
        Ok(ProjectOpened {
            cwd,
            workspace_id: workspace.id.to_string(),
            workspace_name: workspace.name.unwrap_or_else(|| "Project".into()),
            tab_id: opened.tab_id,
            pane_id: opened.pane_id,
            created_workspace,
        })
    }

    /// Every linked machine is read on its own thread; see [`Poller`].
    fn remotes(&self) -> Vec<Remote> {
        let Ok(routes) = self.routes() else {
            return Vec::new();
        };
        // Links that went away take their routed connections with them.
        self.remote
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, _| routes.iter().any(|r| &r.key == key && r.connected));

        let hello = self.hello();
        let reads = routes
            .iter()
            .filter(|route| route.connected)
            .map(|route| {
                let key = route.key.clone();
                let (slot, route, hello) = (self.slot(&key), route.clone(), hello.clone());
                let read: Box<dyn FnOnce() -> Result<Snapshot, String> + Send> =
                    Box::new(move || {
                        let target = || route.target().map_err(io::Error::other);
                        read_tree(|req| routed_request(&slot, target, &hello, req))
                            .map_err(|e| e.to_string())
                    });
                (key, read)
            })
            .collect();
        let mut trees = self.trees.poll(reads, REMOTE_BUDGET);
        routes
            .into_iter()
            .map(|route| Remote {
                name: route.host().unwrap_or(&route.key).to_string(),
                snapshot: trees
                    .remove(&route.key)
                    .flatten()
                    .map(|read| read.map_err(io::Error::other)),
                connected: route.connected,
                key: route.key,
            })
            .collect()
    }

    fn observe(&self, machine: Option<&str>, pane_id: u64) -> io::Result<Box<dyn PaneFeed>> {
        let client = self.panes_on(machine)?;
        let (input, output) = client.observe(pane_id, OBSERVE_SIZE)?.split();
        Ok(Box::new(Observed {
            output,
            leases: Some(Box::new(Leases {
                input,
                client,
                supported: None,
            })),
        }))
    }

    fn send_input(&self, machine: Option<&str>, pane_id: u64, bytes: &[u8]) -> io::Result<()> {
        self.panes_on(machine)?.send_input(pane_id, bytes)
    }

    /// The two steps `tty7 tab new` takes: spawn a shell owned by the
    /// workspace, then hang a tab on it — on whichever machine it lives on.
    fn new_tab(
        &self,
        machine: Option<&str>,
        workspace_id: &str,
        cwd: Option<String>,
        size: Option<GridSize>,
    ) -> io::Result<TabCreated> {
        self.open_tab(machine, workspace_id, cwd, size, None)
    }

    /// What `tty7 pane close` does: take the pane out of its workspace's tree
    /// on the machine it lives on, then hang up every pane that removal left
    /// with no tab — the pane itself, normally.
    fn close_pane(&self, machine: Option<&str>, pane_id: u64) -> io::Result<()> {
        let tree = match self.request_on(machine, ControlRequest::MachineGet)? {
            ReplyOk::MachineTree(tree) => *tree,
            other => return Err(unexpected("MachineGet", &other)),
        };
        let workspace = tree
            .workspaces
            .iter()
            .find(|ws| ws.tabs.iter().any(|tab| tab.root.contains(pane_id)))
            .map(|ws| ws.id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "that pane is not open any more")
            })?;
        let removed = match self.request_on(
            machine,
            ControlRequest::PaneClose {
                workspace,
                pane: pane_id,
            },
        )? {
            ReplyOk::Panes(panes) => panes,
            other => return Err(unexpected("PaneClose", &other)),
        };
        let panes = self.panes_on(machine)?;
        for pane in removed {
            panes.kill(pane)?;
        }
        Ok(())
    }

    fn running_panes(&self) -> Option<std::collections::HashSet<u64>> {
        let panes = PaneClient::local().list().ok()?;
        Some(
            panes
                .into_iter()
                .filter(|p| p.alive)
                .map(|p| p.pane_id)
                .collect(),
        )
    }

    /// As the desktop closes a tab: onto the workspace's recently-closed
    /// list, its panes stopped with their screens kept, so it can be put
    /// back. A machine that keeps no such list closes it for good, and its
    /// panes are ended from here, as `tty7 tab close` does.
    fn close_tab(&self, machine: Option<&str>, workspace_id: &str, tab_id: &str) -> io::Result<()> {
        let invalid =
            |what: &str| io::Error::new(io::ErrorKind::InvalidInput, format!("no {what}"));
        let workspace: WorkspaceId = workspace_id
            .parse()
            .map_err(|_| invalid(&format!("workspace {workspace_id}")))?;
        // A tab id is a UUID on the wire; it has no `FromStr` of its own.
        let tab: TabId = serde_json::from_value(serde_json::Value::String(tab_id.to_string()))
            .map_err(|_| invalid(&format!("tab {tab_id}")))?;
        let remembered = ControlRequest::TabCloseRemembered {
            workspace,
            tab,
            panes: Vec::new(),
        };
        let ended = match self.request_on(machine, remembered) {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                match self.request_on(machine, ControlRequest::TabClose { workspace, tab })? {
                    ReplyOk::Panes(panes) => panes,
                    other => return Err(unexpected("TabClose", &other)),
                }
            }
            Err(e) => return Err(e),
        };
        let panes = self.panes_on(machine)?;
        for pane in ended {
            panes.kill(pane)?;
        }
        Ok(())
    }
}

struct Observed {
    output: PaneOutput,
    leases: Option<Box<dyn PaneLeases>>,
}

impl PaneFeed for Observed {
    fn leases(&mut self) -> Option<Box<dyn PaneLeases>> {
        self.leases.take()
    }

    fn partial(&self) -> usize {
        self.output.partial()
    }

    fn recv(&mut self, wait: Duration) -> io::Result<Option<DaemonMsg>> {
        self.output.set_recv_timeout(Some(wait))?;
        match self.output.recv() {
            Ok(msg) => Ok(Some(msg)),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
}

/// How long a new tab is held at the phone's size for the phone to open it.
const PHONE_HOLD: Duration = Duration::from_secs(10);

/// Takes the pane at `size` over `feed` and keeps it so on a thread of its
/// own, until the phone's stream takes the lease from it (the daemon tells
/// the one displaced), the pane ends, or [`PHONE_HOLD`] passes.
fn hold_for_phone(mut feed: Box<dyn PaneFeed>, size: WinSize) {
    let Some(mut leases) = feed.leases() else {
        return;
    };
    let take = LeaseRequest::Take {
        size,
        by: "a phone".to_string(),
    };
    if leases.send(take).is_err() {
        return;
    }
    std::thread::spawn(move || {
        let until = std::time::Instant::now() + PHONE_HOLD;
        let mut held = false;
        while std::time::Instant::now() < until {
            match feed.recv(Duration::from_millis(250)) {
                Ok(Some(DaemonMsg::Lease(Some(_)))) => held = true,
                Ok(Some(DaemonMsg::Lease(None))) if held => return,
                Ok(Some(DaemonMsg::Exited { .. })) | Err(_) => return,
                _ => {}
            }
        }
        drop(leases);
    });
}

/// The observer connection's writing half, for leases.
struct Leases {
    input: PaneInput,
    client: PaneClient,
    /// Whether the pane's daemon knows leases, asked on the first one. A
    /// daemon that does not would drop the observer on reading one.
    supported: Option<bool>,
}

impl PaneLeases for Leases {
    fn send(&mut self, request: LeaseRequest) -> io::Result<()> {
        let supported = match self.supported {
            Some(known) => known,
            None => {
                let known = self
                    .client
                    .version()?
                    .features
                    .iter()
                    .any(|f| f == FEATURE_SIZE_LEASE);
                *self.supported.insert(known)
            }
        };
        if !supported {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the tty7 running this pane is too old to take over; update it",
            ));
        }
        self.input.lease(request)
    }
}

fn unexpected(req: &str, reply: &ReplyOk) -> io::Error {
    io::Error::other(format!("unexpected answer to {req}: {reply:?}"))
}

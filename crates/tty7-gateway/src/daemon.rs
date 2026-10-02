//! The real [`Backend`]: this machine's tty7 server, over its local sockets —
//! and through it, every machine it holds an SSH link to.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tty7_core::client::{ControlClient, PaneClient, PaneInput, PaneOutput};
use tty7_core::core::machine::{Machine, TabId};
use tty7_core::daemon::control::{
    ControlHello, ControlRequest, PaneAgentState, PaneSeed, ReplyOk, RouteInfo, WorkspaceId,
};
use tty7_core::daemon::protocol::{DaemonMsg, FEATURE_SIZE_LEASE, LeaseRequest, WinSize};
use tty7_core::daemon::router::RouteTarget;
use tty7_mobile_proto::{GridSize, TabCreated};

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
}

impl Default for Daemon {
    fn default() -> Daemon {
        Daemon {
            host: hostname(),
            control: Mutex::new(None),
            remote: Mutex::new(HashMap::new()),
            trees: Poller::default(),
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
        let workspace: WorkspaceId = workspace_id.parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("no workspace {workspace_id}"),
            )
        })?;
        let size = size.map_or(NEW_TAB_SIZE, |s| WinSize {
            cols: s.cols.clamp(20, 500),
            rows: s.rows.clamp(5, 300),
            ..NEW_TAB_SIZE
        });
        let owner = workspace.to_string();
        let session = self.panes_on(machine)?.spawn(
            cwd.as_deref().map(PathBuf::from),
            size,
            None,
            Some(owner.clone()),
            Some(owner),
        )?;
        let pane = session.pane_id();
        session.detach()?;
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
            tab: None,
        };
        match self.request_on(machine, create)? {
            ReplyOk::TabTree(tab) => Ok(TabCreated {
                tab_id: tab.id.to_string(),
                pane_id: pane,
            }),
            other => Err(unexpected("TabCreate", &other)),
        }
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

/// This machine's name as the phone lists it.
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for its whole length, and gethostname
        // writes at most that many bytes.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end]);
            // `studio.local`, `Mac.lan`: the domain is the network's, not the
            // machine's name.
            let name = name.split('.').next().unwrap_or_default();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "tty7".to_string())
}

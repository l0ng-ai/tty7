use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::core::cli_agent::CLIAgent;
use crate::core::session::WorkspaceId;
use crate::daemon::protocol::{NativeSshSpec, ShellSpec};

pub const MACHINE_FILE: &str = "machine.json";

pub const APPEARANCE_FILE: &str = "appearance.json";

pub const DATA_DIR_ENV: &str = "TTY7_DATA_DIR";

pub const MAX_WORKSPACES: usize = 1024;

pub const MAX_PANES: usize = 16 * 1024;

#[cfg(not(test))]
pub const FACT_FLUSH_INTERVAL: Duration = Duration::from_secs(2);

#[cfg(test)]
pub const FACT_FLUSH_INTERVAL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TabId(uuid::Uuid);

impl TabId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for TabId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TabId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    A,
    B,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub panes: Vec<PaneRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Proof that a connection is the one holding the workspace, so it stays
    /// between that connection and the server: it goes over no wire and onto
    /// no disk. A peer asking who holds a workspace gets the name and the
    /// time, never the means to pose as them.
    #[serde(skip)]
    pub token: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub since: u64,
}

impl Attachment {
    pub fn new(token: impl Into<String>, hostname: impl Into<String>) -> Attachment {
        Attachment {
            token: token.into(),
            hostname: hostname.into(),
            since: unix_now(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    #[serde(default)]
    pub id: WorkspaceId,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub last_active: u64,
    #[serde(default)]
    pub tabs: Vec<Tab>,
    #[serde(default)]
    pub active_tab: Option<TabId>,
    /// Who is holding this workspace right now. Answered over the wire so a
    /// peer can see the workspace is spoken for, but stripped before the
    /// document is written: an attachment belongs to a live connection, and
    /// one read back at boot would name a holder that no longer exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<Attachment>,
}

impl Default for Workspace {
    fn default() -> Self {
        Workspace {
            id: WorkspaceId::new(),
            name: None,
            last_active: unix_now(),
            tabs: Vec::new(),
            active_tab: None,
            attachment: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    #[serde(default)]
    pub id: TabId,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub sidebar_group: Option<String>,
    pub root: PaneNode,
}

impl Tab {
    pub fn leaf(pane: u64) -> Tab {
        Tab {
            id: TabId::new(),
            name: None,
            sidebar_group: None,
            root: PaneNode::Leaf { pane },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PaneNode {
    Leaf {
        pane: u64,
    },
    Split {
        axis: Axis,
        #[serde(default = "default_ratio")]
        ratio: f32,
        a: Box<PaneNode>,
        b: Box<PaneNode>,
    },
}

fn default_ratio() -> f32 {
    0.5
}

impl PaneNode {
    pub fn pane_ids(&self) -> Vec<u64> {
        let mut out = Vec::new();
        self.collect_panes(&mut out);
        out
    }

    fn collect_panes(&self, out: &mut Vec<u64>) {
        match self {
            PaneNode::Leaf { pane } => out.push(*pane),
            PaneNode::Split { a, b, .. } => {
                a.collect_panes(out);
                b.collect_panes(out);
            }
        }
    }

    pub fn contains(&self, pane: u64) -> bool {
        match self {
            PaneNode::Leaf { pane: p } => *p == pane,
            PaneNode::Split { a, b, .. } => a.contains(pane) || b.contains(pane),
        }
    }

    pub fn descend_mut(&mut self, path: &[Side]) -> Option<&mut PaneNode> {
        match path.split_first() {
            None => Some(self),
            Some((side, rest)) => match self {
                PaneNode::Leaf { .. } => None,
                PaneNode::Split { a, b, .. } => match side {
                    Side::A => a.descend_mut(rest),
                    Side::B => b.descend_mut(rest),
                },
            },
        }
    }

    pub fn split_leaf(&mut self, pane: u64, new: u64, axis: Axis, ratio: f32, first: bool) -> bool {
        match self {
            PaneNode::Leaf { pane: p } if *p == pane => {
                let old = PaneNode::Leaf { pane };
                let added = PaneNode::Leaf { pane: new };
                let (a, b) = if first { (added, old) } else { (old, added) };
                *self = PaneNode::Split {
                    axis,
                    ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                };
                true
            }
            PaneNode::Leaf { .. } => false,
            PaneNode::Split { a, b, .. } => {
                a.split_leaf(pane, new, axis, ratio, first)
                    || b.split_leaf(pane, new, axis, ratio, first)
            }
        }
    }

    pub fn remove_leaf(&mut self, pane: u64) -> Option<bool> {
        match self {
            PaneNode::Leaf { pane: p } => {
                if *p == pane {
                    None
                } else {
                    Some(false)
                }
            }
            PaneNode::Split { a, b, .. } => {
                if matches!(&**a, PaneNode::Leaf { pane: p } if *p == pane) {
                    *self = (**b).clone();
                    return Some(true);
                }
                if matches!(&**b, PaneNode::Leaf { pane: p } if *p == pane) {
                    *self = (**a).clone();
                    return Some(true);
                }
                match a.remove_leaf(pane) {
                    Some(true) => Some(true),
                    Some(false) => b.remove_leaf(pane),
                    None => Some(false),
                }
            }
        }
    }

    pub fn replace_leaf(&mut self, old: u64, new: u64) -> bool {
        match self {
            PaneNode::Leaf { pane } if *pane == old => {
                *pane = new;
                true
            }
            PaneNode::Leaf { .. } => false,
            PaneNode::Split { a, b, .. } => a.replace_leaf(old, new) || b.replace_leaf(old, new),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneRecord {
    pub id: u64,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub title: String,
    /// The title the terminal itself reports (OSC 0/2): a shell writes
    /// `user@host:~/dir` from its integration, an agent overwrites it with what
    /// it is working on. Distinct from `title` above, which is the foreground
    /// process name.
    ///
    /// This is the name the window that owns the pane puts on its tab, so it is
    /// also what anyone else has to read to agree with that window — the
    /// switcher listing a workspace it does not own, `tty7 tab ls` across a
    /// socket. `None` until the pane emits one; a pane that resets its title
    /// clears it back.
    #[serde(default)]
    pub osc_title: Option<String>,
    #[serde(default)]
    pub ssh_spec: Option<Box<NativeSshSpec>>,
    #[serde(default)]
    pub agent: Option<AgentFacts>,
    /// What the pane is actually running, resolved: the spawn's override if it
    /// had one, otherwise the shell the config named at the time.
    ///
    /// Without it a pane rebuilt from this tree comes back on whatever the
    /// default shell is now, so a daemon restart silently turns a bash pane
    /// into a PowerShell one. The rest of the record describes where a pane is
    /// and what is running in it; this is the part that says what it *is*.
    #[serde(default)]
    pub shell: Option<ShellSpec>,
    #[serde(default)]
    pub live: bool,
}

impl PaneRecord {
    pub fn new(id: u64) -> PaneRecord {
        PaneRecord {
            id,
            cwd: None,
            title: String::new(),
            osc_title: None,
            ssh_spec: None,
            agent: None,
            shell: None,
            live: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentFacts {
    pub agent: CLIAgent,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub launch_argv: Option<Vec<String>>,
    #[serde(default)]
    pub status: Option<crate::core::cli_agent::AgentStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneSeed {
    pub pane: u64,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub ssh_spec: Option<Box<NativeSshSpec>>,
    #[serde(default)]
    pub agent: Option<AgentFacts>,
    /// See [`PaneRecord::shell`]. Carried here too so a pane that reaches the
    /// tree as a seed — a split, a `tty7` CLI call — names its shell from the
    /// start rather than only once the daemon has observed it.
    #[serde(default)]
    pub shell: Option<ShellSpec>,
}

impl PaneSeed {
    pub fn bare(pane: u64) -> PaneSeed {
        PaneSeed {
            pane,
            cwd: None,
            ssh_spec: None,
            agent: None,
            shell: None,
        }
    }

    fn into_record(self, live: bool) -> PaneRecord {
        PaneRecord {
            id: self.pane,
            cwd: self.cwd,
            title: String::new(),
            osc_title: None,
            ssh_spec: self.ssh_spec.map(|s| Box::new(s.without_secrets())),
            agent: self.agent,
            shell: self.shell,
            live,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutDelta {
    WorkspaceCreated {
        workspace: Workspace,
    },
    WorkspaceRenamed {
        name: Option<String>,
    },
    WorkspaceDeleted,
    WorkspaceTouched {
        last_active: u64,
    },
    ActiveTabChanged {
        tab: TabId,
    },
    TabCreated {
        at: usize,
        tab: Tab,
    },
    TabClosed {
        tab: TabId,
    },
    TabRenamed {
        tab: TabId,
        name: Option<String>,
    },
    TabMoved {
        tab: TabId,
        to: usize,
    },
    TabRegrouped {
        tab: TabId,
        group: Option<String>,
    },
    TabRestructured {
        tab: Tab,
        pane: Option<PaneRecord>,
    },
    RatioChanged {
        tab: TabId,
        path: Vec<Side>,
        ratio: f32,
    },
    PaneFacts {
        pane: PaneRecord,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubscriberId(pub u64);

pub type Notify = Arc<dyn Fn(&str, &LayoutDelta) + Send + Sync>;

pub struct Subscription {
    store: Arc<MachineStore>,
    id: SubscriberId,
}

impl Subscription {
    pub fn id(&self) -> SubscriberId {
        self.id
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.store.unsubscribe(self.id);
    }
}

pub type LivenessProbe = Arc<dyn Fn(u64) -> bool + Send + Sync>;

pub struct MachineStore {
    path: PathBuf,
    state: Mutex<Machine>,
    liveness: Mutex<Option<LivenessProbe>>,
    notify_order: Mutex<()>,
    subscribers: Mutex<Vec<(SubscriberId, Notify)>>,
    next_subscriber: AtomicU64,
    unwritten: AtomicBool,
    flushing: AtomicBool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Persist {
    Now,
    Soon,
}

fn refuse(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

fn not_found(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, msg.into())
}

impl MachineStore {
    pub fn open(path: impl Into<PathBuf>) -> Arc<MachineStore> {
        let path = path.into();
        let machine = load_machine(&path);
        Arc::new(MachineStore {
            path,
            state: Mutex::new(machine),
            liveness: Mutex::new(None),
            notify_order: Mutex::new(()),
            subscribers: Mutex::new(Vec::new()),
            next_subscriber: AtomicU64::new(1),
            unwritten: AtomicBool::new(false),
            flushing: AtomicBool::new(false),
        })
    }

    pub fn set_liveness_probe(&self, probe: LivenessProbe) {
        *self.liveness.lock().unwrap_or_else(|e| e.into_inner()) = Some(probe);
    }

    fn seed_is_live(&self, pane: u64) -> bool {
        let probe = self
            .liveness
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match probe {
            Some(probe) => probe(pane),
            None => true,
        }
    }

    pub fn shared() -> io::Result<Arc<MachineStore>> {
        Ok(MachineStore::open(default_machine_path()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn machine(&self) -> Machine {
        self.locked().clone()
    }

    pub fn workspace(&self, id: WorkspaceId) -> io::Result<Workspace> {
        self.locked()
            .workspaces
            .iter()
            .find(|w| w.id == id)
            .cloned()
            .ok_or_else(|| not_found(format!("no workspace {id} on this machine")))
    }

    pub fn pane(&self, id: u64) -> Option<PaneRecord> {
        self.locked().panes.iter().find(|p| p.id == id).cloned()
    }

    pub fn workspace_create(
        &self,
        id: Option<WorkspaceId>,
        name: Option<String>,
        origin: Option<SubscriberId>,
    ) -> io::Result<Workspace> {
        let created = self.mutate(origin, |m| {
            if m.workspaces.len() >= MAX_WORKSPACES {
                return Err(refuse(format!(
                    "this machine already holds {MAX_WORKSPACES} workspaces"
                )));
            }
            if let Some(id) = id
                && m.workspaces.iter().any(|w| w.id == id)
            {
                return Err(refuse(format!("workspace {id} already exists")));
            }
            let workspace = Workspace {
                id: id.unwrap_or_default(),
                name: name.clone(),
                ..Workspace::default()
            };
            m.workspaces.push(workspace.clone());
            Ok((
                workspace.clone(),
                vec![(
                    workspace.id,
                    LayoutDelta::WorkspaceCreated {
                        workspace: workspace.clone(),
                    },
                )],
            ))
        })?;
        Ok(created)
    }

    pub fn workspace_rename(
        &self,
        id: WorkspaceId,
        name: Option<String>,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        self.mutate(origin, |m| {
            let ws = find_workspace(m, id)?;
            ws.name = name.clone();
            Ok(((), vec![(id, LayoutDelta::WorkspaceRenamed { name })]))
        })
    }

    pub fn workspace_delete(
        &self,
        id: WorkspaceId,
        origin: Option<SubscriberId>,
    ) -> io::Result<Vec<u64>> {
        self.mutate(origin, |m| {
            let index = m
                .workspaces
                .iter()
                .position(|w| w.id == id)
                .ok_or_else(|| not_found(format!("no workspace {id} on this machine")))?;
            m.workspaces.remove(index);
            let orphans = collect_orphan_panes(m);
            m.panes.retain(|p| !orphans.contains(&p.id));
            Ok((orphans, vec![(id, LayoutDelta::WorkspaceDeleted)]))
        })
    }

    pub fn workspace_touch(
        self: &Arc<Self>,
        id: WorkspaceId,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        self.ensure_flusher();
        self.mutate_with(origin, Persist::Soon, |m| {
            let ws = find_workspace(m, id)?;
            let now = unix_now();
            ws.last_active = now;
            Ok((
                (),
                vec![(id, LayoutDelta::WorkspaceTouched { last_active: now })],
            ))
        })
    }

    pub fn workspace_set_active_tab(
        &self,
        id: WorkspaceId,
        tab: TabId,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        self.mutate(origin, |m| {
            let ws = find_workspace(m, id)?;
            if !ws.tabs.iter().any(|t| t.id == tab) {
                return Err(not_found(format!("workspace {id} has no tab {tab}")));
            }
            ws.active_tab = Some(tab);
            Ok(((), vec![(id, LayoutDelta::ActiveTabChanged { tab })]))
        })
    }

    pub fn tab_create(
        &self,
        workspace: WorkspaceId,
        at: Option<usize>,
        pane: PaneSeed,
        id: Option<TabId>,
        origin: Option<SubscriberId>,
    ) -> io::Result<Tab> {
        let live = self.seed_is_live(pane.pane);
        self.mutate(origin, |m| {
            if let Some(id) = id
                && m.workspaces
                    .iter()
                    .any(|w| w.tabs.iter().any(|t| t.id == id))
            {
                return Err(refuse(format!("tab {id} already exists")));
            }
            register_pane(m, pane.clone(), live)?;
            let ws = find_workspace(m, workspace)?;
            let mut tab = Tab::leaf(pane.pane);
            if let Some(id) = id {
                tab.id = id;
            }
            let tab = tab;
            let at = at.unwrap_or(ws.tabs.len()).min(ws.tabs.len());
            ws.tabs.insert(at, tab.clone());
            ws.active_tab = Some(tab.id);
            let active = tab.id;
            Ok((
                tab.clone(),
                vec![
                    (workspace, LayoutDelta::TabCreated { at, tab }),
                    (workspace, LayoutDelta::ActiveTabChanged { tab: active }),
                ],
            ))
        })
    }

    pub fn tab_close(
        &self,
        workspace: WorkspaceId,
        tab: TabId,
        origin: Option<SubscriberId>,
    ) -> io::Result<Vec<u64>> {
        self.mutate(origin, |m| {
            let ws = find_workspace(m, workspace)?;
            let index = ws
                .tabs
                .iter()
                .position(|t| t.id == tab)
                .ok_or_else(|| not_found(format!("workspace {workspace} has no tab {tab}")))?;
            ws.tabs.remove(index);
            let mut deltas = vec![(workspace, LayoutDelta::TabClosed { tab })];
            if let Some(active) = heal_active_tab(ws, index) {
                deltas.push((workspace, LayoutDelta::ActiveTabChanged { tab: active }));
            }
            let orphans = collect_orphan_panes(m);
            m.panes.retain(|p| !orphans.contains(&p.id));
            Ok((orphans, deltas))
        })
    }

    pub fn tab_rename(
        &self,
        workspace: WorkspaceId,
        tab: TabId,
        name: Option<String>,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        self.mutate(origin, |m| {
            let t = find_tab(m, workspace, tab)?;
            t.name = name.clone();
            Ok(((), vec![(workspace, LayoutDelta::TabRenamed { tab, name })]))
        })
    }

    pub fn tab_move(
        &self,
        workspace: WorkspaceId,
        tab: TabId,
        to: usize,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        self.mutate(origin, |m| {
            let ws = find_workspace(m, workspace)?;
            let from = ws
                .tabs
                .iter()
                .position(|t| t.id == tab)
                .ok_or_else(|| not_found(format!("workspace {workspace} has no tab {tab}")))?;
            let moved = ws.tabs.remove(from);
            let to = to.min(ws.tabs.len());
            ws.tabs.insert(to, moved);
            Ok(((), vec![(workspace, LayoutDelta::TabMoved { tab, to })]))
        })
    }

    pub fn tab_set_group(
        &self,
        workspace: WorkspaceId,
        tab: TabId,
        group: Option<String>,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        self.mutate(origin, |m| {
            let t = find_tab(m, workspace, tab)?;
            t.sidebar_group = group.clone();
            Ok((
                (),
                vec![(workspace, LayoutDelta::TabRegrouped { tab, group })],
            ))
        })
    }

    pub fn pane_split(
        &self,
        workspace: WorkspaceId,
        pane: u64,
        axis: Axis,
        ratio: f32,
        new: PaneSeed,
        first: bool,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        let ratio = clamp_ratio(ratio)?;
        let live = self.seed_is_live(new.pane);
        self.mutate(origin, |m| {
            register_pane(m, new.clone(), live)?;
            let record = m
                .panes
                .iter()
                .find(|p| p.id == new.pane)
                .cloned()
                .expect("registered above");
            let ws = find_workspace(m, workspace)?;
            let tab = ws
                .tabs
                .iter_mut()
                .find(|t| t.root.contains(pane))
                .ok_or_else(|| {
                    not_found(format!("workspace {workspace} has no pane {pane} to split"))
                })?;
            tab.root.split_leaf(pane, new.pane, axis, ratio, first);
            let delta = LayoutDelta::TabRestructured {
                tab: tab.clone(),
                pane: Some(record),
            };
            Ok(((), vec![(workspace, delta)]))
        })
    }

    pub fn pane_close(
        &self,
        workspace: WorkspaceId,
        pane: u64,
        origin: Option<SubscriberId>,
    ) -> io::Result<Vec<u64>> {
        self.mutate(origin, |m| {
            let ws = find_workspace(m, workspace)?;
            let index = ws
                .tabs
                .iter()
                .position(|t| t.root.contains(pane))
                .ok_or_else(|| not_found(format!("workspace {workspace} has no pane {pane}")))?;
            let mut deltas = Vec::new();
            match ws.tabs[index].root.remove_leaf(pane) {
                None => {
                    let closed = ws.tabs.remove(index);
                    deltas.push((workspace, LayoutDelta::TabClosed { tab: closed.id }));
                    if let Some(active) = heal_active_tab(ws, index) {
                        deltas.push((workspace, LayoutDelta::ActiveTabChanged { tab: active }));
                    }
                }
                Some(true) => deltas.push((
                    workspace,
                    LayoutDelta::TabRestructured {
                        tab: ws.tabs[index].clone(),
                        pane: None,
                    },
                )),
                Some(false) => unreachable!("the tab was chosen because it contains the pane"),
            };
            let orphans = collect_orphan_panes(m);
            m.panes.retain(|p| !orphans.contains(&p.id));
            Ok((orphans, deltas))
        })
    }

    pub fn pane_set_ratio(
        &self,
        workspace: WorkspaceId,
        tab: TabId,
        path: Vec<Side>,
        ratio: f32,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        let ratio = clamp_ratio(ratio)?;
        self.mutate(origin, |m| {
            let t = find_tab(m, workspace, tab)?;
            match t.root.descend_mut(&path) {
                Some(PaneNode::Split { ratio: r, .. }) => *r = ratio,
                _ => {
                    return Err(refuse(format!(
                        "tab {tab} has no split at that path any more"
                    )));
                }
            }
            Ok((
                (),
                vec![(workspace, LayoutDelta::RatioChanged { tab, path, ratio })],
            ))
        })
    }

    pub fn pane_move(
        &self,
        workspace: WorkspaceId,
        pane: u64,
        to: u64,
        axis: Axis,
        first: bool,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        if pane == to {
            return Err(refuse("a pane cannot be moved next to itself"));
        }
        self.mutate(origin, |m| {
            let ws = find_workspace(m, workspace)?;
            let from = ws
                .tabs
                .iter()
                .position(|t| t.root.contains(pane))
                .ok_or_else(|| not_found(format!("workspace {workspace} has no pane {pane}")))?;
            let dest = ws
                .tabs
                .iter()
                .position(|t| t.root.contains(to))
                .ok_or_else(|| not_found(format!("workspace {workspace} has no pane {to}")))?;

            let mut deltas: Vec<(WorkspaceId, LayoutDelta)> = Vec::new();
            match ws.tabs[from].root.remove_leaf(pane) {
                None => {
                    if from == dest {
                        return Err(refuse("a pane cannot be moved next to itself".to_string()));
                    }
                    let closed = ws.tabs.remove(from);
                    deltas.push((workspace, LayoutDelta::TabClosed { tab: closed.id }));
                    if let Some(active) = heal_active_tab(ws, from) {
                        deltas.push((workspace, LayoutDelta::ActiveTabChanged { tab: active }));
                    }
                }
                Some(true) => {
                    deltas.push((
                        workspace,
                        LayoutDelta::TabRestructured {
                            tab: ws.tabs[from].clone(),
                            pane: None,
                        },
                    ));
                }
                Some(false) => unreachable!("the tab was chosen because it contains the pane"),
            }
            let dest_tab = ws
                .tabs
                .iter_mut()
                .find(|t| t.root.contains(to))
                .expect("the destination tab still exists; only the source tab can close");
            dest_tab.root.split_leaf(to, pane, axis, 0.5, first);
            deltas.push((
                workspace,
                LayoutDelta::TabRestructured {
                    tab: dest_tab.clone(),
                    pane: None,
                },
            ));
            Ok(((), deltas))
        })
    }

    pub fn pane_replace(
        &self,
        workspace: WorkspaceId,
        old: u64,
        new: PaneSeed,
        origin: Option<SubscriberId>,
    ) -> io::Result<()> {
        let live = self.seed_is_live(new.pane);
        self.mutate(origin, |m| {
            register_pane(m, new.clone(), live)?;
            let record = m
                .panes
                .iter()
                .find(|p| p.id == new.pane)
                .cloned()
                .expect("registered above");
            let ws = find_workspace(m, workspace)?;
            let tab = ws
                .tabs
                .iter_mut()
                .find(|t| t.root.contains(old))
                .ok_or_else(|| not_found(format!("workspace {workspace} has no pane {old}")))?;
            tab.root.replace_leaf(old, new.pane);
            let delta = LayoutDelta::TabRestructured {
                tab: tab.clone(),
                pane: Some(record),
            };
            m.panes.retain(|p| p.id != old);
            Ok(((), vec![(workspace, delta)]))
        })
    }

    pub fn note_pane_facts(self: &Arc<Self>, pane: u64, update: impl FnOnce(&mut PaneRecord)) {
        self.ensure_flusher();
        let result: io::Result<()> = self.mutate_with(None, Persist::Soon, |m| {
            let Some(record) = m.panes.iter_mut().find(|p| p.id == pane) else {
                return Ok(((), Vec::new()));
            };
            let before = record.clone();
            update(record);
            record.id = before.id;
            if *record == before {
                return Ok(((), Vec::new()));
            }
            let record = record.clone();
            let workspaces: Vec<WorkspaceId> = m
                .workspaces
                .iter()
                .filter(|w| w.tabs.iter().any(|t| t.root.contains(pane)))
                .map(|w| w.id)
                .collect();
            Ok((
                (),
                workspaces
                    .into_iter()
                    .map(|w| {
                        (
                            w,
                            LayoutDelta::PaneFacts {
                                pane: record.clone(),
                            },
                        )
                    })
                    .collect(),
            ))
        });
        if let Err(e) = result {
            log::warn!("could not record facts about pane {pane}: {e}");
        }
    }

    pub fn attach(&self, workspace: WorkspaceId, who: Attachment) -> Option<Attachment> {
        let mut m = self.locked();
        let ws = m.workspaces.iter_mut().find(|w| w.id == workspace)?;
        ws.attachment.replace(who)
    }

    pub fn attachment(&self, workspace: WorkspaceId) -> Option<Attachment> {
        self.locked()
            .workspaces
            .iter()
            .find(|w| w.id == workspace)
            .and_then(|w| w.attachment.clone())
    }

    pub fn detach(&self, workspace: WorkspaceId, token: &str) -> bool {
        let mut m = self.locked();
        let Some(ws) = m.workspaces.iter_mut().find(|w| w.id == workspace) else {
            return false;
        };
        if ws.attachment.as_ref().is_some_and(|a| a.token == token) {
            ws.attachment = None;
            true
        } else {
            false
        }
    }

    pub fn subscribe(self: &Arc<Self>, f: Notify) -> Subscription {
        let id = SubscriberId(self.next_subscriber.fetch_add(1, Ordering::Relaxed));
        self.subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, f));
        Subscription {
            store: Arc::clone(self),
            id,
        }
    }

    fn unsubscribe(&self, id: SubscriberId) {
        self.subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(sid, _)| *sid != id);
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Machine> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn mutate<T>(
        &self,
        origin: Option<SubscriberId>,
        op: impl FnOnce(&mut Machine) -> io::Result<(T, Vec<(WorkspaceId, LayoutDelta)>)>,
    ) -> io::Result<T> {
        self.mutate_with(origin, Persist::Now, op)
    }

    fn mutate_with<T>(
        &self,
        origin: Option<SubscriberId>,
        persist: Persist,
        op: impl FnOnce(&mut Machine) -> io::Result<(T, Vec<(WorkspaceId, LayoutDelta)>)>,
    ) -> io::Result<T> {
        let _order = self.notify_order.lock().unwrap_or_else(|e| e.into_inner());
        let deltas;
        let value;
        {
            let mut m = self.locked();
            let before = m.clone();
            match op(&mut m).and_then(|out| {
                if *m != before {
                    match persist {
                        Persist::Now => self.persist(&m)?,
                        Persist::Soon => self.unwritten.store(true, Ordering::Release),
                    }
                }
                Ok(out)
            }) {
                Ok((v, d)) => {
                    value = v;
                    deltas = d;
                }
                Err(e) => {
                    *m = before;
                    return Err(e);
                }
            }
        }
        if !deltas.is_empty() {
            self.notify_all(&deltas, origin);
        }
        Ok(value)
    }

    pub fn flush(&self) {
        if !self.unwritten.load(Ordering::Acquire) {
            return;
        }
        let _order = self.notify_order.lock().unwrap_or_else(|e| e.into_inner());
        let m = self.locked();
        self.unwritten.store(false, Ordering::Release);
        if let Err(e) = self.persist(&m) {
            log::warn!("could not write {}: {e}", self.path.display());
            self.unwritten.store(true, Ordering::Release);
        }
    }

    fn ensure_flusher(self: &Arc<Self>) {
        if self.flushing.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name("tty7-machine-flush".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(FACT_FLUSH_INTERVAL);
                    let Some(store) = weak.upgrade() else { return };
                    store.flush();
                }
            });
        if let Err(e) = spawned {
            log::warn!("could not start the machine-tree flusher ({e}); writing facts inline");
            self.flushing.store(false, Ordering::Release);
            self.flush();
        }
    }

    fn persist(&self, m: &Machine) -> io::Result<()> {
        let mut doc = m.clone();
        for ws in &mut doc.workspaces {
            ws.attachment = None;
        }
        let bytes = serde_json::to_vec_pretty(&doc).map_err(io::Error::other)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::core::config::write_atomic_private(&self.path, &bytes)
    }

    fn notify_all(&self, deltas: &[(WorkspaceId, LayoutDelta)], origin: Option<SubscriberId>) {
        let subscribers: Vec<(SubscriberId, Notify)> = self
            .subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for (workspace, delta) in deltas {
            let key = workspace.to_string();
            for (sid, f) in &subscribers {
                if Some(*sid) != origin {
                    f(&key, delta);
                }
            }
        }
    }
}

fn find_workspace(m: &mut Machine, id: WorkspaceId) -> io::Result<&mut Workspace> {
    m.workspaces
        .iter_mut()
        .find(|w| w.id == id)
        .ok_or_else(|| not_found(format!("no workspace {id} on this machine")))
}

fn find_tab(m: &mut Machine, workspace: WorkspaceId, tab: TabId) -> io::Result<&mut Tab> {
    let ws = find_workspace(m, workspace)?;
    ws.tabs
        .iter_mut()
        .find(|t| t.id == tab)
        .ok_or_else(|| not_found(format!("workspace {workspace} has no tab {tab}")))
}

fn heal_active_tab(ws: &mut Workspace, removed: usize) -> Option<TabId> {
    let named = ws
        .active_tab
        .is_some_and(|active| ws.tabs.iter().any(|t| t.id == active));
    if named || ws.tabs.is_empty() {
        ws.active_tab = ws.active_tab.filter(|_| named);
        return None;
    }
    let active = ws.tabs[removed.min(ws.tabs.len() - 1)].id;
    ws.active_tab = Some(active);
    Some(active)
}

fn register_pane(m: &mut Machine, seed: PaneSeed, live: bool) -> io::Result<()> {
    let shown = m
        .workspaces
        .iter()
        .any(|w| w.tabs.iter().any(|t| t.root.contains(seed.pane)));
    if shown || m.panes.iter().any(|p| p.id == seed.pane) {
        return Err(refuse(format!(
            "pane {} is already part of this machine's tree",
            seed.pane
        )));
    }
    if m.panes.len() >= MAX_PANES {
        return Err(refuse(format!(
            "this machine's tree already references {MAX_PANES} panes"
        )));
    }
    m.panes.push(seed.into_record(live));
    Ok(())
}

fn collect_orphan_panes(m: &Machine) -> Vec<u64> {
    m.panes
        .iter()
        .map(|p| p.id)
        .filter(|id| {
            !m.workspaces
                .iter()
                .any(|w| w.tabs.iter().any(|t| t.root.contains(*id)))
        })
        .collect()
}

fn clamp_ratio(ratio: f32) -> io::Result<f32> {
    if !ratio.is_finite() {
        return Err(refuse("a split ratio must be a finite number"));
    }
    Ok(ratio.clamp(0.05, 0.95))
}

fn load_machine(path: &Path) -> Machine {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Machine::default(),
        Err(e) => {
            log::warn!("could not read {}; quarantining it: {e}", path.display());
            crate::core::config::quarantine_by_rename(path);
            return Machine::default();
        }
    };
    match serde_json::from_str::<Machine>(crate::core::config::strip_bom(&text)) {
        Ok(mut machine) => {
            for pane in &mut machine.panes {
                pane.live = false;
            }
            machine
        }
        Err(e) => {
            log::warn!("{} does not parse ({e}); quarantining it", path.display());
            crate::core::config::quarantine(path);
            Machine::default()
        }
    }
}

static OBSERVED: Mutex<Option<Arc<MachineStore>>> = Mutex::new(None);

pub fn publish_observations(store: &Arc<MachineStore>) {
    *OBSERVED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(store));
}

pub fn observe_pane(pane: u64, f: impl FnOnce(&mut PaneRecord)) {
    let store = OBSERVED.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(store) = store {
        store.note_pane_facts(pane, f);
    }
}

pub fn observed_store() -> Option<Arc<MachineStore>> {
    OBSERVED.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[cfg(test)]
pub(crate) fn withdraw_observations() {
    *OBSERVED.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
pub(crate) static OBSERVE_SLOT: Mutex<()> = Mutex::new(());

pub fn default_machine_path() -> io::Result<PathBuf> {
    Ok(data_dir()?.join(MACHINE_FILE))
}

pub fn appearance_path() -> io::Result<PathBuf> {
    Ok(data_dir()?.join(APPEARANCE_FILE))
}

/// The light/dark mode the GUI last applied, cached beside the machine tree.
///
/// The daemon needs it when it spawns a pane on Windows, to fill in the
/// `COLORFGBG` hint (see `daemon::pane::pane_environment`), and the daemon is a
/// separate process from the GUI that owns the theme. This is derived state,
/// not a setting: it is
/// written by whichever process paints the window and read by whoever needs to
/// describe that window, so it lives here rather than in `config.json` — the
/// user's file, which nothing should rewrite behind their back.
///
/// A file of its own rather than a field on [`Machine`]: the machine tree is
/// owned by the daemon, held in memory and flushed on a timer, so a second
/// writer would clobber the workspaces and panes it had not seen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Appearance {
    #[serde(default)]
    pub dark: bool,
}

/// Read the cached appearance, or the default when there is none to read.
///
/// Absent, unreadable, and unparsable all answer `dark: false`, which is what
/// tty7's default preset is: a daemon that spawns a pane before the GUI has
/// ever applied a theme describes the default window rather than guessing.
pub fn appearance() -> Appearance {
    match appearance_path() {
        Ok(path) => read_appearance(&path),
        Err(e) => {
            log::debug!("no appearance hint ({e}); assuming the default preset");
            Appearance::default()
        }
    }
}

/// Record the appearance the GUI just applied. A no-op when it has not changed,
/// so repainting the same theme does not touch the disk.
pub fn note_appearance(dark: bool) {
    let Ok(path) = appearance_path() else { return };
    let next = Appearance { dark };
    if read_appearance(&path) == next && path.exists() {
        return;
    }
    if let Err(e) = write_appearance(&path, next) {
        log::warn!("could not write {}: {e}", path.display());
    }
}

fn read_appearance(path: &Path) -> Appearance {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Appearance::default();
    };
    serde_json::from_str::<Appearance>(crate::core::config::strip_bom(&text)).unwrap_or_else(|e| {
        log::warn!(
            "{} does not parse ({e}); assuming the default preset",
            path.display()
        );
        Appearance::default()
    })
}

fn write_appearance(path: &Path, appearance: Appearance) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(&appearance).map_err(io::Error::other)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::core::config::write_atomic_private(path, &bytes)
}

/// Where the machine tree lives: the config directory, unless a test pins one
/// with [`DATA_DIR_ENV`].
///
/// It used to be the XDG data directory, resolved from `HOME` alone. That made
/// the tree the one piece of an instance that `--config-dir` did not move, so
/// two tty7s pointed at different config directories — each holding its own
/// `daemon.lock`, each certain it was the only server — still co-owned one
/// `machine.json`. `MachineStore::persist` writes the document whole, so the
/// second one to flush replaced the first one's workspaces with its own, and
/// the next daemon to start read the survivor's tree as the machine's. A tree
/// that comes back empty is not distinguishable from a machine that really has
/// nothing on it, so the GUI does what an empty tree means (see
/// `tree_sync::on_workspace_deleted`) and forgets the workspaces for good.
///
/// The lock and the thing it protects have to be keyed alike. Everything else
/// an instance owns — `views.json`, the scrollback, the history, both sockets,
/// the pidfile, the lock — is already keyed by the config directory; the tree
/// and the appearance hint were the last two files that were not.
fn data_dir() -> io::Result<PathBuf> {
    if let Some(explicit) = std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(explicit));
    }
    crate::core::config::config_dir_path().ok_or_else(|| {
        io::Error::other(format!(
            "no config directory to place {MACHINE_FILE} in; set {DATA_DIR_ENV}"
        ))
    })
}

/// Where [`data_dir`] pointed before it followed the config directory.
///
/// Deliberately still resolved the old way, `TTY7_DATA_DIR` included: this
/// answers "where would the build the user just upgraded from have put it",
/// and that build read the environment, not the config directory.
fn legacy_data_dir() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    #[cfg(not(windows))]
    let base = env_dir("XDG_DATA_HOME")
        .or_else(|| env_dir("HOME").map(|h| h.join(".local").join("share")));
    #[cfg(windows)]
    let base = env_dir("LOCALAPPDATA")
        .or_else(|| env_dir("USERPROFILE").map(|h| h.join(".local").join("share")));

    base.map(|b| b.join("tty7"))
}

/// Carry an upgrading install's tree and appearance hint over to the config
/// directory.
///
/// Moving the path without this would lose every workspace on the machine at
/// the moment of upgrade, which is the failure this whole change exists to
/// stop.
///
/// Called by the daemon at startup, before it opens the store, and by nothing
/// else. The daemon is the tree's writer, so it is the process entitled to move
/// it; and a path getter that touches the disk as a side effect is one no test
/// can call without putting the developer's own tree at risk — which is the
/// accident this whole change is about.
///
/// The appearance hint rides along, and it does have a second writer: the GUI
/// records it whenever it applies a theme ([`note_appearance`]). Both writers
/// land their file with a rename, so the worst a collision costs is one of two
/// booleans, and the next theme the GUI applies overwrites it either way.
pub fn adopt_legacy_data_dir() {
    adopt_into(
        data_dir().ok().as_deref(),
        legacy_data_dir().as_deref(),
        this_is_the_machines_instance(),
    );
}

/// [`adopt_legacy_data_dir`] with every process-wide answer already looked up,
/// so a test can state the situation instead of arranging one.
fn adopt_into(current: Option<&Path>, legacy: Option<&Path>, machines_instance: bool) {
    let (Some(current), Some(legacy)) = (current, legacy) else {
        return;
    };
    // The same directory under two names is not a migration. This is also what
    // makes `TTY7_DATA_DIR` a no-op: both getters answer with it.
    if legacy == current {
        return;
    }
    if !machines_instance {
        decline_legacy(legacy);
        return;
    }
    for file in [MACHINE_FILE, APPEARANCE_FILE] {
        adopt_legacy_file(&legacy.join(file), &current.join(file));
    }
}

/// Whether this process is the machine's tty7 rather than an instance somebody
/// pointed somewhere else.
///
/// There is one legacy tree and there can be any number of instances, so "who
/// inherits it" has to have exactly one answer, and it cannot be "whoever
/// starts first". Every build before this change read the tree that
/// [`legacy_data_dir`] names, so the instance entitled to it is the one still
/// running out of the config directory this machine resolves to on its own
/// ([`config::machine_config_dir`](crate::core::config::machine_config_dir)) —
/// `$TTY7_CONFIG_DIR` when the box sets one, `$HOME`'s otherwise. Anything
/// aimed elsewhere is a second tty7 by definition, and a second tty7 starts on
/// an empty tree.
///
/// Comparing paths rather than asking whether `--config-dir` was passed is what
/// makes the ordinary install work: [`daemon::spawn`](crate::daemon::spawn)
/// hands the daemon an explicit `--config-dir` every time, its own resolved
/// directory included, so "was the flag given" is true for everybody and would
/// decline for everybody.
///
/// Deciding by start order instead would let a second instance rename the tree
/// out from under the first — this bug wearing a different hat, the primary
/// coming up owning nothing and every workspace with no window on it gone. It
/// would also fire in our own test suite, where `routed_pane` and friends
/// launch a real `tty7-server --config-dir <TempDir>` under the developer's own
/// `HOME`: a start-order rule moves the developer's real `machine.json` into a
/// scratch directory and deletes it with the `TempDir`.
///
/// Counting `$TTY7_CONFIG_DIR` as the machine's is what keeps remote hosts
/// upgrading: a remote `tty7-server` is launched with no `--config-dir` and
/// finds its config directory exactly this way (see
/// `daemon::remote_link::remote_control_socket`), so a rule written against
/// `$HOME` alone would strand the tree on every box that names one.
fn this_is_the_machines_instance() -> bool {
    is_the_machines_instance(
        crate::core::config::config_dir_path().as_deref(),
        crate::core::config::machine_config_dir().as_deref(),
    )
}

fn is_the_machines_instance(current: Option<&Path>, machines: Option<&Path>) -> bool {
    matches!((current, machines), (Some(c), Some(m)) if c == m)
}

/// Say where the tree was left when this instance is not the one entitled to
/// take it, so "my workspaces are gone" has an answer in the log rather than
/// only in this comment.
fn decline_legacy(legacy: &Path) {
    let file = legacy.join(MACHINE_FILE);
    if !file.exists() {
        return;
    }
    log::info!(
        "leaving {} where it is: this instance runs on a config directory of its own, and \
         {LEGACY_NOTE}. Move it in by hand if this is the instance that should have it.",
        file.display()
    );
}

/// The destination existing at all is the whole guard. It means some newer run
/// already owns this file, and the legacy copy beside it is stale — a build
/// from before this change, started once since the move, writing where it still
/// believes the tree lives. Overwriting would hand that stale tree back.
///
/// Two processes migrating at once is safe for the same reason they cannot both
/// win: they rename the same source, so the loser's rename fails with the
/// source already gone — see [`adopt_by_copy`], which tells that apart from a
/// rename that failed with something still to carry over.
fn adopt_legacy_file(legacy: &Path, current: &Path) {
    if current.exists() || !legacy.exists() {
        return;
    }
    if let Some(parent) = current.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        log::warn!(
            "could not create {} to receive {}: {e}",
            parent.display(),
            legacy.display()
        );
        return;
    }
    match std::fs::rename(legacy, current) {
        Ok(()) => log::info!(
            "moved {} to {} ({LEGACY_NOTE})",
            legacy.display(),
            current.display()
        ),
        Err(e) => adopt_by_copy(legacy, current, &e),
    }
}

/// The rename did not go through. Two reasons to tell apart, because only one
/// of them is a problem.
///
/// The source being gone is the concurrent-migration race: another process
/// renamed it away between the check above and here, and its result is the one
/// this call wanted anyway. Reporting that as a failure — or copying over it —
/// would turn the one benign race into noise in the log.
///
/// Otherwise the usual reason is that the two directories are on different
/// filesystems, where `rename` refuses and a copy is the only way over. Writing
/// it with `create_new` keeps "never overwrite what is already there" true
/// against a racing writer and not merely against the `exists` check, which by
/// now is several syscalls stale. The original stays: it costs a file nobody
/// reads, and losing the tree to a half-finished move is the one outcome worth
/// ruling out.
fn adopt_by_copy(legacy: &Path, current: &Path, why: &io::Error) {
    if !legacy.exists() {
        log::debug!(
            "{} was carried over by another process ({why})",
            legacy.display()
        );
        return;
    }
    match copy_new(legacy, current) {
        Ok(()) => log::info!(
            "copied {} to {} ({LEGACY_NOTE}); the original was left in place",
            legacy.display(),
            current.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => log::debug!(
            "{} already has one; leaving {} where it is",
            current.display(),
            legacy.display()
        ),
        Err(e) => log::warn!(
            "could not move {} to {} ({why}), nor copy it: {e}",
            legacy.display(),
            current.display()
        ),
    }
}

/// Copy `from` onto a `to` that must not already exist, leaving nothing behind
/// if the write does not finish: a truncated tree is a tree, and the next
/// startup would adopt nothing over it.
///
/// Read whole rather than streamed so a failure lands before the destination is
/// created, and private because that is how both files are written
/// ([`crate::core::config::write_atomic_private`]) — `fs::copy` would carry the
/// legacy mode over, which is the same thing only as long as the legacy file
/// was ours.
fn copy_new(from: &Path, to: &Path) -> io::Result<()> {
    use std::io::Write as _;

    let bytes = std::fs::read(from)?;
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        open.mode(0o600);
    }
    let mut file = open.open(to)?;
    if let Err(e) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(to);
        return Err(e);
    }
    Ok(())
}

const LEGACY_NOTE: &str = "the machine tree now lives beside the rest of the config directory";

fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Arc<MachineStore>, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        (MachineStore::open(dir.path().join(MACHINE_FILE)), dir)
    }

    #[test]
    fn the_appearance_hint_round_trips_and_defaults_to_light() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nested").join(APPEARANCE_FILE);

        assert_eq!(
            read_appearance(&path),
            Appearance { dark: false },
            "a hint nobody has written yet reads as the default preset"
        );

        write_appearance(&path, Appearance { dark: true }).unwrap();
        assert_eq!(read_appearance(&path), Appearance { dark: true });

        write_appearance(&path, Appearance { dark: false }).unwrap();
        assert_eq!(read_appearance(&path), Appearance { dark: false });

        std::fs::write(&path, "not json").unwrap();
        assert_eq!(
            read_appearance(&path),
            Appearance { dark: false },
            "a corrupt hint must not decide the background either"
        );
    }

    fn seed(pane: u64, cwd: &str) -> PaneSeed {
        PaneSeed {
            pane,
            cwd: Some(cwd.to_string()),
            ssh_spec: None,
            agent: None,
            shell: None,
        }
    }

    fn store_with_tab() -> (Arc<MachineStore>, tempfile::TempDir, WorkspaceId, Tab) {
        let (store, dir) = store();
        let ws = store
            .workspace_create(None, Some("api".into()), None)
            .unwrap();
        let tab = store
            .tab_create(ws.id, None, seed(1, "/work"), None, None)
            .unwrap();
        (store, dir, ws.id, tab)
    }

    fn recorded(
        store: &Arc<MachineStore>,
    ) -> (Subscription, Arc<Mutex<Vec<(String, LayoutDelta)>>>) {
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        let sub = store.subscribe(Arc::new(move |ws: &str, delta: &LayoutDelta| {
            sink.lock().unwrap().push((ws.to_string(), delta.clone()));
        }));
        (sub, heard)
    }

    #[test]
    fn a_client_minted_workspace_id_is_kept_and_a_duplicate_is_refused() {
        let (store, _dir) = store();
        let id = WorkspaceId::new();
        let ws = store
            .workspace_create(Some(id), Some("api".into()), None)
            .unwrap();
        assert_eq!(ws.id, id, "the id the client named is the id it gets");

        let refused = store
            .workspace_create(Some(id), None, None)
            .expect_err("a second create on the same id must refuse");
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            store.machine().workspaces.len(),
            1,
            "the refusal changed nothing"
        );
    }

    #[test]
    fn a_client_minted_tab_id_is_kept_and_a_duplicate_is_refused_anywhere() {
        let (store, _dir, ws, _tab) = store_with_tab();
        let id = TabId::new();
        let tab = store
            .tab_create(ws, None, seed(2, "/b"), Some(id), None)
            .unwrap();
        assert_eq!(tab.id, id);

        let other = store.workspace_create(None, None, None).unwrap();
        let refused = store
            .tab_create(other.id, None, seed(3, "/c"), Some(id), None)
            .expect_err("a taken tab id must refuse");
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
        assert!(
            store.pane(3).is_none(),
            "the refused create adopted no pane either"
        );
    }

    #[test]
    fn the_tree_round_trips_through_the_file() {
        let (store, dir) = store();
        let ws = store
            .workspace_create(None, Some("api".into()), None)
            .unwrap();
        store
            .tab_create(ws.id, None, seed(1, "/work"), None, None)
            .unwrap();
        store
            .pane_split(
                ws.id,
                1,
                Axis::Vertical,
                0.3,
                seed(2, "/work/api"),
                false,
                None,
            )
            .unwrap();

        let reopened = MachineStore::open(dir.path().join(MACHINE_FILE));
        let machine = reopened.machine();
        assert_eq!(machine.workspaces.len(), 1);
        let back = &machine.workspaces[0];
        assert_eq!(back.id, ws.id, "workspace identity survives a restart");
        assert_eq!(back.name.as_deref(), Some("api"));
        assert_eq!(back.tabs.len(), 1);
        assert_eq!(back.tabs[0].root.pane_ids(), vec![1, 2]);
        match &back.tabs[0].root {
            PaneNode::Split { axis, ratio, .. } => {
                assert_eq!(*axis, Axis::Vertical);
                assert!((ratio - 0.3).abs() < 1e-6);
            }
            PaneNode::Leaf { .. } => panic!("the split has to survive"),
        }
        assert_eq!(
            machine.panes.iter().map(|p| p.id).collect::<Vec<_>>(),
            vec![1, 2],
            "the pane registry rides the same file"
        );
        assert_eq!(machine.panes[0].cwd.as_deref(), Some("/work"));
    }

    #[test]
    fn a_reopened_store_marks_every_pane_awaiting_revival() {
        let (store, dir) = store();
        let ws = store.workspace_create(None, None, None).unwrap();
        store
            .tab_create(ws.id, None, seed(7, "/work"), None, None)
            .unwrap();
        assert!(
            store.pane(7).unwrap().live,
            "the pane its own client just seeded is live"
        );

        let restarted = MachineStore::open(dir.path().join(MACHINE_FILE));
        let record = restarted.pane(7).expect("the record survives the restart");
        assert!(!record.live, "a restarted daemon has no live panes");
        assert_eq!(
            record.cwd.as_deref(),
            Some("/work"),
            "the facts a successor spawns from survive"
        );
        assert_eq!(
            restarted.workspace(ws.id).unwrap().tabs[0].root.pane_ids(),
            vec![7],
            "the leaf still names the dead pane: that is the revival slot"
        );
    }

    #[test]
    fn a_seed_for_an_already_dead_pane_registers_as_awaiting_revival() {
        let (store, _dir) = store();
        store.set_liveness_probe(Arc::new(|id| id == 1));
        let ws = store.workspace_create(None, None, None).unwrap();
        store
            .tab_create(ws.id, None, PaneSeed::bare(1), None, None)
            .unwrap();
        store
            .pane_split(
                ws.id,
                1,
                Axis::Vertical,
                0.5,
                PaneSeed::bare(2),
                false,
                None,
            )
            .unwrap();

        assert!(store.pane(1).unwrap().live, "the probe vouched for pane 1");
        assert!(
            !store.pane(2).unwrap().live,
            "pane 2 died before its adopting op; its record must be born revivable"
        );
    }

    #[test]
    fn replacing_a_dead_pane_rebinds_the_leaf_and_spends_the_record() {
        let (store, dir) = store();
        let ws = store.workspace_create(None, None, None).unwrap();
        store
            .tab_create(ws.id, None, seed(7, "/work"), None, None)
            .unwrap();

        let restarted = MachineStore::open(dir.path().join(MACHINE_FILE));
        let (_sub, heard) = recorded(&restarted);
        restarted
            .pane_replace(ws.id, 7, seed(42, "/work"), None)
            .unwrap();

        assert_eq!(
            restarted.workspace(ws.id).unwrap().tabs[0].root.pane_ids(),
            vec![42]
        );
        assert!(restarted.pane(7).is_none(), "the old record is spent");
        assert!(restarted.pane(42).unwrap().live);
        let heard = heard.lock().unwrap();
        assert_eq!(heard.len(), 1);
        match &heard[0].1 {
            LayoutDelta::TabRestructured { tab, pane } => {
                assert_eq!(tab.root.pane_ids(), vec![42]);
                assert_eq!(pane.as_ref().map(|p| p.id), Some(42));
            }
            other => panic!("expected TabRestructured, got {other:?}"),
        }
    }

    #[test]
    fn workspace_create_rename_touch_delete_land_and_broadcast() {
        let (store, _dir) = store();
        let (_sub, heard) = recorded(&store);

        let ws = store
            .workspace_create(None, Some("api".into()), None)
            .unwrap();
        store
            .workspace_rename(ws.id, Some("web".into()), None)
            .unwrap();
        store.workspace_touch(ws.id, None).unwrap();
        assert_eq!(store.workspace(ws.id).unwrap().name.as_deref(), Some("web"));

        store.workspace_delete(ws.id, None).unwrap();
        assert!(store.workspace(ws.id).is_err());

        let heard = heard.lock().unwrap();
        let kinds: Vec<&LayoutDelta> = heard.iter().map(|(_, d)| d).collect();
        assert!(matches!(kinds[0], LayoutDelta::WorkspaceCreated { .. }));
        assert!(matches!(kinds[1], LayoutDelta::WorkspaceRenamed { name: Some(n) } if n == "web"));
        assert!(matches!(kinds[2], LayoutDelta::WorkspaceTouched { .. }));
        assert!(matches!(kinds[3], LayoutDelta::WorkspaceDeleted));
        assert!(
            heard.iter().all(|(key, _)| key == &ws.id.to_string()),
            "every delta names the workspace it is about"
        );
    }

    #[test]
    fn deleting_a_workspace_forgets_the_panes_only_it_referenced() {
        let (store, _dir, ws, _tab) = store_with_tab();
        let dropped = store.workspace_delete(ws, None).unwrap();
        assert_eq!(dropped, vec![1], "the caller is told which PTYs to kill");
        assert!(store.pane(1).is_none());
    }

    #[test]
    fn a_created_tab_lands_at_its_position_and_becomes_active() {
        let (store, _dir, ws, first) = store_with_tab();
        let second = store
            .tab_create(ws, None, seed(2, "/b"), None, None)
            .unwrap();
        let between = store
            .tab_create(ws, Some(1), seed(3, "/c"), None, None)
            .unwrap();

        let workspace = store.workspace(ws).unwrap();
        let order: Vec<TabId> = workspace.tabs.iter().map(|t| t.id).collect();
        assert_eq!(order, vec![first.id, between.id, second.id]);
        assert_eq!(workspace.active_tab, Some(between.id));

        let clamped = store
            .tab_create(ws, Some(99), seed(4, "/d"), None, None)
            .unwrap();
        assert_eq!(
            store.workspace(ws).unwrap().tabs.last().unwrap().id,
            clamped.id
        );
    }

    #[test]
    fn closing_a_tab_forgets_its_panes_and_heals_the_active_tab() {
        let (store, _dir, ws, first) = store_with_tab();
        let second = store
            .tab_create(ws, None, seed(2, "/b"), None, None)
            .unwrap();
        store.workspace_set_active_tab(ws, second.id, None).unwrap();

        let (_sub, heard) = recorded(&store);
        let dropped = store.tab_close(ws, second.id, None).unwrap();
        assert_eq!(dropped, vec![2]);
        let workspace = store.workspace(ws).unwrap();
        assert_eq!(workspace.tabs.len(), 1);
        assert_eq!(
            workspace.active_tab,
            Some(first.id),
            "the active tab may not dangle on a closed id"
        );
        assert!(
            matches!(
                heard.lock().unwrap().as_slice(),
                [
                    (_, LayoutDelta::TabClosed { tab }),
                    (_, LayoutDelta::ActiveTabChanged { tab: active })
                ] if *tab == second.id && *active == first.id
            ),
            "heard {:?}",
            heard.lock().unwrap()
        );

        heard.lock().unwrap().clear();
        let dropped = store.tab_close(ws, first.id, None).unwrap();
        assert_eq!(dropped, vec![1]);
        assert_eq!(
            store.workspace(ws).unwrap().active_tab,
            None,
            "a workspace with no tabs has no active one — the home-page state"
        );
        assert_eq!(
            heard.lock().unwrap().len(),
            1,
            "losing the last tab needs no ActiveTabChanged: no tabs, no active tab"
        );
    }

    #[test]
    fn tabs_rename_move_and_regroup_in_place() {
        let (store, _dir, ws, first) = store_with_tab();
        let second = store
            .tab_create(ws, None, seed(2, "/b"), None, None)
            .unwrap();

        store
            .tab_rename(ws, first.id, Some("build".into()), None)
            .unwrap();
        store
            .tab_set_group(ws, first.id, Some("/repo/tty7".into()), None)
            .unwrap();
        store.tab_move(ws, first.id, 1, None).unwrap();

        let workspace = store.workspace(ws).unwrap();
        assert_eq!(workspace.tabs[0].id, second.id);
        assert_eq!(workspace.tabs[1].name.as_deref(), Some("build"));
        assert_eq!(
            workspace.tabs[1].sidebar_group.as_deref(),
            Some("/repo/tty7")
        );
    }

    #[test]
    fn splitting_and_closing_panes_reshapes_the_tree() {
        let (store, _dir, ws, tab) = store_with_tab();
        store
            .pane_split(ws, 1, Axis::Horizontal, 0.5, seed(2, "/b"), false, None)
            .unwrap();
        store
            .pane_split(ws, 2, Axis::Vertical, 0.5, seed(3, "/c"), true, None)
            .unwrap();
        assert_eq!(
            store.workspace(ws).unwrap().tabs[0].root.pane_ids(),
            vec![1, 3, 2],
            "`first` puts the new pane on the a side"
        );

        let dropped = store.pane_close(ws, 3, None).unwrap();
        assert_eq!(dropped, vec![3]);
        assert_eq!(
            store.workspace(ws).unwrap().tabs[0].root.pane_ids(),
            vec![1, 2]
        );

        store.pane_close(ws, 2, None).unwrap();
        assert!(matches!(
            store.workspace(ws).unwrap().tabs[0].root,
            PaneNode::Leaf { pane: 1 }
        ));

        let (_sub, heard) = recorded(&store);
        store.pane_close(ws, 1, None).unwrap();
        assert!(store.workspace(ws).unwrap().tabs.is_empty());
        assert!(matches!(
            heard.lock().unwrap()[0].1,
            LayoutDelta::TabClosed { tab: id } if id == tab.id
        ));
    }

    #[test]
    fn a_ratio_change_lands_on_the_split_its_path_names() {
        let (store, _dir, ws, tab) = store_with_tab();
        store
            .pane_split(ws, 1, Axis::Horizontal, 0.5, seed(2, "/b"), false, None)
            .unwrap();
        store
            .pane_split(ws, 2, Axis::Vertical, 0.5, seed(3, "/c"), false, None)
            .unwrap();

        store
            .pane_set_ratio(ws, tab.id, vec![Side::B], 0.7, None)
            .unwrap();
        match &store.workspace(ws).unwrap().tabs[0].root {
            PaneNode::Split { a, b, ratio, .. } => {
                assert!((ratio - 0.5).abs() < 1e-6, "the root ratio is untouched");
                assert!(matches!(&**a, PaneNode::Leaf { pane: 1 }));
                match &**b {
                    PaneNode::Split { ratio, .. } => assert!((ratio - 0.7).abs() < 1e-6),
                    PaneNode::Leaf { .. } => panic!("the nested split is gone"),
                }
            }
            PaneNode::Leaf { .. } => panic!("the root split is gone"),
        }

        let err = store
            .pane_set_ratio(ws, tab.id, vec![Side::A], 0.6, None)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        store
            .pane_set_ratio(ws, tab.id, vec![], 0.0001, None)
            .unwrap();
        match &store.workspace(ws).unwrap().tabs[0].root {
            PaneNode::Split { ratio, .. } => assert!(*ratio >= 0.05),
            PaneNode::Leaf { .. } => unreachable!(),
        }
    }

    #[test]
    fn moving_a_pane_between_tabs_dissolves_an_emptied_tab() {
        let (store, _dir, ws, first) = store_with_tab();
        let second = store
            .tab_create(ws, None, seed(2, "/b"), None, None)
            .unwrap();

        store
            .pane_move(ws, 2, 1, Axis::Vertical, false, None)
            .unwrap();
        let workspace = store.workspace(ws).unwrap();
        assert_eq!(workspace.tabs.len(), 1, "the emptied tab dissolved");
        assert_eq!(workspace.tabs[0].id, first.id);
        assert_eq!(workspace.tabs[0].root.pane_ids(), vec![1, 2]);
        assert!(
            !workspace.tabs.iter().any(|t| t.id == second.id),
            "the source tab is gone"
        );
        assert!(store.pane(2).is_some(), "the pane moved; it did not die");

        let err = store
            .pane_move(ws, 2, 2, Axis::Vertical, false, None)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_refused_operation_changes_nothing_and_notifies_nobody() {
        let (store, _dir, ws, tab) = store_with_tab();
        let before = store.machine();
        let (_sub, heard) = recorded(&store);

        let missing = WorkspaceId::new();
        assert!(store.workspace_rename(missing, None, None).is_err());
        assert!(
            store
                .tab_create(missing, None, seed(9, "/x"), None, None)
                .is_err()
        );
        assert!(store.tab_close(ws, TabId::new(), None).is_err());
        assert!(
            store
                .pane_split(ws, 999, Axis::Vertical, 0.5, seed(9, "/x"), false, None)
                .is_err()
        );
        assert!(store.pane_close(ws, 999, None).is_err());
        assert!(
            store
                .pane_set_ratio(ws, tab.id, vec![Side::A], 0.5, None)
                .is_err()
        );
        assert!(
            store
                .pane_set_ratio(ws, tab.id, vec![], f32::NAN, None)
                .is_err()
        );
        assert!(store.pane_replace(ws, 999, seed(9, "/x"), None).is_err());

        assert_eq!(store.machine(), before);
        assert!(heard.lock().unwrap().is_empty());
        assert!(
            store.pane(9).is_none(),
            "a seed on a refused op must not leak into the registry"
        );
    }

    #[test]
    fn a_delta_reaches_every_subscriber_but_its_author() {
        let (store, _dir, ws, _tab) = store_with_tab();
        let (author, heard_by_author) = recorded(&store);
        let (_other, heard_by_other) = recorded(&store);

        store
            .workspace_rename(ws, Some("renamed".into()), Some(author.id()))
            .unwrap();
        assert!(heard_by_author.lock().unwrap().is_empty());
        assert_eq!(heard_by_other.lock().unwrap().len(), 1);

        store.workspace_rename(ws, None, None).unwrap();
        assert_eq!(heard_by_author.lock().unwrap().len(), 1);
        assert_eq!(heard_by_other.lock().unwrap().len(), 2);
    }

    #[test]
    fn dropping_a_subscription_stops_the_deltas() {
        let (store, _dir, ws, _tab) = store_with_tab();
        let (sub, heard) = recorded(&store);
        store.workspace_touch(ws, None).unwrap();
        assert_eq!(heard.lock().unwrap().len(), 1);
        drop(sub);
        store.workspace_touch(ws, None).unwrap();
        assert_eq!(heard.lock().unwrap().len(), 1);
    }

    #[test]
    fn pane_facts_update_the_record_and_reach_every_client() {
        let (store, _dir, ws, _tab) = store_with_tab();
        let (sub, heard) = recorded(&store);

        store.note_pane_facts(1, |p| {
            p.cwd = Some("/work/deeper".into());
        });
        let record = store.pane(1).unwrap();
        assert_eq!(record.cwd.as_deref(), Some("/work/deeper"));
        {
            let heard = heard.lock().unwrap();
            assert_eq!(heard.len(), 1);
            assert_eq!(heard[0].0, ws.to_string());
            assert!(matches!(&heard[0].1, LayoutDelta::PaneFacts { pane } if pane.id == 1));
        }

        store.note_pane_facts(1, |_| {});
        store.note_pane_facts(999, |p| p.cwd = Some("/ghost".into()));
        assert_eq!(heard.lock().unwrap().len(), 1);
        drop(sub);
    }

    #[test]
    fn a_pane_already_in_the_tree_cannot_be_adopted_again() {
        let (store, _dir, ws, _tab) = store_with_tab();
        store.note_pane_facts(1, |p| p.cwd = Some("/observed".into()));

        let other = store.workspace_create(None, None, None).unwrap();
        let err = store
            .tab_create(other.id, None, seed(1, "/stale"), None, None)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(
            store.workspace(other.id).unwrap().tabs.is_empty(),
            "the refused tab must not half-exist"
        );
        assert_eq!(
            store.pane(1).unwrap().cwd.as_deref(),
            Some("/observed"),
            "and the stale seed must not clobber the daemon's own facts"
        );
        let _ = ws;
    }

    #[test]
    fn published_observations_land_in_the_installed_store() {
        let _slot = OBSERVE_SLOT.lock().unwrap_or_else(|e| e.into_inner());
        observe_pane(1, |p| p.cwd = Some("/nowhere".into()));

        let (store, _dir, _ws, _tab) = store_with_tab();
        publish_observations(&store);
        observe_pane(1, |p| p.cwd = Some("/observed/here".into()));
        assert_eq!(
            store.pane(1).unwrap().cwd.as_deref(),
            Some("/observed/here")
        );
        withdraw_observations();
    }

    #[test]
    fn attachments_takeover_and_are_never_persisted() {
        let (store, dir, ws, _tab) = store_with_tab();
        assert_eq!(store.attachment(ws), None);

        let laptop = Attachment::new("tok-1", "laptop");
        assert_eq!(store.attach(ws, laptop.clone()), None);
        let desktop = Attachment::new("tok-2", "desktop");
        assert_eq!(store.attach(ws, desktop.clone()), Some(laptop.clone()));

        assert!(!store.detach(ws, &laptop.token));
        assert_eq!(store.attachment(ws).unwrap().hostname, "desktop");
        assert!(store.detach(ws, &desktop.token));
        assert_eq!(store.attachment(ws), None);

        store.attach(ws, Attachment::new("secret-token", "laptop"));
        store
            .workspace_rename(ws, Some("web".into()), None)
            .unwrap();
        let text = std::fs::read_to_string(dir.path().join(MACHINE_FILE)).unwrap();
        assert!(!text.contains("secret-token"), "{text}");
        assert_eq!(
            MachineStore::open(dir.path().join(MACHINE_FILE)).attachment(ws),
            None
        );
    }

    #[test]
    fn an_attachment_travels_by_name_and_never_by_token() {
        let (store, _dir, ws, _tab) = store_with_tab();
        store.attach(ws, Attachment::new("secret-token", "laptop"));

        // What `MachineGet` hands a peer: `tty7 ls` reads its ATTACHED column
        // out of this, so a held workspace has to say so here.
        let wire = serde_json::to_string(&store.machine()).unwrap();
        assert!(wire.contains("laptop"), "{wire}");
        assert!(!wire.contains("secret-token"), "{wire}");

        let seen: Machine = serde_json::from_str(&wire).unwrap();
        let held = seen.workspaces.iter().find(|w| w.id == ws).unwrap();
        assert_eq!(held.attachment.as_ref().unwrap().hostname, "laptop");
        assert!(held.attachment.as_ref().unwrap().token.is_empty());
    }

    #[test]
    fn an_attachment_dies_with_its_workspace() {
        let (store, _dir, ws, _tab) = store_with_tab();
        store.attach(ws, Attachment::new("tok", "laptop"));
        assert!(store.attachment(ws).is_some());
        store.workspace_delete(ws, None).unwrap();
        assert_eq!(store.attachment(ws), None);
    }

    #[test]
    fn the_default_path_ends_at_the_documented_file() {
        match default_machine_path() {
            Ok(path) => assert_eq!(
                path.file_name().and_then(|n| n.to_str()),
                Some(MACHINE_FILE)
            ),
            Err(e) => assert!(e.to_string().contains(DATA_DIR_ENV)),
        }
    }

    /// The invariant the whole file rests on: one config directory is one
    /// instance, tree included.
    ///
    /// While the tree resolved from `HOME` alone, `--config-dir` moved the
    /// lock, both sockets, `views.json` and the scrollback but left the tree
    /// behind, so two tty7s that each believed they were the only server on
    /// this machine wrote one `machine.json` between them — whole-document
    /// writes, last one wins, and the workspaces the loser knew about were
    /// gone.
    #[test]
    fn the_tree_lives_in_the_config_directory() {
        if std::env::var_os(DATA_DIR_ENV).is_some() {
            return;
        }
        // Against whatever the config directory resolves to, not against the
        // path this call passed in: `set_config_dir` is first-wins and
        // process-wide, so the pin only guarantees *a* scratch directory is in
        // force — which test put it there depends on the order they ran in.
        let _ = crate::core::session::test_support::pin_config_dir();
        let config_dir = crate::core::config::config_dir_path();
        assert!(config_dir.is_some(), "the pin puts one in force");
        assert_eq!(
            default_machine_path().unwrap().parent(),
            config_dir.as_deref(),
            "a config directory nobody else names must not share anybody's tree"
        );
    }

    /// The other half of "one config directory is one instance": there is one
    /// legacy tree, so exactly one instance may inherit it, and start order
    /// must not be what picks. A second tty7 renaming the tree into its own
    /// directory leaves the primary owning nothing — the same bug, one upgrade
    /// later.
    #[test]
    fn only_the_machines_own_instance_inherits_the_legacy_tree() {
        let machines = PathBuf::from("/home/u/.config/tty7");
        assert!(is_the_machines_instance(
            Some(&machines),
            Some(&machines.clone())
        ));
        assert!(
            !is_the_machines_instance(Some(Path::new("/tmp/scratch")), Some(&machines)),
            "an instance pointed somewhere of its own starts on an empty tree"
        );
        assert!(
            !is_the_machines_instance(None, Some(&machines)),
            "nowhere to put it is not a licence to move it"
        );
        assert!(!is_the_machines_instance(Some(&machines), None));
    }

    #[test]
    fn an_instance_of_its_own_leaves_the_machines_tree_where_it_is() {
        let old = tempfile::TempDir::new().unwrap();
        let new = tempfile::TempDir::new().unwrap();
        let legacy = old.path().join(MACHINE_FILE);
        std::fs::write(&legacy, br#"{"workspaces":[],"panes":[]}"#).unwrap();

        adopt_into(Some(new.path()), Some(old.path()), false);

        assert!(
            legacy.exists(),
            "a --config-dir instance that takes the tree hands the primary an \
             empty one, which is the failure this change exists to stop"
        );
        assert!(!new.path().join(MACHINE_FILE).exists());
    }

    #[test]
    fn an_upgrade_carries_the_appearance_hint_along_with_the_tree() {
        let old = tempfile::TempDir::new().unwrap();
        let new = tempfile::TempDir::new().unwrap();
        for file in [MACHINE_FILE, APPEARANCE_FILE] {
            std::fs::write(old.path().join(file), b"{}").unwrap();
        }

        adopt_into(Some(new.path()), Some(old.path()), true);

        for file in [MACHINE_FILE, APPEARANCE_FILE] {
            assert!(new.path().join(file).exists(), "{file} was left behind");
            assert!(!old.path().join(file).exists(), "{file} was not moved");
        }
    }

    /// `TTY7_DATA_DIR` answers both getters, so the sandboxes every test
    /// harness pins with it must come out the far side untouched.
    #[test]
    fn one_directory_under_two_names_is_not_a_migration() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join(MACHINE_FILE);
        std::fs::write(&file, b"live").unwrap();

        adopt_into(Some(dir.path()), Some(dir.path()), true);

        assert_eq!(std::fs::read(&file).unwrap(), b"live");
    }

    /// The cross-filesystem path, where `rename` refuses and copying is the
    /// only way over. It may add a file and it may not replace one.
    #[test]
    fn a_copied_tree_never_lands_on_one_already_there() {
        let dir = tempfile::TempDir::new().unwrap();
        let (from, to) = (dir.path().join("from"), dir.path().join("to"));
        std::fs::write(&from, b"carried").unwrap();

        copy_new(&from, &to).expect("nothing is there yet");
        assert_eq!(std::fs::read(&to).unwrap(), b"carried");

        let second = copy_new(&from, &to).expect_err("the second must not replace the first");
        assert_eq!(second.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&to).unwrap(), b"carried");
    }

    /// A rename that failed because somebody else already carried the file
    /// over is the one benign race, and it must not turn into a copy — there
    /// is nothing left to copy, and the winner's file is what was wanted.
    #[test]
    fn a_source_already_carried_over_is_not_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let (legacy, current) = (dir.path().join("legacy"), dir.path().join("current"));
        std::fs::write(&current, b"the winner's").unwrap();

        adopt_by_copy(&legacy, &current, &io::Error::from(io::ErrorKind::NotFound));

        assert_eq!(std::fs::read(&current).unwrap(), b"the winner's");
    }

    #[test]
    fn an_upgrade_carries_the_legacy_tree_forward() {
        let old = tempfile::TempDir::new().unwrap();
        let new = tempfile::TempDir::new().unwrap();
        let (legacy, current) = (old.path().join(MACHINE_FILE), new.path().join(MACHINE_FILE));
        std::fs::write(&legacy, br#"{"workspaces":[],"panes":[]}"#).unwrap();

        adopt_legacy_file(&legacy, &current);

        assert!(
            current.exists(),
            "moving the path without the tree would lose every workspace at the \
             moment of upgrade, which is the failure this change exists to stop"
        );
        assert!(
            !legacy.exists(),
            "a move leaves nothing to be adopted twice"
        );
    }

    /// The stale-overwrite case, and the reason the guard is "the destination
    /// exists" rather than "the source is newer": a build from before the move,
    /// run once since, rewrites the legacy path with a tree that predates
    /// everything done since. Adopting that over the live file would hand the
    /// old tree back — a slower version of the bug, not a fix for it.
    #[test]
    fn a_legacy_file_never_overwrites_the_tree_in_use() {
        let old = tempfile::TempDir::new().unwrap();
        let new = tempfile::TempDir::new().unwrap();
        let (legacy, current) = (old.path().join(MACHINE_FILE), new.path().join(MACHINE_FILE));
        std::fs::write(&legacy, b"stale").unwrap();
        std::fs::write(&current, b"live").unwrap();

        adopt_legacy_file(&legacy, &current);

        assert_eq!(std::fs::read(&current).unwrap(), b"live");
        assert!(
            legacy.exists(),
            "the one it did not adopt is not the one it may delete"
        );
    }

    #[test]
    fn nothing_to_adopt_creates_nothing() {
        let old = tempfile::TempDir::new().unwrap();
        let new = tempfile::TempDir::new().unwrap();
        let current = new.path().join(MACHINE_FILE);

        adopt_legacy_file(&old.path().join(MACHINE_FILE), &current);

        assert!(
            !current.exists(),
            "a fresh install has nothing to carry forward and must not be handed an empty tree"
        );
    }

    #[test]
    fn an_observation_is_broadcast_at_once_and_written_a_little_later() {
        let (store, dir, ws, _tab) = store_with_tab();
        let path = dir.path().join(MACHINE_FILE);
        let (_sub, heard) = recorded(&store);

        store.note_pane_facts(1, |p| p.cwd = Some("/work/deeper".into()));
        assert_eq!(
            heard.lock().unwrap().len(),
            1,
            "the client hears the fact immediately; only the disk waits"
        );
        assert!(
            !std::fs::read_to_string(&path).unwrap().contains("deeper"),
            "an observation must not write the document synchronously"
        );

        store.flush();
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("deeper"),
            "…and the flush is what puts it on disk"
        );
        let before = std::fs::metadata(&path).unwrap().len();
        store.flush();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), before);

        store.note_pane_facts(1, |p| p.cwd = Some("/work/deepest".into()));
        store
            .workspace_rename(ws, Some("web".into()), None)
            .unwrap();
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("deepest"),
            "a structural write carries whatever the facts left unwritten"
        );
    }

    #[test]
    fn a_structural_edit_is_on_disk_before_anyone_hears_about_it() {
        let (store, dir) = store();
        let path = dir.path().join(MACHINE_FILE);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let path_in_callback = path.clone();
        let _sub = store.subscribe(Arc::new(move |_ws: &str, _delta: &LayoutDelta| {
            sink.lock()
                .unwrap()
                .push(std::fs::read_to_string(&path_in_callback).unwrap_or_default());
        }));

        let ws = store
            .workspace_create(None, Some("api".into()), None)
            .unwrap();
        let _ = ws;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(
            seen[0].contains("api"),
            "the delta arrived before the file said so: {}",
            seen[0]
        );
    }

    #[test]
    fn a_corrupt_file_is_quarantined_rather_than_overwritten() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(MACHINE_FILE);
        std::fs::write(&path, b"{ this is not json").unwrap();

        let store = MachineStore::open(&path);
        assert!(store.machine().workspaces.is_empty());
        store.workspace_create(None, None, None).unwrap();
        let aside = std::fs::read_to_string(path.with_extension("json.corrupt")).unwrap();
        assert_eq!(aside, "{ this is not json");

        std::fs::write(&path, b"corrupt again").unwrap();
        let store = MachineStore::open(&path);
        store.workspace_create(None, None, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(path.with_extension("json.corrupt")).unwrap(),
            "{ this is not json",
            "the first rescue copy is still the first one"
        );
        assert_eq!(
            std::fs::read_to_string(path.with_extension("json.corrupt.1")).unwrap(),
            "corrupt again"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_document_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let (store, dir) = store();
        store.workspace_create(None, None, None).unwrap();
        let mode = std::fs::metadata(dir.path().join(MACHINE_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_moved_aside_rather_than_overwritten() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(MACHINE_FILE);
        std::fs::write(&path, b"{\"workspaces\":[]}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_to_string(&path).is_ok() {
            return;
        }

        let store = MachineStore::open(&path);
        store.workspace_create(None, None, None).unwrap();

        let aside = path.with_extension("json.corrupt");
        assert!(aside.exists(), "the unreadable original must be kept");
        std::fs::set_permissions(&aside, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&aside).unwrap(),
            "{\"workspaces\":[]}",
            "moved aside byte-for-byte, ready for a hand repair"
        );
    }

    #[test]
    fn a_sparse_document_decodes_with_defaults() {
        let machine: Machine =
            serde_json::from_str(r#"{"workspaces":[{"tabs":[{"root":{"Leaf":{"pane":3}}}]}]}"#)
                .expect("missing fields default rather than fail");
        assert_eq!(machine.workspaces.len(), 1);
        assert_eq!(machine.workspaces[0].tabs[0].root.pane_ids(), vec![3]);
        assert!(machine.panes.is_empty());
    }
}

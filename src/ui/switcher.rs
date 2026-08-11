use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, MouseButton, MouseDownEvent, Subscription,
    Window, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use tty7_core::core::machine::TabId;
use tty7_core::core::session::{RemoteTarget, RouteSnapshot, WorkspaceId};

use crate::core::actions::{SwitcherAcross, SwitcherAcrossBack};
use crate::core::session::WorkspaceStore;
use crate::daemon::install::InstallPhase;
use crate::terminal::pane_liveness::Liveness;
use crate::ui::app::Tty7App;
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::remote_connect::{self, HostChoice, RemoteWorkspaceRow, human_bytes};
use crate::ui::remote_workspace::{ConnectFlow, MachineStatus, RemoteLinks};

const CARD_W: f32 = 840.0;

const LEFT_W: f32 = 340.0;

const CARD_TOP: f32 = 120.0;

/// Breathing room the card keeps from the window edge, and the height its own
/// search row and footer take on top of the body.
const CARD_MARGIN: f32 = 24.0;
const CARD_CHROME_H: f32 = 84.0;

const BODY_H: f32 = 420.0;

const ROW_AVATAR: f32 = 20.0;

const ROW_H: f32 = 32.0;
const HOST_H: f32 = 34.0;

const GUTTER: f32 = 26.0;

const ICON: f32 = 16.0;

const KID_INDENT: f32 = 16.0;

const RAIL_X: f32 = ROW_PAD + GUTTER / 2.;

const ROW_PAD: f32 = 8.0;

const TAB_PATH_W: f32 = 160.0;

const PROGRESS_H: f32 = 3.0;

/// `Failed` stays a unit variant so `Link` can be `Copy` and travel by value in
/// `GroupRef`; what went wrong rides in `Group::error` instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    Local,
    Connected,
    Connecting,
    /// The supervisor is retrying this machine on its backoff. Distinct from
    /// `Offline` because the group still has rows worth showing and Disconnect
    /// still has something to call off.
    Reconnecting {
        attempt: u32,
    },
    Failed,
    Offline,
}

/// Whether Disconnect has anything to stop. A machine the supervisor is still
/// retrying has no live link, but calling the retry off is exactly what the
/// verb is for, so it has to stay enabled.
fn link_is_engaged(link: Link) -> bool {
    matches!(link, Link::Connected | Link::Reconnecting { .. })
}

/// How one machine's row is drawn, from the two things that know.
///
/// This window's own `connect` still comes first — it is the attempt the user
/// is watching, and it is the only one that can be Failed with a reason on
/// screen. Everything after it is the supervisor's, which is the only view that
/// survives the window that started the connect. Reading the `HostLinks` table
/// alone, as this used to, cannot tell a machine that is being retried right
/// now from one nobody has ever connected to: the pump drops the entry the
/// moment the link dies, and the group then collapses as if it were empty.
fn link_from(
    connect: Option<&ConnectFlow>,
    target: &RemoteTarget,
    supervised: Option<&MachineStatus>,
    has_link: bool,
) -> Link {
    match connect {
        Some(ConnectFlow::Connecting { choice }) if &choice.target == target => {
            return Link::Connecting;
        }
        Some(ConnectFlow::Failed { choice, .. }) if &choice.target == target => {
            return Link::Failed;
        }
        _ => {}
    }
    match supervised {
        Some(MachineStatus::Connecting) => Link::Connecting,
        Some(MachineStatus::Attached) => Link::Connected,
        Some(MachineStatus::Reconnecting { attempt, .. }) => {
            Link::Reconnecting { attempt: *attempt }
        }
        Some(MachineStatus::Failed(_)) => Link::Failed,
        None if has_link => Link::Connected,
        None => Link::Offline,
    }
}

struct Group {
    key: String,
    label: String,
    endpoint: String,
    target: Option<RemoteTarget>,
    link: Link,
    home: Option<PathBuf>,
    error: Option<String>,
    installing: Option<InstallPhase>,
    /// Another client is holding at least one workspace of this machine. The
    /// link itself is fine, so nothing in `link` would ever say so.
    preempted: bool,
    /// The route behind this group no longer resolves — its profile was
    /// deleted or its alias left the ssh config (#485). Parked groups are
    /// not retried; the group shows the snapshot label and offers to forget
    /// its entries instead of a retry button that could never succeed.
    parked: bool,
    rows: Vec<Row>,
}

struct Row {
    id: WorkspaceId,
    name: String,
    path: String,
    when: String,
    live: Liveness,
    open: bool,
    current: bool,
    preempted: bool,
    adopt: Option<Box<RemoteWorkspaceRow>>,
    remote_id: Option<WorkspaceId>,
    tabs: Vec<TabRow>,
}

/// One tab in the right-hand column. Built once per frame for every workspace
/// on the left, so the search can match tab names and the column can render
/// without a second pass over the machine tree.
#[derive(Clone)]
struct TabRow {
    id: TabId,
    /// Position in the owning workspace's tab order — what `activate` wants.
    index: usize,
    label: String,
    path: String,
    /// Whether `label` is a name someone gave the tab. When it is not, the
    /// label is already derived from the working directory and showing `path`
    /// next to it just prints the same place twice.
    named: bool,
    agent: Option<crate::core::cli_agent::CLIAgent>,
    status: Option<crate::core::cli_agent::AgentStatus>,
    unread: usize,
    ssh: Option<u32>,
    active: bool,
    /// Branch and diff counts, the same line the tab sidebar shows. Only this
    /// window's own tabs have it — the machine tree carries no git state.
    git: Option<tty7_core::core::git::GitStatus>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Column {
    Left,
    Right,
}

/// A selectable line in the left column. Rendering and keyboard navigation walk
/// the same list so an arrow key can never land somewhere the eye cannot see.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Nav {
    Host(usize),
    Row(usize, usize),
    OthersHeader,
    Other(usize),
}

pub(crate) struct HostSnapshot {
    pub target: RemoteTarget,
    pub rows: Vec<RemoteWorkspaceRow>,
}

pub(crate) struct Switcher {
    pub query: Entity<InputState>,
    collapsed: HashSet<String>,
    show_others: bool,
    renaming: Option<(WorkspaceId, Entity<InputState>)>,
    column: Column,
    left_sel: usize,
    right_sel: usize,
    /// Order the tab column most-recently-used first. Set when Ctrl+Tab opened
    /// the panel; the plain Cmd+Shift+O panel keeps strip order.
    mru: bool,
    /// The modifiers held down when Ctrl+Tab opened the panel. Releasing them
    /// commits the highlighted tab, IDEA-style.
    hold: Option<gpui::Modifiers>,
    left_scroll: gpui::ScrollHandle,
    right_scroll: gpui::ScrollHandle,
    /// Anchors on the two scrolls, worn by whichever row is selected. Both
    /// columns hold their rows inside one child element, and `scroll_to_item`
    /// indexes a scroll's *direct* children — so it could only ever find item
    /// 0, and walking the list with the arrows quietly left the selection off
    /// the bottom of the column.
    left_anchor: gpui::ScrollAnchor,
    right_anchor: gpui::ScrollAnchor,
    _subs: Vec<Subscription>,
}

impl Switcher {
    fn text(&self, cx: &App) -> String {
        self.query.read(cx).value().trim().to_lowercase()
    }

    pub(crate) fn expand(&mut self, key: &str) {
        self.collapsed.remove(key);
    }
}

/// Everything the panel needs for one frame: the groups, which of their rows
/// survived the search, and the flattened left column.
struct Layout {
    groups: Vec<Group>,
    /// Per group, the row indices the search left visible. `None` hides the
    /// whole group.
    shown: Vec<Option<Vec<usize>>>,
    others: Vec<HostChoice>,
    other_hits: Vec<usize>,
    others_expanded: bool,
    nav: Vec<Nav>,
}

impl Layout {
    /// Which workspace row the tab column is showing. A host header borrows its
    /// group's first workspace so walking past a header does not blank the
    /// column.
    fn subject(&self, sel: usize) -> Option<(usize, usize)> {
        match self.nav.get(sel)? {
            Nav::Row(g, r) => Some((*g, *r)),
            Nav::Host(g) => self.shown[*g]
                .as_ref()
                .and_then(|rows| rows.first())
                .map(|r| (*g, *r)),
            _ => None,
        }
    }

    fn subject_row(&self, sel: usize) -> Option<&Row> {
        let (g, r) = self.subject(sel)?;
        self.groups[g].rows.get(r)
    }
}

impl Tty7App {
    pub(crate) fn toggle_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.is_some() {
            self.close_switcher(window, cx);
        } else {
            self.open_switcher(window, cx);
        }
    }

    pub(crate) fn open_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_switcher_in(Column::Left, false, None, window, cx);
    }

    fn open_switcher_in(
        &mut self,
        column: Column,
        mru: bool,
        hold: Option<gpui::Modifiers>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        remote_connect::register(cx);
        remote_connect::sweep_wsl(cx);
        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::ui::i18n::t(
                crate::ui::i18n::L10nKey::SearchWorkspacesAndMachines,
            ))
        });
        query.update(cx, |state, cx| state.focus(window, cx));
        let subs = vec![cx.subscribe_in(
            &query,
            window,
            |this, _input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change) {
                    // A narrower list can strand the cursor past its end. Land
                    // it on the first workspace rather than a machine header so
                    // the tab column shows the hits straight away.
                    let layout = this.switcher_layout(cx);
                    let at = layout
                        .nav
                        .iter()
                        .position(|n| matches!(n, Nav::Row(..)))
                        .unwrap_or(0);
                    if let Some(sw) = this.switcher.as_mut() {
                        sw.left_sel = at;
                        sw.right_sel = 0;
                    }
                    cx.notify();
                }
            },
        )];
        let (left_scroll, right_scroll) = (gpui::ScrollHandle::new(), gpui::ScrollHandle::new());
        self.switcher = Some(Switcher {
            query,
            collapsed: HashSet::new(),
            show_others: false,
            renaming: None,
            column,
            left_sel: 0,
            right_sel: 0,
            mru,
            hold,
            left_scroll: left_scroll.clone(),
            right_scroll: right_scroll.clone(),
            left_anchor: gpui::ScrollAnchor::for_handle(left_scroll),
            right_anchor: gpui::ScrollAnchor::for_handle(right_scroll),
            _subs: subs,
        });
        // Park the left cursor on this window's own workspace so the tab column
        // opens on something useful.
        let layout = self.switcher_layout(cx);
        let here = self.workspace;
        if let Some(at) = layout.nav.iter().position(|item| match item {
            Nav::Row(g, r) => layout.groups[*g].rows[*r].id == here,
            _ => false,
        }) && let Some(sw) = self.switcher.as_mut()
        {
            sw.left_sel = at;
        }
        cx.notify();
    }

    /// Ctrl+Tab. The first press raises the panel on the tab column with the
    /// previously used tab already highlighted; further presses walk it. Holding
    /// the modifier keeps the panel up, releasing it commits — IDEA's gesture.
    ///
    /// With fewer than two tabs there is nothing to cycle, so the panel opens on
    /// the workspace column and *stays* — no hold, no commit-on-release. The
    /// gesture degrades into "open the switcher" rather than doing nothing.
    pub(crate) fn tab_switch(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.switcher.is_some() {
            let layout = self.switcher_layout(cx);
            self.switcher_step_right(&layout, forward, window, cx);
            return;
        }
        let n = self.tabs.len();
        let cycling = n >= 2;
        let held = window.modifiers();
        self.open_switcher_in(
            match cycling {
                true => Column::Right,
                false => Column::Left,
            },
            cycling,
            (cycling && held.modified()).then_some(held),
            window,
            cx,
        );
        if cycling && let Some(sw) = self.switcher.as_mut() {
            sw.right_sel = match forward {
                true => 1,
                false => n - 1,
            };
        }
        cx.notify();
    }

    /// Watches the modifiers while a Ctrl+Tab panel is up. Letting go of any
    /// part of the combination that raised it is the commit gesture.
    pub(crate) fn switcher_hold_changed(
        &mut self,
        now: &gpui::Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(hold) = self.switcher.as_ref().and_then(|sw| sw.hold) else {
            return;
        };
        if !now.modified() || !hold.is_subset_of(now) {
            self.switcher_commit_hold(window, cx);
        }
    }

    /// Called when the modifier that raised the panel comes back up.
    pub(crate) fn switcher_commit_hold(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.as_ref().and_then(|sw| sw.hold).is_none() {
            return;
        }
        let layout = self.switcher_layout(cx);
        self.switcher_confirm(&layout, false, window, cx);
        // Confirming a tab already closed the panel; anything else (an empty
        // column, a workspace with nothing in it) still has to come down.
        if self.switcher.is_some() {
            self.close_switcher(window, cx);
        }
    }

    /// Drops the hold without acting on it. The modifier release will never
    /// arrive at a window that is no longer focused, so the panel would
    /// otherwise sit there waiting forever.
    pub(crate) fn switcher_release_hold(&mut self, cx: &mut Context<Self>) {
        if let Some(sw) = self.switcher.as_mut()
            && sw.hold.take().is_some()
        {
            cx.notify();
        }
    }

    pub(crate) fn close_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.take().is_some() {
            if matches!(self.connect, Some(ConnectFlow::Failed { .. })) {
                self.connect = None;
            }
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn switcher_groups(&self, cx: &mut Context<Self>) -> Vec<Group> {
        let now = crate::ui::home::now_secs();
        let current = self.workspace;
        crate::terminal::pane_liveness::sweep(cx);

        let mut groups: Vec<Group> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        {
            let app: &App = cx;
            let store = WorkspaceStore::all(app);
            for w in &store.views {
                let (key, label, endpoint, target) = match w.host.as_ref() {
                    None => (
                        String::new(),
                        t(L10nKey::SwitcherThisComputer).to_string(),
                        String::new(),
                        None,
                    ),
                    Some(r) => {
                        let key = r.target.to_string();
                        // The live profile's own name overwrites this below
                        // while the route still resolves; when it does not,
                        // the snapshot — or, for entries saved before
                        // snapshots existed, a placeholder — is all the entry
                        // has. Never the bare profile UUID (#485).
                        let label = r.route_label(t(L10nKey::RemoteProfileGone));
                        let endpoint = r
                            .via
                            .as_ref()
                            .map(RouteSnapshot::endpoint)
                            .filter(|e| e != &label)
                            .unwrap_or_default();
                        (key, label, endpoint, Some(r.target.clone()))
                    }
                };
                let slot = *index.entry(key.clone()).or_insert_with(|| {
                    groups.push(Group {
                        key,
                        label,
                        endpoint,
                        target,
                        link: Link::Offline,
                        home: None,
                        error: None,
                        installing: None,
                        preempted: false,
                        parked: false,
                        rows: Vec::new(),
                    });
                    groups.len() - 1
                });
                groups[slot].rows.push(Row {
                    id: w.id,
                    name: crate::ui::machine_mirror::display_name(app, w)
                        .unwrap_or_else(|| t(L10nKey::WindowUntitled).to_string()),
                    path: crate::ui::machine_mirror::subject_path(app, w)
                        .map(|p| crate::ui::home::display_path(std::path::Path::new(&p)))
                        .unwrap_or_default(),
                    when: crate::ui::home::relative_time(now, w.last_active),
                    live: crate::terminal::pane_liveness::liveness_of(app, w),
                    open: w.open,
                    current: w.id == current,
                    preempted: false,
                    adopt: None,
                    remote_id: w.host.as_ref().map(|r| r.workspace),
                    tabs: self.tab_rows_for(w.id, app),
                });
            }
        }

        for target in self.pending_machines() {
            let key = target.to_string();
            if index.contains_key(&key) {
                continue;
            }
            index.insert(key.clone(), groups.len());
            groups.push(Group {
                label: key.clone(),
                key,
                endpoint: String::new(),
                target: Some(target),
                link: Link::Offline,
                home: None,
                error: None,
                installing: None,
                preempted: false,
                parked: false,
                rows: Vec::new(),
            });
        }

        if !index.contains_key("") {
            groups.insert(
                0,
                Group {
                    key: String::new(),
                    label: t(L10nKey::SwitcherThisComputer).to_string(),
                    endpoint: String::new(),
                    target: None,
                    link: Link::Offline,
                    home: None,
                    error: None,
                    installing: None,
                    preempted: false,
                    parked: false,
                    rows: Vec::new(),
                },
            );
        }

        // This window's own workspace has to be in the list even when the store
        // has not caught up with it: Ctrl+Tab reaches its tabs through the same
        // left column, and a missing row would leave the panel with nothing to
        // switch between.
        if !groups
            .iter()
            .any(|g| g.rows.iter().any(|r| r.id == current))
            && let Some(slot) = groups.iter().position(|g| g.key.is_empty())
        {
            let app: &App = cx;
            groups[slot].rows.insert(
                0,
                Row {
                    id: current,
                    name: crate::ui::machine_mirror::display_name_for(app, current)
                        .unwrap_or_else(|| t(L10nKey::WindowUntitled).to_string()),
                    path: String::new(),
                    when: crate::ui::home::relative_time(now, now),
                    live: Liveness::Alive,
                    open: true,
                    current: true,
                    preempted: false,
                    adopt: None,
                    remote_id: None,
                    tabs: self.tab_rows_for(current, app),
                },
            );
        }

        // Workspaces this machine holds that the store has never heard of: the
        // CLI makes them too, and one that never appears here looks to the
        // person who ran `tty7 new` like nothing happened at all. They open
        // like any other row — the id in the tree is the id a window claims.
        if let Some(slot) = groups.iter().position(|g| g.key.is_empty()) {
            // Measured against the rows already listed rather than against the
            // store, which is what put them there: the block above lists this
            // window's own workspace before the store has caught up with it,
            // and two rows under one id would be two ways into one window.
            let listed: Vec<WorkspaceId> = groups
                .iter()
                .flat_map(|g| g.rows.iter().map(|r| r.id))
                .collect();
            let app: &App = cx;
            let rows: Vec<Row> = crate::ui::machine_mirror::unclaimed_local_workspaces(app)
                .into_iter()
                .filter(|ws| !listed.contains(&ws.id))
                .map(|ws| Row {
                    id: ws.id,
                    name: ws.name,
                    path: ws
                        .path
                        .map(|p| crate::ui::home::display_path(std::path::Path::new(&p)))
                        .unwrap_or_default(),
                    when: crate::ui::home::relative_time(now, ws.last_active),
                    live: match ws.live {
                        true => Liveness::Alive,
                        false => Liveness::Stopped,
                    },
                    open: false,
                    current: false,
                    preempted: false,
                    adopt: None,
                    remote_id: None,
                    tabs: self.tab_rows_for(ws.id, app),
                })
                .collect();
            groups[slot].rows.extend(rows);
        }

        for group in &mut groups {
            group.rows.sort_by(|a, b| {
                b.current
                    .cmp(&a.current)
                    .then_with(|| b.open.cmp(&a.open))
                    .then_with(|| a.name.cmp(&b.name))
            });
        }
        groups.sort_by(|a, b| a.key.is_empty().cmp(&b.key.is_empty()).reverse());

        let configured = remote_connect::available_hosts(cx);
        for group in &mut groups {
            let Some(target) = group.target.clone() else {
                group.link = Link::Local;
                continue;
            };
            if let Some(known) = configured.iter().find(|h| h.target == target) {
                group.label = known.label.clone();
                if known.detail != known.label {
                    group.endpoint = known.detail.clone();
                }
            }
            let id = target.host_id();
            let supervised = RemoteLinks::machine_status(cx, id);
            group.link = self.link_state(&target, supervised.as_ref(), cx);
            // A route that no longer resolves parks the whole group (#485):
            // not retried, labeled from the snapshot, forget-not-retry for
            // actions. A live link is exempt — its panes keep working
            // regardless of what happened to the profile that made them.
            group.parked =
                group.link != Link::Connected && !remote_connect::route_resolvable(cx, &target);
            if let Some(ConnectFlow::Failed { choice, error }) = &self.connect
                && choice.target == target
            {
                group.error = Some(error.clone());
            }
            if group.error.is_none() {
                if let Some(error) = self.remote_host_errors.get(&target.to_string()) {
                    group.error = Some(error.clone());
                }
            }
            // The supervisor's own attempts never touch this window's `connect`,
            // so without this last fallback the route it could not build, and
            // the reconnect that has been failing for an hour, have nowhere at
            // all to be said.
            if group.error.is_none() {
                group.error = match supervised {
                    Some(MachineStatus::Failed(e)) => Some(e),
                    Some(MachineStatus::Reconnecting { last_error, .. }) => last_error,
                    _ => None,
                };
            }
            let taken: HashSet<WorkspaceId> =
                RemoteLinks::preempted_on(cx, id).into_iter().collect();
            group.preempted = !taken.is_empty();
            for row in &mut group.rows {
                row.preempted = taken.contains(&row.id);
            }
            let reported = remote_connect::install_progress_for(id);
            if group.link == Link::Connecting
                || matches!(group.link, Link::Reconnecting { .. })
                || group.error.is_some()
                || matches!(reported, Some(InstallPhase::Restarting))
            {
                group.installing = reported;
            }
            group.home = remote_connect::HostLinks::home(cx, id);
            if let Some(snapshot) = self.host_snapshots.get(&id) {
                group.merge(&snapshot.rows, now);
            }
        }
        groups
    }

    /// The tab column's rows for one workspace. This window's own workspace has
    /// live in-memory tabs (agent status, unread counts, MRU order); every other
    /// workspace comes out of the machine mirror, which is the only view this
    /// process has of windows it does not own.
    fn tab_rows_for(&self, id: WorkspaceId, cx: &App) -> Vec<TabRow> {
        if id == self.workspace {
            let order = match self.switcher.as_ref().is_some_and(|sw| sw.mru) {
                true => self.tabs_by_mru(),
                false => (0..self.tabs.len()).collect(),
            };
            return order
                .into_iter()
                .map(|i| {
                    let tab = &self.tabs[i];
                    TabRow {
                        id: tab.tree_id.get(),
                        index: i,
                        label: self.tab_label(tab, i, None, cx),
                        named: tab.name.as_deref().is_some_and(|n| !n.trim().is_empty())
                            || tab.agent(cx).is_some(),
                        path: tab
                            .pane
                            .terminals()
                            .first()
                            .and_then(|leaf| leaf.read(cx).cwd())
                            .map(|p| crate::ui::home::display_path(&p))
                            .unwrap_or_default(),
                        agent: tab.agent(cx),
                        status: tab.agent_status(cx),
                        unread: tab.agent_unread_count(cx),
                        ssh: self.tab_ssh_dot(tab, cx),
                        active: i == self.active,
                        git: tab.git_status(None, cx),
                    }
                })
                .collect();
        }

        let Some((views, active)) = crate::ui::machine_mirror::tab_views_for(cx, id) else {
            return Vec::new();
        };
        // Git state is cached globally per (host, cwd) and outlives the panes
        // that filled it, so a workspace this window recently left still has
        // its branches on hand. Read-only on purpose: probing every cwd of
        // every workspace to populate a panel that closes in a second would
        // cost a git invocation each, and a round trip each when the host is
        // remote.
        let host = WorkspaceStore::all(cx).get(id).map(|w| w.host_id());
        let git = |cwd: Option<&str>| -> Option<tty7_core::core::git::GitStatus> {
            let (host, cwd) = (host?, cwd?);
            cx.try_global::<crate::terminal::git_status::GitStatusCache>()?
                .status_for(host, std::path::Path::new(cwd))
        };
        views
            .into_iter()
            .enumerate()
            .map(|(i, v)| TabRow {
                label: tab_view_label(&v, i),
                // The label only stands in for the path when it came *from* the
                // path; a name or an agent leaves the location still worth
                // printing.
                named: v.name.as_deref().is_some_and(|n| !n.trim().is_empty()) || v.agent.is_some(),
                path: v
                    .cwd
                    .as_deref()
                    .map(|p| crate::ui::home::display_path(std::path::Path::new(p)))
                    .unwrap_or_default(),
                agent: v.agent,
                status: v.status,
                unread: 0,
                ssh: None,
                active: Some(v.id) == active,
                git: git(v.cwd.as_deref()),
                index: i,
                id: v.id,
            })
            .collect()
    }

    /// Builds one frame's worth of panel: the groups, what the search left
    /// visible, and the flattened left column the arrow keys walk.
    fn switcher_layout(&self, cx: &mut Context<Self>) -> Layout {
        let groups = self.switcher_groups(cx);
        let others = self.other_hosts(&groups, cx);
        let query = self
            .switcher
            .as_ref()
            .map(|sw| sw.text(cx))
            .unwrap_or_default();

        let mut shown: Vec<Option<Vec<usize>>> = Vec::with_capacity(groups.len());
        let mut nav: Vec<Nav> = Vec::new();
        for (g, group) in groups.iter().enumerate() {
            let matched_host = group.label.to_lowercase().contains(&query);
            let rows: Vec<usize> = group
                .rows
                .iter()
                .enumerate()
                .filter(|(_, r)| query.is_empty() || matched_host || r.matches(&query))
                .map(|(i, _)| i)
                .collect();
            if !query.is_empty() && !matched_host && rows.is_empty() {
                shown.push(None);
                continue;
            }
            nav.push(Nav::Host(g));
            let collapsed = self
                .switcher
                .as_ref()
                .is_some_and(|sw| sw.collapsed.contains(&group.key));
            let expanded = (!collapsed || !query.is_empty()) && group.link != Link::Offline;
            if expanded {
                nav.extend(rows.iter().map(|r| Nav::Row(g, *r)));
            }
            shown.push(Some(if expanded { rows } else { Vec::new() }));
        }

        let other_hits: Vec<usize> = match (others.is_empty(), query.is_empty()) {
            (true, _) => Vec::new(),
            (false, true) => (0..others.len()).collect(),
            (false, false) => {
                let hits = remote_connect::filter_hosts(&others, &query);
                others
                    .iter()
                    .enumerate()
                    .filter(|(_, h)| hits.iter().any(|x| x.target == h.target))
                    .map(|(i, _)| i)
                    .collect()
            }
        };
        let others_expanded = self.switcher.as_ref().is_some_and(|sw| sw.show_others)
            || (!query.is_empty() && !other_hits.is_empty());
        if !other_hits.is_empty() {
            nav.push(Nav::OthersHeader);
            if others_expanded {
                nav.extend(other_hits.iter().map(|i| Nav::Other(*i)));
            }
        }

        Layout {
            groups,
            shown,
            others,
            other_hits,
            others_expanded,
            nav,
        }
    }

    fn pending_machines(&self) -> Vec<RemoteTarget> {
        let mut out: Vec<RemoteTarget> = self
            .host_snapshots
            .values()
            .map(|s| s.target.clone())
            .collect();
        if let Some(choice) = self.connect.as_ref().and_then(ConnectFlow::choice) {
            out.push(choice.target.clone());
        }
        out
    }

    fn link_state(
        &self,
        target: &RemoteTarget,
        supervised: Option<&MachineStatus>,
        cx: &mut Context<Self>,
    ) -> Link {
        let has_link = remote_connect::HostLinks::get(cx, target.host_id()).is_some();
        link_from(self.connect.as_ref(), target, supervised, has_link)
    }

    fn other_hosts(&self, groups: &[Group], cx: &App) -> Vec<HostChoice> {
        let known: HashSet<&str> = groups.iter().map(|g| g.key.as_str()).collect();
        remote_connect::available_hosts(cx)
            .into_iter()
            .filter(|h| !known.contains(h.target.to_string().as_str()))
            .collect()
    }

    fn switcher_toggle_host(&mut self, group: &GroupRef, cx: &mut Context<Self>) {
        if group.link == Link::Offline
            && let Some(target) = group.target.clone()
        {
            let choice = HostChoice {
                target,
                label: group.label.clone(),
                detail: String::new(),
            };
            self.connect_to_host(choice, cx);
            if let Some(sw) = self.switcher.as_mut() {
                sw.collapsed.remove(&group.key);
            }
            return;
        }
        if let Some(sw) = self.switcher.as_mut() {
            if !sw.collapsed.remove(&group.key) {
                sw.collapsed.insert(group.key.clone());
            }
        }
        cx.notify();
    }

    fn switcher_open(
        &mut self,
        row: RowRef,
        new_window: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_switcher(window, cx);
        match row.adopt {
            Some((target, remote)) => self.open_remote_workspace(target, *remote, window, cx),
            None if new_window => crate::ui::windows::open(cx, Some(row.id)),
            None => self.reveal_workspace(row.id, window, cx),
        }
    }

    fn switcher_rename(&mut self, id: WorkspaceId, window: &mut Window, cx: &mut Context<Self>) {
        let current = crate::ui::machine_mirror::display_name_for(cx, id).unwrap_or_default();
        let input = Self::rename_box(current, window, cx);
        let sub = cx.subscribe_in(
            &input,
            window,
            move |this, _input, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    this.switcher_commit_rename(window, cx)
                }
                _ => {}
            },
        );
        if let Some(sw) = self.switcher.as_mut() {
            sw.renaming = Some((id, input));
            sw._subs.push(sub);
        }
        cx.notify();
    }

    fn switcher_commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, input)) = self.switcher.as_mut().and_then(|sw| sw.renaming.take()) else {
            return;
        };
        let value = input.read(cx).value().trim().to_string();
        crate::ui::tree_sync::rename_workspace(cx, id, (!value.is_empty()).then_some(value));
        crate::ui::windows::refresh_menu(cx);
        if id == self.workspace {
            self.sync_window_title(window, cx);
        }
        if let Some(sw) = self.switcher.as_ref() {
            sw.query.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    fn switcher_disconnect(&mut self, target: &RemoteTarget, cx: &mut Context<Self>) {
        crate::ui::remote_workspace::RemoteLinks::disconnect(cx, target.host_id());
        if self
            .connect
            .as_ref()
            .and_then(ConnectFlow::choice)
            .is_some_and(|c| &c.target == target)
        {
            self.connect = None;
        }
        cx.notify();
    }

    fn switcher_new(&mut self, group: &GroupRef, window: &mut Window, cx: &mut Context<Self>) {
        self.close_switcher(window, cx);
        match (group.target.clone(), group.home.clone()) {
            (Some(target), Some(home)) => self.create_remote_workspace(target, home, window, cx),
            (Some(_), None) => {}
            (None, _) => self.switch_workspace(None, window, cx),
        }
    }

    /// Arrow keys and Enter for the panel. These run ahead of the text input's
    /// own `MoveUp`/`MoveDown` bindings, so anything handled here must stop
    /// propagating or the cursor jumps inside the search box instead.
    fn on_switcher_key(
        &mut self,
        ev: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(sw) = self.switcher.as_ref() else {
            return;
        };
        let (key, mods) = (ev.keystroke.key.as_str(), ev.keystroke.modifiers);
        if key == "escape" {
            cx.stop_propagation();
            // A rename in progress is the smaller thing to back out of; the
            // panel itself only closes on a second press.
            if let Some(sw) = self.switcher.as_mut()
                && sw.renaming.take().is_some()
            {
                let query = sw.query.clone();
                query.update(cx, |state, cx| state.focus(window, cx));
                cx.notify();
                return;
            }
            self.close_switcher(window, cx);
            return;
        }
        // A rename box owns every key while it is up.
        if sw.renaming.is_some() {
            return;
        }
        let column = sw.column;
        let layout = self.switcher_layout(cx);
        match key_intent(key, mods) {
            // Escape already returned above; closing again is harmless and
            // beats a panic in the render path if that ever stops being true.
            Key::Close => self.close_switcher(window, cx),
            Key::Pass => {}
            Key::Step(forward) => {
                cx.stop_propagation();
                match column {
                    Column::Left => self.switcher_step_left(&layout, forward, window, cx),
                    Column::Right => self.switcher_step_right(&layout, forward, window, cx),
                }
            }
            // Once there is a query, left and right belong to the caret in the
            // search box; Tab is then the way across.
            Key::ToColumn(Column::Left) if column == Column::Right && sw.text(cx).is_empty() => {
                cx.stop_propagation();
                self.switcher_focus(Column::Left, cx);
            }
            Key::ToColumn(Column::Right) if column == Column::Left && sw.text(cx).is_empty() => {
                let has_tabs = layout
                    .subject_row(sw.left_sel)
                    .is_some_and(|r| !r.tabs.is_empty());
                if has_tabs {
                    cx.stop_propagation();
                    self.switcher_focus(Column::Right, cx);
                }
            }
            Key::ToColumn(_) => {}
            Key::Tab(forward) => {
                cx.stop_propagation();
                self.switcher_step_right(&layout, forward, window, cx);
            }
            Key::Confirm(new_window) => {
                cx.stop_propagation();
                self.switcher_confirm(&layout, new_window, window, cx);
            }
        }
    }

    /// The anchor a row wears while it is the selected one, so stepping the
    /// cursor with the keyboard carries the column to it.
    fn switcher_anchor(&self, column: Column, picked: bool) -> Option<gpui::ScrollAnchor> {
        let sw = self.switcher.as_ref()?;
        picked.then(|| match column {
            Column::Left => sw.left_anchor.clone(),
            Column::Right => sw.right_anchor.clone(),
        })
    }

    /// Moves the left cursor to a clicked row so the tab column follows it.
    /// Deliberately not wired to hover — the tab column swapping out from under
    /// the pointer on the way to somewhere else is noise, not a preview.
    fn switcher_point_at(&mut self, at: usize, cx: &mut Context<Self>) {
        let Some(sw) = self.switcher.as_mut() else {
            return;
        };
        if sw.left_sel == at && sw.column == Column::Left {
            return;
        }
        sw.left_sel = at;
        sw.right_sel = 0;
        sw.column = Column::Left;
        cx.notify();
    }

    /// Aims the tab cursor at one row, without acting on it.
    fn switcher_point_tab(&mut self, nth: usize, cx: &mut Context<Self>) {
        if let Some(sw) = self.switcher.as_mut() {
            sw.column = Column::Right;
            sw.right_sel = nth;
            cx.notify();
        }
    }

    fn switcher_focus(&mut self, column: Column, cx: &mut Context<Self>) {
        if let Some(sw) = self.switcher.as_mut() {
            sw.column = column;
        }
        cx.notify();
    }

    fn switcher_step_left(
        &mut self,
        layout: &Layout,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let n = layout.nav.len();
        let Some(sw) = self.switcher.as_mut() else {
            return;
        };
        if n == 0 {
            return;
        }
        sw.column = Column::Left;
        sw.left_sel = step(sw.left_sel.min(n - 1), n, forward);
        // A different workspace means a different tab column.
        sw.right_sel = 0;
        let anchor = sw.left_anchor.clone();
        anchor.scroll_to(window, cx);
        cx.notify();
    }

    fn switcher_step_right(
        &mut self,
        layout: &Layout,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sel = self.switcher.as_ref().map(|sw| sw.left_sel).unwrap_or(0);
        let query = self
            .switcher
            .as_ref()
            .map(|sw| sw.text(cx))
            .unwrap_or_default();
        let n = layout
            .subject_row(sel)
            .map(|row| visible_tabs(row, &query).len())
            .unwrap_or(0);
        let Some(sw) = self.switcher.as_mut() else {
            return;
        };
        if n == 0 {
            return;
        }
        sw.column = Column::Right;
        sw.right_sel = step(sw.right_sel.min(n - 1), n, forward);
        let anchor = sw.right_anchor.clone();
        anchor.scroll_to(window, cx);
        cx.notify();
    }

    fn switcher_confirm(
        &mut self,
        layout: &Layout,
        new_window: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(sw) = self.switcher.as_ref() else {
            return;
        };
        let (sel, column, right_sel) = (sw.left_sel, sw.column, sw.right_sel);
        if column == Column::Right {
            let query = sw.text(cx);
            let Some(row) = layout.subject_row(sel) else {
                return;
            };
            let Some(tab) = visible_tabs(row, &query)
                .get(right_sel)
                .and_then(|i| row.tabs.get(*i))
            else {
                return;
            };
            let (ws, id, index) = (row.id, tab.id, tab.index);
            self.switcher_open_tab(ws, id, index, new_window, window, cx);
            return;
        }
        match layout.nav.get(sel) {
            Some(Nav::Row(g, r)) => {
                let group = &layout.groups[*g];
                let row = RowRef::of(group, &group.rows[*r]);
                self.switcher_open(row, new_window, window, cx);
            }
            Some(Nav::Host(g)) => {
                let group = GroupRef::of(&layout.groups[*g]);
                self.switcher_toggle_host(&group, cx);
            }
            Some(Nav::OthersHeader) => {
                if let Some(sw) = self.switcher.as_mut() {
                    sw.show_others = !sw.show_others;
                }
                cx.notify();
            }
            Some(Nav::Other(i)) => {
                if let Some(choice) = layout.others.get(*i).cloned() {
                    self.connect_to_host(choice, cx);
                }
            }
            None => {}
        }
    }

    /// Activates one tab of `ws`, wherever that workspace happens to live.
    fn switcher_open_tab(
        &mut self,
        ws: WorkspaceId,
        tab: TabId,
        index: usize,
        new_window: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_switcher(window, cx);
        if new_window {
            crate::ui::windows::open_at_tab(cx, ws, tab);
            return;
        }
        if ws == self.workspace {
            self.activate(index, window, cx);
            return;
        }
        // A workspace that already has a window belongs to that window; anything
        // else comes here, the same as picking the workspace itself.
        if crate::ui::windows::WindowRegistry::window_for(cx, ws).is_some() {
            crate::ui::windows::open_at_tab(cx, ws, tab);
            return;
        }
        self.switch_workspace(Some(ws), window, cx);
        self.activate_tree_tab(tab, window, cx);
    }

    pub(crate) fn render_switcher(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let sw = self.switcher.as_ref()?;
        let (sel, column) = (sw.left_sel, sw.column);
        let (left_scroll, right_scroll) = (sw.left_scroll.clone(), sw.right_scroll.clone());
        let layout = self.switcher_layout(cx);

        let theme = cx.theme();
        let (border, card_bg) = (theme.border, theme.popover);
        let scrim = crate::ui::presets::scrim_fill(cx);

        let mut list = v_flex().gap(px(6.));
        let mut shown = 0usize;
        for g in 0..layout.groups.len() {
            if layout.shown[g].is_none() {
                continue;
            }
            shown += 1;
            list = list.child(self.render_group(&layout, g, sel, column, cx));
        }
        if let Some(band) = self.render_other_hosts(&layout, sel, column, cx) {
            shown += 1;
            list = list.child(band);
        }
        if shown == 0 {
            list = list.child(
                div()
                    .px(px(ROW_PAD))
                    .py(px(14.))
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t(L10nKey::SwitcherNoMatch)),
            );
        }

        // Fixed height, not fit-to-content: the tab column changes length every
        // time the left cursor moves, and a card that resizes under the pointer
        // is unusable.
        // The card is fixed-size by design — a panel that resizes under the
        // pointer is unusable — but it still has to fit the window it floats
        // over. Below its natural size it takes what there is.
        let viewport = window.viewport_size();
        let card_w = CARD_W
            .min(viewport.width.as_f32() - 2. * CARD_MARGIN)
            .max(320.);
        let body_h = BODY_H
            .min(viewport.height.as_f32() - CARD_TOP - CARD_CHROME_H - CARD_MARGIN)
            .max(120.);
        let body = div()
            .flex()
            .flex_row()
            .items_stretch()
            .h(px(body_h))
            // Both columns scroll once the lists outrun the card, and neither
            // said so. The border moves out to the column so it stays put
            // while the rows underneath it move.
            .child(
                v_flex()
                    .w(px(LEFT_W.min(card_w * 0.5)))
                    .flex_shrink_0()
                    .border_r_1()
                    .border_color(border)
                    .child(crate::ui::scrollbar::with_vertical_scrollbar(
                        "switcher-workspaces-scrollbar",
                        div()
                            .id("switcher-workspaces")
                            .track_scroll(&left_scroll)
                            .size_full()
                            .overflow_y_scroll()
                            .p(px(6.))
                            .child(list),
                        &left_scroll,
                    )),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(crate::ui::scrollbar::with_vertical_scrollbar(
                        "switcher-tabs-scrollbar",
                        div()
                            .id("switcher-tabs")
                            .track_scroll(&right_scroll)
                            .size_full()
                            .overflow_y_scroll()
                            .p(px(6.))
                            .child(self.render_tabs(&layout, sel, column, cx)),
                        &right_scroll,
                    )),
            );

        let card = v_flex()
            .w(px(card_w))
            .bg(card_bg)
            .border_1()
            .border_color(border)
            .rounded(px(10.))
            .shadow_xl()
            .overflow_hidden()
            .child(self.render_search(cx))
            .child(body)
            .child(self.render_footer(cx));

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(CARD_TOP))
                .bg(scrim)
                .key_context("Switcher")
                .on_action(cx.listener(|this, _: &SwitcherAcross, window, cx| {
                    let layout = this.switcher_layout(cx);
                    this.switcher_step_right(&layout, true, window, cx);
                }))
                .on_action(cx.listener(|this, _: &SwitcherAcrossBack, window, cx| {
                    let layout = this.switcher_layout(cx);
                    this.switcher_step_right(&layout, false, window, cx);
                }))
                .on_key_down(cx.listener(Self::on_switcher_key))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        this.close_switcher(window, cx)
                    }),
                )
                .child(div().occlude().child(card))
                .into_any_element(),
        )
    }

    fn render_search(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        h_flex()
            .items_center()
            .gap(px(8.))
            .pl(px(6. + ROW_PAD))
            .pr(px(12.))
            .h(px(42.))
            .border_b_1()
            .border_color(border)
            .child(glyph_col(
                GUTTER,
                Icon::new(IconName::Search).size(px(ICON)).text_color(muted),
            ))
            .children(
                self.switcher
                    .as_ref()
                    .map(|sw| Input::new(&sw.query).appearance(false).small().pl_0()),
            )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (muted, dim, border) = (
            theme.muted_foreground,
            theme.muted_foreground.opacity(0.7),
            theme.border,
        );
        let hover = hover_fill(cx);
        let holding = self.switcher.as_ref().is_some_and(|sw| sw.hold.is_some());
        // With a query in the box, ← and → belong to the caret and Tab becomes
        // the way across. That remap is deliberate, and until now it was also
        // silent: arrows simply stopped working with nothing said.
        let filtering = self
            .switcher
            .as_ref()
            .is_some_and(|sw| !sw.text(cx).trim().is_empty());
        h_flex()
            .items_center()
            .justify_between()
            .border_t_1()
            .border_color(border)
            .p(px(6.))
            .child(
                h_flex()
                    .id("switcher-add-host")
                    .items_center()
                    .gap(px(8.))
                    .h(px(ROW_H))
                    .px(px(ROW_PAD))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .hover(move |r| r.bg(hover))
                    .text_sm()
                    .text_color(muted)
                    .child(glyph_col(
                        GUTTER,
                        Icon::new(IconName::Plus).size(px(ICON)).text_color(dim),
                    ))
                    .child(t(L10nKey::AddSshHost))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_switcher(window, cx);
                        this.open_settings_section(
                            crate::ui::settings::SettingsSection::Ssh,
                            window,
                            cx,
                        );
                    })),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap(px(6.))
                    .pr(px(ROW_PAD))
                    .text_xs()
                    .text_color(dim)
                    .when(!holding && filtering, |hint| {
                        hint.child(t(L10nKey::SwitcherTabToCrossColumns))
                    })
                    .when(!holding && !filtering, |hint| {
                        hint.child(
                            div()
                                .px(px(5.))
                                .py(px(1.))
                                .rounded(px(4.))
                                .border_1()
                                .border_color(border)
                                .child(crate::ui::keymap::secondary_glyph()),
                        )
                        .child(t(L10nKey::ClickForNewWindow))
                    })
                    .when(holding, |hint| hint.child(t(L10nKey::SwitcherHoldToSwitch))),
            )
    }

    fn render_group(
        &self,
        layout: &Layout,
        g: usize,
        sel: usize,
        column: Column,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let group = &layout.groups[g];
        let rows: &[usize] = layout.shown[g].as_deref().unwrap_or(&[]);
        // The layout only lists rows for an expanded group, so a group with a
        // collapsed or offline body comes through with none.
        let expanded = !rows.is_empty()
            || (group.rows.is_empty()
                && group.link != Link::Offline
                && !self
                    .switcher
                    .as_ref()
                    .is_some_and(|sw| sw.collapsed.contains(&group.key)));

        let mut block = v_flex().gap(px(1.));
        block = block.child(self.render_group_header(
            group,
            expanded,
            layout.nav.get(sel) == Some(&Nav::Host(g)) && column == Column::Left,
            layout.nav.iter().position(|n| *n == Nav::Host(g)),
            cx,
        ));
        if let Some(phase) = group.installing {
            block = block.child(self.render_install_progress(phase, cx));
        }
        if group.parked && !self.parked_dismissed.contains(&group.key) && group.installing.is_none()
        {
            block = block.child(self.render_parked_notice(group, cx));
        } else if let Some(error) = group.error.as_ref().filter(|_| group.installing.is_none()) {
            let retry = GroupRef::of(group);
            let replace = retry.clone();
            let retry_key = group.key.clone();
            let replace_key = group.key.clone();
            let dismiss_key = group.key.clone();
            let dismiss_target = group.target.clone();
            let shown = dialect_complaint(error, &group.label).unwrap_or_else(|| error.clone());
            let theme = cx.theme();
            block =
                block.child(
                    v_flex()
                        .gap(px(4.))
                        .ml(px(KID_INDENT))
                        .mr(px(4.))
                        .mb(px(2.))
                        .px(px(10.))
                        .py(px(8.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(theme.danger.opacity(0.4))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(shown),
                        )
                        .child(
                            h_flex()
                                .gap(px(4.))
                                .child(
                                    Button::new(gpui::SharedString::from(format!(
                                        "switcher-retry:{}",
                                        group.key
                                    )))
                                    .label(t(L10nKey::TryAgain))
                                    .ghost()
                                    .xsmall()
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        this.remote_host_errors.remove(&retry_key);
                                        if let Some(target) = retry.target.clone() {
                                            this.connect_to_host(
                                                HostChoice {
                                                    target,
                                                    label: retry.label.clone(),
                                                    detail: String::new(),
                                                },
                                                cx,
                                            );
                                        }
                                    })),
                                )
                                .when(
                                    crate::daemon::control::is_dialect_refusal(error)
                                        && replace.target.is_some(),
                                    |row| {
                                        row.child(
                                            Button::new(gpui::SharedString::from(format!(
                                                "switcher-replace:{}",
                                                group.key
                                            )))
                                            .label(t(L10nKey::RemoteMismatchReplaceServer))
                                            .ghost()
                                            .xsmall()
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.remote_host_errors.remove(&replace_key);
                                                if let Some(target) = replace.target.clone() {
                                                    this.confirm_replace_remote_server(
                                                        target,
                                                        replace.label.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            })),
                                        )
                                    },
                                )
                                .child(
                                    Button::new(gpui::SharedString::from(format!(
                                        "switcher-dismiss:{}",
                                        group.key
                                    )))
                                    .label(t(L10nKey::Dismiss))
                                    .ghost()
                                    .xsmall()
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        this.remote_host_errors.remove(&dismiss_key);
                                        // The other half of this block can come from a
                                        // failed connect. Retire that too, but only when
                                        // it is this host's failure — a connect to
                                        // anywhere else is still in flight.
                                        if let Some(ConnectFlow::Failed { choice, .. }) =
                                            &this.connect
                                            && Some(&choice.target) == dismiss_target.as_ref()
                                        {
                                            this.connect = None;
                                        }
                                        cx.notify();
                                    })),
                                ),
                        ),
                );
        }
        if !rows.is_empty() {
            let mut kids = v_flex().gap(px(1.));
            for r in rows {
                let item = Nav::Row(g, *r);
                let picked = layout.nav.get(sel) == Some(&item) && column == Column::Left;
                let at = layout.nav.iter().position(|n| *n == item);
                kids = kids.child(self.render_row(group, &group.rows[*r], picked, at, cx));
            }
            block = block.child(self.indent(group, kids, cx));
        }
        block.into_any_element()
    }

    /// The parked group's notice (#485): no retry button — nothing it could
    /// try would succeed — just the way back (a fresh profile rediscovers
    /// the session) and the two honest actions.
    fn render_parked_notice(&self, group: &Group, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let forget_key = group.key.clone();
        let dismiss_key = group.key.clone();
        let dismiss_target = group.target.clone();
        let entries: Vec<WorkspaceId> = group.rows.iter().map(|r| r.id).collect();
        v_flex()
            .gap(px(4.))
            .ml(px(KID_INDENT))
            .mr(px(4.))
            .mb(px(2.))
            .px(px(10.))
            .py(px(8.))
            .rounded(px(6.))
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t(L10nKey::RemoteRouteParkedHint)),
            )
            .child(
                h_flex()
                    .gap(px(4.))
                    .child(
                        Button::new(gpui::SharedString::from(format!(
                            "switcher-forget:{}",
                            group.key
                        )))
                        .label(t(L10nKey::RemoteActionRemoveEntry))
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                // Forget, never delete: nothing is sent to the
                                // machine, so the remote sessions keep running
                                // (#485). Adopt rows name no local entry and are
                                // skipped by the store.
                                for id in &entries {
                                    crate::ui::windows::forget_workspace(cx, *id);
                                }
                                this.remote_host_errors.remove(&forget_key);
                                this.parked_dismissed.remove(&forget_key);
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        Button::new(gpui::SharedString::from(format!(
                            "switcher-parked-dismiss:{}",
                            group.key
                        )))
                        .label(t(L10nKey::Dismiss))
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                this.parked_dismissed.insert(dismiss_key.clone());
                                // A parked group can still carry the deterministic
                                // failure that predated the parking — retire it
                                // too, or it pops right back as the error block.
                                this.remote_host_errors.remove(&dismiss_key);
                                if let Some(ConnectFlow::Failed { choice, .. }) = &this.connect
                                    && Some(&choice.target) == dismiss_target.as_ref()
                                {
                                    this.connect = None;
                                }
                                cx.notify();
                            },
                        )),
                    ),
            )
    }

    fn render_install_progress(
        &self,
        phase: InstallPhase,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let accent = theme.warning;
        let fraction = phase.fraction().unwrap_or(0.0);
        let caption = match phase {
            InstallPhase::Restarting => t(L10nKey::SwitcherRestartingServer).to_string(),
            InstallPhase::Downloading { done, total } => match total {
                Some(total) => t_fmt(
                    L10nKey::SwitcherDownloadingServerWithTotal,
                    &[("done", &human_bytes(done)), ("total", &human_bytes(total))],
                ),
                None => t_fmt(
                    L10nKey::SwitcherDownloadingServerNoTotal,
                    &[("done", &human_bytes(done))],
                ),
            },
            InstallPhase::Uploading { done, total } => t_fmt(
                L10nKey::SwitcherCopyingServer,
                &[("done", &human_bytes(done)), ("total", &human_bytes(total))],
            ),
        };

        v_flex()
            .gap(px(6.))
            .ml(px(KID_INDENT))
            .mr(px(4.))
            .mb(px(2.))
            .px(px(10.))
            .py(px(8.))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(caption),
            )
            .child(
                div()
                    .w_full()
                    .h(px(PROGRESS_H))
                    .rounded_full()
                    .bg(theme.border)
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(fraction))
                            .rounded_full()
                            .bg(accent),
                    ),
            )
    }

    fn indent(&self, group: &Group, kids: impl IntoElement, cx: &mut Context<Self>) -> AnyElement {
        let rail = cx.theme().border;
        div()
            .relative()
            .child(div().pl(px(KID_INDENT)).child(kids))
            .when(group.target.is_some(), |wrap| {
                wrap.child(
                    div()
                        .absolute()
                        .left(px(RAIL_X))
                        .top(px(0.))
                        .bottom(px(ROW_H / 2.))
                        .w(px(1.))
                        .bg(rail),
                )
            })
            .into_any_element()
    }

    fn render_group_header(
        &self,
        group: &Group,
        expanded: bool,
        picked: bool,
        nav_at: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, dim) = (
            theme.foreground,
            theme.muted_foreground,
            theme.muted_foreground.opacity(0.7),
        );
        let hover = hover_fill(cx);
        let gref = GroupRef::of(group);
        let menu_ref = gref.clone();
        let ctx_ref = gref.clone();
        let app = cx.entity().downgrade();
        let app2 = app.clone();

        let glyph = match group.target {
            None => "icons/machine-local.svg",
            Some(_) => "icons/machine-remote.svg",
        };

        let (dot, word): (Option<gpui::Hsla>, Option<&'static str>) = match group.link {
            Link::Local => (None, None),
            Link::Connected => (Some(gpui::rgb(crate::ui::tab_strip::LIVE_DOT).into()), None),
            Link::Connecting if matches!(group.installing, Some(InstallPhase::Restarting)) => (
                Some(theme.warning),
                Some(t(L10nKey::SwitcherStatusRestarting)),
            ),
            Link::Connecting if group.installing.is_some() => (
                Some(theme.warning),
                Some(t(L10nKey::SwitcherStatusInstalling)),
            ),
            Link::Connecting => (
                Some(theme.warning),
                Some(t(L10nKey::SwitcherStatusConnecting)),
            ),
            Link::Reconnecting { .. } => (
                Some(theme.warning),
                Some(t(L10nKey::SwitcherStatusReconnecting)),
            ),
            Link::Failed => (
                Some(theme.danger),
                Some(t(L10nKey::SwitcherStatusConnectFailed)),
            ),
            Link::Offline => (
                Some(gpui::rgb(crate::ui::tab_strip::UNKNOWN_DOT).into()),
                Some(t(L10nKey::SwitcherStatusNotConnected)),
            ),
        };
        // A takeover leaves the link untouched, so a machine whose workspace
        // someone else is holding would otherwise draw as plain Connected.
        let (dot, word) = match group.preempted && matches!(group.link, Link::Connected) {
            true => (
                Some(theme.warning),
                Some(t(L10nKey::SwitcherStatusTakenOver)),
            ),
            false => (dot, word),
        };
        let word_color = match group.link {
            _ if group.preempted => theme.warning,
            Link::Connecting | Link::Reconnecting { .. } => theme.warning,
            Link::Failed => theme.danger,
            _ => muted,
        };

        let head = h_flex()
            .id(gpui::SharedString::from(format!(
                "switcher-host:{}",
                group.key
            )))
            .items_center()
            .gap(px(8.))
            .h(px(HOST_H))
            .px(px(ROW_PAD))
            .rounded(px(6.))
            .overflow_hidden()
            .cursor_pointer()
            .when(picked, |r| r.bg(gpui::rgb(rungs(cx).pressed)))
            .anchor_scroll(self.switcher_anchor(Column::Left, picked))
            .hover(move |r| r.bg(hover))
            .child(glyph_col(
                GUTTER,
                Icon::empty()
                    .path(glyph)
                    .size(px(ICON))
                    .text_color(if group.link == Link::Local { muted } else { fg }),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(fg)
                    .child(group.label.clone()),
            )
            .when(!group.endpoint.is_empty(), |head| {
                head.child(
                    div()
                        .flex_shrink_0()
                        .max_w(px(TAB_PATH_W))
                        .truncate()
                        .text_xs()
                        .text_color(dim)
                        .child(group.endpoint.clone()),
                )
            })
            .children(dot.map(|c| div().flex_shrink_0().size(px(6.)).rounded_full().bg(c)))
            .children(word.map(|w| {
                div()
                    .flex_shrink_0()
                    .ml(px(-2.))
                    .text_xs()
                    .text_color(word_color)
                    .child(w)
            }))
            .when(!group.rows.is_empty(), |head| {
                head.child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(dim)
                        .child(format!("{}", group.rows.len())),
                )
            })
            .child(
                div()
                    .flex_shrink_0()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        crate::ui::tab_strip::hit_target(
                            Button::new(gpui::SharedString::from(format!(
                                "switcher-host-more:{}",
                                group.key
                            )))
                            .icon(IconName::Ellipsis)
                            .ghost()
                            .xsmall(),
                        )
                        .tooltip(t(L10nKey::TabTooltipMore))
                        .dropdown_menu(move |menu, _window, _cx| {
                            group_menu(menu, &menu_ref, app.clone())
                        }),
                    ),
            )
            .child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(px(ICON))
                .text_color(dim),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                if let Some(at) = nav_at {
                    this.switcher_point_at(at, cx);
                }
                this.switcher_toggle_host(&gref, cx)
            }));

        // While Ctrl is held for the switch gesture, macOS turns every left
        // click into a right click. Keeping the context menu attached would put
        // it in the way of simply picking something, so it goes away and the
        // right-button press becomes the pick.
        match self.switcher.as_ref().is_some_and(|sw| sw.hold.is_some()) {
            true => head
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        cx.stop_propagation();
                        if let Some(at) = nav_at {
                            this.switcher_point_at(at, cx);
                        }
                    }),
                )
                .into_any_element(),
            false => head
                .context_menu(move |menu, _window, _cx| group_menu(menu, &ctx_ref, app2.clone()))
                .into_any_element(),
        }
    }

    fn render_row(
        &self,
        group: &Group,
        row: &Row,
        picked: bool,
        nav_at: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(sw) = self.switcher.as_ref()
            && let Some((id, input)) = sw.renaming.as_ref()
            && *id == row.id
        {
            return h_flex()
                .id(("switcher-rename", row.id.element_key() as usize))
                .items_center()
                .h(px(ROW_H))
                .px(px(ROW_PAD))
                .rounded(px(6.))
                .bg(hover_fill(cx))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(Input::new(input).appearance(false).xsmall())
                .into_any_element();
        }

        let theme = cx.theme();
        let (fg, muted, dim, warn) = (
            theme.foreground,
            theme.muted_foreground,
            theme.muted_foreground.opacity(0.7),
            theme.warning,
        );
        let sf = rungs(cx);
        let hover = gpui::rgb(sf.hover);
        let rref = RowRef::of(group, row);
        let click_ref = rref.clone();
        let menu_ref = rref.clone();
        let ctx_ref = rref.clone();
        let app = cx.entity().downgrade();
        let app2 = app.clone();
        let key = row.id.element_key() as usize;
        let holding = self.switcher.as_ref().is_some_and(|sw| sw.hold.is_some());

        // "Open" is what this workspace would say either way, and it is the
        // wrong word for one another client is driving.
        let badge = if row.preempted {
            Some((t(L10nKey::SwitcherStatusTakenOver), false))
        } else if row.current {
            Some((t(L10nKey::SwitcherThisWindow), true))
        } else if row.open {
            Some((t(L10nKey::SwitcherOpen), false))
        } else {
            None
        };

        // Two lines rather than one: the left column is only LEFT_W wide, and a
        // workspace name plus path plus badge plus timestamp on one row pushes
        // the trailing pieces straight out over the divider.
        let under = match row.path.is_empty() {
            true => row.when.clone(),
            false => format!("{} · {}", row.path, row.when),
        };

        let line = h_flex()
            .id(("switcher-row", key))
            .group("switcher-row")
            .items_center()
            .gap(px(8.))
            .min_h(px(ROW_H))
            .py(px(4.))
            .px(px(ROW_PAD))
            .rounded(px(6.))
            .overflow_hidden()
            .cursor_pointer()
            .when(picked, |r| r.bg(gpui::rgb(sf.pressed)))
            .anchor_scroll(self.switcher_anchor(Column::Left, picked))
            .hover(move |r| r.bg(hover))
            .child(crate::ui::tab_strip::workspace_avatar(
                &row.name, row.live, ROW_AVATAR, cx,
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(1.))
                    .child(
                        div()
                            .truncate()
                            .text_sm()
                            .when(row.current, |d| d.font_weight(gpui::FontWeight::MEDIUM))
                            .text_color(fg)
                            .child(row.name.clone()),
                    )
                    .when(!under.is_empty(), |col| {
                        col.child(div().truncate().text_xs().text_color(dim).child(under))
                    }),
            )
            .children(badge.map(|(label, here)| {
                div()
                    .flex_shrink_0()
                    .px(px(6.))
                    .py(px(1.))
                    .rounded(px(4.))
                    .text_xs()
                    .bg(gpui::rgb(sf.selected))
                    .text_color(match (row.preempted, here) {
                        (true, _) => warn,
                        (_, true) => fg.opacity(0.85),
                        _ => muted,
                    })
                    .child(label)
            }))
            .child(
                div()
                    .invisible()
                    .flex_shrink_0()
                    .group_hover("switcher-row", |x| x.visible())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        crate::ui::tab_strip::hit_target(
                            Button::new(("switcher-row-more", key))
                                .icon(IconName::Ellipsis)
                                .ghost()
                                .xsmall(),
                        )
                        .tooltip(t(L10nKey::TabTooltipMore))
                        .dropdown_menu(move |menu, _window, _cx| {
                            row_menu(menu, &menu_ref, app.clone())
                        }),
                    ),
            )
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                // One click aims the tab column at this workspace; opening it
                // takes a second click, Enter, or the platform modifier.
                if let Some(at) = nav_at {
                    this.switcher_point_at(at, cx);
                }
                let modified = ev.modifiers().secondary();
                if ev.click_count() >= 2 || modified {
                    this.switcher_open(click_ref.clone(), modified, window, cx);
                }
            }));

        // See `render_group_header`: a held Ctrl makes every click a right
        // click on macOS. Drop the menu and take the right-button press as the
        // pick instead, or clicking during the gesture does nothing at all.
        match holding {
            true => line
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        cx.stop_propagation();
                        if let Some(at) = nav_at {
                            this.switcher_point_at(at, cx);
                        }
                    }),
                )
                .into_any_element(),
            false => line
                .context_menu(move |menu, _window, _cx| row_menu(menu, &ctx_ref, app2.clone()))
                .into_any_element(),
        }
    }

    fn render_other_hosts(
        &self,
        layout: &Layout,
        sel: usize,
        column: Column,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if layout.other_hits.is_empty() {
            return None;
        }
        let (others, expanded) = (&layout.others, layout.others_expanded);
        let at = |item: Nav| layout.nav.get(sel) == Some(&item) && column == Column::Left;

        let theme = cx.theme();
        let (muted, dim) = (theme.muted_foreground, theme.muted_foreground.opacity(0.7));
        let hover = hover_fill(cx);
        let picked = gpui::rgb(rungs(cx).pressed);

        let mut block = v_flex().gap(px(1.)).child(
            h_flex()
                .id("switcher-others")
                .items_center()
                .gap(px(8.))
                .h(px(HOST_H))
                .px(px(ROW_PAD))
                .rounded(px(6.))
                .cursor_pointer()
                .when(at(Nav::OthersHeader), |r| r.bg(picked))
                .anchor_scroll(self.switcher_anchor(Column::Left, at(Nav::OthersHeader)))
                .hover(move |r| r.bg(hover))
                .child(glyph_col(
                    GUTTER,
                    Icon::new(IconName::Globe).size(px(ICON)).text_color(dim),
                ))
                .child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(t(L10nKey::OtherMachines)),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .text_xs()
                        .text_color(dim)
                        .child(format!("{}", others.len())),
                )
                .child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(px(ICON))
                    .text_color(dim),
                )
                .on_click(cx.listener(|this, _, _window, cx| {
                    if let Some(sw) = this.switcher.as_mut() {
                        sw.show_others = !sw.show_others;
                    }
                    cx.notify();
                })),
        );

        if expanded {
            let mut kids = v_flex().gap(px(1.));
            for i in &layout.other_hits {
                let (i, host) = (*i, &others[*i]);
                let choice = host.clone();
                kids = kids.child(
                    h_flex()
                        .id(("switcher-other", i))
                        .items_center()
                        .gap(px(8.))
                        .h(px(ROW_H))
                        .px(px(ROW_PAD))
                        .rounded(px(6.))
                        .overflow_hidden()
                        .cursor_pointer()
                        .when(at(Nav::Other(i)), |r| r.bg(picked))
                        .anchor_scroll(self.switcher_anchor(Column::Left, at(Nav::Other(i))))
                        .hover(move |r| r.bg(hover))
                        .child(glyph_col(
                            ROW_AVATAR,
                            Icon::empty()
                                .path("icons/machine-remote.svg")
                                .size(px(ICON))
                                .text_color(dim),
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .text_color(muted)
                                .child(host.label.clone()),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .max_w(px(TAB_PATH_W))
                                .truncate()
                                .text_xs()
                                .text_color(dim)
                                .child(host.detail.clone()),
                        )
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.connect_to_host(choice.clone(), cx)
                        })),
                );
            }
            block = block.child(div().pl(px(KID_INDENT)).child(kids));
        }
        Some(block.into_any_element())
    }

    /// The right-hand column: the tabs of whichever workspace the left column
    /// is sitting on.
    fn render_tabs(
        &self,
        layout: &Layout,
        sel: usize,
        column: Column,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, dim) = (
            theme.foreground,
            theme.muted_foreground,
            theme.muted_foreground.opacity(0.7),
        );
        let note = |text: String| {
            div()
                .px(px(ROW_PAD))
                .py(px(14.))
                .text_sm()
                .text_color(muted)
                .child(text)
                .into_any_element()
        };

        let Some(row) = layout.subject_row(sel) else {
            return note(t(L10nKey::SwitcherPickAWorkspace).to_string());
        };
        if row.tabs.is_empty() {
            return note(match row.adopt.is_some() {
                true => t(L10nKey::SwitcherTabsAfterOpening).to_string(),
                false => t(L10nKey::SwitcherNoTabs).to_string(),
            });
        }

        let query = self
            .switcher
            .as_ref()
            .map(|sw| sw.text(cx))
            .unwrap_or_default();
        let hits = visible_tabs(row, &query);
        if hits.is_empty() {
            return note(t(L10nKey::SwitcherNoTabMatch).to_string());
        }

        let sf = rungs(cx);
        let (hover, picked_bg) = (gpui::rgb(sf.hover), gpui::rgb(sf.pressed));
        let right_sel = self.switcher.as_ref().map(|sw| sw.right_sel).unwrap_or(0);
        let holding = self.switcher.as_ref().is_some_and(|sw| sw.hold.is_some());
        let ws = row.id;

        let mut list = v_flex().gap(px(1.)).child(
            h_flex()
                .items_center()
                .gap(px(6.))
                .h(px(HOST_H))
                .px(px(ROW_PAD))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(muted)
                        .child(row.name.clone()),
                )
                .child(div().text_xs().text_color(dim).child(match row.tabs.len() {
                    1 => t(L10nKey::SwitcherTabCountOne).to_string(),
                    n => t_fmt(L10nKey::SwitcherTabCount, &[("n", &n.to_string())]),
                })),
        );

        for (nth, i) in hits.iter().enumerate() {
            let tab = &row.tabs[*i];
            let picked = nth == right_sel && column == Column::Right;
            let (id, index) = (tab.id, tab.index);
            // The second line is what tells two tabs on the same repo apart —
            // the branch, then the diff counts, mirroring the tab sidebar.
            let under = tab.git.as_ref().map(|g| {
                h_flex()
                    .items_center()
                    .gap(px(5.))
                    .text_xs()
                    .text_color(dim)
                    .child(
                        gpui::svg()
                            .path("icons/git-branch.svg")
                            .flex_shrink_0()
                            .size(px(11.))
                            .text_color(dim),
                    )
                    .child(div().min_w_0().truncate().child(g.branch.clone()))
                    .when(g.added > 0, |c| {
                        c.child(
                            div()
                                .flex_shrink_0()
                                .text_color(theme.success)
                                .child(format!("+{}", g.added)),
                        )
                    })
                    .when(g.removed > 0, |c| {
                        c.child(
                            div()
                                .flex_shrink_0()
                                .text_color(theme.danger)
                                .child(format!("−{}", g.removed)),
                        )
                    })
            });
            let subtitle = match under {
                Some(line) => Some(line.into_any_element()),
                None if tab.named && !tab.path.is_empty() => Some(
                    div()
                        .text_xs()
                        .truncate()
                        .text_color(dim)
                        .child(tab.path.clone())
                        .into_any_element(),
                ),
                None => None,
            };

            list = list.child(
                h_flex()
                    .id(("switcher-tab", index))
                    .items_center()
                    .gap(px(8.))
                    .min_h(px(ROW_H))
                    .py(px(4.))
                    .px(px(ROW_PAD))
                    .rounded(px(6.))
                    .overflow_hidden()
                    .cursor_pointer()
                    .when(picked, |r| r.bg(picked_bg))
                    .anchor_scroll(self.switcher_anchor(Column::Right, picked))
                    .hover(move |r| r.bg(hover))
                    .child(self.tab_avatar(
                        ("switcher-avatar", index),
                        tab.agent,
                        tab.status,
                        tab.unread,
                        tab.ssh,
                        ROW_AVATAR,
                        cx,
                    ))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(1.))
                            .child(
                                div()
                                    .truncate()
                                    .text_sm()
                                    .when(tab.active, |d| d.font_weight(gpui::FontWeight::MEDIUM))
                                    .text_color(fg)
                                    .child(tab.label.clone()),
                            )
                            .children(subtitle),
                    )
                    .when(tab.active, |r| {
                        r.child(
                            div()
                                .flex_shrink_0()
                                .px(px(6.))
                                .py(px(1.))
                                .rounded(px(4.))
                                .text_xs()
                                .bg(gpui::rgb(sf.selected))
                                .text_color(muted)
                                .child(t(L10nKey::SwitcherActiveTab)),
                        )
                    })
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                        this.switcher_open_tab(
                            ws,
                            id,
                            index,
                            ev.modifiers().secondary(),
                            window,
                            cx,
                        )
                    }))
                    // Mid-gesture a click arrives as a right press on macOS. It
                    // aims the cursor; releasing Ctrl is what commits.
                    .when(holding, |line| {
                        line.on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                                cx.stop_propagation();
                                this.switcher_point_tab(nth, cx);
                            }),
                        )
                    }),
            );
        }
        list.into_any_element()
    }
}

/// Tab indices of `row` the search leaves visible. A workspace matched by its
/// own name keeps all of them — the search told you which workspace, not which
/// tab.
fn visible_tabs(row: &Row, query: &str) -> Vec<usize> {
    if query.is_empty()
        || row.name.to_lowercase().contains(query)
        || row.path.to_lowercase().contains(query)
    {
        return (0..row.tabs.len()).collect();
    }
    let hits: Vec<usize> = row
        .tabs
        .iter()
        .enumerate()
        .filter(|(_, t)| t.matches(query))
        .map(|(i, _)| i)
        .collect();
    match hits.is_empty() {
        // The host name matched, so the workspace is on screen with nothing of
        // its own to narrow by. Show the lot rather than an empty column.
        true => (0..row.tabs.len()).collect(),
        false => hits,
    }
}

impl Row {
    /// A workspace stays in the list when its own name or path matches, and
    /// also when any of its tabs does — searching "claude" should surface the
    /// workspaces running one.
    fn matches(&self, query: &str) -> bool {
        self.name.to_lowercase().contains(query)
            || self.path.to_lowercase().contains(query)
            || self.tabs.iter().any(|t| t.matches(query))
    }
}

impl TabRow {
    fn matches(&self, query: &str) -> bool {
        self.label.to_lowercase().contains(query) || self.path.to_lowercase().contains(query)
    }
}

/// Names a tab of a workspace this window does not own, matching what
/// `Tty7App::tab_label` shows for local ones.
///
/// The two read different sources and have to be talked into agreeing. A local
/// tab is named by its terminal's OSC title, which shells set to the working
/// directory and agents overwrite with their own name. The machine tree has
/// neither: `PaneRecord::title` is the *foreground process name* ("zsh"), so
/// the cwd and the agent stand in for it here.
fn tab_view_label(view: &crate::ui::machine_mirror::TabView, index: usize) -> String {
    let unnamed = || {
        t_fmt(
            L10nKey::TabUnnamedShell,
            &[("n", &((index + 1).to_string()))],
        )
    };
    match view.label() {
        crate::ui::machine_mirror::TabLabel::Named(name) => name.to_string(),
        crate::ui::machine_mirror::TabLabel::Agent(agent) => agent.display_name().to_string(),
        // A cwd can shorten away to nothing (a bare "user@host:"), and the
        // process name is still worth more than a number.
        crate::ui::machine_mirror::TabLabel::Cwd(cwd) => {
            match crate::ui::tab_strip::short_title(cwd) {
                shortened if !shortened.trim().is_empty() => shortened,
                _ => match view.title.trim() {
                    "" => unnamed(),
                    title => title.to_string(),
                },
            }
        }
        crate::ui::machine_mirror::TabLabel::Process(title) => title.to_string(),
        crate::ui::machine_mirror::TabLabel::Unknown => unnamed(),
    }
}

impl Group {
    fn merge(&mut self, remote: &[RemoteWorkspaceRow], now: u64) {
        if self.target.is_none() {
            return;
        }
        let known: HashSet<WorkspaceId> = self.rows.iter().filter_map(|r| r.remote_id).collect();
        for r in remote {
            if known.contains(&r.id) {
                continue;
            }
            self.rows.push(Row {
                id: r.id,
                name: r.name.clone(),
                path: String::new(),
                when: crate::ui::home::relative_time(now, r.last_active),
                live: Liveness::Stopped,
                open: false,
                current: false,
                preempted: false,
                adopt: Some(Box::new(r.clone())),
                remote_id: Some(r.id),
                // A workspace this client has never adopted has no local id to
                // hang a machine-tree lookup on. The tab column says so.
                tabs: Vec::new(),
            });
        }
    }
}

#[derive(Clone)]
struct GroupRef {
    key: String,
    label: String,
    target: Option<RemoteTarget>,
    home: Option<PathBuf>,
    link: Link,
}

impl GroupRef {
    fn of(g: &Group) -> Self {
        Self {
            key: g.key.clone(),
            label: g.label.clone(),
            target: g.target.clone(),
            home: g.home.clone(),
            link: g.link,
        }
    }
}

#[derive(Clone)]
struct RowRef {
    id: WorkspaceId,
    live: bool,
    adopt: Option<(RemoteTarget, Box<RemoteWorkspaceRow>)>,
}

impl RowRef {
    fn of(group: &Group, row: &Row) -> Self {
        Self {
            id: row.id,
            live: row.live == Liveness::Alive,
            adopt: match (&group.target, &row.adopt) {
                (Some(t), Some(r)) => Some((t.clone(), r.clone())),
                _ => None,
            },
        }
    }
}

/// Whether a machine's menu would open with every verb greyed out.
///
/// New Workspace needs the machine's home directory, which only a link that
/// has come up once supplies; Disconnect needs a live link; Restart Server is
/// offered to SSH alone. A WSL or stdio machine that has never connected fails
/// all three, and the menu opened with every row greyed and nothing to say for
/// itself. One line about the link beats three dead verbs.
fn group_menu_is_empty_handed(group: &GroupRef) -> bool {
    let Some(target) = group.target.as_ref() else {
        // A local machine can always take a new workspace.
        return false;
    };
    group.home.is_none() && !link_is_engaged(group.link) && !target.is_ssh()
}

fn group_menu(
    menu: gpui_component::menu::PopupMenu,
    group: &GroupRef,
    app: gpui::WeakEntity<Tty7App>,
) -> gpui_component::menu::PopupMenu {
    let (a1, a2, a3) = (app.clone(), app.clone(), app);
    let gref = group.clone();
    let can_create = group.target.is_none() || group.home.is_some();
    if group_menu_is_empty_handed(group) {
        return menu.item(PopupMenuItem::label(t(L10nKey::SwitcherConnectToUse)));
    }
    let menu = menu.item(
        PopupMenuItem::new(t(L10nKey::AppMenuNewWorkspace))
            .disabled(!can_create)
            .on_click(move |_, window, cx| {
                let _ = a1.update(cx, |this, cx| this.switcher_new(&gref, window, cx));
            }),
    );
    let Some(target) = group.target.clone() else {
        return menu;
    };
    let connected = link_is_engaged(group.link);
    let restartable = target.hosts_our_server();
    let (label, for_restart) = (group.label.clone(), target.clone());
    let menu = menu.separator().item(
        PopupMenuItem::new(t(L10nKey::SwitcherDisconnect))
            .disabled(!connected)
            .on_click(move |_, _window, cx| {
                let _ = a2.update(cx, |this, cx| this.switcher_disconnect(&target, cx));
            }),
    );
    if !restartable {
        return menu;
    }
    menu.item(
        PopupMenuItem::new(t(L10nKey::AppMenuRestartServer)).on_click(move |_, window, cx| {
            let _ = a3.update(cx, |this, cx| {
                this.confirm_restart_remote_server(for_restart.clone(), label.clone(), window, cx);
            });
        }),
    )
}

fn row_menu(
    menu: gpui_component::menu::PopupMenu,
    row: &RowRef,
    app: gpui::WeakEntity<Tty7App>,
) -> gpui_component::menu::PopupMenu {
    let (a1, a2, a3, a4) = (app.clone(), app.clone(), app.clone(), app);
    let (id, adopt) = (row.id, row.adopt.is_some());
    let stoppable = row.live;
    // Every verb below addresses a workspace by its local id, and a remote
    // this client has never adopted has none yet. That greyed all four out at
    // once: the menu opened with nothing to press and no word about why, which
    // reads as broken rather than as not-yet. Say what would make them work,
    // the way the tab pane already says it for the same rows.
    if adopt {
        return menu.item(PopupMenuItem::label(t(L10nKey::SwitcherOpenToManage)));
    }
    menu.item(
        PopupMenuItem::new(t(L10nKey::SwitcherRename)).on_click(move |_, window, cx| {
            let _ = a1.update(cx, |this, cx| this.switcher_rename(id, window, cx));
        }),
    )
    .item(
        PopupMenuItem::new(t(L10nKey::SwitcherOpenInNewWindow)).on_click(move |_, window, cx| {
            let _ = a2.update(cx, |this, cx| {
                this.close_switcher(window, cx);
                crate::ui::windows::open(cx, Some(id));
            });
        }),
    )
    .separator()
    .item(
        PopupMenuItem::new(t(L10nKey::AppMenuStopWorkspace))
            .disabled(!stoppable)
            .on_click(move |_, window, cx| {
                let _ = a3.update(cx, |this, cx| {
                    this.close_switcher(window, cx);
                    this.stop_workspace(id, window, cx);
                });
            }),
    )
    .item(
        PopupMenuItem::new(t(L10nKey::AppMenuDeleteWorkspace)).on_click(move |_, window, cx| {
            let _ = a4.update(cx, |this, cx| {
                this.close_switcher(window, cx);
                this.delete_workspace(id, window, cx);
            });
        }),
    )
}

/// A handshake that failed on the control dialect reaches here as the protocol
/// layer's own wording, which reads like the far end is not tty7 at all. It is —
/// it is just a build the other side of a dialect bump — so say that instead, and
/// say which side has to move. Anything else is shown as it came.
fn dialect_complaint(error: &str, machine: &str) -> Option<String> {
    let refusal = crate::daemon::control::parse_dialect_refusal(error)?;
    let key = if refusal.peer < refusal.ours {
        L10nKey::RemoteServerOutdated
    } else {
        L10nKey::RemoteServerTooNew
    };
    Some(t_fmt(
        key,
        &[("machine", machine), ("build", &refusal.peer_build)],
    ))
}

fn rungs(cx: &App) -> crate::ui::presets::Surface {
    cx.global::<crate::ui::presets::Surfaces>().popover
}

fn hover_fill(cx: &App) -> gpui::Rgba {
    gpui::rgb(rungs(cx).hover)
}

/// What the panel wants to do with a keystroke, before any of the state that
/// only the panel knows (which column has the cursor, whether the search box
/// has text) gets a say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Close,
    Step(bool),
    ToColumn(Column),
    Tab(bool),
    Confirm(bool),
    Pass,
}

fn key_intent(key: &str, mods: gpui::Modifiers) -> Key {
    if key == "escape" {
        return Key::Close;
    }
    // Alt and Fn belong to whoever else wants them, whatever the key.
    if mods.alt || mods.function {
        return Key::Pass;
    }
    // Careful: `control` cannot be lumped in with "some modifier is down", as
    // it *is* the secondary modifier off macOS — Ctrl+Enter has to reach the
    // new-window branch there, not fall through as a stray chord.
    let bare = !mods.control && !mods.secondary();
    match key {
        "up" | "down" if bare => Key::Step(key == "down"),
        "left" if bare => Key::ToColumn(Column::Left),
        "right" if bare => Key::ToColumn(Column::Right),
        // Tab keeps working with Ctrl held — that is the Ctrl+Tab gesture still
        // in progress. Off macOS that chord arrives as the NextTab action
        // instead, which lands in the same place.
        "tab" if !mods.secondary() => Key::Tab(!mods.shift),
        "enter" if bare || mods.secondary() => Key::Confirm(mods.secondary()),
        _ => Key::Pass,
    }
}

/// Wraps a cursor around a list of `n` items.
fn step(at: usize, n: usize, forward: bool) -> usize {
    match forward {
        true => (at + 1) % n,
        false => (at + n - 1) % n,
    }
}

fn glyph_col(w: f32, child: impl IntoElement) -> impl IntoElement {
    div()
        .w(px(w))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .child(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group_ref(target: Option<RemoteTarget>, home: Option<&str>, link: Link) -> GroupRef {
        GroupRef {
            key: "k".into(),
            label: "l".into(),
            target,
            home: home.map(PathBuf::from),
            link,
        }
    }

    /// A menu that opens with every row greyed and no word about why reads as
    /// broken rather than as not-yet, so the one state that reaches it has to
    /// stay pinned as the verbs and their conditions move around.
    #[test]
    fn only_an_unconnected_non_ssh_machine_has_nothing_to_offer() {
        let wsl = || {
            Some(RemoteTarget::Wsl {
                distro: "Ubuntu".into(),
            })
        };
        let ssh = || RemoteTarget::direct("me", "host", 22);

        // The case: never connected, so no home, and not SSH, so no restart.
        assert!(group_menu_is_empty_handed(&group_ref(
            wsl(),
            None,
            Link::Offline
        )));
        assert!(group_menu_is_empty_handed(&group_ref(
            wsl(),
            None,
            Link::Failed
        )));

        // A home from an earlier link still allows a new workspace.
        assert!(!group_menu_is_empty_handed(&group_ref(
            wsl(),
            Some("/home/me"),
            Link::Offline
        )));
        // A live link still allows Disconnect.
        assert!(!group_menu_is_empty_handed(&group_ref(
            wsl(),
            None,
            Link::Connected
        )));
        // SSH always keeps Restart Server.
        assert!(!group_menu_is_empty_handed(&group_ref(
            Some(ssh()),
            None,
            Link::Offline
        )));
        // This machine is never short of verbs.
        assert!(!group_menu_is_empty_handed(&group_ref(
            None,
            None,
            Link::Local
        )));
        // A machine the supervisor is retrying still has a retry to call off.
        assert!(!group_menu_is_empty_handed(&group_ref(
            wsl(),
            None,
            Link::Reconnecting { attempt: 2 }
        )));
    }

    /// The switcher used to read the `HostLinks` table and nothing else, which
    /// cannot tell "being retried right now" from "never heard of". The group
    /// then collapsed mid-reconnect as if the machine had no workspaces.
    #[test]
    fn a_machine_being_retried_does_not_read_as_one_nobody_ever_connected_to() {
        let target = RemoteTarget::direct("me", "build-box", 22);
        let retrying = MachineStatus::Reconnecting {
            attempt: 2,
            last_error: Some("connection refused".into()),
        };

        assert_eq!(
            link_from(None, &target, Some(&retrying), false),
            Link::Reconnecting { attempt: 2 }
        );
        assert_eq!(link_from(None, &target, None, false), Link::Offline);
        assert_eq!(
            link_from(None, &target, Some(&MachineStatus::Attached), false),
            Link::Connected
        );
        assert_eq!(
            link_from(
                None,
                &target,
                Some(&MachineStatus::Failed("no route to host".into())),
                false
            ),
            Link::Failed,
            "a route the supervisor could not build has to reach the panel too"
        );
        // A link this window knows nothing about is still a link.
        assert_eq!(link_from(None, &target, None, true), Link::Connected);
    }

    #[test]
    fn this_windows_own_attempt_is_only_ever_about_its_own_machine() {
        let build = RemoteTarget::direct("me", "build-box", 22);
        let gpu = RemoteTarget::direct("me", "gpu-lab", 22);
        let flow = ConnectFlow::Connecting {
            choice: HostChoice {
                target: gpu,
                label: "gpu-lab".into(),
                detail: String::new(),
            },
        };

        assert_eq!(
            link_from(Some(&flow), &build, Some(&MachineStatus::Attached), true),
            Link::Connected,
            "a connect to the GPU box says nothing about the build box"
        );
    }

    fn tab(label: &str, path: &str) -> TabRow {
        TabRow {
            id: TabId::new(),
            index: 0,
            label: label.to_string(),
            path: path.to_string(),
            named: false,
            agent: None,
            status: None,
            unread: 0,
            ssh: None,
            active: false,
            git: None,
        }
    }

    fn row(name: &str, tabs: Vec<TabRow>) -> Row {
        Row {
            id: WorkspaceId::new(),
            name: name.to_string(),
            path: "~/code".to_string(),
            when: String::new(),
            live: Liveness::Alive,
            open: true,
            current: false,
            preempted: false,
            adopt: None,
            remote_id: None,
            tabs,
        }
    }

    fn group(rows: Vec<Row>) -> Group {
        Group {
            key: String::new(),
            label: "This Computer".to_string(),
            endpoint: String::new(),
            target: None,
            link: Link::Local,
            home: None,
            error: None,
            installing: None,
            preempted: false,
            parked: false,
            rows,
        }
    }

    #[test]
    fn a_workspace_stays_in_the_list_when_only_one_of_its_tabs_matches() {
        let ws = row(
            "notes",
            vec![tab("zsh", "~/notes"), tab("claude", "~/notes")],
        );
        assert!(ws.matches("claude"));
        assert!(!ws.matches("codex"));
    }

    #[test]
    fn searching_a_tab_name_narrows_the_tab_column_to_the_hits() {
        let ws = row("notes", vec![tab("zsh", "~/a"), tab("claude", "~/b")]);
        assert_eq!(visible_tabs(&ws, "claude"), vec![1]);
    }

    #[test]
    fn searching_the_workspace_name_keeps_all_of_its_tabs() {
        let ws = row("notes", vec![tab("zsh", "~/a"), tab("claude", "~/b")]);
        assert_eq!(visible_tabs(&ws, "notes"), vec![0, 1]);
    }

    #[test]
    fn a_workspace_shown_only_because_its_host_matched_keeps_all_its_tabs() {
        // Nothing about the workspace or its tabs matched — the group header
        // did. An empty column here would read as "this workspace is empty".
        let ws = row("notes", vec![tab("zsh", "~/a")]);
        assert_eq!(visible_tabs(&ws, "dev-box"), vec![0]);
    }

    #[test]
    fn a_host_header_borrows_the_first_workspace_of_its_group() {
        let layout = Layout {
            groups: vec![group(vec![row("a", vec![]), row("b", vec![])])],
            shown: vec![Some(vec![0, 1])],
            others: Vec::new(),
            other_hits: Vec::new(),
            others_expanded: false,
            nav: vec![Nav::Host(0), Nav::Row(0, 0), Nav::Row(0, 1)],
        };
        assert_eq!(layout.subject(0), Some((0, 0)));
        assert_eq!(layout.subject(2), Some((0, 1)));
    }

    #[test]
    fn a_collapsed_host_header_has_no_workspace_to_show() {
        let layout = Layout {
            groups: vec![group(vec![row("a", vec![])])],
            shown: vec![Some(Vec::new())],
            others: Vec::new(),
            other_hits: Vec::new(),
            others_expanded: false,
            nav: vec![Nav::Host(0)],
        };
        assert_eq!(layout.subject(0), None);
    }

    #[test]
    fn the_cursor_wraps_at_both_ends() {
        assert_eq!(step(0, 3, true), 1);
        assert_eq!(step(2, 3, true), 0);
        assert_eq!(step(0, 3, false), 2);
    }

    #[test]
    fn a_tab_of_another_window_is_named_the_way_a_local_one_would_be() {
        // `title` here is the foreground process name the machine tree carries,
        // not a terminal title — it must not outrank the working directory the
        // local tab strip would be showing.
        let mut view = crate::ui::machine_mirror::TabView {
            id: TabId::new(),
            name: Some("  build  ".to_string()),
            title: "zsh".to_string(),
            cwd: Some("/Users/x/repo/tty7".to_string()),
            agent: Some(crate::core::cli_agent::CLIAgent::Claude),
            status: None,
            live: true,
            panes: 1,
        };
        assert_eq!(tab_view_label(&view, 0), "build", "a given name wins");

        view.name = None;
        assert_eq!(
            tab_view_label(&view, 0),
            "Claude Code",
            "an agent names its own tab, as its OSC title would locally"
        );

        view.agent = None;
        assert_eq!(
            tab_view_label(&view, 0),
            crate::ui::tab_strip::short_title("/Users/x/repo/tty7"),
            "otherwise the directory, put through the same shortener as the strip"
        );

        view.cwd = None;
        assert_eq!(tab_view_label(&view, 0), "zsh", "process name is last");

        view.title = String::new();
        assert!(tab_view_label(&view, 2).contains('3'));
    }
    fn refusal(peer: u32, ours: u32) -> String {
        format!(
            "java answered, but not as a tty7 server: control peer (build 26.7.7-nightly) \
             speaks control v{peer}, this build speaks v{ours}"
        )
    }

    #[test]
    fn a_dialect_refusal_is_restated_as_which_side_is_behind() {
        let behind = dialect_complaint(&refusal(4, 5), "java").expect("a refusal is recognised");
        assert!(
            behind.contains("java") && behind.contains("26.7.7-nightly"),
            "{behind}"
        );
        assert!(
            !behind.contains("control v"),
            "the dialect numbers mean nothing to the reader: {behind}"
        );

        let ahead = dialect_complaint(&refusal(6, 5), "java").expect("a refusal is recognised");
        assert_ne!(
            ahead, behind,
            "a server newer than this client needs the opposite advice"
        );
    }

    #[test]
    fn every_other_failure_is_shown_as_it_came() {
        assert_eq!(
            dialect_complaint("Connection refused (os error 61)", "java"),
            None
        );
    }
}

#[cfg(all(test, unix))]
mod gpui_tests {
    use gpui::{Modifiers, TestAppContext};

    use super::Column;
    use crate::ui::app::test_window::harness_with_tabs;

    #[gpui::test]
    fn ctrl_tab_raises_the_panel_on_the_previously_used_tab(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 3);
        vcx.simulate_modifiers_change(Modifiers::control());

        app.update_in(&mut vcx, |app, window, cx| app.tab_switch(true, window, cx));

        app.update(cx, |app, _| {
            let sw = app.switcher.as_ref().expect("Ctrl+Tab raises the panel");
            assert_eq!(sw.column, Column::Right, "the tab column takes the cursor");
            assert_eq!(sw.right_sel, 1, "the cursor lands on the previous tab");
            assert!(
                sw.mru,
                "Ctrl+Tab orders the column most-recently-used first"
            );
            assert!(sw.hold.is_some(), "the held modifier is what commits later");
        });
    }

    #[gpui::test]
    fn holding_ctrl_and_pressing_tab_again_walks_further_down(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 3);
        vcx.simulate_modifiers_change(Modifiers::control());

        app.update_in(&mut vcx, |app, window, cx| {
            app.tab_switch(true, window, cx);
            app.tab_switch(true, window, cx);
        });

        app.update(cx, |app, _| {
            assert_eq!(app.switcher.as_ref().expect("still up").right_sel, 2);
            assert_eq!(app.active, 0, "nothing is committed while the key is held");
        });
    }

    #[gpui::test]
    fn releasing_ctrl_commits_the_highlighted_tab_and_closes_the_panel(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 3);
        vcx.simulate_modifiers_change(Modifiers::control());
        app.update_in(&mut vcx, |app, window, cx| app.tab_switch(true, window, cx));

        vcx.simulate_modifiers_change(Modifiers::none());

        app.update(cx, |app, _| {
            assert!(app.switcher.is_none(), "the panel comes down on release");
            assert_eq!(app.active, 1, "the highlighted tab is now the active one");
        });
    }

    #[gpui::test]
    fn a_second_ctrl_tab_goes_back_to_where_it_came_from(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 3);

        vcx.simulate_modifiers_change(Modifiers::control());
        app.update_in(&mut vcx, |app, window, cx| app.tab_switch(true, window, cx));
        vcx.simulate_modifiers_change(Modifiers::none());
        vcx.run_until_parked();
        app.update(cx, |app, _| assert_eq!(app.active, 1));

        vcx.simulate_modifiers_change(Modifiers::control());
        app.update_in(&mut vcx, |app, window, cx| app.tab_switch(true, window, cx));
        vcx.simulate_modifiers_change(Modifiers::none());

        app.update(cx, |app, _| {
            assert_eq!(
                app.active, 0,
                "most-recently-used ordering makes the gesture a toggle"
            );
        });
    }

    #[gpui::test]
    fn a_lone_tab_still_opens_the_panel_but_does_not_hold(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        vcx.simulate_modifiers_change(Modifiers::control());

        app.update_in(&mut vcx, |app, window, cx| app.tab_switch(true, window, cx));

        app.update(cx, |app, _| {
            let sw = app
                .switcher
                .as_ref()
                .expect("with nothing to cycle it still opens the switcher");
            assert_eq!(
                sw.column,
                Column::Left,
                "the workspace column is the useful one"
            );
            assert!(sw.hold.is_none(), "nothing to commit, so nothing to hold");
        });
    }

    #[gpui::test]
    fn a_lone_tabs_panel_survives_letting_go_of_ctrl(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        vcx.simulate_modifiers_change(Modifiers::control());
        app.update_in(&mut vcx, |app, window, cx| app.tab_switch(true, window, cx));

        vcx.simulate_modifiers_change(Modifiers::none());

        app.update(cx, |app, _| {
            assert!(
                app.switcher.is_some(),
                "a panel opened without a hold must not close on release"
            );
        });
    }

    #[gpui::test]
    fn picking_a_tab_mid_gesture_commits_it_on_release(cx: &mut TestAppContext) {
        // What a click during the hold amounts to: aim the cursor, then let go.
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 3);
        vcx.simulate_modifiers_change(Modifiers::control());
        app.update_in(&mut vcx, |app, window, cx| app.tab_switch(true, window, cx));

        app.update(cx, |app, cx| app.switcher_point_tab(2, cx));
        vcx.simulate_modifiers_change(Modifiers::none());

        app.update(cx, |app, _| {
            assert!(app.switcher.is_none());
            assert_eq!(app.active, 2, "the tab the pointer picked is now active");
        });
    }

    #[gpui::test]
    fn a_held_gesture_hides_the_context_menus_it_would_trip_over(cx: &mut TestAppContext) {
        // macOS reports Ctrl+click as a right click, which is exactly what the
        // context menu listens for. Nothing to assert on the element tree from
        // here, so this pins the flag the render path branches on.
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 3);
        vcx.simulate_modifiers_change(Modifiers::control());
        app.update_in(&mut vcx, |app, window, cx| app.tab_switch(true, window, cx));

        app.update(cx, |app, _| {
            assert!(
                app.switcher.as_ref().is_some_and(|sw| sw.hold.is_some()),
                "the render path drops the menus while this is set"
            );
        });
    }

    #[gpui::test]
    fn losing_focus_drops_the_hold_so_the_panel_cannot_hang(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 3);
        vcx.simulate_modifiers_change(Modifiers::control());
        app.update_in(&mut vcx, |app, window, cx| app.tab_switch(true, window, cx));

        vcx.deactivate_window();

        app.update(cx, |app, _| {
            let sw = app.switcher.as_ref().expect("the panel stays up");
            assert!(
                sw.hold.is_none(),
                "a release over another window never reaches us"
            );
        });
    }
}

#[cfg(test)]
mod key_tests {
    use gpui::Modifiers;

    use super::{Column, Key, key_intent};

    #[test]
    fn bare_arrows_and_enter_drive_the_panel() {
        let none = Modifiers::none();
        assert_eq!(key_intent("down", none), Key::Step(true));
        assert_eq!(key_intent("up", none), Key::Step(false));
        assert_eq!(key_intent("left", none), Key::ToColumn(Column::Left));
        assert_eq!(key_intent("right", none), Key::ToColumn(Column::Right));
        assert_eq!(key_intent("enter", none), Key::Confirm(false));
        assert_eq!(key_intent("escape", none), Key::Close);
    }

    #[test]
    fn the_secondary_modifier_turns_enter_into_a_new_window() {
        // ⌘ on macOS, Ctrl everywhere else. Off macOS this is the case that a
        // blanket "control means not ours" check would have swallowed.
        assert_eq!(
            key_intent("enter", Modifiers::secondary_key()),
            Key::Confirm(true)
        );
    }

    #[test]
    fn tab_walks_the_tab_column_in_both_directions() {
        assert_eq!(key_intent("tab", Modifiers::none()), Key::Tab(true));
        assert_eq!(key_intent("tab", Modifiers::shift()), Key::Tab(false));
    }

    #[test]
    fn a_held_control_keeps_tab_working_but_parks_the_arrows() {
        // Mid Ctrl+Tab gesture: Tab still steps, but an arrow key is somebody
        // else's chord. Only macOS sees the raw key here — everywhere else
        // Ctrl is the secondary modifier, the chord arrives as the NextTab
        // action instead, and the raw key must fall through untouched.
        let ctrl = Modifiers::control();
        match cfg!(target_os = "macos") {
            true => assert_eq!(key_intent("tab", ctrl), Key::Tab(true)),
            false => assert_eq!(key_intent("tab", ctrl), Key::Pass),
        }
        assert_eq!(key_intent("up", ctrl), Key::Pass);
    }

    #[test]
    fn alt_and_fn_chords_are_left_alone() {
        assert_eq!(key_intent("down", Modifiers::alt()), Key::Pass);
        assert_eq!(key_intent("enter", Modifiers::alt()), Key::Pass);
    }

    #[test]
    fn escape_closes_even_mid_chord() {
        assert_eq!(key_intent("escape", Modifiers::alt()), Key::Close);
        assert_eq!(key_intent("escape", Modifiers::secondary_key()), Key::Close);
    }

    #[test]
    fn the_secondary_glyph_matches_the_platform() {
        let glyph = crate::ui::keymap::secondary_glyph();
        match cfg!(target_os = "macos") {
            true => assert_eq!(glyph, "⌘"),
            false => assert_eq!(glyph, "Ctrl"),
        }
    }
}

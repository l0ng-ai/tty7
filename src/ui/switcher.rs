use std::collections::{HashMap, HashSet};

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
use crate::ui::remote_connect::{self, HostChoice, RemoteWorkspaceRow};
use crate::ui::remote_workspace::{ConnectFlow, MachineStatus, RemoteLinks};

const CARD_W: f32 = 840.0;

/// The create form's card. Narrower than the list — it is a form, not a
/// browser.
const FORM_W: f32 = 480.0;

/// The host dropdown shows about eight rows before it scrolls.
const FORM_LIST_H: f32 = 8.5 * (ROW_H + 8.0);

const LEFT_W: f32 = 340.0;

pub(crate) const CARD_TOP: f32 = 120.0;

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

const ROW_PAD: f32 = 8.0;

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
    /// Raw timestamp behind `when` — what the flat list sorts by.
    last_active: u64,
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

/// A selectable line in the left column: `(group, row)` into `Layout::groups`.
/// The list is flat — one workspace per line, machines told apart by the badge
/// on the row itself — and rendering and keyboard navigation walk the same
/// list so an arrow key can never land somewhere the eye cannot see.
type Nav = (usize, usize);

pub(crate) struct HostSnapshot {
    pub target: RemoteTarget,
    pub rows: Vec<RemoteWorkspaceRow>,
}

/// A pane the daemon runs that no workspace holds — what an interrupted
/// `tty7 run` leaves behind, and what `tty7 pane ls --all` points the CLI's
/// reaper at. The switcher is where a GUI user finds and closes one (#596).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OrphanPane {
    pub pane_id: u64,
    pub title: String,
    pub cwd: Option<String>,
    pub owner: Option<String>,
}

/// Every pane id the local machine's workspaces hold — what the registry
/// listing is measured against to find the orphans (#596).
fn held_local_pane_ids(cx: &App) -> HashSet<u64> {
    crate::ui::machine_mirror::MachineMirrors::machine(cx, crate::core::session::HostId::LOCAL)
        .map(|machine| {
            machine
                .workspaces
                .iter()
                .flat_map(|ws| ws.tabs.iter())
                .flat_map(|tab| tab.root.pane_ids())
                .collect()
        })
        .unwrap_or_default()
}

/// The registry's live panes minus the ones a workspace holds. Dead entries
/// drop out too: a corpse the daemon has not reaped yet is not something the
/// user can act on.
pub(crate) fn orphan_panes_of(
    listed: Vec<tty7_core::daemon::protocol::PaneInfo>,
    held: &HashSet<u64>,
) -> Vec<OrphanPane> {
    listed
        .into_iter()
        .filter(|info| info.alive && !held.contains(&info.pane_id))
        .map(|info| OrphanPane {
            pane_id: info.pane_id,
            title: info.title,
            cwd: info.cwd.map(|p| p.display().to_string()),
            owner: info.owner,
        })
        .collect()
}

/// Which face the card is showing: the workspace list, or the create form.
pub(crate) enum Page {
    List,
    Create(CreateForm),
}

/// The "New Workspace" form: a name prefilled with what the workspace would
/// have called itself anyway, and a host picked from a combobox that folds
/// however many machines are configured into one row.
pub(crate) struct CreateForm {
    name: Entity<InputState>,
    /// The combobox's filter text. Only meaningful while `open`.
    host: Entity<InputState>,
    /// Whether the host dropdown is unfolded.
    open: bool,
    /// Cursor into the dropdown's item list.
    sel: usize,
    /// The picked host. `None` is this computer.
    chosen: Option<HostChoice>,
    /// What the name box was prefilled with. While its value still says this
    /// (or nothing), picking another host refills it; one keystroke of the
    /// user's own and it is theirs.
    prefill: String,
}

/// A create the user asked for on a machine that was not connected yet: the
/// connect has to land first, because only a live link knows the home
/// directory a fresh workspace is rooted at. `finish_connect` consumes it.
pub(crate) struct PendingCreate {
    pub target: RemoteTarget,
    pub name: Option<String>,
}

pub(crate) struct Switcher {
    pub query: Entity<InputState>,
    page: Page,
    renaming: Option<(WorkspaceId, Entity<InputState>)>,
    /// Panes the local daemon runs that no workspace holds (#596). Filled
    /// asynchronously after the panel opens; empty both while the listing is
    /// in flight and when there is nothing to reap.
    orphans: Vec<OrphanPane>,
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
}

/// Everything the panel needs for one frame: the groups (one per machine,
/// still the unit that carries link state and errors), and the flat,
/// most-recently-used-first left column the arrow keys walk.
struct Layout {
    groups: Vec<Group>,
    nav: Vec<Nav>,
}

impl Layout {
    /// Which workspace row the tab column is showing.
    fn subject(&self, sel: usize) -> Option<(usize, usize)> {
        self.nav.get(sel).copied()
    }

    fn subject_row(&self, sel: usize) -> Option<&Row> {
        let (g, r) = self.subject(sel)?;
        self.groups[g].rows.get(r)
    }
}

/// The flat left column: every row the query leaves visible, one line per
/// workspace, most recently used first with this window's own workspace on
/// top. A query matching a machine's name keeps all of that machine's rows —
/// searching "devbox" is how the old per-machine grouping is asked for now.
fn flatten(groups: &[Group], query: &str) -> Vec<Nav> {
    let mut nav: Vec<Nav> = Vec::new();
    for (g, group) in groups.iter().enumerate() {
        let matched_host = group.label.to_lowercase().contains(query);
        for (r, row) in group.rows.iter().enumerate() {
            if query.is_empty() || matched_host || row.matches(query) {
                nav.push((g, r));
            }
        }
    }
    nav.sort_by(|&(ga, ra), &(gb, rb)| {
        let (a, b) = (&groups[ga].rows[ra], &groups[gb].rows[rb]);
        b.current
            .cmp(&a.current)
            .then_with(|| b.last_active.cmp(&a.last_active))
            .then_with(|| a.name.cmp(&b.name))
    });
    nav
}

/// One line of the create form's host dropdown.
enum HostItem {
    /// This computer — first, while it matches the filter.
    Local,
    Host(HostChoice),
    /// Pinned last, filter or no filter: the way out when the machine wanted
    /// is not configured yet.
    AddHost,
}

/// What the form's name box starts out saying: the same codename a workspace
/// created without one would have been given anyway (`fresh_workspace_name` —
/// "quiet-otter"), rolled against the chosen machine so it stays unique
/// there. Editable before it is spent; clearing the box creates a nameless
/// workspace that shows its directory, the old fallback.
fn default_workspace_name(chosen: Option<&HostChoice>, cx: &App) -> String {
    let host = match chosen {
        None => tty7_core::host::HostId::LOCAL,
        Some(choice) => choice.target.host_id(),
    };
    crate::ui::tree_sync::fresh_workspace_name(cx, host)
}

fn host_items(hosts: Vec<HostChoice>, query: &str, local_label: &str) -> Vec<HostItem> {
    let query = query.trim();
    let mut items: Vec<HostItem> = Vec::new();
    if query.is_empty() || crate::ui::palette::fuzzy_score(query, local_label).is_some() {
        items.push(HostItem::Local);
    }
    items.extend(
        remote_connect::filter_hosts(&hosts, query)
            .into_iter()
            .map(HostItem::Host),
    );
    items.push(HostItem::AddHost);
    items
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
                    // it on the first hit so the tab column shows the hits
                    // straight away.
                    if let Some(sw) = this.switcher.as_mut() {
                        sw.left_sel = 0;
                        sw.right_sel = 0;
                    }
                    cx.notify();
                }
            },
        )];
        let (left_scroll, right_scroll) = (gpui::ScrollHandle::new(), gpui::ScrollHandle::new());
        self.switcher = Some(Switcher {
            query,
            page: Page::List,
            renaming: None,
            orphans: Vec::new(),
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
        if let Some(at) = layout
            .nav
            .iter()
            .position(|&(g, r)| layout.groups[g].rows[r].id == here)
            && let Some(sw) = self.switcher.as_mut()
        {
            sw.left_sel = at;
        }
        self.refresh_orphan_panes(cx);
        cx.notify();
    }

    /// List the local daemon's panes and keep the ones no workspace holds.
    /// The query is blocking daemon I/O, so it runs off the UI thread; the
    /// filter runs back on it, where the machine mirror lives.
    ///
    /// Local on purpose: a remote machine's orphans belong to its own daemon,
    /// and routing a listing per host is what the CLI's reaper already does.
    fn refresh_orphan_panes(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let listed = cx
                .background_spawn(async move { tty7_core::client::PaneClient::local().list() })
                .await;
            let listed = match listed {
                Ok(listed) => listed,
                Err(e) => {
                    log::warn!(target: "tty7::switcher", "orphan pane listing failed: {e}");
                    return;
                }
            };
            let _ = this.update(cx, |this, cx| {
                let held = held_local_pane_ids(cx);
                if let Some(sw) = this.switcher.as_mut() {
                    sw.orphans = orphan_panes_of(listed, &held);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Hang up one orphan pane and show what is left. The kill is
    /// fire-and-forget, so the refresh that follows is also the confirmation:
    /// a pane that survived it simply stays on the list.
    fn close_orphan_pane(&mut self, pane_id: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let listed = cx
                .background_spawn(async move {
                    let client = tty7_core::client::PaneClient::local();
                    client.kill(pane_id)?;
                    client.list()
                })
                .await;
            let listed = match listed {
                Ok(listed) => listed,
                Err(e) => {
                    log::warn!(target: "tty7::switcher", "closing orphan %{pane_id} failed: {e}");
                    return;
                }
            };
            let _ = this.update(cx, |this, cx| {
                let held = held_local_pane_ids(cx);
                if let Some(sw) = this.switcher.as_mut() {
                    sw.orphans = orphan_panes_of(listed, &held);
                }
                cx.notify();
            });
        })
        .detach();
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
                        error: None,
                        installing: None,
                        preempted: false,
                        parked: false,
                        rows: Vec::new(),
                    });
                    groups.len() - 1
                });
                // A row's path is on the workspace's own machine, so that is
                // the machine whose home may shorten it (#580).
                let home = crate::ui::path_display::home_for_host(app, w.host_id());
                groups[slot].rows.push(Row {
                    id: w.id,
                    name: crate::ui::machine_mirror::display_name(app, w)
                        .unwrap_or_else(|| t(L10nKey::WindowUntitled).to_string()),
                    path: crate::ui::machine_mirror::subject_path(app, w)
                        .map(|p| {
                            crate::ui::home::display_path(std::path::Path::new(&p), home.as_deref())
                        })
                        .unwrap_or_default(),
                    when: crate::ui::home::relative_time(now, w.last_active),
                    last_active: w.last_active,
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
                    last_active: now,
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
            // Unclaimed *local* workspaces: their paths are on this machine,
            // so this machine's home is the right one to measure them by.
            let local_home = crate::ui::path_display::local_home();
            let rows: Vec<Row> = crate::ui::machine_mirror::unclaimed_local_workspaces(app)
                .into_iter()
                .filter(|ws| !listed.contains(&ws.id))
                .map(|ws| Row {
                    id: ws.id,
                    name: ws.name,
                    path: ws
                        .path
                        .map(|p| {
                            crate::ui::home::display_path(
                                std::path::Path::new(&p),
                                local_home.as_deref(),
                            )
                        })
                        .unwrap_or_default(),
                    when: crate::ui::home::relative_time(now, ws.last_active),
                    last_active: ws.last_active,
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

        // Row order is `flatten`'s business now — the left column is one flat
        // most-recently-used list. Groups keep local-first order only so the
        // trouble banners under the list come out in a stable order.
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
                            .and_then(|leaf| {
                                let leaf = leaf.read(cx);
                                Some((leaf.cwd()?, leaf.display_home(cx)))
                            })
                            .map(|(p, home)| crate::ui::home::display_path(&p, home.as_deref()))
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
        // These rows describe a workspace on `host`, and the cwds they carry
        // are that machine's. Only its home may shorten them (#580).
        let home = host.and_then(|host| crate::ui::path_display::home_for_host(cx, host));
        views
            .into_iter()
            .enumerate()
            .map(|(i, v)| TabRow {
                label: tab_view_label(&v, i, home.as_deref()),
                // The label only stands in for the path when it came *from* the
                // path; a name or an agent leaves the location still worth
                // printing.
                named: v.name.as_deref().is_some_and(|n| !n.trim().is_empty()) || v.agent.is_some(),
                path: v
                    .cwd
                    .as_deref()
                    .map(|p| {
                        crate::ui::home::display_path(std::path::Path::new(p), home.as_deref())
                    })
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

    /// Builds one frame's worth of panel: the groups, and the flat left
    /// column the arrow keys walk.
    fn switcher_layout(&self, cx: &mut Context<Self>) -> Layout {
        let groups = self.switcher_groups(cx);
        let query = self
            .switcher
            .as_ref()
            .map(|sw| sw.text(cx))
            .unwrap_or_default();
        let nav = flatten(&groups, &query);
        Layout { groups, nav }
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
        // A create still waiting on this machine's link dies with the link:
        // calling the connect off is calling the create off, or the next
        // successful connect would grow a workspace nobody asked it for.
        // One parked on a dialect refusal is waiting all the same, so it
        // dies here too.
        if self
            .pending_create
            .as_ref()
            .is_some_and(|p| &p.target == target)
        {
            self.pending_create = None;
        }
        if self
            .parked_create
            .as_ref()
            .is_some_and(|p| &p.target == target)
        {
            self.parked_create = None;
        }
        cx.notify();
    }

    /// `⌘⇧N` and the footer button. Raises the switcher if it is down and
    /// flips the card to the create form, host defaulting to this computer —
    /// so `⌘⇧N` then Enter is still "a fresh local workspace", two keys.
    pub(crate) fn open_workspace_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.is_none() {
            self.open_switcher(window, cx);
        }
        self.switcher_to_form(None, window, cx);
    }

    fn switcher_to_form(
        &mut self,
        chosen: Option<HostChoice>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prefill = default_workspace_name(chosen.as_ref(), cx);
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t(L10nKey::SwitcherFormNamePlaceholder))
                .default_value(prefill.clone())
        });
        let host = cx.new(|cx| InputState::new(window, cx).placeholder(t(L10nKey::FilterHosts)));
        // Retyping the filter moves the dropdown cursor back onto the first hit.
        let sub = cx.subscribe_in(
            &host,
            window,
            |this, _input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change)
                    && let Some(sw) = this.switcher.as_mut()
                    && let Page::Create(form) = &mut sw.page
                {
                    form.sel = 0;
                    cx.notify();
                }
            },
        );
        name.update(cx, |state, cx| state.focus(window, cx));
        if let Some(sw) = self.switcher.as_mut() {
            sw.page = Page::Create(CreateForm {
                name,
                host,
                open: false,
                sel: 0,
                chosen,
                prefill,
            });
            sw._subs.push(sub);
        }
        cx.notify();
    }

    fn switcher_back_to_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sw) = self.switcher.as_mut() {
            sw.page = Page::List;
            let query = sw.query.clone();
            query.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    /// The host dropdown's lines for the filter text currently in the box.
    fn form_items(&self, form: &CreateForm, cx: &App) -> Vec<HostItem> {
        let query = form.host.read(cx).value().trim().to_string();
        host_items(
            remote_connect::available_hosts(cx),
            &query,
            t(L10nKey::SwitcherThisComputer),
        )
    }

    fn switcher_form_open_hosts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sw) = self.switcher.as_mut()
            && let Page::Create(form) = &mut sw.page
        {
            form.open = true;
            form.sel = 0;
            let host = form.host.clone();
            host.update(cx, |state, cx| {
                state.set_value("", window, cx);
                state.focus(window, cx);
            });
        }
        cx.notify();
    }

    fn switcher_form_close_hosts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sw) = self.switcher.as_mut()
            && let Page::Create(form) = &mut sw.page
        {
            form.open = false;
            let name = form.name.clone();
            name.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    fn switcher_form_pick(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let item = {
            let Some(sw) = self.switcher.as_ref() else {
                return;
            };
            let Page::Create(form) = &sw.page else {
                return;
            };
            let mut items = self.form_items(form, cx);
            if at >= items.len() {
                return;
            }
            items.swap_remove(at)
        };
        match item {
            HostItem::Local => self.switcher_form_choose(None, window, cx),
            HostItem::Host(choice) => self.switcher_form_choose(Some(choice), window, cx),
            HostItem::AddHost => {
                self.close_switcher(window, cx);
                self.open_new_ssh_host(window, cx);
            }
        }
    }

    fn switcher_form_choose(
        &mut self,
        chosen: Option<HostChoice>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prefill = default_workspace_name(chosen.as_ref(), cx);
        if let Some(sw) = self.switcher.as_mut()
            && let Page::Create(form) = &mut sw.page
        {
            form.chosen = chosen;
            form.open = false;
            form.sel = 0;
            let (host, name) = (form.host.clone(), form.name.clone());
            // A name the user has not touched follows the host; one they have
            // is theirs and stays.
            let untouched = {
                let value = name.read(cx).value().trim().to_string();
                value.is_empty() || value == form.prefill
            };
            if untouched {
                form.prefill = prefill.clone();
                name.update(cx, |state, cx| state.set_value(&prefill, window, cx));
            }
            host.update(cx, |state, cx| state.set_value("", window, cx));
            name.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    /// Enter on the form. Local and already-connected machines create on the
    /// spot; a machine with no live link has to connect first — the create is
    /// parked on the app and `finish_connect` completes it, because only a
    /// live link knows the home directory a fresh workspace is rooted at.
    fn switcher_form_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (name, chosen) = {
            let Some(sw) = self.switcher.as_ref() else {
                return;
            };
            let Page::Create(form) = &sw.page else {
                return;
            };
            let name = form.name.read(cx).value().trim().to_string();
            ((!name.is_empty()).then_some(name), form.chosen.clone())
        };
        match chosen {
            None => {
                self.close_switcher(window, cx);
                self.switch_workspace(None, window, cx);
                self.name_fresh_workspace(name, window, cx);
            }
            Some(choice) => match remote_connect::HostLinks::home(cx, choice.target.host_id()) {
                Some(home) => {
                    self.close_switcher(window, cx);
                    self.create_remote_workspace(choice.target.clone(), home, window, cx);
                    self.name_fresh_workspace(name, window, cx);
                }
                None => {
                    self.pending_create = Some(PendingCreate {
                        target: choice.target.clone(),
                        name,
                    });
                    // Back to the list, where the connect has somewhere to be
                    // seen while it works.
                    self.switcher_back_to_list(window, cx);
                    self.connect_to_host(choice, cx);
                }
            },
        }
    }

    /// Names the workspace this window just created and switched to.
    pub(crate) fn name_fresh_workspace(
        &mut self,
        name: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = name else {
            return;
        };
        crate::ui::tree_sync::name_new_workspace(cx, self.workspace, name);
        crate::ui::windows::refresh_menu(cx);
        self.sync_window_title(window, cx);
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
        if matches!(sw.page, Page::Create(_)) {
            self.on_form_key(ev, window, cx);
            return;
        }
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

    /// The create form's keys. Characters fall through to whichever input is
    /// focused; everything structural is decided here, ahead of the inputs'
    /// own bindings, so it has to stop propagating whatever it handles.
    fn on_form_key(
        &mut self,
        ev: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (open, sel, n) = {
            let Some(sw) = self.switcher.as_ref() else {
                return;
            };
            let Page::Create(form) = &sw.page else {
                return;
            };
            (form.open, form.sel, self.form_items(form, cx).len())
        };
        let (key, mods) = (ev.keystroke.key.as_str(), ev.keystroke.modifiers);
        let bare = !mods.alt && !mods.function && !mods.control && !mods.secondary();
        match key {
            // The dropdown is the smaller thing to back out of; the form only
            // folds back into the list on a second press.
            "escape" => {
                cx.stop_propagation();
                match open {
                    true => self.switcher_form_close_hosts(window, cx),
                    false => self.switcher_back_to_list(window, cx),
                }
            }
            "tab" if bare || mods.shift => {
                cx.stop_propagation();
                match open {
                    true => self.switcher_form_close_hosts(window, cx),
                    false => self.switcher_form_open_hosts(window, cx),
                }
            }
            "up" | "down" if bare => {
                cx.stop_propagation();
                match open {
                    true if n > 0 => {
                        if let Some(sw) = self.switcher.as_mut()
                            && let Page::Create(form) = &mut sw.page
                        {
                            form.sel = step(sel.min(n - 1), n, key == "down");
                        }
                        cx.notify();
                    }
                    false if key == "down" => self.switcher_form_open_hosts(window, cx),
                    _ => {}
                }
            }
            "enter" if bare => {
                cx.stop_propagation();
                match open {
                    true => self.switcher_form_pick(sel, window, cx),
                    false => self.switcher_form_create(window, cx),
                }
            }
            _ => {}
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
        if let Some(&(g, r)) = layout.nav.get(sel) {
            let group = &layout.groups[g];
            let row = RowRef::of(group, &group.rows[r]);
            self.switcher_open(row, new_window, window, cx);
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
        let scrim = crate::ui::presets::scrim_fill(cx);
        let card = match &sw.page {
            Page::Create(form) => self.render_create_card(form, window, cx),
            Page::List => self.render_list_card(window, cx),
        };
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

    fn render_list_card(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let sw = self.switcher.as_ref().expect("only rendered while up");
        let (sel, column) = (sw.left_sel, sw.column);
        let (left_scroll, right_scroll) = (sw.left_scroll.clone(), sw.right_scroll.clone());
        let layout = self.switcher_layout(cx);

        let theme = cx.theme();
        let (border, card_bg) = (theme.border, theme.popover);

        let mut list = v_flex().gap(px(1.));
        for (at, &(g, r)) in layout.nav.iter().enumerate() {
            let picked = sel == at && column == Column::Left;
            let group = &layout.groups[g];
            list = list.child(self.render_row(group, &group.rows[r], picked, Some(at), cx));
        }
        if layout.nav.is_empty() {
            list = list.child(
                div()
                    .px(px(ROW_PAD))
                    .py(px(14.))
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t(L10nKey::SwitcherNoMatch)),
            );
        }
        // Orphan panes belong to the machine, not to a workspace, so they are
        // not rows and join no navigation — the bottom of the workspace list
        // is simply where a user looking for them finds them (#596). A search
        // narrows the panel to workspaces, and the box steps out of the way
        // for one.
        if !sw.orphans.is_empty() && sw.text(cx).is_empty() {
            list = list.child(self.render_orphan_panes(cx));
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

        v_flex()
            .w(px(card_w))
            .bg(card_bg)
            .border_1()
            .border_color(border)
            .rounded(px(10.))
            .shadow_xl()
            .overflow_hidden()
            .child(self.render_search(cx))
            .child(body)
            .children(self.render_banners(&layout, cx))
            .child(self.render_footer(cx))
            .into_any_element()
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
                    .id("switcher-new-workspace")
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
                    .child(t(L10nKey::AppMenuNewWorkspace))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.switcher_to_form(None, window, cx);
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

    /// The machine-trouble bands between the list and the footer: install
    /// progress, a failed connect with its retry, a parked route (#485), or a
    /// connect still in flight. With the per-machine headers gone this is
    /// where a machine's state gets to speak; a healthy machine says nothing
    /// here — its state dot rides on its workspace rows.
    fn render_banners(&self, layout: &Layout, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out: Vec<AnyElement> = Vec::new();
        for group in &layout.groups {
            if group.target.is_none() {
                continue;
            }
            if let Some(phase) = group.installing {
                out.push(
                    self.render_install_progress(&group.label, phase, cx)
                        .into_any_element(),
                );
            } else if group.parked
                && !self.parked_dismissed.contains(&group.key)
                && !group.rows.is_empty()
            {
                out.push(self.render_parked_notice(group, cx).into_any_element());
            } else if let Some(error) = group.error.clone() {
                out.push(self.render_error_band(group, &error, cx));
            } else if matches!(group.link, Link::Connecting | Link::Reconnecting { .. }) {
                let theme = cx.theme();
                out.push(
                    h_flex()
                        .items_center()
                        .gap(px(6.))
                        .px(px(12.))
                        .py(px(6.))
                        .border_t_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex_shrink_0()
                                .size(px(6.))
                                .rounded_full()
                                .bg(theme.warning),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(t_fmt(
                                    L10nKey::SwitcherConnectingTo,
                                    &[("machine", &group.label)],
                                )),
                        )
                        .into_any_element(),
                );
            }
        }
        out
    }

    fn render_error_band(&self, group: &Group, error: &str, cx: &mut Context<Self>) -> AnyElement {
        let retry = GroupRef::of(group);
        let replace = retry.clone();
        let retry_key = group.key.clone();
        let replace_key = group.key.clone();
        let dismiss_key = group.key.clone();
        let dismiss_target = group.target.clone();
        // The band no longer sits under a machine header, so plain errors
        // carry the machine's name themselves; the dialect restatement
        // already names it.
        let shown = remote_connect::dialect_complaint(error, &group.label)
            .unwrap_or_else(|| format!("{}: {error}", group.label));
        let replace_action = crate::ui::remote_workspace::mismatch_action_key(error);
        let theme = cx.theme();
        v_flex()
            .gap(px(4.))
            .px(px(12.))
            .py(px(8.))
            .border_t_1()
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
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
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
                            },
                        )),
                    )
                    .when(
                        crate::daemon::control::is_dialect_refusal(error)
                            // Same gate as the workspace strip's: a machine
                            // whose server is not ours to install cannot be
                            // helped by this button, and a click that can only
                            // fail is worse than no button.
                            && replace
                                .target
                                .as_ref()
                                .is_some_and(|t| t.hosts_our_server()),
                        |row| {
                            row.child(
                                Button::new(gpui::SharedString::from(format!(
                                    "switcher-replace:{}",
                                    group.key
                                )))
                                .label(t(replace_action))
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.remote_host_errors.remove(&replace_key);
                                        if let Some(target) = replace.target.clone() {
                                            this.confirm_replace_remote_server(
                                                target,
                                                replace.label.clone(),
                                                replace_action,
                                                window,
                                                cx,
                                            );
                                        }
                                    },
                                )),
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
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                this.remote_host_errors.remove(&dismiss_key);
                                // The other half of this block can come from a
                                // failed connect. Retire that too, but only when
                                // it is this host's failure — a connect to
                                // anywhere else is still in flight.
                                if let Some(ConnectFlow::Failed { choice, .. }) = &this.connect
                                    && Some(&choice.target) == dismiss_target.as_ref()
                                {
                                    this.connect = None;
                                }
                                // Dismissing the refusal is giving up on the
                                // machine, and the create parked on it goes
                                // with it — an update run weeks later must
                                // not revive a workspace nobody remembers
                                // asking for.
                                if this
                                    .parked_create
                                    .as_ref()
                                    .is_some_and(|p| Some(&p.target) == dismiss_target.as_ref())
                                {
                                    this.parked_create = None;
                                }
                                cx.notify();
                            },
                        )),
                    ),
            )
            .into_any_element()
    }

    /// The orphan block under the local group (#596): one line per pane no
    /// window holds — id, owner, where it runs — and the way to stop it.
    fn render_orphan_panes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let Some(switcher) = self.switcher.as_ref() else {
            return v_flex();
        };
        let mut list = v_flex().gap(px(2.));
        for orphan in &switcher.orphans {
            let mut bits = vec![format!("%{}", orphan.pane_id)];
            if let Some(owner) = &orphan.owner {
                bits.push(owner.clone());
            }
            if let Some(cwd) = &orphan.cwd {
                bits.push(cwd.clone());
            } else if !orphan.title.is_empty() {
                bits.push(orphan.title.clone());
            }
            let pane_id = orphan.pane_id;
            list = list.child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.foreground)
                            .child(bits.join(" · ")),
                    )
                    .child(
                        Button::new(gpui::SharedString::from(format!(
                            "switcher-close-orphan:{pane_id}"
                        )))
                        .label(t(L10nKey::Close))
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                this.close_orphan_pane(pane_id, cx);
                            },
                        )),
                    ),
            );
        }
        v_flex()
            .gap(px(4.))
            .mx(px(4.))
            .mt(px(6.))
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
                    .child(t(L10nKey::SwitcherOrphanPanes)),
            )
            .child(list)
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
            .px(px(12.))
            .py(px(8.))
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "{} — {}",
                        group.label,
                        t(L10nKey::RemoteRouteParkedHint)
                    )),
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
        label: &str,
        phase: InstallPhase,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let accent = theme.warning;
        let fraction = phase.fraction().unwrap_or(0.0);
        let caption = crate::ui::remote_workspace::install_phase_caption(phase);

        v_flex()
            .gap(px(6.))
            .px(px(12.))
            .py(px(8.))
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("{label} — {caption}")),
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
        let gref = GroupRef::of(group);
        let menu_host = gref.clone();
        let ctx_host = gref;
        let app = cx.entity().downgrade();
        let app2 = app.clone();
        let key = row.id.element_key() as usize;
        let holding = self.switcher.as_ref().is_some_and(|sw| sw.hold.is_some());
        // A machine with no link cannot show this row's panes right now; the
        // row stays, muted, and opening it is what asks for the connection.
        let unlit = group.target.is_some() && matches!(group.link, Link::Offline | Link::Failed);

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
        // the trailing pieces straight out over the divider. The second line
        // leads with the machine the workspace lives on — the flat list's only
        // grouping — with its link state as the dot's color.
        let when_path = match row.path.is_empty() {
            true => row.when.clone(),
            false => format!("{} · {}", row.path, row.when),
        };
        let host_dot: Option<gpui::Hsla> = match group.link {
            Link::Local => None,
            Link::Connected if group.preempted => Some(warn),
            Link::Connected => Some(gpui::rgb(crate::ui::tab_strip::LIVE_DOT).into()),
            Link::Connecting | Link::Reconnecting { .. } => Some(warn),
            Link::Failed => Some(theme.danger),
            Link::Offline => Some(gpui::rgb(crate::ui::tab_strip::UNKNOWN_DOT).into()),
        };
        let host_label = match group.target.is_some() {
            true => group.label.clone(),
            false => t(L10nKey::SwitcherLocalHost).to_string(),
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
                            .text_color(match unlit {
                                true => muted,
                                false => fg,
                            })
                            .child(row.name.clone()),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(5.))
                            .min_w_0()
                            .text_xs()
                            .text_color(dim)
                            .child(match host_dot {
                                Some(color) => div()
                                    .flex_shrink_0()
                                    .size(px(6.))
                                    .rounded_full()
                                    .bg(color)
                                    .into_any_element(),
                                None => gpui::svg()
                                    .path("icons/machine-local.svg")
                                    .flex_shrink_0()
                                    .size(px(10.))
                                    .text_color(dim)
                                    .into_any_element(),
                            })
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .max_w(px(120.))
                                    .truncate()
                                    .text_color(muted)
                                    .child(host_label),
                            )
                            .when(!when_path.is_empty(), |line| {
                                line.child(div().flex_shrink_0().child("·"))
                                    .child(div().min_w_0().truncate().child(when_path))
                            }),
                    ),
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
                            row_menu(menu, &menu_ref, &menu_host, app.clone())
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

        // A held Ctrl makes every click a right click on macOS. Drop the menu
        // and take the right-button press as the pick instead, or clicking
        // during the gesture does nothing at all.
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
                .context_menu(move |menu, _window, _cx| {
                    row_menu(menu, &ctx_ref, &ctx_host, app2.clone())
                })
                .into_any_element(),
        }
    }

    /// The "New Workspace" form: a name, and one combobox row that folds
    /// however many machines are configured into a searchable dropdown.
    fn render_create_card(
        &self,
        form: &CreateForm,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (border, card_bg) = (theme.border, theme.popover);
        let (fg, muted, dim) = (
            theme.foreground,
            theme.muted_foreground,
            theme.muted_foreground.opacity(0.7),
        );
        let sf = rungs(cx);
        let (hover, picked_bg) = (gpui::rgb(sf.hover), gpui::rgb(sf.pressed));
        let viewport = window.viewport_size();
        let card_w = FORM_W
            .min(viewport.width.as_f32() - 2. * CARD_MARGIN)
            .max(320.);

        let chosen_label = form
            .chosen
            .as_ref()
            .map(|h| h.label.clone())
            .unwrap_or_else(|| t(L10nKey::SwitcherThisComputer).to_string());
        let chosen_local = form.chosen.is_none();
        let chosen_dot: Option<gpui::Hsla> = form.chosen.as_ref().map(|h| {
            match remote_connect::HostLinks::get(cx, h.target.host_id()).is_some() {
                true => gpui::rgb(crate::ui::tab_strip::LIVE_DOT).into(),
                false => gpui::rgb(crate::ui::tab_strip::UNKNOWN_DOT).into(),
            }
        });

        let header = h_flex()
            .items_center()
            .gap(px(6.))
            .h(px(42.))
            .px(px(8.))
            .border_b_1()
            .border_color(border)
            .child(
                crate::ui::tab_strip::hit_target(
                    Button::new("switcher-form-back")
                        .icon(IconName::ChevronLeft)
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.switcher_back_to_list(window, cx);
                        })),
                )
                .tooltip(t(L10nKey::SwitcherFormBack)),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(fg)
                    .child(t(L10nKey::AppMenuNewWorkspace)),
            );

        let label_col = |text: &'static str| {
            div()
                .w(px(52.))
                .flex_shrink_0()
                .text_sm()
                .text_color(muted)
                .child(text)
        };
        let field = |inner: gpui::Div| {
            inner
                .flex_1()
                .min_w_0()
                .items_center()
                .gap(px(6.))
                .px(px(8.))
                .h(px(30.))
                .rounded(px(6.))
                .border_1()
                .border_color(border)
        };

        let name_row = h_flex()
            .items_center()
            .gap(px(8.))
            .child(label_col(t(L10nKey::SwitcherFormName)))
            .child(field(h_flex()).child(Input::new(&form.name).appearance(false).small()));

        // The trigger: the picked host while folded, the filter box while
        // open. One row either way, so the form does not jump.
        let host_glyph = |local: bool, dot: Option<gpui::Hsla>| match dot {
            Some(color) => div()
                .flex_shrink_0()
                .size(px(6.))
                .rounded_full()
                .bg(color)
                .into_any_element(),
            None if local => gpui::svg()
                .path("icons/machine-local.svg")
                .flex_shrink_0()
                .size(px(12.))
                .text_color(muted)
                .into_any_element(),
            None => gpui::svg()
                .path("icons/machine-remote.svg")
                .flex_shrink_0()
                .size(px(12.))
                .text_color(muted)
                .into_any_element(),
        };
        let trigger: AnyElement = match form.open {
            false => field(h_flex())
                .id("switcher-form-host")
                .cursor_pointer()
                .hover(move |r| r.bg(hover))
                .child(host_glyph(chosen_local, chosen_dot))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(fg)
                        .child(chosen_label),
                )
                .child(
                    Icon::new(IconName::ChevronDown)
                        .size(px(ICON))
                        .text_color(dim),
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.switcher_form_open_hosts(window, cx);
                }))
                .into_any_element(),
            true => field(h_flex())
                .child(Icon::new(IconName::Search).size(px(14.)).text_color(muted))
                .child(Input::new(&form.host).appearance(false).small())
                .into_any_element(),
        };

        let mut host_block = v_flex().flex_1().min_w_0().gap(px(4.)).child(trigger);
        if form.open {
            let items = self.form_items(form, cx);
            let mut list = v_flex().gap(px(1.)).p(px(4.));
            for (i, item) in items.iter().enumerate() {
                let picked = i == form.sel;
                let base = h_flex()
                    .id(("switcher-form-item", i))
                    .items_center()
                    .gap(px(8.))
                    .min_h(px(ROW_H))
                    .py(px(4.))
                    .px(px(ROW_PAD))
                    .rounded(px(6.))
                    .overflow_hidden()
                    .cursor_pointer()
                    .when(picked, |r| r.bg(picked_bg))
                    .hover(move |r| r.bg(hover))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.switcher_form_pick(i, window, cx);
                    }));
                let line = match item {
                    HostItem::Local => base
                        .child(host_glyph(true, None))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .text_color(fg)
                                .child(t(L10nKey::SwitcherThisComputer)),
                        )
                        .into_any_element(),
                    HostItem::Host(host) => {
                        let lit =
                            remote_connect::HostLinks::get(cx, host.target.host_id()).is_some();
                        base.child(host_glyph(
                            false,
                            Some(match lit {
                                true => gpui::rgb(crate::ui::tab_strip::LIVE_DOT).into(),
                                false => gpui::rgb(crate::ui::tab_strip::UNKNOWN_DOT).into(),
                            }),
                        ))
                        .child(
                            div()
                                .flex_shrink_0()
                                .max_w(px(180.))
                                .truncate()
                                .text_sm()
                                .text_color(fg)
                                .child(host.label.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(dim)
                                .child(host.detail.clone()),
                        )
                        .into_any_element()
                    }
                    HostItem::AddHost => base
                        .border_t_1()
                        .border_color(border)
                        .rounded_none()
                        .child(Icon::new(IconName::Plus).size(px(14.)).text_color(dim))
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(t(L10nKey::AddSshHost)),
                        )
                        .into_any_element(),
                };
                list = list.child(line);
            }
            host_block = host_block.child(
                div().rounded(px(6.)).border_1().border_color(border).child(
                    div()
                        .id("switcher-form-hosts")
                        .max_h(px(FORM_LIST_H))
                        .overflow_y_scroll()
                        .child(list),
                ),
            );
        }

        let host_row = h_flex()
            .items_start()
            .gap(px(8.))
            .child(
                label_col(t(L10nKey::SwitcherFormHost))
                    .h(px(30.))
                    .flex()
                    .items_center(),
            )
            .child(host_block);

        let footer = h_flex()
            .items_center()
            .px(px(12.))
            .py(px(8.))
            .border_t_1()
            .border_color(border)
            .text_xs()
            .text_color(dim)
            .child(match form.open {
                true => t(L10nKey::SwitcherFormPickHint),
                false => t(L10nKey::SwitcherFormCreateHint),
            });

        v_flex()
            .w(px(card_w))
            .bg(card_bg)
            .border_1()
            .border_color(border)
            .rounded(px(10.))
            .shadow_xl()
            .overflow_hidden()
            .child(header)
            .child(
                v_flex()
                    .p(px(12.))
                    .gap(px(10.))
                    .child(name_row)
                    .child(host_row),
            )
            .child(footer)
            .into_any_element()
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
            // A remote workspace this client has never opened has no tab
            // mirror to read — "no tabs" would be a claim nobody checked.
            let unseen = row.adopt.is_some() || (row.remote_id.is_some() && !row.open);
            return note(match unseen {
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
/// tab is named by its live terminal's OSC title, which shells set to the
/// working directory and agents overwrite with what they are doing. The tree
/// carries a copy of that title (`PaneRecord::osc_title`), which is what makes
/// the two columns agree; `PaneRecord::title` is the *foreground process name*
/// ("zsh") and only stands in when there is no title at all.
fn tab_view_label(
    view: &crate::ui::machine_mirror::TabView,
    index: usize,
    home: Option<&std::path::Path>,
) -> String {
    let unnamed = || {
        t_fmt(
            L10nKey::TabUnnamedShell,
            &[("n", &((index + 1).to_string()))],
        )
    };
    // A path can shorten away to nothing (a bare "user@host:"), and the process
    // name is still worth more than a number.
    let shortened = |raw: &str| match crate::ui::tab_strip::short_title(raw, home) {
        shortened if !shortened.trim().is_empty() => shortened,
        _ => match view.title.trim() {
            "" => unnamed(),
            title => title.to_string(),
        },
    };
    match view.label() {
        crate::ui::machine_mirror::TabLabel::Named(name) => name.to_string(),
        // Through `short_title` because the local strip puts its own titles
        // through it too: the shell integration writes `user@host:~/dir`, and a
        // tab that spelled that out in full where the strip says "…/dir" would
        // be the same disagreement in a new place.
        crate::ui::machine_mirror::TabLabel::Osc(title) => shortened(title),
        crate::ui::machine_mirror::TabLabel::Agent(agent) => agent.display_name().to_string(),
        crate::ui::machine_mirror::TabLabel::Cwd(cwd) => shortened(cwd),
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
                last_active: r.last_active,
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
    label: String,
    target: Option<RemoteTarget>,
    link: Link,
}

impl GroupRef {
    fn of(g: &Group) -> Self {
        Self {
            label: g.label.clone(),
            target: g.target.clone(),
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

/// What the machine menu's host row says, or `None` for a machine that has no
/// SSH host behind it at all — WSL and the local stdio server are configured
/// nowhere this form could edit.
fn host_form_label(target: &RemoteTarget) -> Option<&'static str> {
    match target {
        RemoteTarget::Profile { .. } => Some(t(L10nKey::SwitcherEditHost)),
        RemoteTarget::Alias { .. } | RemoteTarget::Direct { .. } => {
            Some(t(L10nKey::SwitcherSaveAsHost))
        }
        RemoteTarget::Wsl { .. } | RemoteTarget::LocalStdio { .. } => None,
    }
}

fn row_menu(
    menu: gpui_component::menu::PopupMenu,
    row: &RowRef,
    host: &GroupRef,
    app: gpui::WeakEntity<Tty7App>,
) -> gpui_component::menu::PopupMenu {
    let (a1, a2, a3, a4) = (app.clone(), app.clone(), app.clone(), app.clone());
    let (id, adopt) = (row.id, row.adopt.is_some());
    let stoppable = row.live;
    // Every workspace verb addresses the row by its local id, and a remote
    // this client has never adopted has none yet. That greyed them all out at
    // once, which reads as broken rather than as not-yet. Say what would make
    // them work, the way the tab pane already says it for the same rows.
    let menu = match adopt {
        true => menu.item(PopupMenuItem::label(t(L10nKey::SwitcherOpenToManage))),
        false => menu
            .item(
                PopupMenuItem::new(t(L10nKey::SwitcherRename)).on_click(move |_, window, cx| {
                    let _ = a1.update(cx, |this, cx| this.switcher_rename(id, window, cx));
                }),
            )
            .item(
                PopupMenuItem::new(t(L10nKey::SwitcherOpenInNewWindow)).on_click(
                    move |_, window, cx| {
                        let _ = a2.update(cx, |this, cx| {
                            this.close_switcher(window, cx);
                            crate::ui::windows::open(cx, Some(id));
                        });
                    },
                ),
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
                PopupMenuItem::new(t(L10nKey::AppMenuDeleteWorkspace)).on_click(
                    move |_, window, cx| {
                        let _ = a4.update(cx, |this, cx| {
                            this.close_switcher(window, cx);
                            this.delete_workspace(id, window, cx);
                        });
                    },
                ),
            ),
    };
    host_menu(menu, host, app)
}

/// The machine verbs that used to live on the group headers, now appended
/// under the machine's own name to every one of its rows. Local rows carry
/// none — the footer's New Workspace covers this computer.
fn host_menu(
    menu: gpui_component::menu::PopupMenu,
    host: &GroupRef,
    app: gpui::WeakEntity<Tty7App>,
) -> gpui_component::menu::PopupMenu {
    let Some(target) = host.target.clone() else {
        return menu;
    };
    let (a1, a2, a3, a4) = (app.clone(), app.clone(), app.clone(), app);
    let menu = menu
        .separator()
        .item(PopupMenuItem::label(host.label.clone()));
    let engaged = link_is_engaged(host.link);
    let create_choice = HostChoice {
        target: target.clone(),
        label: host.label.clone(),
        detail: String::new(),
    };
    let menu = menu.item(
        PopupMenuItem::new(t(L10nKey::AppMenuNewWorkspace)).on_click(move |_, window, cx| {
            let _ = a1.update(cx, |this, cx| {
                this.switcher_to_form(Some(create_choice.clone()), window, cx);
            });
        }),
    );
    // The host as it is configured, reachable from the one place it is on
    // screen. A machine dialled by address has no profile yet, so the same
    // row offers to make one instead (#438).
    let menu = match host_form_label(&target) {
        Some(label) => {
            let for_edit = target.clone();
            menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                let for_edit = for_edit.clone();
                let _ = a4.update(cx, |this, cx| {
                    this.close_switcher(window, cx);
                    this.edit_ssh_host_of_target(&for_edit, window, cx);
                });
            }))
        }
        None => menu,
    };
    let menu = match engaged {
        true => {
            let for_disconnect = target.clone();
            menu.item(PopupMenuItem::new(t(L10nKey::SwitcherDisconnect)).on_click(
                move |_, _window, cx| {
                    let _ = a2.update(cx, |this, cx| this.switcher_disconnect(&for_disconnect, cx));
                },
            ))
        }
        false => {
            let connect_choice = HostChoice {
                target: target.clone(),
                label: host.label.clone(),
                detail: String::new(),
            };
            menu.item(
                PopupMenuItem::new(t(L10nKey::Connect))
                    .disabled(matches!(host.link, Link::Connecting))
                    .on_click(move |_, _window, cx| {
                        let _ = a2.update(cx, |this, cx| {
                            this.connect_to_host(connect_choice.clone(), cx)
                        });
                    }),
            )
        }
    };
    if !target.hosts_our_server() {
        return menu;
    }
    let (label, for_restart) = (host.label.clone(), target);
    menu.item(
        PopupMenuItem::new(t(L10nKey::AppMenuRestartServer)).on_click(move |_, window, cx| {
            let _ = a3.update(cx, |this, cx| {
                this.confirm_restart_remote_server(for_restart.clone(), label.clone(), window, cx);
            });
        }),
    )
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

    /// A wrong hostname or a stale password used to be fixable only by
    /// finding the same machine again in Settings (#438). The machine is on
    /// screen here, so its host row is too — worded for what the row can
    /// actually do, since a machine reached by address has no profile to open.
    #[test]
    fn every_ssh_machine_offers_its_host_form_and_nothing_else_does() {
        crate::ui::i18n::set_locale("en");
        assert_eq!(
            host_form_label(&RemoteTarget::Profile {
                id: uuid::Uuid::new_v4()
            }),
            Some("Edit Host…")
        );
        assert_eq!(
            host_form_label(&RemoteTarget::Alias {
                alias: "prod".into()
            }),
            Some("Save as SSH Host…"),
            "an alias lives in ~/.ssh/config, which this form does not write"
        );
        assert_eq!(
            host_form_label(&RemoteTarget::direct("me", "10.0.0.5", 22)),
            Some("Save as SSH Host…")
        );
        assert_eq!(
            host_form_label(&RemoteTarget::Wsl {
                distro: "Ubuntu".into()
            }),
            None,
            "a WSL distro is configured nowhere this form could reach"
        );
        assert_eq!(
            host_form_label(&RemoteTarget::LocalStdio {
                program: "tty7-server".into(),
                args: Vec::new()
            }),
            None
        );
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
            last_active: 0,
            live: Liveness::Alive,
            open: true,
            current: false,
            preempted: false,
            adopt: None,
            remote_id: None,
            tabs,
        }
    }

    fn aged(mut r: Row, last_active: u64) -> Row {
        r.last_active = last_active;
        r
    }

    fn group(rows: Vec<Row>) -> Group {
        Group {
            key: String::new(),
            label: "This Computer".to_string(),
            endpoint: String::new(),
            target: None,
            link: Link::Local,
            error: None,
            installing: None,
            preempted: false,
            parked: false,
            rows,
        }
    }

    fn named_group(label: &str, rows: Vec<Row>) -> Group {
        Group {
            key: label.to_lowercase(),
            label: label.to_string(),
            target: Some(RemoteTarget::direct("me", label, 22)),
            link: Link::Offline,
            ..group(rows)
        }
    }

    #[test]
    fn orphan_panes_of_keeps_only_live_panes_no_workspace_holds() {
        use tty7_core::daemon::protocol::PaneInfo;

        fn info(pane_id: u64, alive: bool) -> PaneInfo {
            PaneInfo {
                pane_id,
                cwd: Some(std::path::PathBuf::from("/tmp/x")),
                title: "zsh".to_string(),
                osc_title: None,
                alive,
                owner: Some("tty7-cli".to_string()),
            }
        }

        let held: HashSet<u64> = [2].into_iter().collect();
        let orphans = orphan_panes_of(vec![info(1, true), info(2, true), info(3, false)], &held);
        assert_eq!(
            orphans,
            vec![OrphanPane {
                pane_id: 1,
                title: "zsh".to_string(),
                cwd: Some("/tmp/x".to_string()),
                owner: Some("tty7-cli".to_string()),
            }],
            "%2 is held by a workspace and %3 is dead — neither is a leak to reap (#596)"
        );
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
    fn the_flat_list_is_most_recently_used_first_across_machines() {
        // Two machines, interleaved activity: the list must interleave too —
        // grouping by machine is exactly what this design retired.
        let groups = vec![
            group(vec![
                aged(row("old-local", vec![]), 10),
                aged(row("fresh-local", vec![]), 40),
            ]),
            named_group("devbox", vec![aged(row("remote", vec![]), 30)]),
        ];
        let names: Vec<&str> = flatten(&groups, "")
            .into_iter()
            .map(|(g, r)| groups[g].rows[r].name.as_str())
            .collect();
        assert_eq!(names, vec!["fresh-local", "remote", "old-local"]);
    }

    #[test]
    fn this_windows_workspace_outranks_everything_however_stale() {
        let mut here = aged(row("here", vec![]), 1);
        here.current = true;
        let groups = vec![group(vec![aged(row("busy", vec![]), 99), here])];
        let first = flatten(&groups, "")[0];
        assert_eq!(groups[first.0].rows[first.1].name, "here");
    }

    #[test]
    fn a_query_matching_a_machines_name_keeps_all_of_its_rows() {
        // Searching "devbox" is how the retired per-machine grouping is asked
        // for now, so a host-name hit must surface every row of that machine
        // and nothing of the others.
        let groups = vec![
            group(vec![row("local-notes", vec![])]),
            named_group("devbox", vec![row("api", vec![]), row("web", vec![])]),
        ];
        let hits = flatten(&groups, "devbox");
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|&(g, _)| g == 1));
    }

    #[test]
    fn the_tab_column_follows_the_flat_cursor() {
        let layout = Layout {
            groups: vec![group(vec![row("a", vec![]), row("b", vec![])])],
            nav: vec![(0, 1), (0, 0)],
        };
        assert_eq!(layout.subject(0), Some((0, 1)));
        assert_eq!(layout.subject(1), Some((0, 0)));
        assert_eq!(layout.subject(2), None, "past the end is nothing");
    }

    fn host(label: &str) -> HostChoice {
        HostChoice {
            target: RemoteTarget::direct("me", label, 22),
            label: label.to_string(),
            detail: format!("me@{label}:22"),
        }
    }

    #[test]
    fn the_host_dropdown_leads_local_and_pins_add_host_last() {
        let items = host_items(vec![host("devbox")], "", "This Computer");
        assert!(matches!(items.first(), Some(HostItem::Local)));
        assert!(matches!(items.last(), Some(HostItem::AddHost)));
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn filtering_the_dropdown_narrows_hosts_but_never_loses_the_way_out() {
        let items = host_items(
            vec![host("devbox"), host("staging")],
            "stag",
            "This Computer",
        );
        // "stag" matches neither the local label nor devbox.
        assert_eq!(items.len(), 2);
        assert!(
            matches!(&items[0], HostItem::Host(h) if h.label == "staging"),
            "the one matching machine survives"
        );
        assert!(
            matches!(items.last(), Some(HostItem::AddHost)),
            "Add SSH Host… is pinned, filter or no filter"
        );
    }

    #[test]
    fn the_local_row_answers_to_its_own_name() {
        let items = host_items(vec![host("devbox")], "this comp", "This Computer");
        assert!(matches!(items.first(), Some(HostItem::Local)));
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
            osc_title: Some("✳ 修复 workspace switcher".to_string()),
            cwd: Some("/Users/x/repo/tty7".to_string()),
            agent: Some(crate::core::cli_agent::CLIAgent::Claude),
            status: None,
            live: true,
            panes: 1,
        };
        assert_eq!(tab_view_label(&view, 0, None), "build", "a given name wins");

        view.name = None;
        assert_eq!(
            tab_view_label(&view, 0, None),
            "✳ 修复 workspace switcher",
            "then the title the local strip would be showing, verbatim"
        );

        view.osc_title = Some("user@host:~/repo/025/tty7".to_string());
        assert_eq!(
            tab_view_label(&view, 0, None),
            crate::ui::tab_strip::short_title("user@host:~/repo/025/tty7", None),
            "a shell's title goes through the shortener the strip uses"
        );

        view.osc_title = Some("user@host:".to_string());
        assert_eq!(
            tab_view_label(&view, 0, None),
            "zsh",
            "a title that shortens away to nothing falls through"
        );

        view.osc_title = None;
        assert_eq!(
            tab_view_label(&view, 0, None),
            "Claude Code",
            "an agent names a tab that has told us nothing else"
        );

        view.agent = None;
        assert_eq!(
            tab_view_label(&view, 0, None),
            crate::ui::tab_strip::short_title("/Users/x/repo/tty7", None),
            "otherwise the directory, put through the same shortener as the strip"
        );

        view.cwd = None;
        assert_eq!(
            tab_view_label(&view, 0, None),
            "zsh",
            "process name is last"
        );

        view.title = String::new();
        assert!(tab_view_label(&view, 2, None).contains('3'));
    }
}

#[cfg(all(test, unix))]
mod gpui_tests {
    use gpui::{Modifiers, TestAppContext};

    use super::Column;
    use crate::ui::app::test_window::harness_with_tabs;

    /// Deleting a remote workspace has to take the listing snapshot's row with
    /// it. The snapshot every window keeps for the switcher is merged into the
    /// panel every frame, and with the store entry gone nothing dedups that
    /// row any more — the workspace the user just deleted popped straight back
    /// as an adoptable machine row until the next reconnect replaced the
    /// snapshot.
    #[gpui::test]
    fn a_deleted_remote_workspace_leaves_the_switcher(cx: &mut TestAppContext) {
        use gpui::VisualContext as _;

        use crate::core::session::{RemoteRef, RemoteTarget, WindowView, WorkspaceId};

        let (app, vcx) = crate::ui::app::test_window::harness(cx);
        let handle = vcx.window_handle();
        let weak = app.downgrade();

        let target = RemoteTarget::Alias {
            alias: "build-box".into(),
        };
        let machine_ws = WorkspaceId::new();
        let kept_ws = WorkspaceId::new();
        let view = WindowView::on_remote(RemoteRef::new(target.clone(), machine_ws));
        let doomed = view.id;

        app.update(cx, |app, cx| {
            crate::core::session::WorkspaceStore::install_for_test(
                cx,
                crate::core::session::WindowViews {
                    views: vec![view],
                    active: None,
                },
            );
            crate::ui::windows::WindowRegistry::init(cx);
            crate::ui::windows::WindowRegistry::register(cx, app.workspace, handle, weak);
            app.host_snapshots.insert(
                target.host_id(),
                super::HostSnapshot {
                    target: target.clone(),
                    rows: vec![
                        crate::ui::remote_connect::RemoteWorkspaceRow {
                            id: machine_ws,
                            name: "doomed".into(),
                            panes: 0,
                            last_active: 0,
                        },
                        crate::ui::remote_connect::RemoteWorkspaceRow {
                            id: kept_ws,
                            name: "kept".into(),
                            panes: 0,
                            last_active: 0,
                        },
                    ],
                },
            );
        });

        // From a plain app context, the way `confirm_and_delete`'s spawned
        // task calls it — inside the entity's own update this would abort on
        // the reentrant read (#617's lesson).
        cx.update(|cx| {
            crate::ui::windows::delete_workspace(cx, doomed);
        });

        app.update(cx, |app, cx| {
            let listed: Vec<WorkspaceId> = app
                .switcher_groups(cx)
                .iter()
                .flat_map(|g| g.rows.iter().filter_map(|r| r.remote_id))
                .collect();
            assert!(
                !listed.contains(&machine_ws),
                "the deleted workspace came back from the listing snapshot"
            );
            assert!(
                listed.contains(&kept_ws),
                "its machine's other workspaces are still on offer"
            );
        });
    }

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
    fn the_new_workspace_gesture_raises_the_create_form(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);

        app.update_in(&mut vcx, |app, window, cx| {
            app.open_workspace_form(window, cx)
        });

        app.update(cx, |app, cx| {
            let sw = app.switcher.as_ref().expect("the switcher is up");
            let super::Page::Create(form) = &sw.page else {
                panic!("the card shows the create form");
            };
            assert!(
                form.chosen.is_none(),
                "the host defaults to this computer, so Enter alone creates locally"
            );
            assert!(!form.open, "the dropdown starts folded");
            assert!(
                !form.name.read(cx).value().trim().is_empty(),
                "the name box starts prefilled with the generated default"
            );
        });
    }

    /// Creating a workspace switches this window over to it, and the pull that
    /// follows used to read the window back out of the registry to see what it
    /// was showing — the very entity `switch_workspace` is holding. gpui
    /// aborts the process for that, so the window had to be registered here for
    /// the lookup to find anything and the crash to reproduce (#617).
    #[gpui::test]
    fn creating_a_workspace_does_not_read_the_window_making_it(cx: &mut TestAppContext) {
        use gpui::VisualContext;

        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        let handle = vcx.window_handle();
        let weak = app.downgrade();
        app.update(cx, |app, cx| {
            crate::ui::windows::WindowRegistry::init(cx);
            crate::ui::windows::WindowRegistry::register(cx, app.workspace, handle, weak);
        });

        app.update_in(&mut vcx, |app, window, cx| {
            app.open_workspace_form(window, cx)
        });
        app.update_in(&mut vcx, |app, window, cx| {
            app.switcher_form_create(window, cx)
        });

        app.update(cx, |app, cx| {
            assert!(
                crate::ui::windows::WindowRegistry::app_for(cx, app.workspace).is_some(),
                "the window followed its new workspace into the registry"
            );
        });
    }

    /// The name the form collected has to reach the create, not chase it. It
    /// used to go out as a rename the moment the window switched — before the
    /// workspace existed on the machine to be renamed — so it was answered
    /// `NotFound`, dropped, and the generated name stood (#618).
    #[gpui::test]
    fn a_name_typed_into_the_form_is_held_for_the_create(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);

        app.update_in(&mut vcx, |app, window, cx| {
            app.name_fresh_workspace(Some("deploy".into()), window, cx)
        });

        app.update(cx, |app, cx| {
            assert_eq!(
                crate::ui::tree_sync::chosen_name_for(cx, app.workspace).as_deref(),
                Some("deploy"),
                "the create this window is about to make spends it"
            );
        });
    }

    #[gpui::test]
    fn the_form_folds_back_into_the_list_rather_than_closing(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_workspace_form(window, cx)
        });

        app.update_in(&mut vcx, |app, window, cx| {
            app.switcher_back_to_list(window, cx)
        });

        app.update(cx, |app, _| {
            let sw = app
                .switcher
                .as_ref()
                .expect("still up — Esc backs out one step");
            assert!(matches!(sw.page, super::Page::List));
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

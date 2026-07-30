//! The workspace switcher: the one surface that answers "which workspace, on
//! which machine, should this window show?"
//!
//! It replaces three partial answers to that question — the title-bar chip's
//! dropdown (every workspace, but no machine on the rows, so two boxes with a
//! `tty7` checkout each rendered as two identical lines), the home page's
//! picker (closed workspaces only, six of them, and only on an *empty* window)
//! and the home page's "Connect to Host" wizard (the only place a machine was
//! ever a first-class thing, and it stopped existing the moment it connected).
//!
//! # The one idea
//!
//! **The machine is the grouping dimension.** This computer is a group like any
//! other, so local and remote read as one grammar rather than "the normal case"
//! and "the special case", and no row can be ambiguous about where it lives.
//!
//! # Why an overlay and not a `PopupMenu`
//!
//! A popup dismisses on any mouse-down outside its own bounds, so a nested menu
//! tears its host down before its own click ever lands — `ui::home` hit exactly
//! this and wrote it down. The rows here carry a `⋯` menu, so this has to be an
//! overlay in the palette's shape (a scrim plus a card) rather than a dropdown
//! hanging off the chip. That also hands us a real search field and Esc for
//! free.
//!
//! # Where the rows come from
//!
//! The view store records remote workspaces (`WindowView::host`), so a
//! machine's workspaces are listed **without connecting to it** — the client
//! remembers which ones it saw, and their display facts come from the
//! machine's mirror (`ui::machine_mirror`). Connecting only ever *adds*: the
//! remote's own tree is the authority, so its rows are merged in when a link
//! exists and anything this client had not heard of shows up then (see
//! [`Group::merge`]). That is what makes "expand a machine" a lazy, cheap
//! gesture rather than a wizard.

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

use tty7_core::core::session::{RemoteTarget, WorkspaceId};

use crate::core::session::WorkspaceStore;
use crate::daemon::install::InstallPhase;
use crate::terminal::pane_liveness::Liveness;
use crate::ui::app::Tty7App;
use crate::ui::remote_connect::{self, HostChoice, RemoteWorkspaceRow, human_bytes};
use crate::ui::remote_workspace::ConnectFlow;

/// Card width — the command palette's, to the pixel.
///
/// These are the app's two full-window overlays and they open on the same
/// gesture-shaped question ("which one?"), so they are one card wearing two
/// contents. 460 was chosen when this panel was the narrower of the two, and it
/// showed: a row here carries four columns (name, path, state, age) where a
/// palette row carries one, and cramming them into 100px less made the panel
/// read as the smaller, lesser surface of the pair.
const CARD_W: f32 = 560.0;

/// How far down the window both overlays open. The palette's, for the same
/// reason as [`CARD_W`].
const CARD_TOP: f32 = 120.0;

/// How tall the scrolling body may get before it scrolls. Sized so a typical
/// two-machine setup never scrolls at all.
const BODY_MAX_H: f32 = 420.0;

/// Monogram diameter on a workspace row.
const ROW_AVATAR: f32 = 20.0;

/// Every row in the panel is exactly this tall, machines one step taller.
///
/// Fixed rather than padding-derived because the old layout set `py(6)` on some
/// rows and `py(7)` on others, which is invisible in the source and reads as a
/// stutter down the list.
const ROW_H: f32 = 32.0;
const HOST_H: f32 = 34.0;

/// The one column every glyph in the panel lines up in.
///
/// A machine's icon, a workspace's monogram and a `+` all centre here, so the
/// eye finds one vertical axis instead of re-finding the start of each row. The
/// old layout had none: headers began at `px(10)`, rows at `ml(16) + px(10)`,
/// and the glyphs inside them ranged 12–19px.
const GUTTER: f32 = 26.0;

/// Icon size everywhere in the panel. One value, deliberately — the old set ran
/// 12, 13 and 19 px plus two `▸`/`▾` *text* glyphs, which follow the UI font's
/// weight rather than the icon set's.
const ICON: f32 = 16.0;

/// How far a machine's workspaces sit in from its own row.
const KID_INDENT: f32 = 16.0;

/// Where the guide line down a remote machine's rows is drawn: the centre of
/// that machine's own icon, which is what makes it read as descending *from*
/// the machine.
const RAIL_X: f32 = ROW_PAD + GUTTER / 2.;

/// Horizontal padding inside a row.
const ROW_PAD: f32 = 8.0;

/// The age column. Fixed width and right-aligned so the ages read as a column
/// instead of drifting with whatever path sits to their left.
///
/// Sized to `relative_time`'s **longest** string, "over a week ago", not to a
/// typical one: the column is fixed, so anything that does not fit wraps to a
/// second line and takes the row's height with it. ("1 min ago" over two lines
/// is what 52px bought.)
const WHEN_W: f32 = 96.0;

/// Height of the install bar's track. Thin on purpose: it is a thing to glance
/// at while waiting, not a control, and anything taller starts to compete with
/// the workspace rows under it for the eye.
const PROGRESS_H: f32 = 3.0;

/// What a machine's connection is doing, as far as this panel is concerned.
///
/// Deliberately coarser than
/// [`RemoteStatus`](crate::ui::remote_workspace::RemoteStatus): that one drives
/// a window's read-only degrade and has to distinguish `Reconnecting` from
/// `Preempted`. A *list* only needs to know whether expanding this row will
/// cost a connect.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Link {
    /// This computer. Never connects, never fails.
    Local,
    /// A live control connection exists.
    Connected,
    /// A connect is in flight right now.
    Connecting,
    /// The last attempt failed. A resting state, not a transient one: design
    /// A failure always stays put and offers the next move, so the
    /// reason rides along on the group (see [`Group::error`]).
    Failed,
    /// No connection. **Not an error** — the rows below it are what this client
    /// remembers, and they are almost certainly still running over there.
    Offline,
}

/// One machine and the workspaces on it.
struct Group {
    /// Stable identity for collapse state and element ids. The local group's is
    /// the empty string, which no `RemoteTarget` can render as.
    key: String,
    label: String,
    /// Where this machine actually is (`thomas@10.0.4.12:2222`), when that is
    /// something other than the label already on the row.
    ///
    /// A `~/.ssh/config` alias says nothing about the box behind it, and two
    /// aliases can point at one machine; the endpoint is what tells them apart.
    /// Empty for the local group and for a target whose label *is* its endpoint.
    endpoint: String,
    /// `None` for the local group.
    target: Option<RemoteTarget>,
    link: Link,
    /// The remote's `$HOME`, once a handshake has reported it. Only `Some` for
    /// a connected remote — which is exactly when "New Workspace" can name a
    /// directory it is not guessing at.
    home: Option<PathBuf>,
    /// Why the last connect failed, shown under the header until the user acts
    /// on it.
    error: Option<String>,
    /// How far this machine's first install has got, while one is running.
    ///
    /// Shares the header's under-slot with [`error`](Self::error) and cannot
    /// collide with it: a connect is either still installing or has already
    /// failed.
    installing: Option<InstallPhase>,
    rows: Vec<Row>,
}

/// One workspace, flattened for rendering.
///
/// Owned rather than borrowed from the store: collecting these releases the
/// borrow on the global before the row closures capture `cx`, which is the same
/// dance `ui::home`'s picker does and for the same reason.
struct Row {
    id: WorkspaceId,
    name: String,
    path: String,
    when: String,
    live: Liveness,
    /// Shown by some window right now.
    open: bool,
    /// Shown by *this* window.
    current: bool,
    /// Set for a workspace that exists on the remote but has no local record
    /// yet: opening it has to claim it first. `None` once it is in
    /// the view store like any other.
    adopt: Option<Box<RemoteWorkspaceRow>>,
    /// This row's id **on its own machine**, for a remote workspace. It is what
    /// the remote's list is matched against — the local [`WorkspaceId`] above is
    /// this client's own handle and means nothing over there.
    remote_id: Option<WorkspaceId>,
}

/// A machine's handshake result, kept so the panel can show more than one
/// connected machine at a time.
///
/// [`ConnectFlow`] cannot do this job: it is a *flow*, so it holds one machine
/// and forgets it the moment the user connects to another. The panel lists
/// every machine at once, so the results have to accumulate somewhere that is
/// not the flow.
pub(crate) struct HostSnapshot {
    /// Which machine this is. Kept alongside the id it is filed under because
    /// [`HostId`](crate::ui::host_registry::HostId) is a one-way hash of the
    /// connection key — a machine that connected but has no workspace on this
    /// client yet has no other route back to a `RemoteTarget`, and without one
    /// it could never be given a group to appear in.
    pub target: RemoteTarget,
    /// What the remote said it had. The machine's `$HOME` is deliberately *not*
    /// here — it lives in `HostLinks`, app-wide, because every window
    /// needs it and only one of them ever did the connecting.
    pub rows: Vec<RemoteWorkspaceRow>,
}

/// The switcher's own state, alive only while it is on screen.
pub(crate) struct Switcher {
    /// The search field. Focused on open so keystrokes stay off the PTY.
    pub query: Entity<InputState>,
    /// Machines the user has folded away, by [`Group::key`].
    collapsed: HashSet<String>,
    /// Whether the "other SSH hosts" band is expanded.
    show_others: bool,
    /// An in-place rename, by the row it is editing.
    renaming: Option<(WorkspaceId, Entity<InputState>)>,
    _subs: Vec<Subscription>,
}

impl Switcher {
    fn text(&self, cx: &App) -> String {
        self.query.read(cx).value().trim().to_lowercase()
    }
}

impl Tty7App {
    // ----- open / close -----------------------------------------------------

    /// Toggle the switcher. Bound to the chip, ⌘⇧O and the palette's
    /// "Switch Workspace…".
    pub(crate) fn toggle_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.is_some() {
            self.close_switcher(window, cx);
        } else {
            self.open_switcher(window, cx);
        }
    }

    pub(crate) fn open_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The install-consent handler has to be live before any row can start a
        // connect. Idempotent and last-call-wins, exactly as `begin_connect`
        // registered it.
        remote_connect::register(cx);
        // Named, because the field is the panel's only affordance that does not
        // say what it does: a bare magnifier over a list of machines reads as
        // "filter these rows", and it also finds machines that have no row yet.
        let query =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search workspaces and machines"));
        query.update(cx, |state, cx| state.focus(window, cx));
        let subs = vec![cx.subscribe_in(
            &query,
            window,
            |_this, _input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            },
        )];
        self.switcher = Some(Switcher {
            query,
            collapsed: HashSet::new(),
            show_others: false,
            renaming: None,
            _subs: subs,
        });
        cx.notify();
    }

    pub(crate) fn close_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.take().is_some() {
            // A connect that is still in the air keeps running — the window it
            // lands in is chosen by the flow, not by whether this panel is
            // still open. A *failure* with nobody to show it to is dropped.
            if matches!(self.connect, Some(ConnectFlow::Failed { .. })) {
                self.connect = None;
            }
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    // ----- the model --------------------------------------------------------

    /// Every machine and its workspaces, in display order: this computer first,
    /// then remotes by how recently anything on them was used.
    ///
    /// Sorting remotes by recency rather than alphabetically is the same rule
    /// the rest of the app follows (the chip menu, the old picker): the list is
    /// a "get back to what you were doing" surface, not a directory.
    fn switcher_groups(&self, cx: &mut Context<Self>) -> Vec<Group> {
        let now = crate::ui::home::now_secs();
        let current = self.workspace;
        // One shared read of the store, and the liveness sweep started before
        // it so the dots are warm by the time a row asks.
        crate::terminal::pane_liveness::sweep(cx);

        let mut groups: Vec<Group> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        {
            let app: &App = cx;
            let store = WorkspaceStore::all(app);
            for w in &store.views {
                let (key, label, target) = match w.host.as_ref() {
                    None => (String::new(), "This Computer".to_string(), None),
                    Some(r) => {
                        let key = r.target.to_string();
                        (key.clone(), key, Some(r.target.clone()))
                    }
                };
                let slot = *index.entry(key.clone()).or_insert_with(|| {
                    groups.push(Group {
                        key,
                        label,
                        endpoint: String::new(),
                        target,
                        link: Link::Offline,
                        home: None,
                        error: None,
                        installing: None,
                        rows: Vec::new(),
                    });
                    groups.len() - 1
                });
                groups[slot].rows.push(Row {
                    id: w.id,
                    // Both read the machine's mirror — the tree owns the
                    // layout these used to be derived from. A machine not
                    // pulled yet (launch's first frames; an unreached remote)
                    // renders the not-knowing rather than a stale guess.
                    name: crate::ui::machine_mirror::display_name(app, w)
                        .unwrap_or_else(|| "Untitled".to_string()),
                    path: crate::ui::machine_mirror::subject_path(app, w)
                        .map(|p| crate::ui::home::display_path(std::path::Path::new(&p)))
                        .unwrap_or_default(),
                    when: crate::ui::home::relative_time(now, w.last_active),
                    live: crate::terminal::pane_liveness::liveness_of(app, w),
                    open: w.open,
                    current: w.id == current,
                    adopt: None,
                    remote_id: w.host.as_ref().map(|r| r.workspace),
                });
            }
        }

        // A machine the user just reached, or is reaching right now, gets a
        // group of its own even with no workspace on it.
        //
        // Without this, picking a brand-new machine out of "Other SSH Hosts"
        // looks like nothing happened: the connect runs, succeeds, and has
        // nowhere to land — groups came only from workspaces this client had
        // records for, and a machine that has never been used has none. That is
        // also precisely the machine the user is about to make a workspace on.
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
                rows: Vec::new(),
            });
        }

        // The local group is always present and always first, even with nothing
        // in it: "this computer" is not a thing that can be missing, and a panel
        // that hides it on a fresh install would have no way to make the first
        // workspace.
        if !index.contains_key("") {
            groups.insert(
                0,
                Group {
                    key: String::new(),
                    label: "This Computer".to_string(),
                    endpoint: String::new(),
                    target: None,
                    link: Link::Offline,
                    home: None,
                    error: None,
                    installing: None,
                    rows: Vec::new(),
                },
            );
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

        // Link state, the machine's own name, and the remote's own view.
        //
        // The name matters: a group's label starts life as `RemoteTarget`'s
        // `Display`, which for a saved profile is its *uuid* — the type
        // deliberately cannot reach into the profile store, so the panel has to
        // do the lookup that turns that into the name the user typed.
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
            group.link = self.link_state(&target, cx);
            if let Some(ConnectFlow::Failed { choice, error }) = &self.connect
                && choice.target == target
            {
                group.error = Some(error.clone());
            }
            let id = target.host_id();
            // Only while *this* window is the one connecting. Another window's
            // install is its own business, and a bar under a row this panel is
            // not driving would have no "Try Again" to turn into.
            if group.link == Link::Connecting {
                group.installing = remote_connect::install_progress_for(id);
            }
            // Read app-wide, not from this window's snapshot: any window's
            // connect, and every reconnect, records the machine's `$HOME` — and
            // that row is the only way to make a workspace on a machine, so it
            // has no business depending on which window did the connecting.
            group.home = remote_connect::HostLinks::home(cx, id);
            if let Some(snapshot) = self.host_snapshots.get(&id) {
                group.merge(&snapshot.rows, now);
            }
        }
        groups
    }

    /// Machines that have earned a group without owning a workspace: the one
    /// being connected to right now, the one that just failed (its error is the
    /// group's whole content), and every one that has answered a handshake this
    /// session.
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

    /// What a machine's connection is doing.
    fn link_state(&self, target: &RemoteTarget, cx: &mut Context<Self>) -> Link {
        match &self.connect {
            Some(ConnectFlow::Connecting { choice }) if &choice.target == target => {
                return Link::Connecting;
            }
            Some(ConnectFlow::Failed { choice, .. }) if &choice.target == target => {
                return Link::Failed;
            }
            _ => {}
        }
        match remote_connect::HostLinks::get(cx, target.host_id()) {
            Some(_) => Link::Connected,
            None => Link::Offline,
        }
    }

    /// The machines that have no workspace on this client, so they never earned
    /// a group of their own.
    ///
    /// This band exists because `~/.ssh/config` is not a list of development
    /// machines — it also holds git transport aliases (`github.com` and
    /// friends), which can never host a workspace. Rather than guess which is
    /// which (guess wrong and the user cannot reach their own box), the panel
    /// sorts by *whether it has been used* and folds the rest away.
    fn other_hosts(&self, groups: &[Group], cx: &App) -> Vec<HostChoice> {
        let known: HashSet<&str> = groups.iter().map(|g| g.key.as_str()).collect();
        remote_connect::available_hosts(cx)
            .into_iter()
            .filter(|h| !known.contains(h.target.to_string().as_str()))
            .collect()
    }

    // ----- actions ----------------------------------------------------------

    /// Expand a machine — which, for one that is not connected, *is* the
    /// connect. The panel stays open the whole way through.
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

    /// Show a workspace, closing the panel behind it.
    fn switcher_open(
        &mut self,
        row: RowRef,
        new_window: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_switcher(window, cx);
        match row.adopt {
            // A workspace this client has never seen: claim it first, then it
            // is an ordinary local record pointing at a remote id.
            Some((target, remote)) => self.open_remote_workspace(target, *remote, window, cx),
            None if new_window => crate::ui::windows::open(cx, Some(row.id)),
            None => self.reveal_workspace(row.id, window, cx),
        }
    }

    /// Start an in-place rename on a row.
    ///
    /// The chip's own rename (`start_workspace_rename`) can only ever edit the
    /// window's *current* workspace, because the field it opens is the chip. A
    /// list needs to rename the row that was aimed at, so it gets its own.
    fn switcher_rename(&mut self, id: WorkspaceId, window: &mut Window, cx: &mut Context<Self>) {
        let current = crate::ui::machine_mirror::display_name_for(cx, id).unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current));
        input.update(cx, |state, cx| state.focus(window, cx));
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
        // Focus goes back to the search field, not the terminal: the panel is
        // still open, and a rename is rarely the last thing the user wants.
        if let Some(sw) = self.switcher.as_ref() {
            sw.query.update(cx, |state, cx| state.focus(window, cx));
        }
        cx.notify();
    }

    /// Stop holding a connection to a machine.
    ///
    /// The panel stays open and the machine keeps its group: its workspaces are
    /// still there, still listed from what this client remembers, and the header
    /// now reads "not connected" — clicking it connects again. Windows showing
    /// them stay open and go read-only; see [`RemoteLinks::disconnect`] for why
    /// this closes nothing.
    fn switcher_disconnect(&mut self, target: &RemoteTarget, cx: &mut Context<Self>) {
        crate::ui::remote_workspace::RemoteLinks::disconnect(cx, target.host_id());
        // A finished connect flow for this machine described an attempt that has
        // just been undone; leaving it would keep painting a stale error (or a
        // success) over the group that no longer holds.
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

    /// Make a workspace on a machine. Local goes through the ordinary
    /// `NewWorkspace` path; a remote one lands in *that machine's* `$HOME`.
    fn switcher_new(&mut self, group: &GroupRef, window: &mut Window, cx: &mut Context<Self>) {
        self.close_switcher(window, cx);
        match (group.target.clone(), group.home.clone()) {
            (Some(target), Some(home)) => self.create_remote_workspace(target, home, window, cx),
            // No home means no handshake, which the row's own visibility rule
            // already prevents — belt and braces rather than inventing `~`.
            (Some(_), None) => {}
            (None, _) => crate::ui::windows::open(cx, None),
        }
    }

    // ----- render -----------------------------------------------------------

    /// The switcher overlay, or `None` when it is closed.
    pub(crate) fn render_switcher(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.switcher.as_ref()?;
        let groups = self.switcher_groups(cx);
        let others = self.other_hosts(&groups, cx);
        let query = self
            .switcher
            .as_ref()
            .map(|sw| sw.text(cx))
            .unwrap_or_default();

        let theme = cx.theme();
        let (border, card_bg) = (theme.border, theme.popover);
        // Card, radius, shadow and width are the command palette's, deliberately
        // — these are the app's two full-window overlays and they should read as
        // one thing wearing two contents, not two panels. The only colour that
        // moved is the ground: the old dim was a wash of the window's *own*
        // colour, so a dimmed window and a 5%-lifted card landed at the same
        // value and the whole panel read as though something were lying over it.
        // `scrim_fill` is the near-black that actually drops the window away.
        let scrim = crate::ui::presets::scrim_fill(cx);

        // 6px between machines, 1px within one: a machine and its workspaces
        // are a single block, and the gap is the only thing that says so.
        let mut body = v_flex().gap(px(6.));
        let mut shown = 0usize;
        for group in &groups {
            let Some(rendered) = self.render_group(group, &query, cx) else {
                continue;
            };
            shown += 1;
            body = body.child(rendered);
        }
        if let Some(band) = self.render_other_hosts(&others, &query, cx) {
            shown += 1;
            body = body.child(band);
        }
        if shown == 0 {
            body = body.child(
                div()
                    .px(px(ROW_PAD))
                    .py(px(14.))
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("No workspace or machine matches."),
            );
        }

        let card = v_flex()
            .w(px(CARD_W))
            .bg(card_bg)
            .border_1()
            .border_color(border)
            .rounded(px(10.))
            .shadow_xl()
            .overflow_hidden()
            .child(self.render_search(cx))
            .child(
                div()
                    .id("switcher-body")
                    .max_h(px(BODY_MAX_H))
                    .overflow_y_scroll()
                    .p(px(6.))
                    .child(body),
            )
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
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                    if ev.keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.close_switcher(window, cx);
                    }
                }))
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
            // `pl` matches the body's own padding so the magnifier lands on the
            // same axis as every machine icon under it, rather than 7px inside
            // it as it did when the two were padded independently.
            .pl(px(6. + ROW_PAD))
            .pr(px(12.))
            .h(px(42.))
            .border_b_1()
            .border_color(border)
            .child(glyph_col(
                GUTTER,
                Icon::new(IconName::Search).size(px(ICON)).text_color(muted),
            ))
            .children(self.switcher.as_ref().map(|sw| {
                // `pl_0`: a `small` input carries 8px of its own horizontal
                // padding whether or not it draws a box, and stacked on this
                // row's gap it set the query text 8px right of every label
                // under it — the one thing in the panel that did not share the
                // list's left edge.
                Input::new(&sw.query).appearance(false).small().pl_0()
            }))
    }

    /// The one row the panel keeps below the fold — adding a machine it does not
    /// know about yet — and the one gesture nothing else advertises.
    ///
    /// Deliberately *not* an "add host" form. A machine is
    /// configured once and remote workspaces reuse whatever is already set up,
    /// so this points at where that lives instead of growing a second place to
    /// do it.
    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (muted, dim, border) = (
            theme.muted_foreground,
            theme.muted_foreground.opacity(0.7),
            theme.border,
        );
        let hover = hover_fill(cx);
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
                    .child("Add SSH Host…")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_switcher(window, cx);
                        this.open_settings_section(
                            crate::ui::settings::SettingsSection::Ssh,
                            window,
                            cx,
                        );
                    })),
            )
            // A plain click reuses this window, ⌘-click opens another. Every
            // row's own label already says *where* the click lands (see the
            // badges in `render_row`); this is the one part of the answer that
            // has nowhere else to live, and it was previously discoverable only
            // by accident.
            .child(
                h_flex()
                    .items_center()
                    .gap(px(6.))
                    .pr(px(ROW_PAD))
                    .text_xs()
                    .text_color(dim)
                    .child(
                        div()
                            .px(px(5.))
                            .py(px(1.))
                            .rounded(px(4.))
                            .border_1()
                            .border_color(border)
                            .child("⌘"),
                    )
                    .child("click for a new window"),
            )
    }

    /// One machine: its header, and — when expanded — its workspaces.
    fn render_group(
        &self,
        group: &Group,
        query: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let matched_host = group.label.to_lowercase().contains(query);
        let rows: Vec<&Row> = group
            .rows
            .iter()
            .filter(|r| {
                query.is_empty()
                    || matched_host
                    || r.name.to_lowercase().contains(query)
                    || r.path.to_lowercase().contains(query)
            })
            .collect();
        if !query.is_empty() && !matched_host && rows.is_empty() {
            return None;
        }

        // A search expands everything it matched: hunting for a name and then
        // having to open the group it is in would make the field feel broken.
        let collapsed = self
            .switcher
            .as_ref()
            .map(|sw| sw.collapsed.contains(&group.key))
            .unwrap_or(false);
        let expanded = (!collapsed || !query.is_empty()) && group.link != Link::Offline;

        let mut block = v_flex().gap(px(1.));
        block = block.child(self.render_group_header(group, expanded, cx));
        if let Some(phase) = group.installing {
            block = block.child(self.render_install_progress(phase, cx));
        }
        // A failure is a resting state — it stays on screen with its reason
        // in full and its next move one click away, rather than reverting the
        // panel and leaving the user to guess between VPN, keys and the box.
        if let Some(error) = group.error.as_ref() {
            let retry = GroupRef::of(group);
            let theme = cx.theme();
            block = block.child(
                v_flex()
                    .gap(px(4.))
                    .ml(px(KID_INDENT))
                    .mr(px(4.))
                    .mb(px(2.))
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(theme.danger.opacity(0.35))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(error.clone()),
                    )
                    .child(
                        Button::new(gpui::SharedString::from(format!(
                            "switcher-retry:{}",
                            group.key
                        )))
                        .label("Try Again")
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
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
                    ),
            );
        }
        // A machine with no workspaces on it renders as its header alone. There
        // used to be a "New Workspace" row to fill the space; it lives in the
        // header's `⋯` now, so an empty group has nothing under it and must not
        // draw an indent block (and a guide rail) around nothing.
        if expanded && !rows.is_empty() {
            let mut kids = v_flex().gap(px(1.));
            for row in rows {
                kids = kids.child(self.render_row(group, row, cx));
            }
            block = block.child(self.indent(group, kids, cx));
        }
        Some(block.into_any_element())
    }

    /// The bar under a machine that is being installed onto for the first time.
    ///
    /// Sits in the same slot as the failure box, indented and inset to the same
    /// numbers, because it is the same kind of thing: a sentence about this
    /// machine's connect that outlives a single frame. The two can never both be
    /// present — a connect is either still running or has already failed — so
    /// the slot needs no arbitration.
    ///
    /// No border, unlike the failure box. A failure is a thing to act on and
    /// earns an outline; this is a thing to wait through, and a box around it
    /// would give a routine 20 seconds the weight of an error.
    fn render_install_progress(
        &self,
        phase: InstallPhase,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        // The same warning colour the header's dot and "installing…" already
        // use, so the row and the bar read as one state and not two.
        let accent = theme.warning;
        let (verb, done, total) = match phase {
            InstallPhase::Downloading { done, total } => ("Downloading", done, total),
            InstallPhase::Uploading { done, total } => ("Copying", done, Some(total)),
        };
        // An unknown total (no Content-Length) still gets a line of text and a
        // bar — just an empty one. A bar that guessed at a fraction would be
        // lying, and one that vanished would read as the install having stopped.
        let fraction = phase.fraction().unwrap_or(0.0);
        let caption = match total {
            Some(total) => format!(
                "{verb} tty7's server… {} / {}",
                human_bytes(done),
                human_bytes(total)
            ),
            None => format!("{verb} tty7's server… {}", human_bytes(done)),
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
                // Track and fill are one element inside another rather than a
                // gauge widget: the panel has no other progress indicator to be
                // consistent with, and 3px of rounded div needs no abstraction.
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

    /// A machine's rows, set in from its own row — and, on a *remote* machine,
    /// tied to it by a guide line descending from that machine's icon.
    ///
    /// The rail is the panel's one piece of pure structure, and it earns its
    /// place by saying the thing the old layout made you scroll up to find out:
    /// these rows are on another computer. The local group gets none, which is
    /// the point — "here" needs no marking, and two rails in two greys would
    /// only be one signal drawn twice.
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
                        // Stops short of the last row's baseline rather than
                        // running to the edge: a line that ends level with the
                        // final row's glyph reads as enclosing the block, one
                        // that runs past it reads as unfinished.
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
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (fg, muted, dim) = (
            theme.foreground,
            theme.muted_foreground,
            theme.muted_foreground.opacity(0.75),
        );
        let hover = hover_fill(cx);
        let gref = GroupRef::of(group);
        let menu_ref = gref.clone();
        let ctx_ref = gref.clone();
        let app = cx.entity().downgrade();
        let app2 = app.clone();

        // A machine wears the shape of what it is, which is the only thing on
        // the row that says "somewhere else" before a word of it is read. Both
        // glyphs are tty7's own: the stock set has no laptop and no server, and
        // its nearest neighbours (`hard-drive`, `cpu`) draw a *component*
        // rather than a computer. See `ui::assets`.
        let glyph = match group.target {
            None => "icons/machine-local.svg",
            Some(_) => "icons/machine-remote.svg",
        };

        // Only the states that are *not* resting get words. A green dot beside
        // the word "connected" says one thing twice; a grey dot alone says
        // nothing at all to someone who has not learnt this panel yet.
        let (dot, word): (Option<gpui::Hsla>, Option<&'static str>) = match group.link {
            Link::Local => (None, None),
            Link::Connected => (Some(gpui::rgb(crate::ui::tab_strip::LIVE_DOT).into()), None),
            // "installing…" while bytes are moving: the bar underneath says how
            // far along, and a header still reading "connecting…" over it would
            // describe a step that finished a while ago.
            Link::Connecting if group.installing.is_some() => {
                (Some(theme.warning), Some("installing…"))
            }
            Link::Connecting => (Some(theme.warning), Some("connecting…")),
            Link::Failed => (Some(theme.danger), Some("couldn't connect")),
            Link::Offline => (
                Some(gpui::rgb(crate::ui::tab_strip::UNKNOWN_DOT).into()),
                Some("not connected"),
            ),
        };
        let word_color = match group.link {
            Link::Connecting => theme.warning,
            Link::Failed => theme.danger,
            _ => muted,
        };

        h_flex()
            .id(gpui::SharedString::from(format!(
                "switcher-host:{}",
                group.key
            )))
            .items_center()
            .gap(px(8.))
            .h(px(HOST_H))
            .px(px(ROW_PAD))
            .rounded(px(6.))
            .cursor_pointer()
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
                    .flex_shrink_0()
                    .truncate()
                    .text_sm()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(fg)
                    .child(group.label.clone()),
            )
            // An alias says nothing about the box behind it, so the endpoint
            // rides alongside where there is one to show — and doubles as the
            // row's spacer, so it gives way to the status on its right rather
            // than pushing it off the row.
            .when(group.endpoint.is_empty(), |head| head.child(div().flex_1()))
            .when(!group.endpoint.is_empty(), |head| {
                head.child(
                    div()
                        .flex_1()
                        .min_w_0()
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
                    // Tighter than the row's gap: the dot and the word are one
                    // reading, not two items.
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
            // The machine's own actions, in the same `⋯` its rows use — but
            // always on, where a row's appears on hover. Two reasons it earns
            // the pixels a row's does not: there are a handful of machines and
            // dozens of rows, so a permanent glyph here is one mark and not a
            // column of them; and since "New Workspace" stopped being a row this
            // is the *only* way to reach it, where a row's menu only duplicates
            // what clicking the row already does.
            //
            // Without the `stop_propagation` the press underneath reaches the
            // header and folds the machine away behind its own menu.
            .child(
                div()
                    .flex_shrink_0()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new(gpui::SharedString::from(format!(
                            "switcher-host-more:{}",
                            group.key
                        )))
                        .icon(IconName::Ellipsis)
                        .ghost()
                        .xsmall()
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
                this.switcher_toggle_host(&gref, cx)
            }))
            // Right-click reaches the same menu, exactly as a row's does.
            .context_menu(move |menu, _window, _cx| group_menu(menu, &ctx_ref, app2.clone()))
    }

    fn render_row(&self, group: &Group, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        // A rename replaces the row's contents in place, so the name is edited
        // where it is read.
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
        let (fg, muted, dim) = (
            theme.foreground,
            theme.muted_foreground,
            theme.muted_foreground.opacity(0.7),
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

        // "Where will this click land?" written on the row rather than left to
        // be discovered: the same gesture focuses another window, swaps this
        // one over, or opens a new one, and only the row knows which.
        //
        // Still words rather than a coloured mark. The green pill this replaces
        // was the loudest thing in the panel, but the fix is to quieten it, not
        // to swap it for a bar or a ring — `workspace_avatar` already argues
        // that the current row is marked by *subtraction* (every other badge
        // fades, this one doesn't), and a second marker on the one row needing
        // no introduction would undo that.
        let badge = if row.current {
            Some(("this window", true))
        } else if row.open {
            Some(("open", false))
        } else {
            None
        };

        h_flex()
            .id(("switcher-row", key))
            .group("switcher-row")
            .items_center()
            .gap(px(8.))
            .h(px(ROW_H))
            .px(px(ROW_PAD))
            .rounded(px(6.))
            .cursor_pointer()
            .hover(move |r| r.bg(hover))
            .child(crate::ui::tab_strip::workspace_avatar(
                &row.name,
                row.live,
                row.current,
                ROW_AVATAR,
                cx,
            ))
            .child(
                div()
                    .flex_shrink_0()
                    .truncate()
                    .text_sm()
                    .when(row.current, |d| d.font_weight(gpui::FontWeight::MEDIUM))
                    .text_color(fg)
                    .child(row.name.clone()),
            )
            .child(
                div()
                    .flex_1()
                    // `min_w_0` + `truncate`, not `overflow_hidden`: a flex
                    // child will not shrink below its content without the
                    // former, so a long path pushed the age column off the row
                    // instead of ellipsing itself.
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(dim)
                    .child(row.path.clone()),
            )
            .children(badge.map(|(label, here)| {
                div()
                    .flex_shrink_0()
                    .px(px(6.))
                    .py(px(1.))
                    .rounded(px(4.))
                    .text_xs()
                    .bg(gpui::rgb(sf.selected))
                    .text_color(if here { fg.opacity(0.85) } else { muted })
                    .child(label)
            }))
            .child(
                div()
                    .flex_shrink_0()
                    // Fixed and right-aligned: the ages are a column, and they
                    // stopped being one the moment their left edge was allowed
                    // to follow whatever path sat beside them.
                    .w(px(WHEN_W))
                    .truncate()
                    .text_right()
                    .text_xs()
                    .text_color(dim)
                    .child(row.when.clone()),
            )
            .child(
                div()
                    .invisible()
                    .flex_shrink_0()
                    .group_hover("switcher-row", |x| x.visible())
                    // Without this the press also reaches the row underneath
                    // and opens the very workspace being renamed or removed.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new(("switcher-row-more", key))
                            .icon(IconName::Ellipsis)
                            .ghost()
                            .xsmall()
                            .dropdown_menu(move |menu, _window, _cx| {
                                row_menu(menu, &menu_ref, app.clone())
                            }),
                    ),
            )
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                this.switcher_open(click_ref.clone(), ev.modifiers().platform, window, cx)
            }))
            // Right-click opens the same menu — the second-tier convention
            // every other list in this app already uses. Last, because
            // `context_menu` wraps the element and the wrapper has no
            // `on_click` of its own.
            .context_menu(move |menu, _window, _cx| row_menu(menu, &ctx_ref, app2.clone()))
            .into_any_element()
    }

    /// The machines with nothing on them yet, folded into one row.
    fn render_other_hosts(
        &self,
        others: &[HostChoice],
        query: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if others.is_empty() {
            return None;
        }
        // `filter_hosts` rather than a substring test: it is the same fuzzy
        // ranking the machine list already used, so typing `awx` still finds
        // `aws-xy` here exactly as it did in the old flow.
        let hits: Vec<HostChoice> = match query.is_empty() {
            true => others.to_vec(),
            false => remote_connect::filter_hosts(others, query),
        };
        if hits.is_empty() {
            return None;
        }
        let expanded = self
            .switcher
            .as_ref()
            .map(|sw| sw.show_others)
            .unwrap_or(false)
            || !query.is_empty();

        let theme = cx.theme();
        let (muted, dim) = (theme.muted_foreground, theme.muted_foreground.opacity(0.7));
        let hover = hover_fill(cx);

        let mut block = v_flex().gap(px(1.)).child(
            h_flex()
                .id("switcher-others")
                .items_center()
                .gap(px(8.))
                .h(px(HOST_H))
                .px(px(ROW_PAD))
                .rounded(px(6.))
                .cursor_pointer()
                .hover(move |r| r.bg(hover))
                .child(glyph_col(
                    GUTTER,
                    Icon::new(IconName::Globe).size(px(ICON)).text_color(dim),
                ))
                .child(div().text_sm().text_color(muted).child("Other SSH Hosts"))
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
            // No rail here: these rows are machines, not a machine's
            // workspaces, so there is no parent for a line to descend from.
            let mut kids = v_flex().gap(px(1.));
            for (i, host) in hits.iter().enumerate() {
                let choice = (*host).clone();
                kids = kids.child(
                    h_flex()
                        .id(("switcher-other", i))
                        .items_center()
                        .gap(px(8.))
                        .h(px(ROW_H))
                        .px(px(ROW_PAD))
                        .rounded(px(6.))
                        .cursor_pointer()
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
                                .truncate()
                                .text_sm()
                                .text_color(muted)
                                .child(host.label.clone()),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .flex_shrink_0()
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
}

impl Group {
    /// Fold the remote's own workspace list into this group.
    ///
    /// The remote store is the authority on what exists over there, so anything
    /// it names that this client has no record of becomes an extra row marked
    /// for adoption. Rows this client *does* have are left alone: their local
    /// record carries window geometry and the `open` flag, which are this
    /// client's business and not the remote's (the storage split).
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
                // The remote's list carries a pane *count*, not the pane ids a
                // liveness answer is matched against — so there is nothing here
                // to be right or wrong about yet.
                live: Liveness::Stopped,
                open: false,
                current: false,
                adopt: Some(Box::new(r.clone())),
                remote_id: Some(r.id),
            });
        }
    }
}

/// Everything a header's click handler needs, owned — the group itself borrows
/// the store, and a closure cannot hold that across a `&mut cx`.
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

/// The same, for a row.
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

/// A machine's second-tier actions.
///
/// "New Workspace" lives here rather than in a row of its own under every
/// machine. It was the one line in the panel that was not a workspace, it
/// repeated once per machine, and on a client with four boxes it pushed the
/// thing the panel is *for* — the list — a quarter of a card further down.
///
/// The `⋯` is also where a machine's own verbs belong now there is more than
/// one of them: expanding a machine already means "connect", so its inverse
/// needed somewhere to be said, and it is not a row either.
fn group_menu(
    menu: gpui_component::menu::PopupMenu,
    group: &GroupRef,
    app: gpui::WeakEntity<Tty7App>,
) -> gpui_component::menu::PopupMenu {
    let (a1, a2) = (app.clone(), app);
    let gref = group.clone();
    // A remote machine can only be given a workspace once a handshake has said
    // where its `$HOME` is — `~` guessed from this client would be the wrong
    // directory on the wrong computer. This one needs no handshake.
    let can_create = group.target.is_none() || group.home.is_some();
    let menu = menu.item(
        PopupMenuItem::new("New Workspace")
            .disabled(!can_create)
            .on_click(move |_, window, cx| {
                let _ = a1.update(cx, |this, cx| this.switcher_new(&gref, window, cx));
            }),
    );
    let Some(target) = group.target.clone() else {
        // This computer. There is no connection to drop, and "Disconnect"
        // greyed out under every local group would only invite the question.
        return menu;
    };
    let connected = group.link == Link::Connected;
    menu.separator().item(
        PopupMenuItem::new("Disconnect")
            .disabled(!connected)
            .on_click(move |_, _window, cx| {
                let _ = a2.update(cx, |this, cx| this.switcher_disconnect(&target, cx));
            }),
    )
}

/// A row's second-tier actions.
///
/// One `⋯` rather than a cluster of glyphs, and the destructive one behind it
/// rather than out in the open — the rule the sidebar and the old picker
/// already settled on, with the difference that what is revealed here is the
/// menu and not the delete.
fn row_menu(
    menu: gpui_component::menu::PopupMenu,
    row: &RowRef,
    app: gpui::WeakEntity<Tty7App>,
) -> gpui_component::menu::PopupMenu {
    let (a1, a2, a3, a4) = (app.clone(), app.clone(), app.clone(), app);
    let (id, adopt) = (row.id, row.adopt.is_some());
    let stoppable = row.live;
    menu.item(
        // A workspace this client has not adopted yet has no local record to
        // rename, stop or remove — opening it is the only thing that can be
        // done to it, and that is the row's own click.
        PopupMenuItem::new("Rename…")
            .disabled(adopt)
            .on_click(move |_, window, cx| {
                let _ = a1.update(cx, |this, cx| this.switcher_rename(id, window, cx));
            }),
    )
    .item(
        PopupMenuItem::new("Open in New Window")
            .disabled(adopt)
            .on_click(move |_, window, cx| {
                let _ = a2.update(cx, |this, cx| {
                    this.close_switcher(window, cx);
                    crate::ui::windows::open(cx, Some(id));
                });
            }),
    )
    .separator()
    .item(
        PopupMenuItem::new("End Sessions…")
            .disabled(adopt || !stoppable)
            .on_click(move |_, window, cx| {
                let _ = a3.update(cx, |this, cx| {
                    this.close_switcher(window, cx);
                    this.stop_workspace(id, window, cx);
                });
            }),
    )
    .item(
        PopupMenuItem::new("Remove from List…")
            .disabled(adopt)
            .on_click(move |_, window, cx| {
                let _ = a4.update(cx, |this, cx| {
                    this.close_switcher(window, cx);
                    this.delete_workspace(id, window, cx);
                });
            }),
    )
}

/// This card's interaction ladder — the popover one, the same rungs every menu
/// and the command palette paint on.
fn rungs(cx: &App) -> crate::ui::presets::Surface {
    cx.global::<crate::ui::presets::Surfaces>().popover
}

/// The hover rung, read from the shared surface presets rather than an alpha
/// over whatever shows through.
fn hover_fill(cx: &App) -> gpui::Rgba {
    gpui::rgb(rungs(cx).hover)
}

/// One glyph, centred in a column of `w`.
///
/// Every icon in the panel goes through this, which is the whole point: the old
/// layout let each row start its own glyph wherever its padding happened to
/// land, so nothing shared a vertical axis. A machine's column is [`GUTTER`];
/// the rows underneath use the narrower [`ROW_AVATAR`], so their monograms
/// share one axis of their own rather than sitting 3px off the machines'.
fn glyph_col(w: f32, child: impl IntoElement) -> impl IntoElement {
    div()
        .w(px(w))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .child(child)
}

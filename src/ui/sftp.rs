//! Pane-contextual SFTP file panel (Workstream 5).
//!
//! Renders as a bottom-docked panel (tabby-style) over the lower part of the
//! terminal body for the focused **native-SSH** pane (a PTY pane, or a
//! foreground `ssh` typed into a local shell, has no russh connection to browse,
//! so the panel doesn't open). Mirrors the `ui::forwards` pattern: a set of
//! `impl Tty7App` render helpers plus a [`SftpPanelState`] held on `Tty7App`, and
//! one-shot [`RemoteTerminal`] control calls to the daemon (`sftp_list` /
//! `sftp_op` / `sftp_transfer_*`) — the blocking round-trips run on a background
//! executor so directory navigation never freezes the UI.
//!
//! Layout (interaction modelled on tabby's SFTP panel): a breadcrumb path bar
//! whose root reads `SFTP` and which double-clicks into a "type a path" text
//! input; a toolbar (refresh / filter / new folder / upload / go-to-shell-cwd);
//! a filter box hidden behind the toolbar's Filter toggle; a dir-first entry
//! list led by a `..` parent row (when not at the root) whose per-row actions
//! (open/download / follow-symlink / rename / chmod / delete) live in a
//! right-click context menu (PRD §6.3: hotkeys + right-click, not a permanent
//! toolbar); an inline edit form; and a bottom transfer tray that polls job
//! progress while the panel is open.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Div, ExternalPaths, FontWeight, PathPromptOptions, SharedString,
    Stateful, Subscription, Window, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, InteractiveElementExt as _, Sizable as _, h_flex, v_flex,
};

use crate::daemon::protocol::{
    SftpEntry, SftpEntryKind, SftpJobProgress, SftpJobState, SftpOp, SftpOpResult,
    SftpTransferKind, SftpTransferSpec,
};
use crate::daemon::ssh::sftp::{remote_basename, remote_join, remote_parent, safe_local_name};
use crate::terminal::RemoteTerminal;
use crate::ui::app::{CONTENT_INSET, Tty7App};

/// The remote Files header's `⋯` entries. The directory-wide actions that used
/// to be a row of toolbar buttons; per-row actions stay on the row's own
/// right-click menu.
#[derive(Clone, Copy)]
enum SftpMenuAction {
    NewFolder,
    NewFile,
    Upload,
    GotoShellCwd,
    ToggleHistory,
}

/// One in-progress inline edit form in the panel.
pub(crate) enum SftpEdit {
    NewFolder(gpui::Entity<InputState>),
    NewFile(gpui::Entity<InputState>),
    Rename {
        original: String,
        input: gpui::Entity<InputState>,
    },
    Chmod {
        path: String,
        /// The entry's current mode as `rwxr-xr-x`, shown beside the form's octal
        /// field. The row itself no longer has room for a permissions column, so
        /// this is where the readable form lives now.
        readable: String,
        input: gpui::Entity<InputState>,
    },
}

/// Which SSH connection this panel's requests run on — the *only* thing that
/// differs between an SSH pane and a remote workspace (design §15).
///
/// A plain `Copy`-able bundle rather than a lookup at each call site, because
/// every request runs on a background executor: the pane entity is not reachable
/// from there, so the route has to be resolved on the UI thread and moved in.
/// Both arms end at the same `SftpManager` on the same daemon; a remote
/// workspace's transfers are local-daemon SFTP over the workspace's own
/// connection, which is what makes "drag a file to Finder" land on *this*
/// machine.
#[derive(Clone, Debug)]
pub(crate) struct SftpRoute {
    pane_id: u64,
    workspace: Option<crate::terminal::PaneWorkspace>,
}

impl SftpRoute {
    /// The route to a pane, given the workspace it belongs to (`None` for a
    /// pane that owns its own connection). Public to the crate because the
    /// panel is not the only caller any more: Tab completion lists a remote
    /// directory over the same route, and must not grow a second copy of the
    /// pane-vs-workspace decision.
    pub(crate) fn new(pane_id: u64, workspace: Option<crate::terminal::PaneWorkspace>) -> Self {
        Self { pane_id, workspace }
    }

    fn workspace_op(
        &self,
        op: crate::daemon::protocol::WorkspaceOp,
    ) -> Option<crate::daemon::protocol::WorkspaceRequest> {
        RemoteTerminal::workspace_request(self.workspace.as_ref()?, self.pane_id, op)
    }

    pub(crate) fn list(&self, path: &str) -> Result<Vec<SftpEntry>, String> {
        let Some(req) = self.workspace_op(crate::daemon::protocol::WorkspaceOp::SftpList {
            path: path.to_string(),
        }) else {
            return RemoteTerminal::sftp_list(self.pane_id, path);
        };
        match RemoteTerminal::on_workspace(req) {
            Ok(crate::daemon::protocol::DaemonMsg::SftpEntries(e)) => Ok(e),
            Ok(other) => Err(format!("unexpected reply: {other:?}")),
            Err(e) => Err(e.to_string()),
        }
    }

    pub(crate) fn op(&self, op: SftpOp) -> SftpOpResult {
        let Some(req) =
            self.workspace_op(crate::daemon::protocol::WorkspaceOp::SftpOp { op: op.clone() })
        else {
            return RemoteTerminal::sftp_op(self.pane_id, op);
        };
        match RemoteTerminal::on_workspace(req) {
            Ok(crate::daemon::protocol::DaemonMsg::SftpOpResult(r)) => r,
            Ok(other) => SftpOpResult::Error(format!("unexpected reply: {other:?}")),
            Err(e) => SftpOpResult::Error(e.to_string()),
        }
    }

    pub(crate) fn transfer_start(&self, spec: SftpTransferSpec) -> Result<u64, String> {
        let Some(req) =
            self.workspace_op(crate::daemon::protocol::WorkspaceOp::SftpTransferStart {
                spec: spec.clone(),
            })
        else {
            return RemoteTerminal::sftp_transfer_start(spec);
        };
        match RemoteTerminal::on_workspace(req) {
            Ok(crate::daemon::protocol::DaemonMsg::SftpTransferStarted { job_id }) => Ok(job_id),
            Ok(other) => Err(format!("unexpected reply: {other:?}")),
            Err(e) => Err(e.to_string()),
        }
    }

    pub(crate) fn transfer_list(&self) -> Vec<SftpJobProgress> {
        let Some(req) = self.workspace_op(crate::daemon::protocol::WorkspaceOp::SftpTransferList)
        else {
            return RemoteTerminal::sftp_transfer_list(self.pane_id);
        };
        match RemoteTerminal::on_workspace(req) {
            Ok(crate::daemon::protocol::DaemonMsg::SftpTransferProgress(jobs)) => jobs,
            Ok(other) => {
                log::warn!("unexpected reply to a workspace transfer list: {other:?}");
                Vec::new()
            }
            Err(e) => {
                log::warn!("workspace transfer list failed: {e}");
                Vec::new()
            }
        }
    }
}

/// State for the remote file browser. One pane's listing at a time, bound to a
/// pane id — the detail panel shows one pane, so the browser follows it.
pub(crate) struct SftpPanelState {
    /// The pane whose listing is on screen, or `None` while the Files tab is
    /// showing a local tree. Set by the Files render path from the detail pane,
    /// not by a toggle: the browser is a *view of the pane*, so which pane you're
    /// looking at is the only thing that decides it.
    pub(crate) open_pane_id: Option<u64>,
    /// The remote workspace the open pane belongs to, when it is one (design
    /// §15). Captured beside `open_pane_id` because every SFTP call needs it and
    /// the calls run on a background executor, where the pane entity is out of
    /// reach. `None` — the case for SSH panes — keeps the pane-addressed path.
    pub(crate) open_workspace: Option<crate::terminal::PaneWorkspace>,
    /// The remote directory currently listed (absolute POSIX path).
    pub(crate) cwd: String,
    /// Where each pane was last browsing, so switching panes — or tabs — and
    /// coming back lands where you left rather than back at the shell cwd. Keyed
    /// by pane id and dropped with the pane.
    pub(crate) cwds: std::collections::HashMap<u64, String>,
    pub(crate) entries: Vec<SftpEntry>,
    pub(crate) filter_input: gpui::Entity<InputState>,
    /// Last listing error, shown in place of the list.
    pub(crate) error: Option<String>,
    /// Latest transfer-job snapshots for the tray.
    pub(crate) jobs: Vec<SftpJobProgress>,
    /// Job ids the user dismissed from the tray; filtered out until a fresh
    /// transfer (a new id) reopens it. Cleared when the panel closes/reopens.
    dismissed_jobs: HashSet<u64>,
    /// When set, the transfers footer is pinned open and shows the full history
    /// (every job, including dismissed ones), toggled from the Files `⋯` menu.
    show_history: bool,
    /// Whether the transfers footer is showing its per-job list. Collapsed by
    /// default: a running transfer is a glance, not a watch.
    tray_expanded: bool,
    /// A directory listing is in flight (the daemon round-trip runs off-thread,
    /// so the UI never blocks). Guards feedback while the old listing stays up.
    pub(crate) loading: bool,
    /// Bumped on every navigation so a slow/stale listing reply is discarded when
    /// a newer navigation has already superseded it.
    nav_gen: u64,
    pub(crate) editing: Option<SftpEdit>,
    /// When `Some`, the breadcrumb is replaced by a path text input ("type a
    /// path" mode). Committed on Enter, cancelled on Esc/blur.
    pub(crate) editing_path: Option<gpui::Entity<InputState>>,
    /// Keeps the path-input subscription alive while [`editing_path`] is set.
    editing_path_sub: Vec<Subscription>,
    /// Bumped on every (re)open so a stale poll loop exits.
    pub(crate) poll_gen: u64,
    /// Scroll position of the remote listing, owned here so the Files tab's
    /// overlay scrollbar has a handle to read and drag (see `ui::scrollbar`).
    scroll: gpui::ScrollHandle,
    _subs: Vec<Subscription>,
}

impl SftpPanelState {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Tty7App>) -> Self {
        let filter_input = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        // Re-render the panel (and thus re-filter the list) on every keystroke.
        let sub = cx.subscribe_in(&filter_input, window, |_this, _input, ev, _w, cx| {
            if matches!(ev, gpui_component::input::InputEvent::Change) {
                cx.notify();
            }
        });
        Self {
            open_pane_id: None,
            open_workspace: None,
            cwd: "/".to_string(),
            cwds: std::collections::HashMap::new(),
            entries: Vec::new(),
            filter_input,
            error: None,
            jobs: Vec::new(),
            dismissed_jobs: HashSet::new(),
            show_history: false,
            tray_expanded: false,
            loading: false,
            nav_gen: 0,
            editing: None,
            editing_path: None,
            editing_path_sub: Vec::new(),
            poll_gen: 0,
            scroll: gpui::ScrollHandle::new(),
            _subs: vec![sub],
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (tested).
// ---------------------------------------------------------------------------

fn is_dir_like(e: &SftpEntry) -> bool {
    matches!(e.kind, SftpEntryKind::Dir)
        || (matches!(e.kind, SftpEntryKind::Symlink) && e.target_is_dir)
}

/// Directory-first, then case-insensitive by name; substring-filtered (case
/// insensitive). Returns borrows into `entries` in display order.
pub(crate) fn sorted_filtered_entries<'a>(
    entries: &'a [SftpEntry],
    filter: &str,
) -> Vec<&'a SftpEntry> {
    let needle = filter.to_lowercase();
    let mut out: Vec<&SftpEntry> = entries
        .iter()
        .filter(|e| needle.is_empty() || e.name.to_lowercase().contains(&needle))
        .collect();
    out.sort_by(|a, b| {
        let (ad, bd) = (is_dir_like(a), is_dir_like(b));
        // Directories first, then name.
        bd.cmp(&ad)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

/// Split a remote path into clickable breadcrumb segments: `(label, full_path)`,
/// always starting with the root `("/", "/")`.
pub(crate) fn breadcrumb_segments(path: &str) -> Vec<(String, String)> {
    let mut out = vec![("/".to_string(), "/".to_string())];
    let mut acc = String::new();
    for comp in path.split('/').filter(|s| !s.is_empty()) {
        acc.push('/');
        acc.push_str(comp);
        out.push((comp.to_string(), acc.clone()));
    }
    out
}

/// Compact human-readable byte size (`1.5M`).
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

/// A `-rwxr-xr-x`-style mode string from Unix permission bits (low 9 bits).
fn mode_string(mode: u32) -> String {
    let rwx = |bits: u32| {
        format!(
            "{}{}{}",
            if bits & 0o4 != 0 { 'r' } else { '-' },
            if bits & 0o2 != 0 { 'w' } else { '-' },
            if bits & 0o1 != 0 { 'x' } else { '-' },
        )
    };
    format!(
        "{}{}{}",
        rwx((mode >> 6) & 0o7),
        rwx((mode >> 3) & 0o7),
        rwx(mode & 0o7)
    )
}

/// The daemon-process home directory used as the local base for transfers.
fn local_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Where downloads land locally: `~/Downloads` (created on demand by the daemon).
fn local_download_dir() -> PathBuf {
    local_home().join("Downloads")
}

// ---------------------------------------------------------------------------
// Tty7App: open / navigate / operations.
// ---------------------------------------------------------------------------

impl Tty7App {
    /// `ToggleSftp` / the palette's "SSH: Remote Files": show the remote browser,
    /// which means putting the detail panel on its Files tab. The tab renders the
    /// browser by itself once it's looking at a native-SSH pane, so there is no
    /// separate panel to open — showing it is entirely "take me there".
    ///
    /// It still earns the `Toggle` in its name: pressing it again while the panel
    /// is already sitting on Files closes the panel. A key you bound to reach
    /// something should put it away too, and without this the binding is a dead
    /// press whenever you're already there.
    ///
    /// A pane with no native connection (a foreground `ssh` typed into a local
    /// shell, or a plain PTY) has nothing to list; the Files tab shows its local
    /// tree instead, which is the right answer rather than an error.
    pub(crate) fn toggle_sftp(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        use crate::core::config::RightPanelTab;
        // This window's own panel state, not `right_panel_open`: this toggles
        // exactly what `toggle_right_panel` does, so the two agree on what
        // "open" means even with no tabs to render into. (And not the config
        // either — that is only what a *new* window starts with.)
        if self.right_panel_visible && self.right_panel_tab == RightPanelTab::Files {
            self.toggle_right_panel(cx);
            return;
        }
        self.set_right_panel_tab(RightPanelTab::Files, cx);
    }

    /// Point the browser at `pane_id`, or tear it down when the Files tab has
    /// moved to a local pane (`None`). Called from the Files render path, so the
    /// browser's lifetime is exactly "the detail panel is showing this remote
    /// pane" — no open/close state of its own to fall out of step.
    ///
    /// Returns `true` when the caller should render the remote browser.
    pub(crate) fn sftp_sync_pane(
        &mut self,
        pane_id: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(pane_id) = pane_id else {
            if self.sftp_panel.open_pane_id.is_some() {
                self.sftp_close_browser(cx);
            }
            return false;
        };
        if self.sftp_panel.open_pane_id != Some(pane_id) {
            self.sftp_open_at(pane_id, window, cx);
        }
        true
    }

    /// Stop browsing: drop the listing and retire the poll loops. Transfers are
    /// untouched — they run in the daemon and keep running; only this view of
    /// them goes away.
    pub(crate) fn sftp_close_browser(&mut self, cx: &mut Context<Self>) {
        self.sftp_panel.open_pane_id = None;
        self.sftp_panel.entries.clear();
        self.sftp_panel.error = None;
        self.sftp_panel.editing = None;
        self.sftp_panel.editing_path = None;
        self.sftp_panel.editing_path_sub.clear();
        // The jobs are one pane's. Leaving them would have the footer report the
        // old pane's transfers under the next one until its first poll lands —
        // the same flash `sync_procs` clears the process list for.
        self.sftp_panel.jobs.clear();
        self.sftp_panel.open_workspace = None;
        // Invalidate the poll loop.
        self.sftp_panel.poll_gen = self.sftp_panel.poll_gen.wrapping_add(1);
        cx.notify();
    }

    /// How this panel's requests reach the far side: the open pane's own
    /// connection, or — for a remote-workspace pane — the workspace's (§15).
    ///
    /// Resolved on the UI thread and cloned into every background call, because
    /// the pane entity is not reachable from a background executor.
    fn sftp_route(&self) -> SftpRoute {
        SftpRoute {
            pane_id: self.sftp_panel.open_pane_id.unwrap_or_default(),
            workspace: self.sftp_panel.open_workspace.clone(),
        }
    }

    /// The remote workspace a pane belongs to, read off the pane itself.
    fn pane_workspace(
        &self,
        pane_id: u64,
        window: &Window,
        cx: &App,
    ) -> Option<crate::terminal::PaneWorkspace> {
        let leaf = self
            .tabs
            .get(self.active)?
            .pane
            .focused_or_first(window, cx)?;
        let leaf = leaf.read(cx);
        (leaf.pane_id == pane_id).then(|| leaf.workspace().cloned())?
    }

    fn sftp_open_at(&mut self, pane_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.sftp_panel.open_pane_id = Some(pane_id);
        self.sftp_panel.open_workspace = self.pane_workspace(pane_id, window, cx);
        self.sftp_panel.entries.clear();
        self.sftp_panel.error = None;
        self.sftp_panel.editing = None;
        self.sftp_panel.editing_path = None;
        self.sftp_panel.editing_path_sub.clear();
        self.sftp_panel.show_history = false;
        self.sftp_poll_jobs(cx);
        self.sftp_start_polling(cx);

        // Where this pane was last time you looked, else the shell's cwd.
        if let Some(start) = self
            .sftp_panel
            .cwds
            .get(&pane_id)
            .cloned()
            .or_else(|| self.pane_shell_cwd(pane_id, window, cx))
        {
            self.sftp_navigate(start, cx);
            return;
        }
        // Neither: ask the far side where "." is. The shell only reports its cwd
        // when tty7's shell integration is installed on the remote — which on a
        // host you just connected to it usually isn't — and `/` is a poor place
        // to open a file browser. SFTP's own REALPATH resolves to the login
        // directory, which is where a fresh session actually is.
        self.sftp_navigate_login_dir(pane_id, cx);
    }

    /// Resolve the session's login directory (`realpath "."`) and open there.
    /// Falls back to `/` when the round-trip fails — a browser at the root still
    /// works, and the error would be noise on a path nobody typed.
    fn sftp_navigate_login_dir(&mut self, pane_id: u64, cx: &mut Context<Self>) {
        self.sftp_panel.loading = true;
        let route = self.sftp_route();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { route.op(SftpOp::Realpath { path: ".".into() }) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // The browser may have moved on (pane switch, or the user typed a
                // path) while the round-trip was out.
                if this.sftp_panel.open_pane_id != Some(pane_id) {
                    return;
                }
                let home = match result {
                    SftpOpResult::Link(path) if path.starts_with('/') => path,
                    _ => "/".to_string(),
                };
                this.sftp_navigate(home, cx);
            });
        })
        .detach();
    }

    /// The focused pane's OSC-7 cwd as an absolute remote path, if tracked.
    fn pane_shell_cwd(&self, pane_id: u64, window: &Window, cx: &App) -> Option<String> {
        let leaf = self
            .tabs
            .get(self.active)?
            .pane
            .focused_or_first(window, cx)?;
        let leaf = leaf.read(cx);
        if leaf.pane_id != pane_id {
            return None;
        }
        let path = leaf.cwd()?;
        let s = path.to_string_lossy().to_string();
        s.starts_with('/').then_some(s)
    }

    /// List `path` on the pane's SFTP session and show it. The daemon round-trip
    /// (`sftp_list`) is a blocking socket request, so it runs on a background
    /// executor — the UI thread keeps painting while a big or high-latency
    /// directory loads. The old listing stays visible until the new one arrives;
    /// errors are surfaced in the panel body rather than thrown away.
    pub(crate) fn sftp_navigate(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(pane_id) = self.sftp_panel.open_pane_id else {
            return;
        };
        // A newer navigation invalidates any listing still in flight.
        self.sftp_panel.nav_gen = self.sftp_panel.nav_gen.wrapping_add(1);
        let generation = self.sftp_panel.nav_gen;
        self.sftp_panel.loading = true;
        cx.notify();

        let list_path = path.clone();
        let route = self.sftp_route();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { route.list(&list_path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Drop the reply if the panel moved on (closed, switched pane, or a
                // later navigation started).
                if this.sftp_panel.open_pane_id != Some(pane_id)
                    || this.sftp_panel.nav_gen != generation
                {
                    return;
                }
                this.sftp_panel.loading = false;
                match result {
                    Ok(mut entries) => {
                        entries.sort_by(|a, b| a.name.cmp(&b.name));
                        // Remember where this pane got to, so coming back to it
                        // resumes rather than restarts. Recorded on arrival, not
                        // on the way out: only a directory that actually listed is
                        // worth returning to.
                        this.sftp_panel.cwds.insert(pane_id, path.clone());
                        this.sftp_panel.cwd = path;
                        this.sftp_panel.entries = entries;
                        this.sftp_panel.error = None;
                        // Leave "type a path" mode once we've landed somewhere.
                        this.sftp_panel.editing_path = None;
                        this.sftp_panel.editing_path_sub.clear();
                    }
                    Err(e) => {
                        // Keep the old listing; just report the failure.
                        this.sftp_panel.error = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn sftp_refresh(&mut self, cx: &mut Context<Self>) {
        let cwd = self.sftp_panel.cwd.clone();
        self.sftp_navigate(cwd, cx);
    }

    pub(crate) fn sftp_up(&mut self, cx: &mut Context<Self>) {
        let parent = remote_parent(&self.sftp_panel.cwd);
        self.sftp_navigate(parent, cx);
    }

    // --- editable path bar (tabby "type a path" mode) ----------------------

    /// Replace the breadcrumb with a text input pre-filled with the current
    /// directory, so you can type a destination directly. Enter navigates,
    /// Esc/blur cancels back to the breadcrumb.
    pub(crate) fn sftp_begin_edit_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sftp_panel.open_pane_id.is_none() {
            return;
        }
        let cwd = self.sftp_panel.cwd.clone();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(cwd));
        input.update(cx, |s, cx| s.focus(window, cx));
        let sub = cx.subscribe_in(
            &input,
            window,
            |this, _input, ev: &InputEvent, _window, cx| match ev {
                InputEvent::PressEnter { .. } => this.sftp_commit_edit_path(cx),
                InputEvent::Blur => this.sftp_cancel_edit_path(cx),
                _ => {}
            },
        );
        self.sftp_panel.editing_path = Some(input);
        self.sftp_panel.editing_path_sub = vec![sub];
        cx.notify();
    }

    pub(crate) fn sftp_commit_edit_path(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.sftp_panel.editing_path.take() else {
            return;
        };
        self.sftp_panel.editing_path_sub.clear();
        let value = input.read(cx).value().trim().to_string();
        if value.is_empty() {
            cx.notify();
            return;
        }
        // A successful navigate stays put; a failed one keeps the old listing and
        // surfaces the error (breadcrumb is already restored above).
        self.sftp_navigate(value, cx);
    }

    pub(crate) fn sftp_cancel_edit_path(&mut self, cx: &mut Context<Self>) {
        self.sftp_panel.editing_path = None;
        self.sftp_panel.editing_path_sub.clear();
        cx.notify();
    }

    /// Clear the always-visible search box (bound to Esc while it is focused).
    pub(crate) fn sftp_clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sftp_panel
            .filter_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    /// Enter an entry if it is a directory (or symlink-to-directory); do nothing
    /// for a file. Bound to a row double-click — downloading a file is only ever
    /// triggered explicitly from the right-click menu, never by clicking.
    pub(crate) fn sftp_enter_dir(&mut self, entry: SftpEntry, cx: &mut Context<Self>) {
        if is_dir_like(&entry) {
            let target = remote_join(&self.sftp_panel.cwd, &entry.name);
            self.sftp_navigate(target, cx);
        }
    }

    /// Context-menu primary action: enter a directory (or symlink-to-directory),
    /// or download a file/other symlink.
    pub(crate) fn sftp_open_entry(&mut self, entry: SftpEntry, cx: &mut Context<Self>) {
        let target = remote_join(&self.sftp_panel.cwd, &entry.name);
        if is_dir_like(&entry) {
            self.sftp_navigate(target, cx);
        } else {
            self.sftp_download_entry(entry, cx);
        }
    }

    pub(crate) fn sftp_download_entry(&mut self, entry: SftpEntry, cx: &mut Context<Self>) {
        let Some(pane_id) = self.sftp_panel.open_pane_id else {
            return;
        };
        // The entry name is server-supplied: a traversing name (`..`, `a/b`,
        // absolute — which `Path::join` would let replace the base entirely)
        // must not become the local destination. Same guard the daemon applies
        // to names discovered during the recursive walk.
        if !safe_local_name(&entry.name) {
            self.sftp_panel.error = Some(format!("refusing unsafe remote name {:?}", entry.name));
            cx.notify();
            return;
        }
        let remote = remote_join(&self.sftp_panel.cwd, &entry.name);
        let local = local_download_dir().join(&entry.name);
        let recursive = matches!(entry.kind, SftpEntryKind::Dir);
        let spec = SftpTransferSpec {
            pane_id,
            kind: SftpTransferKind::Download,
            local,
            remote,
            recursive,
        };
        match self.sftp_route().transfer_start(spec) {
            Ok(_) => self.sftp_panel.error = None,
            Err(e) => self.sftp_panel.error = Some(e),
        }
        self.sftp_poll_jobs(cx);
        self.sftp_start_polling(cx);
    }

    pub(crate) fn sftp_delete_entry(&mut self, entry: SftpEntry, cx: &mut Context<Self>) {
        let Some(pane_id) = self.sftp_panel.open_pane_id else {
            return;
        };
        let path = remote_join(&self.sftp_panel.cwd, &entry.name);
        // A directory (not a symlink to one) deletes recursively; everything else
        // is a plain file unlink.
        let op = if matches!(entry.kind, SftpEntryKind::Dir) {
            SftpOp::RemoveDir { path }
        } else {
            SftpOp::RemoveFile { path }
        };
        self.sftp_run_op(pane_id, op, cx);
    }

    /// Follow a symlink: readlink, then navigate to the resolved target's
    /// directory (or the target itself when it is a directory). The readlink
    /// round-trip runs off-thread so a slow link never freezes the UI.
    pub(crate) fn sftp_follow_symlink(&mut self, entry: SftpEntry, cx: &mut Context<Self>) {
        let Some(pane_id) = self.sftp_panel.open_pane_id else {
            return;
        };
        let cwd = self.sftp_panel.cwd.clone();
        let path = remote_join(&cwd, &entry.name);
        let route = self.sftp_route();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { route.op(SftpOp::Readlink { path }) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.sftp_panel.open_pane_id != Some(pane_id) {
                    return;
                }
                match result {
                    SftpOpResult::Link(target) => {
                        let resolved = if target.starts_with('/') {
                            target
                        } else {
                            remote_join(&cwd, &target)
                        };
                        // Navigate to the target if it's a directory, else its parent.
                        let dest = if entry.target_is_dir {
                            resolved
                        } else {
                            remote_parent(&resolved)
                        };
                        this.sftp_navigate(dest, cx);
                    }
                    SftpOpResult::Error(e) => {
                        this.sftp_panel.error = Some(e);
                        cx.notify();
                    }
                    _ => {}
                }
            });
        })
        .detach();
    }

    /// Run a one-shot SFTP op (mkdir/rename/chmod/delete) off-thread, then refresh
    /// the listing on success. Keeps the UI responsive during the round-trip.
    fn sftp_run_op(&mut self, pane_id: u64, op: SftpOp, cx: &mut Context<Self>) {
        let route = self.sftp_route();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { route.op(op) }).await;
            let _ = this.update(cx, |this, cx| {
                if this.sftp_panel.open_pane_id != Some(pane_id) {
                    return;
                }
                match result {
                    SftpOpResult::Error(e) => {
                        this.sftp_panel.error = Some(e);
                        cx.notify();
                    }
                    _ => {
                        this.sftp_panel.editing = None;
                        this.sftp_refresh(cx);
                    }
                }
            });
        })
        .detach();
    }

    // --- inline edit forms -------------------------------------------------

    pub(crate) fn sftp_begin_new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("New folder name"));
        self.sftp_panel.editing = Some(SftpEdit::NewFolder(input));
        cx.notify();
    }

    pub(crate) fn sftp_begin_new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("New file name"));
        self.sftp_panel.editing = Some(SftpEdit::NewFile(input));
        cx.notify();
    }

    pub(crate) fn sftp_begin_rename(
        &mut self,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name.clone()));
        self.sftp_panel.editing = Some(SftpEdit::Rename {
            original: name,
            input,
        });
        cx.notify();
    }

    pub(crate) fn sftp_begin_chmod(
        &mut self,
        entry: SftpEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let octal = format!("{:o}", entry.permissions & 0o777);
        let readable = mode_string(entry.permissions);
        let path = remote_join(&self.sftp_panel.cwd, &entry.name);
        let input = cx.new(|cx| InputState::new(window, cx).default_value(octal));
        self.sftp_panel.editing = Some(SftpEdit::Chmod {
            path,
            readable,
            input,
        });
        cx.notify();
    }

    pub(crate) fn sftp_cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.sftp_panel.editing = None;
        cx.notify();
    }

    pub(crate) fn sftp_commit_edit(&mut self, cx: &mut Context<Self>) {
        let Some(pane_id) = self.sftp_panel.open_pane_id else {
            return;
        };
        let op = match &self.sftp_panel.editing {
            Some(SftpEdit::NewFolder(input)) => {
                let name = input.read(cx).value().trim().to_string();
                if name.is_empty() {
                    return;
                }
                Some(SftpOp::Mkdir {
                    path: remote_join(&self.sftp_panel.cwd, &name),
                })
            }
            Some(SftpEdit::NewFile(input)) => {
                let name = input.read(cx).value().trim().to_string();
                if name.is_empty() {
                    return;
                }
                Some(SftpOp::CreateFile {
                    path: remote_join(&self.sftp_panel.cwd, &name),
                })
            }
            Some(SftpEdit::Rename { original, input }) => {
                let name = input.read(cx).value().trim().to_string();
                if name.is_empty() || name == *original {
                    self.sftp_panel.editing = None;
                    cx.notify();
                    return;
                }
                Some(SftpOp::Rename {
                    from: remote_join(&self.sftp_panel.cwd, original),
                    to: remote_join(&self.sftp_panel.cwd, &name),
                })
            }
            Some(SftpEdit::Chmod { path, input, .. }) => {
                match u32::from_str_radix(input.read(cx).value().trim(), 8) {
                    Ok(mode) => Some(SftpOp::Chmod {
                        path: path.clone(),
                        mode,
                    }),
                    Err(_) => {
                        self.sftp_panel.error = Some("invalid octal mode".to_string());
                        cx.notify();
                        return;
                    }
                }
            }
            None => None,
        };
        if let Some(op) = op {
            self.sftp_run_op(pane_id, op, cx);
        }
    }

    // --- uploads (picker + drag&drop) --------------------------------------

    /// FR-T5 fallback / toolbar action: open a native file picker and upload the
    /// chosen paths into the current remote directory.
    pub(crate) fn sftp_pick_upload(&mut self, cx: &mut Context<Self>) {
        if self.sftp_panel.open_pane_id.is_none() {
            return;
        }
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update(cx, |this, cx| this.sftp_upload_paths(paths, cx));
            }
        })
        .detach();
    }

    /// Upload local paths into the current remote directory (used by the picker
    /// and by FR-T5 Finder drops). Directories upload recursively.
    pub(crate) fn sftp_upload_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(pane_id) = self.sftp_panel.open_pane_id else {
            return;
        };
        let cwd = self.sftp_panel.cwd.clone();
        for path in paths {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let recursive = path.is_dir();
            let spec = SftpTransferSpec {
                pane_id,
                kind: SftpTransferKind::Upload,
                local: path,
                remote: remote_join(&cwd, &name),
                recursive,
            };
            if let Err(e) = self.sftp_route().transfer_start(spec) {
                self.sftp_panel.error = Some(e);
            }
        }
        self.sftp_poll_jobs(cx);
        self.sftp_start_polling(cx);
        // A little later the uploaded entries will exist; refresh the listing now
        // so at least already-finished small files appear.
        self.sftp_refresh(cx);
    }

    // --- transfer tray -----------------------------------------------------

    pub(crate) fn sftp_cancel_job(&mut self, job_id: u64, cx: &mut Context<Self>) {
        self.sftp_panel.jobs = RemoteTerminal::sftp_transfer_cancel(job_id);
        cx.notify();
    }

    /// Toggle the transfers/history view (header button): when on, the tray is
    /// pinned open and lists every transfer, dismissed or not.
    pub(crate) fn sftp_toggle_history(&mut self, cx: &mut Context<Self>) {
        self.sftp_panel.show_history = !self.sftp_panel.show_history;
        cx.notify();
    }

    /// Expand/collapse the transfers footer's per-job list (clicking its summary
    /// line). History mode forces it open, so this only bites outside history.
    pub(crate) fn sftp_toggle_tray(&mut self, cx: &mut Context<Self>) {
        self.sftp_panel.tray_expanded = !self.sftp_panel.tray_expanded;
        cx.notify();
    }

    /// Close the transfers tray: leave the history view and hide every
    /// currently-known job. A later transfer (a new job id) reopens the auto-tray.
    pub(crate) fn sftp_dismiss_tray(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<u64> = self.sftp_panel.jobs.iter().map(|j| j.job_id).collect();
        self.sftp_panel.dismissed_jobs.extend(ids);
        self.sftp_panel.show_history = false;
        cx.notify();
    }

    /// Reveal a finished download in the OS file manager (Finder), which opens its
    /// containing folder with the file selected.
    pub(crate) fn sftp_reveal_download(&self, local: String, cx: &mut Context<Self>) {
        cx.reveal_path(Path::new(&local));
    }

    fn sftp_poll_jobs(&mut self, cx: &mut Context<Self>) {
        if self.sftp_panel.open_pane_id.is_some() {
            self.sftp_panel.jobs = self.sftp_route().transfer_list();
            cx.notify();
        }
    }

    /// Spawn a background poll loop that refreshes the tray every 500ms while the
    /// panel is open. `poll_gen` guards against overlapping loops after re-opens.
    fn sftp_start_polling(&mut self, cx: &mut Context<Self>) {
        self.sftp_panel.poll_gen = self.sftp_panel.poll_gen.wrapping_add(1);
        let generation = self.sftp_panel.poll_gen;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                // Read the pane still bound to this generation.
                let pane = this
                    .update(cx, |this, cx| {
                        if this.sftp_panel.poll_gen != generation {
                            return None;
                        }
                        // The browser has no window of its own — it's a view
                        // inside the detail panel — so a closed panel means
                        // nobody is looking, and this loop is a daemon
                        // round-trip plus a full re-render twice a second for
                        // a column that isn't on screen.
                        //
                        // The render path retires the browser too, and does it
                        // a frame sooner. This is the backstop: owning the
                        // check here means the loop's lifetime doesn't depend
                        // on `render_right_panel` being called on every frame,
                        // which is a property of a caller it can't see.
                        // Retire rather than pause — reopening the panel on
                        // Files runs the browser's normal open path, which
                        // starts a fresh loop.
                        if !this.right_panel_open(cx) {
                            this.sftp_close_browser(cx);
                            return None;
                        }
                        // The route, not just the id: a remote workspace's
                        // jobs are filed under the workspace, so polling by
                        // pane id would come back empty every time.
                        this.sftp_panel
                            .open_pane_id
                            .is_some()
                            .then(|| this.sftp_route())
                    })
                    .ok()
                    .flatten();
                let Some(route) = pane else { break };
                // Poll off the main thread so the blocking control round-trip
                // doesn't jank the UI.
                let jobs = cx
                    .background_spawn(async move { route.transfer_list() })
                    .await;
                let keep = this
                    .update(cx, |this, cx| {
                        if this.sftp_panel.poll_gen != generation {
                            return false;
                        }
                        this.sftp_panel.jobs = jobs;
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        })
        .detach();
    }

    // ---------------------------------------------------------------------
    // Rendering.
    // ---------------------------------------------------------------------

    /// The Files tab's remote mode: the pane's SFTP browser, rendered as the
    /// panel's own column rather than the bottom dock it used to be. Same
    /// interaction as before — a breadcrumb you can type into, a filter, a
    /// dir-first list led by `..`, per-row right-click actions — relaid out for a
    /// ~260px column: the toolbar collapses to a refresh tile plus a `⋯`, and the
    /// permissions column goes (it's still on the right-click `chmod…`), because
    /// name + size + mode can't share this width without all three truncating.
    ///
    /// `host` names the machine in the header's count slot. It earns that slot:
    /// this tab silently swaps between a local tree and a remote filesystem as the
    /// detail pane changes, and the list carries rename and delete — so which
    /// machine you're deleting on is not something to leave implicit.
    pub(crate) fn render_panel_sftp(
        &mut self,
        host: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let controls = self.sftp_controls(cx);
        let title = self.panel_title("Files", Some(host), Some(controls), window, cx);
        let breadcrumb = self.render_sftp_breadcrumb(cx);
        // The shared panel search box, plus the one behaviour the old SFTP header
        // had that the local tree's doesn't: Esc clears the filter rather than
        // falling through to the terminal.
        let filter = div()
            .id("panel-sftp-filter")
            .child(self.panel_search(&self.sftp_panel.filter_input.clone(), cx))
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                if ev.keystroke.key == "escape" {
                    this.sftp_clear_filter(window, cx);
                }
            }));
        let form = self.render_sftp_edit_form(cx);
        let list = self.render_sftp_list(cx);

        v_flex()
            .id("panel-sftp")
            .flex_1()
            .min_h_0()
            .child(title)
            .child(breadcrumb)
            .child(filter)
            .children(form)
            .child(crate::ui::scrollbar::with_vertical_scrollbar(
                "sftp-list-scrollbar",
                list,
                &self.sftp_panel.scroll,
            ))
            // FR-T5: a Finder drop uploads onto the current directory.
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                this.sftp_upload_paths(paths.paths().to_vec(), cx);
            }))
            .into_any_element()
    }

    /// The remote Files header's controls: refresh, and a `⋯` for everything that
    /// isn't a per-row action. Two tiles is what the header has room for beside a
    /// hostname, and refresh is the one that earns a permanent slot — a remote
    /// listing has no watcher behind it, so it's the only way to see a change
    /// somebody else made.
    fn sftp_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let history = self.sftp_panel.show_history;
        let tile = |button: Button, selected: bool, cx: &mut Context<Self>| {
            crate::ui::tab_strip::chrome_tile(button, selected, cx)
                .xsmall()
                .w(px(24.))
                .h(px(24.))
                .rounded_md()
        };

        h_flex()
            .items_center()
            .gap(px(2.))
            .child(
                // `occlude()` like the `⋯` beside it: `panel_title` is a
                // `WindowControlArea::Drag` now (every header in the window is),
                // which on Windows maps to HTCAPTION — the OS claims the press
                // before gpui hit-tests, so a bare button never fires its click.
                div().occlude().child(
                    tile(
                        Button::new("panel-sftp-refresh")
                            .icon(Icon::empty().path("icons/refresh.svg").size(px(13.))),
                        false,
                        cx,
                    )
                    .tooltip("Refresh")
                    .on_click(cx.listener(|this, _, _w, cx| this.sftp_refresh(cx))),
                ),
            )
            .child(
                div().occlude().child(
                    tile(
                        Button::new("panel-sftp-menu")
                            .icon(Icon::empty().path("icons/ellipsis.svg").size(px(13.))),
                        false,
                        cx,
                    )
                    .tooltip("More")
                    .dropdown_menu_with_anchor(gpui::Anchor::TopRight, {
                        let app = cx.entity().downgrade();
                        move |menu, _window, _cx| {
                            let mut menu = menu.min_w(px(190.));
                            for (label, action) in [
                                ("New folder", SftpMenuAction::NewFolder),
                                ("New file", SftpMenuAction::NewFile),
                                ("Upload…", SftpMenuAction::Upload),
                                ("Go to shell directory", SftpMenuAction::GotoShellCwd),
                            ] {
                                menu = menu.item(PopupMenuItem::new(label).on_click({
                                    let app = app.clone();
                                    move |_, window, cx| {
                                        let _ = app.update(cx, |this, cx| {
                                            this.sftp_menu_action(action, window, cx)
                                        });
                                    }
                                }));
                            }
                            menu.separator().item(
                                PopupMenuItem::new(if history {
                                    "Hide transfer history"
                                } else {
                                    "Transfer history"
                                })
                                .on_click({
                                    let app = app.clone();
                                    move |_, window, cx| {
                                        let _ = app.update(cx, |this, cx| {
                                            this.sftp_menu_action(
                                                SftpMenuAction::ToggleHistory,
                                                window,
                                                cx,
                                            )
                                        });
                                    }
                                }),
                            )
                        }
                    }),
                ),
            )
            .into_any_element()
    }

    /// One arm per `⋯` entry. A single dispatcher rather than five closures each
    /// re-deriving the weak handle, since the menu items all need `&mut Window`.
    fn sftp_menu_action(
        &mut self,
        action: SftpMenuAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            SftpMenuAction::NewFolder => self.sftp_begin_new_folder(window, cx),
            SftpMenuAction::NewFile => self.sftp_begin_new_file(window, cx),
            SftpMenuAction::Upload => self.sftp_pick_upload(cx),
            SftpMenuAction::GotoShellCwd => {
                if let Some(pane_id) = self.sftp_panel.open_pane_id
                    && let Some(cwd) = self.pane_shell_cwd(pane_id, window, cx)
                {
                    self.sftp_navigate(cwd, cx);
                }
            }
            SftpMenuAction::ToggleHistory => self.sftp_toggle_history(cx),
        }
    }

    /// The path bar. Normally a clickable breadcrumb (root shown as `SFTP`, like
    /// tabby); double-clicking anywhere on it switches to a text input so you can
    /// type a destination directly. Enter navigates, Esc/blur returns to the
    /// breadcrumb.
    fn render_sftp_breadcrumb(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        if let Some(input) = &self.sftp_panel.editing_path {
            return h_flex()
                .id("sftp-path-edit")
                .px(px(CONTENT_INSET))
                .pb(px(2.))
                .child(Input::new(input).xsmall())
                // Esc cancels back to the breadcrumb (blur also cancels, via the
                // input subscription).
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _window, cx| {
                    if ev.keystroke.key == "escape" {
                        this.sftp_cancel_edit_path(cx);
                    }
                }));
        }

        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        // Double-click anywhere on the bar enters "type a path" mode.
        let mut row = h_flex()
            .id("sftp-breadcrumb")
            .flex_wrap()
            .items_center()
            .gap_0p5()
            .px(px(CONTENT_INSET))
            .pb(px(4.))
            .on_double_click(
                cx.listener(|this, _, window, cx| this.sftp_begin_edit_path(window, cx)),
            );
        // Root: `/`, the actual path — the header above already says which machine
        // this is, so the old "SFTP" label would be naming the protocol in the one
        // place the user is reading a path. The current (last) segment reads in
        // full ink; ancestors are muted but still clearly legible (the theme
        // `accent` was near-invisible here).
        let segments = breadcrumb_segments(&self.sftp_panel.cwd);
        let last = segments.len().saturating_sub(1);
        for (i, (label, path)) in segments.into_iter().enumerate() {
            if i > 0 {
                row = row.child(div().text_xs().text_color(muted).child("›"));
            }
            let is_current = i == last;
            let label = if i == 0 { "/".to_string() } else { label };
            let weight = if i == 0 || is_current {
                FontWeight::MEDIUM
            } else {
                FontWeight::NORMAL
            };
            let color = if is_current { foreground } else { muted };
            let seg_id = SharedString::from(format!("sftp-crumb-{path}"));
            row = row.child(
                div()
                    .id(seg_id)
                    .text_xs()
                    .font_weight(weight)
                    .text_color(color)
                    .cursor_pointer()
                    .hover(|s| s.text_color(foreground).underline())
                    .child(label)
                    .on_click(
                        cx.listener(move |this, _, _w, cx| this.sftp_navigate(path.clone(), cx)),
                    ),
            );
        }
        // A flex-grow spacer so the double-click target spans the whole row.
        row.child(div().flex_1().min_w(px(20.)).h(px(16.)))
    }

    /// The active inline edit form (new folder / rename / chmod), if any.
    fn render_sftp_edit_form(&self, cx: &mut Context<Self>) -> Option<Div> {
        let secondary = cx.theme().secondary;
        let border = cx.theme().border;
        let foreground = cx.theme().foreground;
        let (title, input): (String, _) = match self.sftp_panel.editing.as_ref()? {
            SftpEdit::NewFolder(input) => ("New folder".to_string(), input),
            SftpEdit::NewFile(input) => ("New file".to_string(), input),
            SftpEdit::Rename { input, .. } => ("Rename".to_string(), input),
            SftpEdit::Chmod {
                readable, input, ..
            } => (format!("Permissions · {readable}"), input),
        };
        Some(
            v_flex()
                .gap(px(5.))
                .mx(px(CONTENT_INSET - 4.))
                .mb(px(4.))
                .p(px(6.))
                .bg(secondary)
                .border_1()
                .border_color(border)
                .rounded_md()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(foreground)
                        .child(title),
                )
                .child(Input::new(input).xsmall())
                .child(
                    h_flex()
                        .gap(px(4.))
                        .justify_end()
                        .child(
                            Button::new("sftp-edit-cancel")
                                .label("Cancel")
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(|this, _, _w, cx| this.sftp_cancel_edit(cx))),
                        )
                        .child(
                            Button::new("sftp-edit-ok")
                                .label("OK")
                                .xsmall()
                                .primary()
                                .on_click(cx.listener(|this, _, _w, cx| this.sftp_commit_edit(cx))),
                        ),
                ),
        )
    }

    fn render_sftp_list(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let danger = cx.theme().danger;
        let muted = cx.theme().muted_foreground;
        // Rows inset themselves so their hover capsule bleeds into the gutter, the
        // same way the local tree's and the Changes list's do.
        let container = div()
            .id("sftp-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.sftp_panel.scroll)
            .px(px(CONTENT_INSET - 6.))
            .pb(px(4.));

        let note = |text: gpui::SharedString, color| {
            div()
                .px(px(6.))
                .py(px(4.))
                .text_size(px(12.))
                .text_color(color)
                .child(text)
        };

        if let Some(err) = &self.sftp_panel.error {
            return container.child(note(err.clone().into(), danger));
        }

        let filter = self.sftp_panel.filter_input.read(cx).value().to_string();
        let entries = sorted_filtered_entries(&self.sftp_panel.entries, &filter);

        // A `..` parent row leads the list when not at the root and not
        // actively filtering — the file-manager convention for going up.
        let show_go_up = self.sftp_panel.cwd != "/" && filter.trim().is_empty();

        if entries.is_empty() && !show_go_up {
            // Distinguish "still loading" from a genuinely empty directory so a
            // slow listing doesn't read as empty.
            let text = if self.sftp_panel.loading {
                "Loading…"
            } else {
                "Empty directory."
            };
            return container.child(note(text.into(), muted));
        }

        let mut list = v_flex().gap(px(1.)).py(px(2.));
        if show_go_up {
            list = list.child(self.render_sftp_go_up_row(cx));
        }
        for entry in entries {
            list = list.child(self.render_sftp_row(entry, cx));
        }
        container.child(list)
    }

    /// The leading `..` parent row (shown when not at the filesystem root), styled
    /// like a directory entry (WinRAR/file-manager convention) so it reads as
    /// "the parent folder" and matches the rows below rather than a toolbar action.
    fn render_sftp_go_up_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let foreground = cx.theme().foreground;
        // Matches the directory rows below it, which paint on the popover surface.
        let sf = cx.global::<crate::ui::presets::Surfaces>().popover;
        h_flex()
            .id("sftp-go-up")
            .items_center()
            .gap_1()
            .pl(px(6.))
            .pr_1()
            .py_1()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .hover(|s| s.bg(gpui::rgb(sf.hover)))
            .child(
                Icon::new(IconName::FolderOpen)
                    .xsmall()
                    .text_color(foreground),
            )
            .child(div().flex_1().min_w_0().text_sm().child(".."))
            .on_click(cx.listener(|this, _, _w, cx| this.sftp_up(cx)))
            .into_any_element()
    }

    /// One entry row: icon + name (+ a `→` marker for symlinks) + a muted size.
    /// The permissions column the bottom dock had is gone — at panel width, name,
    /// size and mode all three truncated, and mode is a specialist datum that the
    /// row's `chmod…` still reads out on demand.
    ///
    /// Per-row actions (open/download, follow, rename, chmod,
    /// delete) live in the right-click context menu built by
    /// [`sftp_row_context_menu`](Self::sftp_row_context_menu) rather than as a
    /// row of inline buttons (PRD §6.3: hotkeys + right-click, not a permanent
    /// toolbar). Left-click / double-click on the name still opens a directory or
    /// downloads a file — the primary interaction is unchanged.
    fn render_sftp_row(&self, entry: &SftpEntry, cx: &mut Context<Self>) -> AnyElement {
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        // Directories use the full foreground ink so they read clearly against the
        // monochrome UI (files stay muted); a coloured folder clashed with the
        // theme, and the old `accent` was near-invisible in the light theme.
        let dir_color = foreground;
        let list_hover = cx.theme().list_hover;
        let entry = entry.clone();
        let dir_like = is_dir_like(&entry);
        let icon = if dir_like {
            IconName::Folder
        } else {
            IconName::File
        };
        let is_symlink = matches!(entry.kind, SftpEntryKind::Symlink);
        let size = if dir_like {
            String::new()
        } else {
            human_size(entry.size)
        };
        let name_label = if is_symlink {
            format!("{} →", entry.name)
        } else {
            entry.name.clone()
        };
        let row_id = SharedString::from(format!("sftp-row-{}", entry.name));

        let open_entry = entry.clone();
        let menu_entry = entry.clone();
        // Weak app handle so the context-menu item handlers (which get `&mut App`,
        // not `Context<Self>`) can call back into `Tty7App`.
        let app = cx.entity().downgrade();

        h_flex()
            .id(row_id)
            .items_center()
            .gap_1()
            .pl(px(6.))
            .pr_1()
            .py_1()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .hover(|s| s.bg(list_hover))
            // Double-click enters a directory; files never download from a click
            // (only from the right-click menu).
            .on_double_click(
                cx.listener(move |this, _, _w, cx| this.sftp_enter_dir(open_entry.clone(), cx)),
            )
            .child(
                Icon::new(icon)
                    .xsmall()
                    .text_color(if dir_like { dir_color } else { muted }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(foreground)
                    .truncate()
                    .child(name_label),
            )
            // Size trails the name, right-aligned in its own column so the sizes
            // line up down the list. Directories contribute an empty string, so
            // the column simply doesn't draw for them.
            .child(div().flex_none().text_xs().text_color(muted).child(size))
            .context_menu(move |menu, _window, cx| {
                let danger = cx.theme().danger;
                Self::sftp_row_context_menu(menu, &menu_entry, dir_like, is_symlink, danger, &app)
            })
            .into_any_element()
    }

    /// Build the per-row right-click menu: the primary open/download action
    /// first, an optional follow-symlink, rename, chmod, and finally the
    /// destructive delete (separated). Each item drives the same `Tty7App`
    /// handler the old inline buttons did, via the weak `app` handle.
    fn sftp_row_context_menu(
        menu: gpui_component::menu::PopupMenu,
        entry: &SftpEntry,
        dir_like: bool,
        is_symlink: bool,
        danger: gpui::Hsla,
        app: &gpui::WeakEntity<Self>,
    ) -> gpui_component::menu::PopupMenu {
        let mut menu = menu.min_w(px(180.));

        // Primary action, first: open a directory or download a file. Reuses
        // `sftp_open_entry`, which dispatches on the entry kind.
        let primary_label = if dir_like { "Open" } else { "Download" };
        menu = menu.item(PopupMenuItem::new(primary_label).on_click({
            let app = app.clone();
            let entry = entry.clone();
            move |_, _window, cx| {
                let entry = entry.clone();
                let _ = app.update(cx, |this, cx| this.sftp_open_entry(entry, cx));
            }
        }));

        // Follow symlink — only for symlinks.
        if is_symlink {
            menu = menu.item(PopupMenuItem::new("Follow symlink").on_click({
                let app = app.clone();
                let entry = entry.clone();
                move |_, _window, cx| {
                    let entry = entry.clone();
                    let _ = app.update(cx, |this, cx| this.sftp_follow_symlink(entry, cx));
                }
            }));
        }

        menu = menu
            .item(PopupMenuItem::new("Rename").on_click({
                let app = app.clone();
                let name = entry.name.clone();
                move |_, window, cx| {
                    let name = name.clone();
                    let _ = app.update(cx, |this, cx| this.sftp_begin_rename(name, window, cx));
                }
            }))
            .item(PopupMenuItem::new("chmod…").on_click({
                let app = app.clone();
                let entry = entry.clone();
                move |_, window, cx| {
                    let entry = entry.clone();
                    let _ = app.update(cx, |this, cx| this.sftp_begin_chmod(entry, window, cx));
                }
            }))
            .separator();

        // Destructive, rendered last in danger red and set apart by the
        // separator above.
        menu.item(
            PopupMenuItem::element(move |_window, _cx| div().text_color(danger).child("Delete"))
                .on_click({
                    let app = app.clone();
                    let entry = entry.clone();
                    move |_, _window, cx| {
                        let entry = entry.clone();
                        let _ = app.update(cx, |this, cx| this.sftp_delete_entry(entry, cx));
                    }
                }),
        )
    }

    /// The transfers footer, pinned to the bottom of the detail panel across all
    /// four of its tabs rather than living inside Files.
    ///
    /// That placement is deliberate: a transfer belongs to the *pane*, not to the
    /// tab you happen to be reading, so going to Info to check a port shouldn't
    /// make a running upload disappear. It stays pane-scoped for the same reason —
    /// aggregating every pane's jobs would quietly turn the panel into a
    /// window-level transfer centre, which is not what this column is.
    ///
    /// Nothing is lost when it goes away: the jobs live in the daemon, keyed by
    /// pane (`sftp_transfer_list`), so switching panes and coming back re-queries
    /// them intact.
    ///
    /// Collapsed by default — one line summarising the run, with its own progress
    /// underline — because a transfer is something you glance at, not something
    /// you watch. Clicking the line expands the per-job list.
    pub(crate) fn sftp_transfers_footer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        // Only the pane the panel is showing. `open_pane_id` is set by the Files
        // tab, so a transfer started there stays visible from any tab — but only
        // while that pane is the one on screen.
        self.sftp_panel.open_pane_id?;
        let history = self.sftp_panel.show_history;
        let jobs: Vec<&SftpJobProgress> = self
            .sftp_panel
            .jobs
            .iter()
            .filter(|j| history || !self.sftp_panel.dismissed_jobs.contains(&j.job_id))
            .collect();
        // Auto mode with nothing to show → no footer at all. History mode stays up
        // (with an empty-state note) so the menu item always reveals something.
        if jobs.is_empty() && !history {
            return None;
        }

        // Colours copied out rather than held as a `theme` binding: the expanded
        // body below needs `&mut cx` for its rows, which an outstanding theme
        // borrow would block.
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let accent = cx.theme().accent;
        let border = cx.theme().border;
        let sidebar = cx.theme().sidebar;
        let hover = gpui::rgb(cx.global::<crate::ui::presets::Surfaces>().sidebar.hover);
        let expanded = self.sftp_panel.tray_expanded || history;

        // The summary line: how many are moving and how far along the run is, as
        // one number. Bytes across jobs, not a mean of percentages, so a big file
        // beside a small one doesn't read as half done the moment the small one is.
        let running = jobs
            .iter()
            .filter(|j| matches!(j.state, SftpJobState::Running))
            .count();
        let (done, total): (u64, u64) = jobs
            .iter()
            .filter(|j| matches!(j.state, SftpJobState::Running))
            .fold((0, 0), |(d, t), j| (d + j.bytes_done, t + j.bytes_total));
        let failed = jobs
            .iter()
            .filter(|j| matches!(j.state, SftpJobState::Error))
            .count();
        let pct = if total > 0 {
            ((done as f64 / total as f64) * 100.0).min(100.0)
        } else {
            0.0
        };
        let summary = if running > 0 {
            format!("{running} transferring · {pct:.0}%")
        } else if failed > 0 {
            format!("{failed} failed")
        } else {
            "Transfers".to_string()
        };
        let summary_color = if running == 0 && failed > 0 {
            danger
        } else {
            muted
        };

        let head = h_flex()
            .id("sftp-transfers-summary")
            .items_center()
            .gap(px(6.))
            .px(px(CONTENT_INSET))
            .h(px(28.))
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(|this, _, _w, cx| this.sftp_toggle_tray(cx)))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(if expanded { "⌄" } else { "›" }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.5))
                    .text_color(summary_color)
                    .child(summary),
            )
            .child(
                div()
                    .flex_none()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        crate::ui::tab_strip::chrome_tile(
                            Button::new("sftp-tray-close")
                                .icon(IconName::Close)
                                .xsmall(),
                            false,
                            cx,
                        )
                        .w(px(18.))
                        .h(px(18.))
                        .rounded(px(4.))
                        .tooltip("Dismiss")
                        .on_click(cx.listener(|this, _, _w, cx| this.sftp_dismiss_tray(cx))),
                    ),
            );

        // The collapsed bar carries the run's progress as a hairline along its own
        // bottom edge, so "how far along" survives the collapse.
        let underline = div().h(px(2.)).w_full().bg(border).child(
            div()
                .h_full()
                .w(gpui::relative((pct / 100.0) as f32))
                .bg(if failed > 0 { danger } else { accent }),
        );

        let body = expanded.then(|| {
            let inner: Div = if jobs.is_empty() {
                v_flex().child(
                    div()
                        .px(px(CONTENT_INSET))
                        .py(px(3.))
                        .text_size(px(11.5))
                        .text_color(muted)
                        .child("No transfers yet."),
                )
            } else {
                let mut list = v_flex().px(px(CONTENT_INSET)).pb(px(6.)).gap(px(6.));
                for job in jobs {
                    list = list.child(self.render_sftp_job(job, cx));
                }
                list
            };
            div()
                .id("sftp-transfers-list")
                // Never more than a third of the column: the footer reports on the
                // panel, it doesn't become it.
                .max_h(px(200.))
                .overflow_y_scroll()
                .child(inner)
        });

        Some(
            v_flex()
                .flex_none()
                .border_t_1()
                .border_color(border)
                .bg(sidebar)
                .child(head)
                .when(running > 0 && !expanded, |this| this.child(underline))
                .children(body)
                .into_any_element(),
        )
    }

    fn render_sftp_job(&self, job: &SftpJobProgress, cx: &mut Context<Self>) -> Div {
        let foreground = cx.theme().foreground;
        let border = cx.theme().border;
        let danger = cx.theme().danger;
        let success = cx.theme().success;
        let muted = cx.theme().muted_foreground;
        let accent = cx.theme().accent;
        let arrow = match job.kind {
            SftpTransferKind::Upload => "↑",
            SftpTransferKind::Download => "↓",
        };
        let name = remote_basename(&job.remote);
        let pct = if job.bytes_total > 0 {
            ((job.bytes_done as f64 / job.bytes_total as f64) * 100.0).min(100.0)
        } else {
            0.0
        };
        let status = match job.state {
            SftpJobState::Running => format!(
                "{} / {} ({pct:.0}%)",
                human_size(job.bytes_done),
                human_size(job.bytes_total)
            ),
            SftpJobState::Done => "done".to_string(),
            SftpJobState::Cancelled => "cancelled".to_string(),
            SftpJobState::Error => job.error.clone().unwrap_or_else(|| "error".to_string()),
        };
        let status_color = match job.state {
            SftpJobState::Error => danger,
            SftpJobState::Done => success,
            _ => muted,
        };
        let bar_color = if matches!(job.state, SftpJobState::Error) {
            danger
        } else {
            accent
        };
        let job_id = job.job_id;
        let running = matches!(job.state, SftpJobState::Running);
        // A finished download can be revealed in Finder from its local path.
        let done_download = matches!(job.state, SftpJobState::Done)
            && matches!(job.kind, SftpTransferKind::Download)
            && !job.local.is_empty();
        let local = job.local.clone();

        v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(foreground)
                            .truncate()
                            .child(format!("{arrow} {name}")),
                    )
                    .when(done_download, |this| {
                        this.child(
                            Button::new(("sftp-reveal-job", job_id as usize))
                                .icon(IconName::FolderOpen)
                                .xsmall()
                                .ghost()
                                .tooltip("Show in Finder")
                                .on_click(cx.listener(move |this, _, _w, cx| {
                                    this.sftp_reveal_download(local.clone(), cx)
                                })),
                        )
                    })
                    .when(running, |this| {
                        this.child(
                            Button::new(("sftp-cancel-job", job_id as usize))
                                .label("✕")
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _w, cx| {
                                    this.sftp_cancel_job(job_id, cx)
                                })),
                        )
                    }),
            )
            .child(
                // A thin progress bar.
                div().h(px(3.)).w_full().rounded_full().bg(border).child(
                    div()
                        .h_full()
                        .w(gpui::relative((pct / 100.0) as f32))
                        .rounded_full()
                        .bg(bar_color),
                ),
            )
            .child(div().text_xs().text_color(status_color).child(status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: SftpEntryKind, target_is_dir: bool) -> SftpEntry {
        SftpEntry {
            name: name.to_string(),
            kind,
            size: 0,
            mtime: 0,
            permissions: 0,
            target_is_dir,
        }
    }

    #[test]
    fn breadcrumb_segments_splits_absolute_paths() {
        assert_eq!(breadcrumb_segments("/"), vec![("/".into(), "/".into())]);
        assert_eq!(
            breadcrumb_segments("/home/deploy"),
            vec![
                ("/".to_string(), "/".to_string()),
                ("home".to_string(), "/home".to_string()),
                ("deploy".to_string(), "/home/deploy".to_string()),
            ]
        );
        // Unicode components survive and build correct cumulative paths.
        assert_eq!(
            breadcrumb_segments("/项目/子"),
            vec![
                ("/".to_string(), "/".to_string()),
                ("项目".to_string(), "/项目".to_string()),
                ("子".to_string(), "/项目/子".to_string()),
            ]
        );
    }

    #[test]
    fn sort_puts_dirs_first_then_name_case_insensitively() {
        let entries = vec![
            entry("Zebra.txt", SftpEntryKind::File, false),
            entry("apple", SftpEntryKind::Dir, false),
            entry("beta.txt", SftpEntryKind::File, false),
            entry("Alpha", SftpEntryKind::Dir, false),
            entry("link-to-dir", SftpEntryKind::Symlink, true),
            entry("link-to-file", SftpEntryKind::Symlink, false),
        ];
        let sorted: Vec<&str> = sorted_filtered_entries(&entries, "")
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        // Dir-likes first (Alpha, apple, link-to-dir), then files/other symlinks.
        assert_eq!(
            sorted,
            vec![
                "Alpha",
                "apple",
                "link-to-dir",
                "beta.txt",
                "link-to-file",
                "Zebra.txt",
            ]
        );
    }

    #[test]
    fn filter_is_case_insensitive_substring() {
        let entries = vec![
            entry("README.md", SftpEntryKind::File, false),
            entry("src", SftpEntryKind::Dir, false),
            entry("Cargo.toml", SftpEntryKind::File, false),
        ];
        // Filter "a" matches "Cargo.toml" (lowercase a) and "README.md" (the
        // uppercase A) — exercising case-insensitive substring matching — but not
        // "src". Sorted by name, "Cargo.toml" precedes "README.md".
        let names: Vec<&str> = sorted_filtered_entries(&entries, "a")
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, vec!["Cargo.toml", "README.md"]);
    }

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0K");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(1024 * 1024), "1.0M");
    }

    #[test]
    fn mode_string_renders_rwx() {
        assert_eq!(mode_string(0o755), "rwxr-xr-x");
        assert_eq!(mode_string(0o644), "rw-r--r--");
        assert_eq!(mode_string(0o000), "---------");
        assert_eq!(mode_string(0o777), "rwxrwxrwx");
    }
}

#[cfg(test)]
mod gpui_tests {
    use crate::core::config::{Config, RightPanelTab};
    use crate::core::session::Session;
    use crate::ui::app::Tty7App;
    use gpui::{AppContext, Entity, TestAppContext, VisualTestContext};

    fn harness(cx: &mut TestAppContext) -> (Entity<Tty7App>, VisualTestContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
            crate::ui::keymap::init(cx);
        });
        // Wrapped in a `Root` like `main.rs` does — gpui-component widgets in the
        // panel reach for it on the window.
        let window = cx.add_window(|window, cx| {
            let app =
                cx.new(|cx| Tty7App::with_session(None, Some(Session::default()), window, cx));
            gpui_component::Root::new(app, window, cx)
        });
        cx.background_executor.run_until_parked();
        let app = window
            .update(cx, |root, _, _| {
                root.view()
                    .clone()
                    .downcast::<Tty7App>()
                    .unwrap_or_else(|_| panic!("window root wraps a Tty7App"))
            })
            .unwrap();
        let vcx = VisualTestContext::from_window(window.into(), cx);
        (app, vcx)
    }

    /// The *window's* panel state, not the config's: the config is only what a
    /// newly opened window starts with, so asserting on it would pass even if
    /// this window's panel never moved.
    fn panel(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> (bool, RightPanelTab) {
        vcx.update(|_, cx| {
            let app = app.read(cx);
            (app.right_panel_visible, app.right_panel_tab)
        })
    }

    /// `ToggleSftp` has to earn its name: it takes you to Files, and pressing it
    /// again there puts the panel away. It used to only ever open, so a key bound
    /// to it was a dead press once you'd arrived.
    #[gpui::test]
    fn toggle_sftp_opens_files_then_closes_the_panel(cx: &mut TestAppContext) {
        let (app, mut vcx) = harness(cx);

        // From closed: opens the panel on Files.
        app.update_in(&mut vcx, |app, window, cx| {
            app.right_panel_visible = false;
            app.toggle_sftp(window, cx);
        });
        assert_eq!(panel(&app, &mut vcx), (true, RightPanelTab::Files));

        // Already there: puts it away rather than re-selecting the same tab.
        app.update_in(&mut vcx, |app, window, cx| app.toggle_sftp(window, cx));
        assert!(!panel(&app, &mut vcx).0, "second press should close");

        // And back again.
        app.update_in(&mut vcx, |app, window, cx| app.toggle_sftp(window, cx));
        assert_eq!(panel(&app, &mut vcx), (true, RightPanelTab::Files));
    }

    /// Open on another tab, `ToggleSftp` is still "take me there" — it switches to
    /// Files rather than closing a panel the user is using for something else.
    #[gpui::test]
    fn toggle_sftp_switches_tabs_before_it_closes(cx: &mut TestAppContext) {
        let (app, mut vcx) = harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.set_right_panel_tab(RightPanelTab::Info, cx);
            app.toggle_sftp(window, cx);
        });
        assert_eq!(panel(&app, &mut vcx), (true, RightPanelTab::Files));
    }
}

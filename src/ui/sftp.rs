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
use crate::ui::i18n::{L10nKey, t, t_fmt};

#[derive(Clone, Copy)]
enum SftpMenuAction {
    NewFolder,
    NewFile,
    Upload,
    GotoShellCwd,
    ToggleHistory,
}

pub(crate) enum SftpEdit {
    NewFolder(gpui::Entity<InputState>),
    NewFile(gpui::Entity<InputState>),
    Rename {
        original: String,
        input: gpui::Entity<InputState>,
    },
    Chmod {
        path: String,
        readable: String,
        input: gpui::Entity<InputState>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct SftpRoute {
    pane_id: u64,
    workspace: Option<crate::terminal::PaneWorkspace>,
}

impl SftpRoute {
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
            Ok(other) => Err(t_fmt(
                L10nKey::SftpErrorUnexpectedReply,
                &[("reply", &format!("{other:?}"))],
            )),
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
            Ok(other) => SftpOpResult::Error(t_fmt(
                L10nKey::SftpErrorUnexpectedReply,
                &[("reply", &format!("{other:?}"))],
            )),
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
            Ok(other) => Err(t_fmt(
                L10nKey::SftpErrorUnexpectedReply,
                &[("reply", &format!("{other:?}"))],
            )),
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

pub(crate) struct SftpPanelState {
    pub(crate) open_pane_id: Option<u64>,
    pub(crate) open_workspace: Option<crate::terminal::PaneWorkspace>,
    pub(crate) cwd: String,
    pub(crate) cwds: std::collections::HashMap<u64, String>,
    pub(crate) entries: Vec<SftpEntry>,
    pub(crate) filter_input: gpui::Entity<InputState>,
    pub(crate) error: Option<String>,
    pub(crate) jobs: Vec<SftpJobProgress>,
    dismissed_jobs: HashSet<u64>,
    show_history: bool,
    tray_expanded: bool,
    pub(crate) loading: bool,
    nav_gen: u64,
    pub(crate) editing: Option<SftpEdit>,
    pub(crate) editing_path: Option<gpui::Entity<InputState>>,
    editing_path_sub: Vec<Subscription>,
    pub(crate) poll_gen: u64,
    scroll: gpui::ScrollHandle,
    _subs: Vec<Subscription>,
}

impl SftpPanelState {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Tty7App>) -> Self {
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::ui::i18n::t(crate::ui::i18n::L10nKey::Search))
        });
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

fn is_dir_like(e: &SftpEntry) -> bool {
    matches!(e.kind, SftpEntryKind::Dir)
        || (matches!(e.kind, SftpEntryKind::Symlink) && e.target_is_dir)
}

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
        bd.cmp(&ad)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

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

fn local_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn local_download_dir() -> PathBuf {
    local_home().join("Downloads")
}

impl Tty7App {
    pub(crate) fn toggle_sftp(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        use crate::core::config::RightPanelTab;
        if self.right_panel_visible && self.right_panel_tab == RightPanelTab::Files {
            self.toggle_right_panel(cx);
            return;
        }
        self.set_right_panel_tab(RightPanelTab::Files, cx);
    }

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

    pub(crate) fn sftp_close_browser(&mut self, cx: &mut Context<Self>) {
        self.sftp_panel.open_pane_id = None;
        self.sftp_panel.entries.clear();
        self.sftp_panel.error = None;
        self.sftp_panel.editing = None;
        self.sftp_panel.editing_path = None;
        self.sftp_panel.editing_path_sub.clear();
        self.sftp_panel.jobs.clear();
        self.sftp_panel.open_workspace = None;
        self.sftp_panel.poll_gen = self.sftp_panel.poll_gen.wrapping_add(1);
        cx.notify();
    }

    fn sftp_route(&self) -> SftpRoute {
        SftpRoute {
            pane_id: self.sftp_panel.open_pane_id.unwrap_or_default(),
            workspace: self.sftp_panel.open_workspace.clone(),
        }
    }

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
        self.sftp_navigate_login_dir(pane_id, cx);
    }

    fn sftp_navigate_login_dir(&mut self, pane_id: u64, cx: &mut Context<Self>) {
        self.sftp_panel.loading = true;
        let route = self.sftp_route();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { route.op(SftpOp::Realpath { path: ".".into() }) })
                .await;
            let _ = this.update(cx, |this, cx| {
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

    pub(crate) fn sftp_navigate(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(pane_id) = self.sftp_panel.open_pane_id else {
            return;
        };
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
                if this.sftp_panel.open_pane_id != Some(pane_id)
                    || this.sftp_panel.nav_gen != generation
                {
                    return;
                }
                this.sftp_panel.loading = false;
                match result {
                    Ok(mut entries) => {
                        entries.sort_by(|a, b| a.name.cmp(&b.name));
                        this.sftp_panel.cwds.insert(pane_id, path.clone());
                        this.sftp_panel.cwd = path;
                        this.sftp_panel.entries = entries;
                        this.sftp_panel.error = None;
                        this.sftp_panel.editing_path = None;
                        this.sftp_panel.editing_path_sub.clear();
                    }
                    Err(e) => {
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
        self.sftp_navigate(value, cx);
    }

    pub(crate) fn sftp_cancel_edit_path(&mut self, cx: &mut Context<Self>) {
        self.sftp_panel.editing_path = None;
        self.sftp_panel.editing_path_sub.clear();
        cx.notify();
    }

    pub(crate) fn sftp_clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sftp_panel
            .filter_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    pub(crate) fn sftp_enter_dir(&mut self, entry: SftpEntry, cx: &mut Context<Self>) {
        if is_dir_like(&entry) {
            let target = remote_join(&self.sftp_panel.cwd, &entry.name);
            self.sftp_navigate(target, cx);
        }
    }

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
        if !safe_local_name(&entry.name) {
            self.sftp_panel.error = Some(t_fmt(
                L10nKey::SftpErrorUnsafeRemoteName,
                &[("name", &format!("{:?}", entry.name))],
            ));
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
        let op = if matches!(entry.kind, SftpEntryKind::Dir) {
            SftpOp::RemoveDir { path }
        } else {
            SftpOp::RemoveFile { path }
        };
        self.sftp_run_op(pane_id, op, cx);
    }

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

    pub(crate) fn sftp_begin_new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::ui::i18n::t(crate::ui::i18n::L10nKey::NewFolderName))
        });
        self.sftp_panel.editing = Some(SftpEdit::NewFolder(input));
        cx.notify();
    }

    pub(crate) fn sftp_begin_new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::ui::i18n::t(crate::ui::i18n::L10nKey::NewFileName))
        });
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
                        self.sftp_panel.error =
                            Some(t(L10nKey::SftpErrorInvalidOctalMode).to_string());
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
        self.sftp_refresh(cx);
    }

    pub(crate) fn sftp_cancel_job(&mut self, job_id: u64, cx: &mut Context<Self>) {
        self.sftp_panel.jobs = RemoteTerminal::sftp_transfer_cancel(job_id);
        cx.notify();
    }

    pub(crate) fn sftp_toggle_history(&mut self, cx: &mut Context<Self>) {
        self.sftp_panel.show_history = !self.sftp_panel.show_history;
        cx.notify();
    }

    pub(crate) fn sftp_toggle_tray(&mut self, cx: &mut Context<Self>) {
        self.sftp_panel.tray_expanded = !self.sftp_panel.tray_expanded;
        cx.notify();
    }

    pub(crate) fn sftp_dismiss_tray(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<u64> = self.sftp_panel.jobs.iter().map(|j| j.job_id).collect();
        self.sftp_panel.dismissed_jobs.extend(ids);
        self.sftp_panel.show_history = false;
        cx.notify();
    }

    pub(crate) fn sftp_reveal_download(&self, local: String, cx: &mut Context<Self>) {
        cx.reveal_path(Path::new(&local));
    }

    fn sftp_poll_jobs(&mut self, cx: &mut Context<Self>) {
        if self.sftp_panel.open_pane_id.is_some() {
            self.sftp_panel.jobs = self.sftp_route().transfer_list();
            cx.notify();
        }
    }

    fn sftp_start_polling(&mut self, cx: &mut Context<Self>) {
        self.sftp_panel.poll_gen = self.sftp_panel.poll_gen.wrapping_add(1);
        let generation = self.sftp_panel.poll_gen;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let pane = this
                    .update(cx, |this, cx| {
                        if this.sftp_panel.poll_gen != generation {
                            return None;
                        }
                        if !this.right_panel_open(cx) {
                            this.sftp_close_browser(cx);
                            return None;
                        }
                        this.sftp_panel
                            .open_pane_id
                            .is_some()
                            .then(|| this.sftp_route())
                    })
                    .ok()
                    .flatten();
                let Some(route) = pane else { break };
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

    pub(crate) fn render_panel_sftp(
        &mut self,
        host: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let controls = self.sftp_controls(cx);
        let title = self.panel_title(
            t(L10nKey::SftpPanelTitleFiles),
            Some(host),
            Some(controls),
            window,
            cx,
        );
        let breadcrumb = self.render_sftp_breadcrumb(cx);
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
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                this.sftp_upload_paths(paths.paths().to_vec(), cx);
            }))
            .into_any_element()
    }

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
                div().occlude().child(
                    tile(
                        Button::new("panel-sftp-refresh")
                            .icon(Icon::empty().path("icons/refresh.svg").size(px(13.))),
                        false,
                        cx,
                    )
                    .tooltip(t(L10nKey::SftpTooltipRefresh))
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
                    .tooltip(t(L10nKey::SftpTooltipMore))
                    .dropdown_menu_with_anchor(gpui::Anchor::TopRight, {
                        let app = cx.entity().downgrade();
                        move |menu, _window, _cx| {
                            let mut menu = menu.min_w(px(190.));
                            for (label, action) in [
                                (t(L10nKey::SftpMenuNewFolder), SftpMenuAction::NewFolder),
                                (t(L10nKey::SftpMenuNewFile), SftpMenuAction::NewFile),
                                (t(L10nKey::SftpMenuUpload), SftpMenuAction::Upload),
                                (
                                    t(L10nKey::SftpMenuGotoShellCwd),
                                    SftpMenuAction::GotoShellCwd,
                                ),
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
                                    t(L10nKey::SftpMenuHideTransferHistory)
                                } else {
                                    t(L10nKey::SftpMenuTransferHistory)
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

    fn render_sftp_breadcrumb(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        if let Some(input) = &self.sftp_panel.editing_path {
            return h_flex()
                .id("sftp-path-edit")
                .px(px(CONTENT_INSET))
                .pb(px(2.))
                .child(Input::new(input).xsmall())
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _window, cx| {
                    if ev.keystroke.key == "escape" {
                        this.sftp_cancel_edit_path(cx);
                    }
                }));
        }

        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
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
        row.child(div().flex_1().min_w(px(20.)).h(px(16.)))
    }

    fn render_sftp_edit_form(&self, cx: &mut Context<Self>) -> Option<Div> {
        let secondary = cx.theme().secondary;
        let border = cx.theme().border;
        let foreground = cx.theme().foreground;
        let (title, input): (String, _) = match self.sftp_panel.editing.as_ref()? {
            SftpEdit::NewFolder(input) => (t(L10nKey::SftpEditNewFolder).to_string(), input),
            SftpEdit::NewFile(input) => (t(L10nKey::SftpEditNewFile).to_string(), input),
            SftpEdit::Rename { input, .. } => (t(L10nKey::SftpEditRename).to_string(), input),
            SftpEdit::Chmod {
                readable, input, ..
            } => (
                t_fmt(L10nKey::SftpEditPermissions, &[("mode", readable)]),
                input,
            ),
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
                                .label(t(L10nKey::Cancel))
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(|this, _, _w, cx| this.sftp_cancel_edit(cx))),
                        )
                        .child(
                            Button::new("sftp-edit-ok")
                                .label(t(L10nKey::Ok))
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

        let show_go_up = self.sftp_panel.cwd != "/" && filter.trim().is_empty();

        if entries.is_empty() && !show_go_up {
            let text: gpui::SharedString = if self.sftp_panel.loading {
                t(L10nKey::SftpLoading).into()
            } else {
                t(L10nKey::SftpEmptyDirectory).into()
            };
            return container.child(note(text, muted));
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

    fn render_sftp_go_up_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let foreground = cx.theme().foreground;
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

    fn render_sftp_row(&self, entry: &SftpEntry, cx: &mut Context<Self>) -> AnyElement {
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
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
            .child(div().flex_none().text_xs().text_color(muted).child(size))
            .context_menu(move |menu, _window, cx| {
                let danger = cx.theme().danger;
                Self::sftp_row_context_menu(menu, &menu_entry, dir_like, is_symlink, danger, &app)
            })
            .into_any_element()
    }

    fn sftp_row_context_menu(
        menu: gpui_component::menu::PopupMenu,
        entry: &SftpEntry,
        dir_like: bool,
        is_symlink: bool,
        danger: gpui::Hsla,
        app: &gpui::WeakEntity<Self>,
    ) -> gpui_component::menu::PopupMenu {
        let mut menu = menu.min_w(px(180.));

        let primary_label = if dir_like {
            t(L10nKey::SftpContextOpen)
        } else {
            t(L10nKey::Download)
        };
        menu = menu.item(PopupMenuItem::new(primary_label).on_click({
            let app = app.clone();
            let entry = entry.clone();
            move |_, _window, cx| {
                let entry = entry.clone();
                let _ = app.update(cx, |this, cx| this.sftp_open_entry(entry, cx));
            }
        }));

        if is_symlink {
            menu = menu.item(
                PopupMenuItem::new(t(L10nKey::SftpContextFollowSymlink)).on_click({
                    let app = app.clone();
                    let entry = entry.clone();
                    move |_, _window, cx| {
                        let entry = entry.clone();
                        let _ = app.update(cx, |this, cx| this.sftp_follow_symlink(entry, cx));
                    }
                }),
            );
        }

        menu = menu
            .item(PopupMenuItem::new(t(L10nKey::SftpContextRename)).on_click({
                let app = app.clone();
                let name = entry.name.clone();
                move |_, window, cx| {
                    let name = name.clone();
                    let _ = app.update(cx, |this, cx| this.sftp_begin_rename(name, window, cx));
                }
            }))
            .item(PopupMenuItem::new(t(L10nKey::SftpContextChmod)).on_click({
                let app = app.clone();
                let entry = entry.clone();
                move |_, window, cx| {
                    let entry = entry.clone();
                    let _ = app.update(cx, |this, cx| this.sftp_begin_chmod(entry, window, cx));
                }
            }))
            .separator();

        menu.item(
            PopupMenuItem::element(move |_window, _cx| {
                div().text_color(danger).child(t(L10nKey::Delete))
            })
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

    pub(crate) fn sftp_transfers_footer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.sftp_panel.open_pane_id?;
        let history = self.sftp_panel.show_history;
        let jobs: Vec<&SftpJobProgress> = self
            .sftp_panel
            .jobs
            .iter()
            .filter(|j| history || !self.sftp_panel.dismissed_jobs.contains(&j.job_id))
            .collect();
        if jobs.is_empty() && !history {
            return None;
        }

        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        let accent = cx.theme().accent;
        let border = cx.theme().border;
        let hover = gpui::rgb(cx.global::<crate::ui::presets::Surfaces>().sidebar.hover);
        let expanded = self.sftp_panel.tray_expanded || history;

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
            t_fmt(
                L10nKey::SftpTransferSummaryRunning,
                &[
                    ("count", &running.to_string()),
                    ("pct", &format!("{pct:.0}")),
                ],
            )
        } else if failed > 0 {
            t_fmt(
                L10nKey::SftpTransferSummaryFailed,
                &[("count", &failed.to_string())],
            )
        } else {
            t(L10nKey::SftpTransferSummaryIdle).to_string()
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
                        .tooltip(t(L10nKey::Dismiss))
                        .on_click(cx.listener(|this, _, _w, cx| this.sftp_dismiss_tray(cx))),
                    ),
            );

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
                        .child(t(L10nKey::SftpNoTransfers)),
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
                .max_h(px(200.))
                .overflow_y_scroll()
                .child(inner)
        });

        Some(
            // No fill: the tray is a child of the right panel, which already
            // paints `workspace_surface_color`. Painting it again stacked a
            // second src-over pass of the same translucent surface and left a
            // visibly darker band with a hard seam under a backdrop material.
            v_flex()
                .flex_none()
                .border_t_1()
                .border_color(border)
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
            SftpJobState::Running => t_fmt(
                L10nKey::SftpTransferProgress,
                &[
                    ("done", &human_size(job.bytes_done)),
                    ("total", &human_size(job.bytes_total)),
                    ("pct", &format!("{pct:.0}")),
                ],
            ),
            SftpJobState::Done => t(L10nKey::SftpTransferDone).to_string(),
            SftpJobState::Cancelled => t(L10nKey::SftpTransferCancelled).to_string(),
            SftpJobState::Error => job
                .error
                .clone()
                .unwrap_or_else(|| t(L10nKey::SftpTransferError).to_string()),
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
                                .tooltip(crate::ui::right_panel::reveal_label())
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

    fn panel(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> (bool, RightPanelTab) {
        vcx.update(|_, cx| {
            let app = app.read(cx);
            (app.right_panel_visible, app.right_panel_tab)
        })
    }

    #[gpui::test]
    fn toggle_sftp_opens_files_then_closes_the_panel(cx: &mut TestAppContext) {
        let (app, mut vcx) = harness(cx);

        app.update_in(&mut vcx, |app, window, cx| {
            app.right_panel_visible = false;
            app.toggle_sftp(window, cx);
        });
        assert_eq!(panel(&app, &mut vcx), (true, RightPanelTab::Files));

        app.update_in(&mut vcx, |app, window, cx| app.toggle_sftp(window, cx));
        assert!(!panel(&app, &mut vcx).0, "second press should close");

        app.update_in(&mut vcx, |app, window, cx| app.toggle_sftp(window, cx));
        assert_eq!(panel(&app, &mut vcx), (true, RightPanelTab::Files));
    }

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

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
use gpui_component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, InteractiveElementExt as _, Selectable as _, Sizable as _,
    h_flex, v_flex,
};

use crate::daemon::protocol::{
    RemoteContext, RemoteKind, SftpEntry, SftpEntryKind, SftpJobProgress, SftpJobState, SftpOp,
    SftpOpResult, SftpTransferKind, SftpTransferSpec,
};
use crate::daemon::ssh::sftp::{remote_basename, remote_join, remote_parent, safe_local_name};
use crate::terminal::RemoteTerminal;
use crate::ui::app::Tty7App;

/// The panel docks along the bottom of the terminal body (tabby-style) and
/// takes this fraction of its height, leaving the shell visible above.
const SFTP_PANEL_HEIGHT_FRAC: f32 = 0.7;

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
        input: gpui::Entity<InputState>,
    },
}

/// State for the SFTP side panel. One panel at a time, bound to a pane id.
pub(crate) struct SftpPanelState {
    pub(crate) open_pane_id: Option<u64>,
    /// The remote directory currently listed (absolute POSIX path).
    pub(crate) cwd: String,
    pub(crate) entries: Vec<SftpEntry>,
    pub(crate) filter_input: gpui::Entity<InputState>,
    /// Last listing error, shown in place of the list.
    pub(crate) error: Option<String>,
    /// Latest transfer-job snapshots for the tray.
    pub(crate) jobs: Vec<SftpJobProgress>,
    /// Job ids the user dismissed from the tray; filtered out until a fresh
    /// transfer (a new id) reopens it. Cleared when the panel closes/reopens.
    dismissed_jobs: HashSet<u64>,
    /// When set, the transfers tray is pinned open and shows the full history
    /// (every job, including dismissed ones), toggled by the header button.
    show_history: bool,
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
            cwd: "/".to_string(),
            entries: Vec::new(),
            filter_input,
            error: None,
            jobs: Vec::new(),
            dismissed_jobs: HashSet::new(),
            show_history: false,
            loading: false,
            nav_gen: 0,
            editing: None,
            editing_path: None,
            editing_path_sub: Vec::new(),
            poll_gen: 0,
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
    /// Toggle the SFTP panel for the focused SSH pane. Every native-SSH pane has a
    /// russh connection to browse over; a pane with no native connection (e.g. a
    /// foreground `ssh` typed into a local shell) has nothing to list, so the
    /// toggle simply doesn't open. A non-SSH focused pane closes any open panel.
    pub(crate) fn toggle_sftp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pane_id, remote)) = self.active_ssh_pane(window, cx) else {
            self.close_sftp_panel(cx);
            return;
        };
        if self.sftp_panel.open_pane_id == Some(pane_id) {
            self.close_sftp_panel(cx);
            return;
        }
        if remote.kind == RemoteKind::NativeSsh {
            self.sftp_open_at(pane_id, window, cx);
        } else {
            // No native connection to browse (a manually-typed foreground ssh).
            self.close_sftp_panel(cx);
        }
    }

    pub(crate) fn close_sftp_panel(&mut self, cx: &mut Context<Self>) {
        self.sftp_panel.open_pane_id = None;
        self.sftp_panel.editing = None;
        self.sftp_panel.editing_path = None;
        self.sftp_panel.editing_path_sub.clear();
        self.sftp_panel.dismissed_jobs.clear();
        self.sftp_panel.show_history = false;
        // Invalidate the poll loop.
        self.sftp_panel.poll_gen = self.sftp_panel.poll_gen.wrapping_add(1);
        cx.notify();
    }

    fn sftp_open_at(&mut self, pane_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.sftp_panel.open_pane_id = Some(pane_id);
        self.sftp_panel.editing = None;
        self.sftp_panel.editing_path = None;
        self.sftp_panel.editing_path_sub.clear();
        self.sftp_panel.dismissed_jobs.clear();
        self.sftp_panel.show_history = false;
        // Start at the shell's OSC-7 cwd when known, else the filesystem root.
        let start = self
            .pane_shell_cwd(pane_id, window, cx)
            .unwrap_or_else(|| "/".to_string());
        self.sftp_navigate(start, cx);
        self.sftp_poll_jobs(cx);
        self.sftp_start_polling(cx);
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
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { RemoteTerminal::sftp_list(pane_id, &list_path) })
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
        match RemoteTerminal::sftp_transfer_start(spec) {
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
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    RemoteTerminal::sftp_op(pane_id, SftpOp::Readlink { path })
                })
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
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { RemoteTerminal::sftp_op(pane_id, op) })
                .await;
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
        let path = remote_join(&self.sftp_panel.cwd, &entry.name);
        let input = cx.new(|cx| InputState::new(window, cx).default_value(octal));
        self.sftp_panel.editing = Some(SftpEdit::Chmod { path, input });
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
            Some(SftpEdit::Chmod { path, input }) => {
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
            if let Err(e) = RemoteTerminal::sftp_transfer_start(spec) {
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
        if let Some(pane_id) = self.sftp_panel.open_pane_id {
            self.sftp_panel.jobs = RemoteTerminal::sftp_transfer_list(pane_id);
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
                    .update(cx, |this, _| {
                        if this.sftp_panel.poll_gen != generation {
                            None
                        } else {
                            this.sftp_panel.open_pane_id
                        }
                    })
                    .ok()
                    .flatten();
                let Some(pane_id) = pane else { break };
                // Poll off the main thread so the blocking control round-trip
                // doesn't jank the UI.
                let jobs = cx
                    .background_spawn(async move { RemoteTerminal::sftp_transfer_list(pane_id) })
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

    /// The bottom-docked SFTP panel (tabby-style), mounted over the lower part of
    /// the terminal body when open for `pane_id`. Returns `None` when not open for
    /// this pane.
    pub(crate) fn render_sftp_overlay(
        &self,
        pane_id: u64,
        remote: &RemoteContext,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.sftp_panel.open_pane_id != Some(pane_id) {
            return None;
        }
        // Only native-SSH panes open the browser (see `toggle_sftp`); nothing else
        // reaches here with the panel open.
        if remote.kind != RemoteKind::NativeSsh {
            return None;
        }
        let popover = cx.theme().popover;
        let border = cx.theme().border;

        let panel = v_flex()
            .id("sftp-panel")
            .absolute()
            .left_0()
            .right_0()
            .bottom_0()
            .h(gpui::relative(SFTP_PANEL_HEIGHT_FRAC))
            .bg(popover)
            .border_t_1()
            .border_color(border)
            .shadow_lg()
            // The panel sits over the terminal body (a sibling), which has its own
            // right-click menu. Occlude so clicks — especially the row right-click —
            // don't also fall through and pop the terminal's context menu.
            .occlude()
            .child(self.render_sftp_header(pane_id, cx))
            .when_some(self.render_sftp_edit_form(cx), |this, form| {
                this.child(form)
            })
            .child(self.render_sftp_list(cx))
            .when_some(self.render_sftp_tray(cx), |this, tray| this.child(tray))
            // FR-T5: a Finder drop uploads onto the current directory.
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                this.sftp_upload_paths(paths.paths().to_vec(), cx);
            }));

        Some(panel.into_any_element())
    }

    /// The panel header: a single row with the breadcrumb path (left, growing)
    /// and a light, borderless action cluster (right), over an always-visible
    /// search box. Kept deliberately compact — the breadcrumb root reads `SFTP`,
    /// so there's no separate redundant title.
    fn render_sftp_header(&self, pane_id: u64, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;

        // Ghost icon buttons (with tooltips) read as a light toolbar rather than a
        // row of heavy labelled pills.
        let actions = h_flex()
            .flex_none()
            .items_center()
            .gap_0p5()
            .child(
                Button::new("sftp-refresh")
                    .icon(IconName::LoaderCircle)
                    .ghost()
                    .small()
                    .tooltip("Refresh")
                    .on_click(cx.listener(|this, _, _w, cx| this.sftp_refresh(cx))),
            )
            .child(
                Button::new("sftp-newfolder")
                    .icon(IconName::FolderClosed)
                    .ghost()
                    .small()
                    .tooltip("New folder")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.sftp_begin_new_folder(window, cx)),
                    ),
            )
            .child(
                Button::new("sftp-newfile")
                    .icon(IconName::File)
                    .ghost()
                    .small()
                    .tooltip("New file")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.sftp_begin_new_file(window, cx)),
                    ),
            )
            .child(
                Button::new("sftp-upload")
                    .icon(IconName::ArrowUp)
                    .ghost()
                    .small()
                    .tooltip("Upload")
                    .on_click(cx.listener(|this, _, _w, cx| this.sftp_pick_upload(cx))),
            )
            .child(
                Button::new("sftp-history")
                    .icon(IconName::Inbox)
                    .ghost()
                    .small()
                    .selected(self.sftp_panel.show_history)
                    .tooltip("Transfers")
                    .on_click(cx.listener(|this, _, _w, cx| this.sftp_toggle_history(cx))),
            )
            .child(
                Button::new(("sftp-close", pane_id))
                    .icon(IconName::Close)
                    .ghost()
                    .small()
                    .tooltip("Close")
                    .on_click(cx.listener(|this, _, _w, cx| this.close_sftp_panel(cx))),
            );

        let top = h_flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_1p5()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.render_sftp_breadcrumb(cx)),
            )
            .child(actions);

        // Always-visible search box with a leading magnifier; Esc clears it.
        let search = h_flex()
            .id("sftp-search")
            .px_3()
            .pb_2()
            .child(
                Input::new(&self.sftp_panel.filter_input)
                    .small()
                    .cleanable(true)
                    .prefix(Icon::new(IconName::Search).small().text_color(muted)),
            )
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                if ev.keystroke.key == "escape" {
                    this.sftp_clear_filter(window, cx);
                }
            }));

        v_flex()
            .border_b_1()
            .border_color(border)
            .child(top)
            .child(search)
    }

    /// The path bar. Normally a clickable breadcrumb (root shown as `SFTP`, like
    /// tabby); double-clicking anywhere on it switches to a text input so you can
    /// type a destination directly. Enter navigates, Esc/blur returns to the
    /// breadcrumb.
    fn render_sftp_breadcrumb(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        if let Some(input) = &self.sftp_panel.editing_path {
            return h_flex()
                .id("sftp-path-edit")
                .px_2()
                .py_1()
                .child(Input::new(input).small())
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
            .px_2()
            .py_1()
            .on_double_click(
                cx.listener(|this, _, window, cx| this.sftp_begin_edit_path(window, cx)),
            );
        // Root: labelled "SFTP", navigates to "/". The current (last) segment reads
        // in full ink; ancestors are muted but still clearly legible (the theme
        // `accent` was near-invisible here).
        let segments = breadcrumb_segments(&self.sftp_panel.cwd);
        let last = segments.len().saturating_sub(1);
        for (i, (label, path)) in segments.into_iter().enumerate() {
            if i > 0 {
                row = row.child(div().text_xs().text_color(muted).child("›"));
            }
            let is_current = i == last;
            let label = if i == 0 { "SFTP".to_string() } else { label };
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
        let (title, input) = match self.sftp_panel.editing.as_ref()? {
            SftpEdit::NewFolder(input) => ("New folder", input),
            SftpEdit::NewFile(input) => ("New file", input),
            SftpEdit::Rename { input, .. } => ("Rename", input),
            SftpEdit::Chmod { input, .. } => ("Permissions (octal)", input),
        };
        Some(
            v_flex()
                .gap_2()
                .m_2()
                .p_2()
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
                .child(Input::new(input).small())
                .child(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new("sftp-edit-cancel")
                                .label("Cancel")
                                .small()
                                .on_click(cx.listener(|this, _, _w, cx| this.sftp_cancel_edit(cx))),
                        )
                        .child(
                            Button::new("sftp-edit-ok")
                                .label("OK")
                                .small()
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
            .px_1();

        if let Some(err) = &self.sftp_panel.error {
            return container.child(div().p_3().text_xs().text_color(danger).child(err.clone()));
        }

        let filter = self.sftp_panel.filter_input.read(cx).value().to_string();
        let entries = sorted_filtered_entries(&self.sftp_panel.entries, &filter);

        // A `..` parent row leads the list when not at the root and not
        // actively filtering — the file-manager convention for going up.
        let show_go_up = self.sftp_panel.cwd != "/" && filter.trim().is_empty();

        if entries.is_empty() && !show_go_up {
            // Distinguish "still loading" from a genuinely empty directory so a
            // slow listing doesn't read as empty.
            let note = if self.sftp_panel.loading {
                "Loading…"
            } else {
                "Empty directory."
            };
            return container.child(div().p_3().text_xs().text_color(muted).child(note));
        }

        let mut list = v_flex().gap_0p5().py_1();
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
        let list_hover = cx.theme().list_hover;
        h_flex()
            .id("sftp-go-up")
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(list_hover))
            .child(Icon::new(IconName::Folder).small().text_color(foreground))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(foreground)
                    .child(".."),
            )
            .on_click(cx.listener(|this, _, _w, cx| this.sftp_up(cx)))
            .into_any_element()
    }

    /// One entry row: icon + name (+ a `→` marker for symlinks) + a muted
    /// size/mode column. Per-row actions (open/download, follow, rename, chmod,
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
            .gap_2()
            .px_3()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(list_hover))
            // Double-click enters a directory; files never download from a click
            // (only from the right-click menu).
            .on_double_click(
                cx.listener(move |this, _, _w, cx| this.sftp_enter_dir(open_entry.clone(), cx)),
            )
            .child(
                Icon::new(icon)
                    .small()
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
            // Right-hand metadata: size then mode, each in its own fixed,
            // right-aligned column so they line up down the list.
            .child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap_5()
                    .child(
                        h_flex()
                            .w(px(56.))
                            .justify_end()
                            .child(div().text_xs().text_color(muted).child(size)),
                    )
                    .when(entry.permissions != 0, |this| {
                        this.child(
                            h_flex().w(px(88.)).justify_end().child(
                                div()
                                    .text_xs()
                                    .font_family("monospace")
                                    .text_color(muted)
                                    .child(mode_string(entry.permissions)),
                            ),
                        )
                    }),
            )
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

    /// The bottom transfer tray. Shows in "auto" mode whenever there are
    /// non-dismissed jobs; the header Transfers button pins it open in history
    /// mode, where it lists every job (dismissed or not) and stays up even empty.
    fn render_sftp_tray(&self, cx: &mut Context<Self>) -> Option<Div> {
        let history = self.sftp_panel.show_history;
        let jobs: Vec<&SftpJobProgress> = self
            .sftp_panel
            .jobs
            .iter()
            .filter(|j| history || !self.sftp_panel.dismissed_jobs.contains(&j.job_id))
            .collect();
        // Auto mode with nothing to show → hide. History mode stays open (with an
        // empty-state note) so the button always reveals a panel.
        if jobs.is_empty() && !history {
            return None;
        }
        let border = cx.theme().border;
        let secondary = cx.theme().secondary;
        let muted = cx.theme().muted_foreground;

        let body = if jobs.is_empty() {
            v_flex().child(
                div()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .child("No transfers yet."),
            )
        } else {
            let mut list = v_flex().gap_1();
            for job in jobs {
                list = list.child(self.render_sftp_job(job, cx));
            }
            list
        };
        Some(
            v_flex()
                .gap_1()
                .p_2()
                .border_t_1()
                .border_color(border)
                .bg(secondary)
                .child(
                    h_flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(muted)
                                .child("Transfers"),
                        )
                        .child(
                            Button::new("sftp-tray-close")
                                .icon(IconName::Close)
                                .ghost()
                                .xsmall()
                                .tooltip("Close")
                                .on_click(
                                    cx.listener(|this, _, _w, cx| this.sftp_dismiss_tray(cx)),
                                ),
                        ),
                )
                .child(body),
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

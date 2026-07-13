//! Pane-contextual SFTP file panel (Workstream 5).
//!
//! Renders as a right-docked, slide-in panel over the terminal body area for the
//! focused **native-SSH** pane (a compat-`ssh` or PTY pane has no russh connection
//! and never shows it). Mirrors the `ui::forwards` pattern: a set of
//! `impl Tty7App` render helpers plus a [`SftpPanelState`] held on `Tty7App`, and
//! synchronous one-shot [`RemoteTerminal`] control calls to the daemon
//! (`sftp_list` / `sftp_op` / `sftp_transfer_*`).
//!
//! Layout: a breadcrumb path bar, a filter box, a toolbar (up / refresh / new
//! folder / upload / go-to-shell-cwd), a dir-first entry list with per-row actions
//! (download / rename / delete / chmod), an inline edit form, and a bottom
//! transfer tray that polls job progress while the panel is open.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Div, ExternalPaths, FontWeight, PathPromptOptions, SharedString,
    Stateful, Subscription, Window, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};

use crate::daemon::protocol::{
    RemoteContext, RemoteKind, SftpEntry, SftpEntryKind, SftpJobProgress, SftpJobState, SftpOp,
    SftpOpResult, SftpTransferKind, SftpTransferSpec,
};
use crate::daemon::ssh::sftp::{remote_basename, remote_join, remote_parent};
use crate::terminal::RemoteTerminal;
use crate::ui::app::Tty7App;

/// Default and clamp range for the panel's width (px).
const SFTP_PANEL_WIDTH: f32 = 380.0;

/// One in-progress inline edit form in the panel.
pub(crate) enum SftpEdit {
    NewFolder(gpui::Entity<InputState>),
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
    pub(crate) width: f32,
    pub(crate) editing: Option<SftpEdit>,
    /// Bumped on every (re)open so a stale poll loop exits.
    pub(crate) poll_gen: u64,
    _subs: Vec<Subscription>,
}

impl SftpPanelState {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Tty7App>) -> Self {
        let filter_input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter"));
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
            width: SFTP_PANEL_WIDTH,
            editing: None,
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
    /// Toggle the SFTP panel for the focused native-SSH pane. A no-op (with a
    /// gentle close) when the focused pane isn't native-SSH.
    pub(crate) fn toggle_sftp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let native = self
            .active_ssh_pane(window, cx)
            .filter(|(_, remote)| remote.kind == RemoteKind::NativeSsh);
        match native {
            Some((pane_id, _)) if self.sftp_panel.open_pane_id == Some(pane_id) => {
                self.close_sftp_panel(cx);
            }
            Some((pane_id, _)) => self.sftp_open_at(pane_id, window, cx),
            // Focused pane can't do SFTP: close any panel that was open.
            None => self.close_sftp_panel(cx),
        }
    }

    pub(crate) fn close_sftp_panel(&mut self, cx: &mut Context<Self>) {
        self.sftp_panel.open_pane_id = None;
        self.sftp_panel.editing = None;
        // Invalidate the poll loop.
        self.sftp_panel.poll_gen = self.sftp_panel.poll_gen.wrapping_add(1);
        cx.notify();
    }

    fn sftp_open_at(&mut self, pane_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.sftp_panel.open_pane_id = Some(pane_id);
        self.sftp_panel.editing = None;
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

    /// FR-T4: navigate to the shell's current cwd (button only enabled when known).
    pub(crate) fn sftp_go_shell_cwd(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane_id) = self.sftp_panel.open_pane_id
            && let Some(cwd) = self.pane_shell_cwd(pane_id, window, cx)
        {
            self.sftp_navigate(cwd, cx);
        }
    }

    /// List `path` on the pane's SFTP session and show it. Errors are surfaced in
    /// the panel body rather than thrown away.
    pub(crate) fn sftp_navigate(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(pane_id) = self.sftp_panel.open_pane_id else {
            return;
        };
        match RemoteTerminal::sftp_list(pane_id, &path) {
            Ok(mut entries) => {
                entries.sort_by(|a, b| a.name.cmp(&b.name));
                self.sftp_panel.cwd = path;
                self.sftp_panel.entries = entries;
                self.sftp_panel.error = None;
            }
            Err(e) => {
                // Keep the old listing; just report the failure.
                self.sftp_panel.error = Some(e);
            }
        }
        cx.notify();
    }

    pub(crate) fn sftp_refresh(&mut self, cx: &mut Context<Self>) {
        let cwd = self.sftp_panel.cwd.clone();
        self.sftp_navigate(cwd, cx);
    }

    pub(crate) fn sftp_up(&mut self, cx: &mut Context<Self>) {
        let parent = remote_parent(&self.sftp_panel.cwd);
        self.sftp_navigate(parent, cx);
    }

    /// Click on an entry: enter a directory (or symlink-to-directory), or download
    /// a file/other symlink.
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
    /// directory (or the target itself when it is a directory).
    pub(crate) fn sftp_follow_symlink(&mut self, entry: SftpEntry, cx: &mut Context<Self>) {
        let Some(pane_id) = self.sftp_panel.open_pane_id else {
            return;
        };
        let path = remote_join(&self.sftp_panel.cwd, &entry.name);
        match RemoteTerminal::sftp_op(pane_id, SftpOp::Readlink { path }) {
            SftpOpResult::Link(target) => {
                let resolved = if target.starts_with('/') {
                    target
                } else {
                    remote_join(&self.sftp_panel.cwd, &target)
                };
                // Navigate to the target if it's a directory, else its parent.
                let dest = if entry.target_is_dir {
                    resolved
                } else {
                    remote_parent(&resolved)
                };
                self.sftp_navigate(dest, cx);
            }
            SftpOpResult::Error(e) => {
                self.sftp_panel.error = Some(e);
                cx.notify();
            }
            _ => {}
        }
    }

    fn sftp_run_op(&mut self, pane_id: u64, op: SftpOp, cx: &mut Context<Self>) {
        match RemoteTerminal::sftp_op(pane_id, op) {
            SftpOpResult::Error(e) => {
                self.sftp_panel.error = Some(e);
                cx.notify();
            }
            _ => {
                self.sftp_panel.editing = None;
                self.sftp_refresh(cx);
            }
        }
    }

    // --- inline edit forms -------------------------------------------------

    pub(crate) fn sftp_begin_new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("New folder name"));
        self.sftp_panel.editing = Some(SftpEdit::NewFolder(input));
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

    /// The right-docked SFTP panel, mounted over the terminal body when open for
    /// `pane_id`. Returns `None` when not open for this pane.
    pub(crate) fn render_sftp_overlay(
        &self,
        pane_id: u64,
        _remote: &RemoteContext,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.sftp_panel.open_pane_id != Some(pane_id) {
            return None;
        }
        let popover = cx.theme().popover;
        let border = cx.theme().border;
        let shell_cwd = self.pane_shell_cwd(pane_id, window, cx);

        let panel = v_flex()
            .id("sftp-panel")
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .w(px(self.sftp_panel.width))
            .bg(popover)
            .border_l_1()
            .border_color(border)
            .shadow_lg()
            .child(self.render_sftp_header(pane_id, shell_cwd.is_some(), cx))
            .child(self.render_sftp_breadcrumb(cx))
            .child(self.render_sftp_filter())
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

    fn render_sftp_header(&self, pane_id: u64, has_shell_cwd: bool, cx: &mut Context<Self>) -> Div {
        let border = cx.theme().border;
        let foreground = cx.theme().foreground;
        let toolbar = h_flex()
            .gap_1()
            .child(
                Button::new("sftp-up")
                    .label("Up")
                    .small()
                    .on_click(cx.listener(|this, _, _w, cx| this.sftp_up(cx))),
            )
            .child(
                Button::new("sftp-refresh")
                    .label("Refresh")
                    .small()
                    .on_click(cx.listener(|this, _, _w, cx| this.sftp_refresh(cx))),
            )
            .child(
                Button::new("sftp-newfolder")
                    .label("New Folder")
                    .small()
                    .on_click(
                        cx.listener(|this, _, window, cx| this.sftp_begin_new_folder(window, cx)),
                    ),
            )
            .child(
                Button::new("sftp-upload")
                    .label("Upload")
                    .small()
                    .on_click(cx.listener(|this, _, _w, cx| this.sftp_pick_upload(cx))),
            )
            .child(
                Button::new("sftp-shell-cwd")
                    .label("Shell cwd")
                    .small()
                    .disabled(!has_shell_cwd)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.sftp_go_shell_cwd(window, cx)),
                    ),
            );

        v_flex()
            .gap_2()
            .p_2()
            .border_b_1()
            .border_color(border)
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(foreground)
                            .child("SFTP"),
                    )
                    .child(
                        Button::new(("sftp-close", pane_id))
                            .icon(IconName::Close)
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _w, cx| this.close_sftp_panel(cx))),
                    ),
            )
            .child(toolbar)
    }

    fn render_sftp_breadcrumb(&self, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let accent = cx.theme().accent;
        let mut row = h_flex().flex_wrap().items_center().gap_0p5().px_2().py_1();
        for (i, (label, path)) in breadcrumb_segments(&self.sftp_panel.cwd)
            .into_iter()
            .enumerate()
        {
            if i > 0 {
                row = row.child(div().text_xs().text_color(muted).child("›"));
            }
            let seg_id = SharedString::from(format!("sftp-crumb-{path}"));
            row = row.child(
                div()
                    .id(seg_id)
                    .text_xs()
                    .text_color(accent)
                    .cursor_pointer()
                    .hover(|s| s.underline())
                    .child(label)
                    .on_click(
                        cx.listener(move |this, _, _w, cx| this.sftp_navigate(path.clone(), cx)),
                    ),
            );
        }
        row
    }

    fn render_sftp_filter(&self) -> Div {
        h_flex()
            .px_2()
            .pb_1()
            .child(Input::new(&self.sftp_panel.filter_input).small())
    }

    /// The active inline edit form (new folder / rename / chmod), if any.
    fn render_sftp_edit_form(&self, cx: &mut Context<Self>) -> Option<Div> {
        let secondary = cx.theme().secondary;
        let border = cx.theme().border;
        let foreground = cx.theme().foreground;
        let (title, input) = match self.sftp_panel.editing.as_ref()? {
            SftpEdit::NewFolder(input) => ("New folder", input),
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
        if entries.is_empty() {
            return container.child(
                div()
                    .p_3()
                    .text_xs()
                    .text_color(muted)
                    .child("Empty directory."),
            );
        }

        let mut list = v_flex().gap_0p5().py_1();
        for entry in entries {
            list = list.child(self.render_sftp_row(entry, cx));
        }
        container.child(list)
    }

    fn render_sftp_row(&self, entry: &SftpEntry, cx: &mut Context<Self>) -> Stateful<Div> {
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let accent = cx.theme().accent;
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
        let del_entry = entry.clone();
        let rename_name = entry.name.clone();
        let chmod_entry = entry.clone();
        let follow_entry = entry.clone();

        let name_id = SharedString::from(format!("sftp-name-{}", entry.name));
        h_flex()
            .id(row_id)
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_md()
            .hover(|s| s.bg(list_hover))
            .child(
                Icon::new(icon)
                    .small()
                    .text_color(if dir_like { accent } else { muted }),
            )
            .child(
                div()
                    .id(name_id)
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(foreground)
                    .cursor_pointer()
                    .truncate()
                    .child(name_label)
                    .on_click(cx.listener(move |this, _, _w, cx| {
                        this.sftp_open_entry(open_entry.clone(), cx)
                    })),
            )
            .child(
                v_flex()
                    .items_end()
                    .child(div().text_xs().text_color(muted).child(size))
                    .when(entry.permissions != 0, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .font_family("monospace")
                                .text_color(muted)
                                .child(mode_string(entry.permissions)),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .when(is_symlink, |row| {
                        row.child(
                            Button::new(SharedString::from(format!("sftp-fl-{}", entry.name)))
                                .label("Follow")
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _w, cx| {
                                    this.sftp_follow_symlink(follow_entry.clone(), cx)
                                })),
                        )
                    })
                    .child(
                        Button::new(SharedString::from(format!("sftp-dl-{}", entry.name)))
                            .label("↓")
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener({
                                let e = entry.clone();
                                move |this, _, _w, cx| this.sftp_download_entry(e.clone(), cx)
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("sftp-rn-{}", entry.name)))
                            .label("Rename")
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.sftp_begin_rename(rename_name.clone(), window, cx)
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("sftp-cm-{}", entry.name)))
                            .label("chmod")
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.sftp_begin_chmod(chmod_entry.clone(), window, cx)
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("sftp-del-{}", entry.name)))
                            .label("Delete")
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(move |this, _, _w, cx| {
                                this.sftp_delete_entry(del_entry.clone(), cx)
                            })),
                    ),
            )
    }

    /// The bottom transfer tray, if there are any jobs to show.
    fn render_sftp_tray(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.sftp_panel.jobs.is_empty() {
            return None;
        }
        let border = cx.theme().border;
        let secondary = cx.theme().secondary;
        let muted = cx.theme().muted_foreground;
        let mut list = v_flex().gap_1();
        for job in &self.sftp_panel.jobs {
            list = list.child(self.render_sftp_job(job, cx));
        }
        Some(
            v_flex()
                .gap_1()
                .p_2()
                .border_t_1()
                .border_color(border)
                .bg(secondary)
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(muted)
                        .child("Transfers"),
                )
                .child(list),
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

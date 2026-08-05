use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Entity, Focusable as _, PromptLevel, SharedString, Subscription, Window,
    div, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState, TabSize};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};

use crate::ui::app::Tty7App;
use crate::ui::host_ops::{HostOps, MTime, SharedHost, WatchSub};
use crate::ui::i18n::{L10nKey, t, t_fmt};

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

const RELOAD_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

pub(crate) struct OpenFile {
    pub(crate) path: PathBuf,
    pub(crate) input: Entity<InputState>,
    pub(crate) dirty: bool,
    disk_mtime: Option<MTime>,
    edit_seq: u64,
    saving: Option<u64>,
    save_pending: bool,
    save_then_close: bool,
    reload_seq: u64,
    pub(crate) conflict: bool,
    pub(crate) preview: bool,
    pub(crate) wrap: bool,
    _sub: Subscription,
    _observe: Subscription,
}

impl OpenFile {
    fn label(&self) -> SharedString {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.display().to_string())
            .into()
    }
}

pub(crate) struct TabCode {
    pub(crate) visible: bool,
    pub(crate) files: Vec<OpenFile>,
    pub(crate) active: usize,
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) expanded: std::collections::HashSet<PathBuf>,
    pub(crate) selected: Option<PathBuf>,
}

impl TabCode {
    pub(crate) fn new() -> Self {
        Self {
            visible: false,
            files: Vec::new(),
            active: 0,
            roots: Vec::new(),
            expanded: std::collections::HashSet::new(),
            selected: None,
        }
    }

    pub(crate) fn active_file(&self) -> Option<&OpenFile> {
        self.files.get(self.active)
    }
}

pub(crate) struct EditorPanelState {
    watch: Option<Arc<WatchSub>>,
    watch_host: Option<SharedHost>,
    watch_opening: bool,
    watch_busy: bool,
    watch_dirty: bool,
    watched_dirs: HashSet<PathBuf>,
    watched_files: HashSet<PathBuf>,
    events_tx: smol::channel::Sender<Vec<PathBuf>>,
}

impl EditorPanelState {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Tty7App>) -> Self {
        let (tx, rx) = smol::channel::unbounded::<Vec<PathBuf>>();
        cx.spawn_in(window, async move |app, cx| {
            while let Ok(first) = rx.recv().await {
                cx.background_executor().timer(RELOAD_DEBOUNCE).await;
                let mut changed: HashSet<PathBuf> = first.into_iter().collect();
                while let Ok(more) = rx.try_recv() {
                    changed.extend(more);
                }
                let ok = app.update_in(cx, |app, window, cx| {
                    for path in changed {
                        if app.editor.watched_files.contains(&path) {
                            app.editor_handle_external_change(&path, window, cx);
                        }
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            watch: None,
            watch_host: None,
            watch_opening: false,
            watch_busy: false,
            watch_dirty: false,
            watched_dirs: HashSet::new(),
            watched_files: HashSet::new(),
            events_tx: tx,
        }
    }
}

pub(crate) fn language_for_path(path: &Path) -> &'static str {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let lowered = name.to_ascii_lowercase();
        match lowered.as_str() {
            "makefile" | "gnumakefile" => return "make",
            "cmakelists.txt" => return "cmake",
            _ => {}
        }
        if lowered.starts_with('.') && (lowered.contains("shrc") || lowered.ends_with("profile")) {
            return "bash";
        }
    }
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return "text";
    };
    match ext.to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "go" => "go",
        "py" | "pyi" => "python",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "json" | "jsonc" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "html" | "htm" => "html",
        "css" => "css",
        "md" | "markdown" => "markdown",
        "sh" | "bash" | "zsh" => "bash",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "lua" => "lua",
        "rb" => "ruby",
        "php" => "php",
        "sql" => "sql",
        "swift" => "swift",
        "scala" => "scala",
        "zig" => "zig",
        "proto" => "proto",
        "diff" | "patch" => "diff",
        "ex" | "exs" => "elixir",
        "erb" => "erb",
        "ejs" => "ejs",
        "svelte" => "svelte",
        "astro" => "astro",
        "graphql" | "gql" => "graphql",
        "cs" => "csharp",
        "cmake" => "cmake",
        _ => "text",
    }
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

#[derive(Debug, PartialEq, Eq)]
enum ExternalChange {
    Ignore,
    Conflict,
    Reload,
}

fn classify_external_change(
    saving: bool,
    dirty: bool,
    disk_mtime: Option<MTime>,
    observed: Option<MTime>,
) -> ExternalChange {
    if saving {
        return ExternalChange::Ignore;
    }
    if observed.is_some() && observed == disk_mtime {
        return ExternalChange::Ignore;
    }
    if dirty {
        ExternalChange::Conflict
    } else {
        ExternalChange::Reload
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SaveLanding {
    clean: bool,
    requeue: bool,
}

fn settle_save(ok: bool, wrote_seq: u64, current_seq: u64, pending: bool) -> SaveLanding {
    SaveLanding {
        clean: ok && wrote_seq == current_seq,
        requeue: ok && pending,
    }
}

impl Tty7App {
    pub(crate) fn tab_code(&self) -> Option<&TabCode> {
        self.tabs.get(self.active)?.code.as_deref()
    }

    pub(crate) fn tab_code_mut(&mut self) -> Option<&mut TabCode> {
        self.tabs.get_mut(self.active)?.code.as_deref_mut()
    }

    pub(crate) fn tab_code_mut_or_init(&mut self) -> Option<&mut TabCode> {
        let tab = self.tabs.get_mut(self.active)?;
        Some(tab.code.get_or_insert_with(|| Box::new(TabCode::new())))
    }

    pub(crate) fn code_panel_visible(&self) -> bool {
        self.tab_code().is_some_and(|c| c.visible)
    }

    fn editor_rebuild_watcher(&mut self, cx: &mut Context<Self>) {
        let files: HashSet<PathBuf> = self
            .tabs
            .iter()
            .filter_map(|t| t.code.as_deref())
            .flat_map(|c| c.files.iter().map(|f| f.path.clone()))
            .collect();
        let dirs: HashSet<PathBuf> = files
            .iter()
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .collect();
        self.editor.watched_files = files;
        if dirs == self.editor.watched_dirs {
            return;
        }
        self.editor.watched_dirs = dirs;
        self.editor_watch_apply(cx);
    }

    fn editor_watch_apply(&mut self, cx: &mut Context<Self>) {
        let want: Vec<PathBuf> = self.editor.watched_dirs.iter().cloned().collect();
        let Some(host) = self.active_host(cx) else {
            return;
        };

        if !self
            .editor
            .watch_host
            .as_ref()
            .is_some_and(|opened_with| Arc::ptr_eq(opened_with, &host))
        {
            self.editor.watch = None;
            self.editor.watch_host = None;
            self.editor.watch_busy = false;
            self.editor.watch_dirty = false;
        }

        if let Some(sub) = self.editor.watch.clone() {
            if self.editor.watch_busy {
                self.editor.watch_dirty = true;
                return;
            }
            self.editor.watch_busy = true;
            HostOps::run(
                host,
                cx,
                move |_| sub.set_dirs(&want),
                |app: &mut Self, result: std::io::Result<()>, cx| {
                    app.editor.watch_busy = false;
                    if let Err(e) = result {
                        log::warn!("editor: could not update the watched set: {e}");
                    }
                    if std::mem::take(&mut app.editor.watch_dirty) {
                        app.editor_watch_apply(cx);
                    }
                },
            );
            return;
        }
        if self.editor.watch_opening {
            return;
        }
        self.editor.watch_opening = true;
        let opened_host = Arc::clone(&host);
        let opened_with = self.editor.watched_dirs.clone();
        HostOps::run(
            host,
            cx,
            {
                let want = want.clone();
                move |h| h.watch(&want).map(Arc::new)
            },
            move |app, result: std::io::Result<Arc<WatchSub>>, cx| {
                app.editor.watch_opening = false;
                let sub = match result {
                    Ok(sub) => sub,
                    Err(e) => {
                        log::warn!("editor: external-change watcher unavailable: {e}");
                        return;
                    }
                };
                let events = sub.events().clone();
                app.editor.watch = Some(sub);
                app.editor.watch_host = Some(opened_host);
                cx.spawn(async move |app, cx| {
                    while let Ok(batch) = events.recv().await {
                        let ok = app.update(cx, |app, _cx| {
                            let _ = app.editor.events_tx.try_send(batch);
                        });
                        if ok.is_err() {
                            break;
                        }
                    }
                })
                .detach();
                if app.editor.watched_dirs != opened_with {
                    app.editor_watch_apply(cx);
                }
            },
        );
    }

    pub(crate) fn open_file_in_editor(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.get(self.active).is_none() {
            return;
        }
        self.raise_code_overlay();
        if self.editor_activate_open(path, window, cx) {
            return;
        }
        let Some(host) = self.active_host(cx) else {
            return;
        };
        let p = path.to_path_buf();
        HostOps::run_in(
            host,
            window,
            cx,
            move |h| -> Result<(PathBuf, String, Option<MTime>), String> {
                let path = h.canonicalize(&p).unwrap_or(p);
                let meta = match h.stat(&path) {
                    Ok(m) => m,
                    Err(e) => {
                        return Err(t_fmt(
                            L10nKey::EditorCantOpen,
                            &[("path", &path.display().to_string()), ("e", &e.to_string())],
                        ));
                    }
                };
                if meta.len > MAX_FILE_BYTES {
                    return Err(t_fmt(
                        L10nKey::EditorFileTooLarge,
                        &[
                            ("path", &path.display().to_string()),
                            ("size", &(meta.len / (1024 * 1024)).to_string()),
                        ],
                    ));
                }
                let bytes = match h.read_file(&path, MAX_FILE_BYTES) {
                    Ok(b) => b,
                    Err(e) => {
                        return Err(t_fmt(
                            L10nKey::EditorCantRead,
                            &[("path", &path.display().to_string()), ("e", &e.to_string())],
                        ));
                    }
                };
                if looks_binary(&bytes) {
                    return Err(t_fmt(
                        L10nKey::EditorBinaryFile,
                        &[("path", &path.display().to_string())],
                    ));
                }
                let text = String::from_utf8(bytes).map_err(|_| {
                    t_fmt(
                        L10nKey::EditorNotUtf8,
                        &[("path", &path.display().to_string())],
                    )
                })?;
                Ok((path, text, meta.mtime))
            },
            move |app, opened, window, cx| match opened {
                Ok((path, text, mtime)) => app.editor_install_file(path, text, mtime, window, cx),
                Err(message) => window.push_notification(message, cx),
            },
        );
    }

    fn editor_activate_open(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(code) = self.tab_code_mut() else {
            return false;
        };
        let Some(ix) = code.files.iter().position(|f| f.path == *path) else {
            return false;
        };
        code.visible = true;
        let f = code.files.remove(ix);
        code.files.insert(0, f);
        code.active = 0;
        self.focus_editor(window, cx);
        cx.notify();
        true
    }

    fn editor_install_file(
        &mut self,
        path: PathBuf,
        text: String,
        mtime: Option<MTime>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor_activate_open(&path, window, cx) {
            return;
        }
        if self.tabs.get(self.active).is_none() {
            return;
        }
        let language = language_for_path(&path);
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor(language)
                .multi_line(true)
                .tab_size(TabSize {
                    tab_size: 4,
                    hard_tabs: false,
                })
                .line_number(true)
                .searchable(true)
                .replaceable(true)
                .folding(true)
                .soft_wrap(false)
                .default_value(text)
        });
        let sub = cx.subscribe_in(&input, window, {
            let path = path.clone();
            move |this: &mut Tty7App, _input, ev, _window, cx| {
                if matches!(ev, InputEvent::Change) {
                    let Some(f) = this
                        .tabs
                        .iter_mut()
                        .filter_map(|t| t.code.as_deref_mut())
                        .flat_map(|c| c.files.iter_mut())
                        .find(|f| f.path == path)
                    else {
                        return;
                    };
                    f.dirty = true;
                    f.edit_seq = f.edit_seq.wrapping_add(1);
                    cx.notify();
                }
            }
        });
        let tab = self
            .tabs
            .get_mut(self.active)
            .expect("checked at function entry");
        let code = tab.code.get_or_insert_with(|| Box::new(TabCode::new()));
        let observe = cx.observe(&input, |_, _, cx| cx.notify());
        code.files.insert(
            0,
            OpenFile {
                path,
                input,
                dirty: false,
                disk_mtime: mtime,
                edit_seq: 0,
                saving: None,
                save_pending: false,
                save_then_close: false,
                reload_seq: 0,
                conflict: false,
                preview: false,
                wrap: false,
                _sub: sub,
                _observe: observe,
            },
        );
        code.active = 0;
        code.visible = true;
        self.editor_rebuild_watcher(cx);
        self.focus_editor(window, cx);
        cx.notify();
    }

    pub(crate) fn toggle_code_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let buried = tab.overlay_top == crate::ui::app::OverlayTop::Diff
            && tab.diff_overlay.is_some()
            && tab.code.as_ref().is_some_and(|c| c.visible);
        tab.overlay_top = crate::ui::app::OverlayTop::Code;
        if buried {
            self.focus_editor(window, cx);
            cx.notify();
            return;
        }
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let code = tab.code.get_or_insert_with(|| Box::new(TabCode::new()));
        if code.visible {
            code.visible = false;
            self.file_tree.editing = None;
            self.focus_active(window, cx);
            cx.notify();
            return;
        }
        code.visible = true;
        self.file_tree_refresh_roots(window, cx);
        if self.tab_code().is_some_and(|c| c.active_file().is_some()) {
            self.focus_editor(window, cx);
        } else {
            self.file_tree.focus_handle.focus(window, cx);
        }
        cx.notify();
    }

    fn raise_code_overlay(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.overlay_top = crate::ui::app::OverlayTop::Code;
        }
    }

    fn focus_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(f) = self.tab_code().and_then(|c| c.active_file()) {
            f.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    pub(crate) fn editor_has_focus(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.code_panel_visible()
            && self
                .tab_code()
                .and_then(|c| c.active_file())
                .is_some_and(|f| {
                    f.input
                        .read(cx)
                        .focus_handle(cx)
                        .contains_focused(window, cx)
                })
    }

    pub(crate) fn editor_save_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self
            .tab_code()
            .and_then(|c| c.active_file())
            .map(|f| f.input.entity_id())
        else {
            return;
        };
        self.editor_save_file(id, false, window, cx);
    }

    fn editor_save_file(
        &mut self,
        id: gpui::EntityId,
        then_close: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.active_host(cx) else {
            return;
        };
        let Some(f) = self.editor_file_mut(id) else {
            return;
        };
        f.save_then_close |= then_close;
        if f.saving.is_some() {
            f.save_pending = true;
            return;
        }
        let seq = f.edit_seq;
        f.saving = Some(seq);
        let text = f.input.read(cx).text().to_string();
        let target = f.path.clone();
        HostOps::run_in(
            host,
            window,
            cx,
            move |h| h.write_file(&target, text.as_bytes()).map(|m| m.mtime),
            move |app, result: std::io::Result<Option<MTime>>, window, cx| {
                let Some(f) = app.editor_file_mut(id) else {
                    return;
                };
                f.saving = None;
                let landing = settle_save(
                    result.is_ok(),
                    seq,
                    f.edit_seq,
                    std::mem::take(&mut f.save_pending),
                );
                match result {
                    Ok(mtime) => {
                        f.disk_mtime = mtime;
                    }
                    Err(e) => HostOps::notify_err(window, cx, t(L10nKey::EditorSaveFailed), &e),
                }
                if landing.clean {
                    f.dirty = false;
                    f.conflict = false;
                }
                if landing.requeue {
                    app.editor_save_file(id, false, window, cx);
                    cx.notify();
                    return;
                }
                let close = app
                    .editor_file_mut(id)
                    .is_some_and(|f| std::mem::take(&mut f.save_then_close) && !f.dirty);
                if close && let Some((tab_ix, ix)) = app.editor_file_position(id) {
                    app.editor_remove_file_in(tab_ix, ix, cx);
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    fn editor_file_mut(&mut self, id: gpui::EntityId) -> Option<&mut OpenFile> {
        self.tabs
            .iter_mut()
            .filter_map(|t| t.code.as_deref_mut())
            .flat_map(|c| c.files.iter_mut())
            .find(|f| f.input.entity_id() == id)
    }

    fn editor_file_position(&self, id: gpui::EntityId) -> Option<(usize, usize)> {
        self.tabs.iter().enumerate().find_map(|(tab_ix, t)| {
            let code = t.code.as_deref()?;
            let ix = code.files.iter().position(|f| f.input.entity_id() == id)?;
            Some((tab_ix, ix))
        })
    }

    pub(crate) fn editor_close_file(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self.tab_code().and_then(|c| c.files.get(ix)) else {
            return;
        };
        if !f.dirty {
            self.editor_remove_file(ix, cx);
            return;
        }
        let name = f.label();
        let answer = window.prompt(
            PromptLevel::Warning,
            &t_fmt(L10nKey::EditorUnsavedChanges, &[("name", &name)]),
            None,
            &[
                t(L10nKey::Save),
                t(L10nKey::EditorDiscard),
                t(L10nKey::Cancel),
            ],
            cx,
        );
        let id = f.input.entity_id();
        cx.spawn_in(window, async move |app, cx| {
            let Ok(choice) = answer.await else { return };
            let _ = app.update_in(cx, |app, window, cx| match choice {
                0 => app.editor_save_file(id, true, window, cx),
                1 => {
                    if let Some((tab_ix, ix)) = app.editor_file_position(id) {
                        app.editor_remove_file_in(tab_ix, ix, cx);
                    }
                }
                _ => {}
            });
        })
        .detach();
    }

    pub(crate) fn editor_close_active_if_focused(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.editor_has_focus(window, cx) {
            return false;
        }
        let Some(code) = self.tab_code_mut() else {
            return false;
        };
        if code.files.is_empty() {
            code.visible = false;
            cx.notify();
            return true;
        }
        let active = code.active;
        self.editor_close_file(active, window, cx);
        true
    }

    fn editor_remove_file(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.editor_remove_file_in(self.active, ix, cx);
    }

    fn editor_remove_file_in(&mut self, tab_ix: usize, ix: usize, cx: &mut Context<Self>) {
        let Some(code) = self
            .tabs
            .get_mut(tab_ix)
            .and_then(|t| t.code.as_deref_mut())
        else {
            return;
        };
        if ix >= code.files.len() {
            return;
        }
        code.files.remove(ix);
        if code.active >= ix && code.active > 0 {
            code.active -= 1;
        }
        self.editor_rebuild_watcher(cx);
        cx.notify();
    }

    pub(crate) fn editor_handle_external_change(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.active_host(cx) else {
            return;
        };
        let p = path.to_path_buf();
        let landed = p.clone();
        HostOps::run_in(
            host,
            window,
            cx,
            move |h| h.stat(&p).ok().and_then(|m| m.mtime),
            move |app, mtime, window, cx| {
                app.editor_apply_external_change(&landed, mtime, window, cx)
            },
        );
    }

    fn editor_apply_external_change(
        &mut self,
        path: &Path,
        mtime: Option<MTime>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut reload: Vec<(usize, usize)> = Vec::new();
        let mut changed = false;
        for (tab_ix, tab) in self.tabs.iter_mut().enumerate() {
            let Some(code) = tab.code.as_deref_mut() else {
                continue;
            };
            for (ix, f) in code.files.iter_mut().enumerate() {
                if f.path != *path {
                    continue;
                }
                match classify_external_change(f.saving.is_some(), f.dirty, f.disk_mtime, mtime) {
                    ExternalChange::Ignore => {}
                    ExternalChange::Conflict => {
                        f.conflict = true;
                        changed = true;
                    }
                    ExternalChange::Reload => reload.push((tab_ix, ix)),
                }
            }
        }
        for (tab_ix, ix) in reload {
            self.editor_reload_from_disk(tab_ix, ix, window, cx);
        }
        if changed {
            cx.notify();
        }
    }

    pub(crate) fn editor_reload_from_disk(
        &mut self,
        tab_ix: usize,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self
            .tabs
            .get_mut(tab_ix)
            .and_then(|t| t.code.as_deref_mut())
            .and_then(|c| c.files.get_mut(ix))
        else {
            return;
        };
        let target = f.path.clone();
        let id = f.input.entity_id();
        f.reload_seq = f.reload_seq.wrapping_add(1);
        let seq = f.reload_seq;
        let Some(host) = self.active_host(cx) else {
            return;
        };
        HostOps::run_in(
            host,
            window,
            cx,
            move |h| {
                let bytes = h.read_file(&target, MAX_FILE_BYTES)?;
                let text = String::from_utf8(bytes).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "not valid UTF-8")
                })?;
                let mtime = h.stat(&target).ok().and_then(|m| m.mtime);
                Ok((text, mtime))
            },
            move |app, result: std::io::Result<(String, Option<MTime>)>, window, cx| {
                let Some(f) = app.editor_file_mut(id) else {
                    return;
                };
                if f.reload_seq != seq {
                    return;
                }
                let Ok((text, mtime)) = result else {
                    f.dirty = true;
                    f.conflict = false;
                    cx.notify();
                    return;
                };
                f.disk_mtime = mtime;
                f.dirty = false;
                f.conflict = false;
                f.edit_seq = f.edit_seq.wrapping_add(1);
                let input = f.input.clone();
                input.update(cx, |input, cx| input.set_value(text, window, cx));
                cx.notify();
            },
        );
    }
}

impl Tty7App {
    pub(crate) fn render_code_overlay(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.code_panel_visible() {
            return None;
        }
        let body = match self.tab_code().and_then(|c| c.active_file()) {
            None => self.render_editor_empty(cx).into_any_element(),
            Some(f) if f.preview => {
                let markdown = f.input.read(cx).text().to_string();
                div()
                    .id("editor-md-preview")
                    .size_full()
                    .overflow_y_scroll()
                    .px_4()
                    .py_3()
                    .child(gpui_component::text::TextView::markdown(
                        "editor-md-preview-body",
                        markdown,
                    ))
                    .into_any_element()
            }
            Some(f) => {
                let input = f.input.clone();
                Input::new(&input)
                    .appearance(false)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(cx.theme().mono_font_size)
                    .size_full()
                    .into_any_element()
            }
        };
        let conflict_banner = self
            .tab_code()
            .and_then(|c| c.active_file())
            .filter(|f| f.conflict)
            .map(|_| self.render_editor_conflict_banner(cx));

        let editor_col = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(self.render_editor_header(window, cx))
            .when_some(conflict_banner, |this, b| this.child(b))
            .child(div().flex_1().min_h_0().child(body));

        Some(
            v_flex()
                .id("code-panel")
                .absolute()
                .inset_0()
                .occlude()
                .bg(cx.theme().background)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                    if ev.keystroke.key == "escape" {
                        this.toggle_code_panel(window, cx);
                    }
                }))
                .child(h_flex().flex_1().min_h_0().w_full().child(editor_col))
                .child(self.render_code_status_bar(window, cx))
                .into_any_element(),
        )
    }

    fn render_editor_header(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let active = self.tab_code().and_then(|c| c.active_file());
        let name = active.map(|f| f.label());
        let dirty = active.is_some_and(|f| f.dirty);
        let lead = if self.left_panel_open(cx) {
            crate::ui::app::CONTENT_INSET
        } else {
            crate::ui::app::TITLE_BAR_LEAD
        };
        crate::ui::app::title_bar_drag(h_flex().id("editor-header"), "editor-header", window, cx)
            .flex_none()
            .h(px(crate::ui::app::TITLE_BAR_HEIGHT))
            .items_center()
            .gap_1p5()
            .pl(px(lead))
            .pr(px(crate::ui::app::tile_trailing_inset()))
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_ellipsis()
                    .text_sm()
                    .when(name.is_none(), |d| {
                        d.text_color(cx.theme().muted_foreground)
                    })
                    .child(
                        name.unwrap_or_else(|| SharedString::from(t(L10nKey::EditorNoFileOpen))),
                    ),
            )
            .when(dirty, |d| {
                d.child(
                    div()
                        .flex_none()
                        .size(px(6.))
                        .rounded_full()
                        .bg(cx.theme().warning),
                )
            })
            .child(
                div().occlude().flex_shrink_0().child(
                    crate::ui::tab_strip::chrome_tile_sized(
                        Button::new("editor-panel-close").icon(Icon::new(IconName::Close)),
                        crate::ui::app::TILE_SIZE,
                        crate::ui::app::TILE_GLYPH_LINE,
                        false,
                        cx,
                    )
                    .rounded_lg()
                    .tooltip(t(L10nKey::EditorBackToTerminal))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_code_panel(window, cx);
                    })),
                ),
            )
    }

    fn render_code_status_bar(&self, _window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let code = self.tab_code();
        let muted = cx.theme().muted_foreground;
        let path_text: Option<SharedString> = code.map(|c| {
            let repo = c
                .roots
                .first()
                .and_then(|r| r.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            match c.active_file() {
                Some(f) => {
                    let rel = c
                        .roots
                        .iter()
                        .find_map(|r| f.path.strip_prefix(r).ok())
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| f.label().to_string());
                    format!("{repo} › {rel}").into()
                }
                None => repo.into(),
            }
        });
        let active = code.and_then(|c| c.active_file());
        let cursor: Option<SharedString> = active.map(|f| {
            let pos = f.input.read(cx).cursor_position();
            t_fmt(
                L10nKey::EditorLnCol,
                &[
                    ("line", &(pos.line + 1).to_string()),
                    ("column", &(pos.character + 1).to_string()),
                ],
            )
            .into()
        });
        let wrap: Option<bool> = active.map(|f| f.wrap);
        let is_markdown = active.is_some_and(|f| language_for_path(&f.path) == "markdown");
        let preview = active.is_some_and(|f| f.preview);

        h_flex()
            .flex_none()
            .w_full()
            .h(px(26.))
            .items_center()
            .gap_3()
            .px_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_xs()
            .text_color(muted)
            .when_some(path_text, |this, t| {
                this.child(div().min_w_0().text_ellipsis().child(t))
            })
            .child(div().flex_1())
            .when(is_markdown, |this| {
                this.child(
                    Button::new("status-md-preview")
                        .label(if preview {
                            t(L10nKey::EditorEdit)
                        } else {
                            t(L10nKey::EditorPreview)
                        })
                        .custom(crate::ui::tab_strip::chrome_tile_variant(cx))
                        .xsmall()
                        .on_click(cx.listener(|this, _, _w, cx| {
                            if let Some(code) = this.tab_code_mut() {
                                let ix = code.active;
                                if let Some(f) = code.files.get_mut(ix) {
                                    f.preview = !f.preview;
                                    cx.notify();
                                }
                            }
                        })),
                )
            })
            .when_some(wrap, |this, wrap| {
                this.child(
                    Button::new("status-wrap")
                        .label(if wrap {
                            t(L10nKey::EditorWrapOn)
                        } else {
                            t(L10nKey::EditorWrapOff)
                        })
                        .custom(crate::ui::tab_strip::chrome_tile_variant(cx))
                        .xsmall()
                        .on_click(cx.listener(|this, _, window, cx| {
                            let Some(code) = this.tab_code_mut() else {
                                return;
                            };
                            let ix = code.active;
                            if let Some(f) = code.files.get_mut(ix) {
                                f.wrap = !f.wrap;
                                let wrap = f.wrap;
                                f.input.clone().update(cx, |st, cx| {
                                    st.set_soft_wrap(wrap, window, cx);
                                });
                            }
                        })),
                )
            })
            .when_some(cursor, |this, t| this.child(div().child(t)))
    }

    fn render_editor_empty(&self, cx: &Context<Self>) -> gpui::Div {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                Icon::new(IconName::File)
                    .large()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::ui::i18n::t(
                        crate::ui::i18n::L10nKey::OpenFileFromTree,
                    )),
            )
    }

    fn render_editor_conflict_banner(&self, cx: &mut Context<Self>) -> AnyElement {
        let tab_ix = self.active;
        let ix = self.tab_code().map(|c| c.active).unwrap_or(0);
        h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .bg(cx.theme().warning.opacity(0.15))
            .border_b_1()
            .border_color(cx.theme().border)
            .text_sm()
            .child(div().flex_1().child(crate::ui::i18n::t(
                crate::ui::i18n::L10nKey::FileChangedOnDisk,
            )))
            .child(
                Button::new("editor-conflict-reload")
                    .label(crate::ui::i18n::t(crate::ui::i18n::L10nKey::Reload))
                    .small()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.editor_reload_from_disk(tab_ix, ix, window, cx);
                    })),
            )
            .child(
                Button::new("editor-conflict-keep")
                    .label(crate::ui::i18n::t(crate::ui::i18n::L10nKey::KeepMine))
                    .ghost()
                    .small()
                    .on_click(cx.listener(move |this, _, _w, cx| {
                        if let Some(f) = this.tab_code_mut().and_then(|c| c.files.get_mut(ix)) {
                            f.conflict = false;
                            cx.notify();
                        }
                    })),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_map_covers_common_extensions() {
        for (path, lang) in [
            ("a/b/main.rs", "rust"),
            ("x.tsx", "tsx"),
            ("x.jsx", "javascript"),
            ("x.yml", "yaml"),
            ("Makefile", "make"),
            ("CMakeLists.txt", "cmake"),
            (".zshrc", "bash"),
            ("notes.md", "markdown"),
            ("query.SQL", "sql"),
            ("unknown.xyz", "text"),
            ("no_ext", "text"),
        ] {
            assert_eq!(language_for_path(Path::new(path)), lang, "path {path}");
        }
    }

    #[test]
    fn binary_sniff_flags_nul_bytes_only() {
        assert!(looks_binary(b"\x7fELF\x00\x01"));
        assert!(!looks_binary("plain text\nwith lines".as_bytes()));
        assert!(!looks_binary("中文 UTF-8 内容".as_bytes()));
    }

    fn t(secs: i64, nanos: u32) -> Option<MTime> {
        Some(MTime { secs, nanos })
    }

    #[test]
    fn external_changes_are_told_apart_from_our_own_saves() {
        let ours = t(100, 0);

        assert_eq!(
            classify_external_change(false, false, ours, ours),
            ExternalChange::Ignore
        );

        assert_eq!(
            classify_external_change(false, false, ours, t(101, 0)),
            ExternalChange::Reload
        );

        assert_eq!(
            classify_external_change(false, true, ours, t(101, 0)),
            ExternalChange::Conflict
        );

        assert_eq!(
            classify_external_change(false, false, t(100, 0), t(100, 1)),
            ExternalChange::Reload
        );

        assert_eq!(
            classify_external_change(true, false, ours, t(101, 0)),
            ExternalChange::Ignore
        );

        assert_eq!(
            classify_external_change(false, false, None, None),
            ExternalChange::Reload
        );
    }

    #[test]
    fn a_landed_save_only_cleans_a_buffer_that_did_not_move() {
        assert_eq!(
            settle_save(true, 7, 7, false),
            SaveLanding {
                clean: true,
                requeue: false
            }
        );

        assert_eq!(
            settle_save(true, 7, 9, false),
            SaveLanding {
                clean: false,
                requeue: false
            }
        );

        assert_eq!(
            settle_save(true, 7, 9, true),
            SaveLanding {
                clean: false,
                requeue: true
            }
        );

        assert_eq!(
            settle_save(false, 7, 7, true),
            SaveLanding {
                clean: false,
                requeue: false
            }
        );
    }
}

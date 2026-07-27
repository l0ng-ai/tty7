//! The code panel: a full-body overlay of `[file tree | editor]` that covers
//! the terminal, settings-overlay style.
//!
//! A lightweight "look at / touch up code without leaving the terminal"
//! editor, not a full IDE. The text engine is `gpui_component::input::
//! InputState` in CodeEditor mode, which brings rope storage, tree-sitter
//! syntax highlighting, line numbers, indent guides, code folding,
//! auto-indent, undo/redo and an in-buffer search/replace bar. This module
//! owns everything around that engine: the open-file set and tab strip, dirty
//! tracking and save, external-modification reload (via `notify`), the
//! unsaved-close confirmation, and the overlay chrome itself (the file-tree
//! column comes from `ui::file_tree`).
//!
//! Deliberately *not* an IDE: there is no language-server integration, and
//! adding one is not a wanted feature. Opening a `.rs` file silently spawning
//! rust-analyzer — a background process indexing the whole workspace for
//! hundreds of megabytes of RAM — is not something a terminal emulator should
//! do to its user. Highlighting comes from tree-sitter grammars compiled into
//! gpui-component (see [`language_for_path`]), which is static, in-process,
//! and costs nothing beyond parsing the open buffer.
//!
//! Layout: overlaying the body (like Settings and the diff overlay) rather
//! than docking a side column means toggling never resizes the terminal — no
//! PTY resize, no reflow — and the editor gets the full body width. The tab
//! sidebar stays visible; switching tabs re-roots the tree. One entry point:
//! the title-bar tile in `tab_strip` (`ToggleCodePanel`, ⌘⇧E; Esc closes).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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

/// Refuse to open files larger than this: the component's code editor is rated
/// to ~50K lines, and a multi-megabyte blob is almost never what a terminal
/// user meant to open in a side panel.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Debounce for external-change reloads, matching the config hot-reload: a
/// save is often a truncate→write→rename burst that should collapse to one.
const RELOAD_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// One open file: the component editor state plus the bookkeeping that turns
/// it into a *file* editor (path, dirty flag, on-disk snapshot identity).
pub(crate) struct OpenFile {
    pub(crate) path: PathBuf,
    pub(crate) input: Entity<InputState>,
    /// The buffer has edits not yet written to `path`.
    pub(crate) dirty: bool,
    /// mtime of the content we last loaded from / saved to disk; used to drop
    /// watcher echoes of our own saves.
    disk_mtime: Option<SystemTime>,
    /// Disk changed under unsaved edits: show the reload/keep banner instead
    /// of silently clobbering either side.
    pub(crate) conflict: bool,
    /// Markdown files can flip the buffer into a rendered preview.
    pub(crate) preview: bool,
    /// Soft-wrap state (mirrored here — the input's own flag isn't readable).
    pub(crate) wrap: bool,
    _sub: Subscription,
    /// Repaints the app when the input notifies (cursor moves, scrolls…) so
    /// the status bar's Ln/Col stays live.
    _observe: Subscription,
}

impl OpenFile {
    /// Tab label: the file name (the path differentiates in the tooltip).
    fn label(&self) -> SharedString {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.display().to_string())
            .into()
    }
}

/// Per-tab code-panel state, hung on [`Tab::code`](crate::ui::app::Tab) with
/// the same lifecycle contract as the diff overlay: only the active tab's
/// panel renders, switching away hides it, closing the tab drops it. The
/// shared caches (directory listings, gitignore matchers, filesystem
/// watchers) live on [`Tty7App`] — this holds only what is truly this tab's:
/// its open files and its tree view state.
pub(crate) struct TabCode {
    /// Whether the overlay is currently shown for this tab. The open-file set
    /// survives hiding (Esc) — only closing the tab drops it.
    pub(crate) visible: bool,
    pub(crate) files: Vec<OpenFile>,
    pub(crate) active: usize,
    /// File-tree roots: this tab's pane cwds resolved to repo roots.
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) expanded: std::collections::HashSet<PathBuf>,
    pub(crate) selected: Option<PathBuf>,
}

impl TabCode {
    pub(crate) fn new() -> Self {
        Self {
            // Born hidden. This state used to be created only by opening the
            // overlay, so defaulting to visible was harmless; now the right
            // panel's Files tab creates it just to hold the tree's roots and
            // expansion, and a default of `true` popped an empty editor open
            // ("No file open") the moment you looked at the tree. Every path
            // that actually wants the overlay sets `visible` itself.
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

/// App-global editor infrastructure shared by every tab's panel.
pub(crate) struct EditorPanelState {
    /// Watches the parent directories of open files (across all tabs) for
    /// external changes. Rebuilt whenever any open set changes; `None` while
    /// nothing is open anywhere.
    watcher: Option<notify::RecommendedWatcher>,
    /// Feeds changed paths from the watcher thread into the UI-side reload
    /// loop spawned in [`EditorPanelState::new`].
    events_tx: smol::channel::Sender<PathBuf>,
}

impl EditorPanelState {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Tty7App>) -> Self {
        // The reload loop lives for the app: it debounces watcher pings and
        // routes them to `handle_external_change` on the UI thread.
        let (tx, rx) = smol::channel::unbounded::<PathBuf>();
        cx.spawn_in(window, async move |app, cx| {
            while let Ok(first) = rx.recv().await {
                cx.background_executor().timer(RELOAD_DEBOUNCE).await;
                let mut changed: HashSet<PathBuf> = HashSet::from([first]);
                while let Ok(more) = rx.try_recv() {
                    changed.insert(more);
                }
                let ok = app.update_in(cx, |app, window, cx| {
                    for path in changed {
                        app.editor_handle_external_change(&path, window, cx);
                    }
                });
                if ok.is_err() {
                    break; // app dropped; stop the loop
                }
            }
        })
        .detach();
        Self {
            watcher: None,
            events_tx: tx,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (tested).
// ---------------------------------------------------------------------------

/// The tree-sitter language name for a path, matching the grammars compiled
/// into gpui-component's `tree-sitter-languages` feature. Falls back to
/// `"text"` (plain, no highlighting) for anything unknown.
pub(crate) fn language_for_path(path: &Path) -> &'static str {
    // Whole-filename matches first (no useful extension).
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        let lowered = name.to_ascii_lowercase();
        match lowered.as_str() {
            "makefile" | "gnumakefile" => return "make",
            "cmakelists.txt" => return "cmake",
            _ => {}
        }
        // Dotfile shell rc's: .zshrc, .bashrc, .profile…
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

/// Quick binary sniff: a NUL byte in the head of the file. Text files never
/// contain NULs; this catches executables/images before `from_utf8` chokes on
/// them with a less helpful error.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

// ---------------------------------------------------------------------------
// Tty7App: open / save / close / external reload.
// ---------------------------------------------------------------------------

impl Tty7App {
    /// The active tab's code-panel state, if the panel was ever opened there.
    pub(crate) fn tab_code(&self) -> Option<&TabCode> {
        self.tabs.get(self.active)?.code.as_deref()
    }

    pub(crate) fn tab_code_mut(&mut self) -> Option<&mut TabCode> {
        self.tabs.get_mut(self.active)?.code.as_deref_mut()
    }

    /// Like [`tab_code_mut`], but creates the state instead of returning `None`.
    /// The panel state used to be born with the code overlay, so anything that
    /// needed it could assume the overlay had been opened at least once — no
    /// longer true now that the right panel's Files tab renders the same tree
    /// without ever opening the overlay.
    pub(crate) fn tab_code_mut_or_init(&mut self) -> Option<&mut TabCode> {
        let tab = self.tabs.get_mut(self.active)?;
        Some(tab.code.get_or_insert_with(|| Box::new(TabCode::new())))
    }

    /// Whether the active tab's code panel is currently shown.
    pub(crate) fn code_panel_visible(&self) -> bool {
        self.tab_code().is_some_and(|c| c.visible)
    }

    /// Rebuild the external-change watcher over every tab's open files.
    /// Watches each file's *parent directory* (non-recursively): editors that
    /// save via rename replace the inode, which a direct file watch loses.
    fn editor_rebuild_watcher(&mut self) {
        use notify::{RecursiveMode, Watcher};
        self.editor.watcher = None;
        let watched: HashSet<PathBuf> = self
            .tabs
            .iter()
            .filter_map(|t| t.code.as_deref())
            .flat_map(|c| c.files.iter().map(|f| f.path.clone()))
            .collect();
        if watched.is_empty() {
            return;
        }
        let dirs: HashSet<PathBuf> = watched
            .iter()
            .filter_map(|p| p.parent().map(Path::to_path_buf))
            .collect();
        let tx = self.editor.events_tx.clone();
        let handler = move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            for p in &event.paths {
                if watched.contains(p) {
                    let _ = tx.try_send(p.clone());
                }
            }
        };
        let mut watcher = match notify::recommended_watcher(handler) {
            Ok(w) => w,
            Err(e) => {
                log::warn!("editor: external-change watcher unavailable: {e}");
                return;
            }
        };
        for dir in dirs {
            if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                log::warn!("editor: failed to watch {}: {e}", dir.display());
            }
        }
        self.editor.watcher = Some(watcher);
    }

    /// Open `path` in the active tab's editor (activating an existing file tab
    /// when it is already open) and reveal the panel. Errors surface as window
    /// notifications rather than a half-open tab.
    pub(crate) fn open_file_in_editor(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs.get(self.active).is_none() {
            return;
        }
        // Opening a file is an act on the editor, so it comes forward — the
        // file tree lives in the right panel and stays clickable even while the
        // diff overlay covers the column.
        self.raise_code_overlay();
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(code) = self.tab_code_mut()
            && let Some(ix) = code.files.iter().position(|f| f.path == path)
        {
            code.visible = true;
            // Activating always surfaces to the front of the strip: the strip
            // is MRU-ordered and only its head fits on screen (see
            // `render_editor_tabs`), so the active file must live there.
            let f = code.files.remove(ix);
            code.files.insert(0, f);
            code.active = 0;
            self.focus_editor(window, cx);
            cx.notify();
            return;
        }
        match std::fs::metadata(&path) {
            Ok(meta) if meta.len() > MAX_FILE_BYTES => {
                window.push_notification(
                    format!(
                        "\"{}\" is too large for the editor ({} MB)",
                        path.display(),
                        meta.len() / (1024 * 1024)
                    ),
                    cx,
                );
                return;
            }
            Err(e) => {
                window.push_notification(format!("Can't open {}: {e}", path.display()), cx);
                return;
            }
            Ok(_) => {}
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                window.push_notification(format!("Can't read {}: {e}", path.display()), cx);
                return;
            }
        };
        if looks_binary(&bytes) {
            window.push_notification(
                format!("\"{}\" looks like a binary file", path.display()),
                cx,
            );
            return;
        }
        let text = match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(_) => {
                window.push_notification(format!("\"{}\" is not valid UTF-8", path.display()), cx);
                return;
            }
        };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
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
        // Dirty tracking: `set_value` suppresses events, so every Change here
        // is a real user edit. Files may be open in any tab, not just the
        // active one, so the lookup scans all tabs.
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
                    if !f.dirty {
                        f.dirty = true;
                    }
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
        // New files join at the front of the MRU strip (always visible).
        code.files.insert(
            0,
            OpenFile {
                path,
                input,
                dirty: false,
                disk_mtime: mtime,
                conflict: false,
                preview: false,
                wrap: false,
                _sub: sub,
                _observe: observe,
            },
        );
        code.active = 0;
        code.visible = true;
        self.editor_rebuild_watcher();
        self.focus_editor(window, cx);
        cx.notify();
    }

    /// `ToggleCodePanel` (⌘⇧E / the title-bar tree icon / Esc): flip the
    /// active tab's code overlay. First open creates the tab's panel state;
    /// hiding keeps it (open files survive Esc), and only closing the tab
    /// drops it. Opening re-roots the file tree from the tab's panes and
    /// focuses the panel; closing hands focus back to the terminal.
    pub(crate) fn toggle_code_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        // Buried under the diff overlay, this shortcut means "come forward" —
        // hiding a panel the user can't see would look like it did nothing.
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

    /// Bring the code overlay in front of the diff overlay. See
    /// [`Tab::overlay_top`](crate::ui::app::Tab).
    fn raise_code_overlay(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.overlay_top = crate::ui::app::OverlayTop::Code;
        }
    }

    /// Focus the active file's text input (e.g. right after opening a file).
    fn focus_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(f) = self.tab_code().and_then(|c| c.active_file()) {
            f.input.update(cx, |input, cx| input.focus(window, cx));
        }
    }

    /// Whether keyboard focus currently sits inside the editor panel. Lets
    /// shared shortcuts (⌘S, ⌘W) route here before their terminal meaning.
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

    /// `EditorSave` (⌘S): write the active buffer back to its path.
    pub(crate) fn editor_save_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(code) = self.tab_code_mut() else {
            return;
        };
        let active = code.active;
        let Some(f) = code.files.get_mut(active) else {
            return;
        };
        let text = f.input.read(cx).text().to_string();
        match std::fs::write(&f.path, &text) {
            Ok(()) => {
                f.dirty = false;
                f.conflict = false;
                f.disk_mtime = std::fs::metadata(&f.path).and_then(|m| m.modified()).ok();
                cx.notify();
            }
            Err(e) => {
                window.push_notification(format!("Save failed: {e}"), cx);
            }
        }
    }

    /// Close the file tab at `ix`. Dirty buffers get a native three-way prompt
    /// (save / discard / cancel) before anything is lost.
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
            &format!("\"{name}\" has unsaved changes"),
            None,
            &["Save", "Discard", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |app, cx| {
            let Ok(choice) = answer.await else { return };
            let _ = app.update_in(cx, |app, window, cx| match choice {
                0 => {
                    // Save, then close. Save failure keeps the tab open.
                    let prev_active = app.tab_code().map(|c| c.active);
                    if let Some(code) = app.tab_code_mut() {
                        code.active = ix;
                    }
                    app.editor_save_active(window, cx);
                    if let (Some(code), Some(prev)) = (app.tab_code_mut(), prev_active) {
                        code.active = prev;
                    }
                    if app
                        .tab_code()
                        .and_then(|c| c.files.get(ix))
                        .is_some_and(|f| !f.dirty)
                    {
                        app.editor_remove_file(ix, cx);
                    }
                }
                1 => app.editor_remove_file(ix, cx),
                _ => {}
            });
        })
        .detach();
    }

    /// If focus is in the editor, close the active file tab and report `true`
    /// (so ⌘W routes here instead of closing the terminal tab).
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
        let Some(code) = self.tab_code_mut() else {
            return;
        };
        if ix >= code.files.len() {
            return;
        }
        code.files.remove(ix);
        if code.active >= ix && code.active > 0 {
            code.active -= 1;
        }
        self.editor_rebuild_watcher();
        cx.notify();
    }

    /// A watched file changed on disk. Clean buffers reload silently; dirty
    /// ones raise the conflict banner and let the user pick a side. The file
    /// may be open in several tabs — each buffer is handled on its own.
    pub(crate) fn editor_handle_external_change(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
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
                // Our own save's echo: mtime matches what we just wrote.
                if mtime.is_some() && mtime == f.disk_mtime {
                    continue;
                }
                if f.dirty {
                    f.conflict = true;
                    changed = true;
                } else {
                    reload.push((tab_ix, ix));
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

    /// Replace one buffer with the on-disk content (used by the silent reload
    /// and the conflict banner's "Reload" choice). A vanished file just keeps
    /// the buffer and marks it dirty — saving will recreate it.
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
        let Ok(text) = std::fs::read_to_string(&f.path) else {
            f.dirty = true;
            f.conflict = false;
            cx.notify();
            return;
        };
        f.disk_mtime = std::fs::metadata(&f.path).and_then(|m| m.modified()).ok();
        f.dirty = false;
        f.conflict = false;
        let input = f.input.clone();
        input.update(cx, |input, cx| input.set_value(text, window, cx));
        cx.notify();
    }
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

impl Tty7App {
    /// The code panel: a full-body overlay of `[file tree | editor]` covering
    /// the terminal (settings/diff-overlay style), or `None` while closed.
    /// The terminal underneath keeps its size — toggling never reflows it.
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
            // Markdown preview replaces the buffer with a rendered view.
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
                // `appearance(false)`: no border/background of its own — the
                // buffer sits flush in the panel instead of in a rounded box.
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
            .child(self.render_editor_header(cx))
            .when_some(conflict_banner, |this, b| this.child(b))
            .child(div().flex_1().min_h_0().child(body));

        Some(
            v_flex()
                .id("code-panel")
                .absolute()
                // Fills its column, which is now everything *except* the detail
                // panel — the panel is a sibling of that column, not a child of it,
                // so the tree that opens files stays visible beside the editor
                // without the overlay needing to know the panel's width.
                .inset_0()
                // The overlay must swallow input to the terminal behind it.
                .occlude()
                .bg(cx.theme().background)
                // Escape (not consumed by the editor's own search/completion
                // handling, which stops propagation) drops back to the terminal.
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                    if ev.keystroke.key == "escape" {
                        this.toggle_code_panel(window, cx);
                    }
                }))
                // No top inset: the header row below *is* the title bar's row, and
                // it clears the window controls itself (see `render_editor_header`).
                // Padding the whole overlay down would cost a blank 40px band and
                // still misalign the editor's top edge with the panel's tab row.
                // No tree column here: the right panel owns the file tree now, and
                // the overlay stops short of it (see the `right` inset above), so
                // the tree stays visible beside the editor instead of being
                // duplicated inside it.
                .child(h_flex().flex_1().min_h_0().w_full().child(editor_col))
                .child(self.render_code_status_bar(window, cx))
                .into_any_element(),
        )
    }

    /// The editor's one header row: which file is open, and a way back to the
    /// terminal. Not a tab strip — the file tree is the switcher now, so this only
    /// has to answer "what am I looking at" without earning a row of chrome for
    /// every buffer that was ever opened. Sits on the title bar's line and matches
    /// its height, so the editor's top edge lines up with the panel's tab row and
    /// the rail's controls across the window.
    fn render_editor_header(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let active = self.tab_code().and_then(|c| c.active_file());
        let name = active.map(|f| f.label());
        let dirty = active.is_some_and(|f| f.dirty);
        // The overlay fills the column left of the detail panel. With the rail out
        // that column starts after it, and the traffic lights sit on the rail's
        // surface — but with the rail collapsed (or in horizontal-tabs mode) the
        // column starts at the window's left edge and the lights are right where
        // the filename would go, so the header takes the window controls' reserve
        // as its inset instead.
        let lead = if self.left_panel_open(cx) {
            crate::ui::app::CONTENT_INSET
        } else {
            crate::ui::app::TITLE_BAR_LEAD
        };
        // The overlay covers the real title bar, so this row inherits its drag and
        // zoom gestures — otherwise opening a file turns the top of the window into
        // a strip that looks like the caption and can't move it.
        crate::ui::app::title_bar_drag(h_flex().id("editor-header"))
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
                    .child(name.unwrap_or_else(|| SharedString::from("No file open"))),
            )
            // Same amber dot the tree marks unsaved files with.
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
                // `occlude()` for the same reason the title bar's own tiles carry
                // it: this row is a `WindowControlArea::Drag`, which on Windows is
                // HTCAPTION, and the OS takes the press before gpui hit-tests.
                div().occlude().flex_shrink_0().child(
                    crate::ui::tab_strip::chrome_tile_sized(
                        // This header is the title bar's own height and sits flush
                        // with it, so its one control is a full chrome tile — not the
                        // half-size one it used to be, which read as a different
                        // class of button on the same line.
                        Button::new("editor-panel-close").icon(Icon::new(IconName::Close)),
                        crate::ui::app::TILE_SIZE,
                        crate::ui::app::TILE_GLYPH_LINE,
                        false,
                        cx,
                    )
                    .rounded_lg()
                    .tooltip("Back to Terminal (Esc)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_code_panel(window, cx);
                    })),
                ),
            )
    }

    /// The Zed-style status bar along the panel bottom: repo-relative path on
    /// the left; preview/wrap toggles and the cursor position on the right.
    fn render_code_status_bar(&self, _window: &Window, cx: &mut Context<Self>) -> gpui::Div {
        let code = self.tab_code();
        let muted = cx.theme().muted_foreground;
        // `repo › relative/path` for the active file; just the repo otherwise.
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
            format!("Ln {}, Col {}", pos.line + 1, pos.character + 1).into()
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
                        .label(if preview { "Edit" } else { "Preview" })
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
                        .label(if wrap { "Wrap: on" } else { "Wrap: off" })
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

    /// Empty state: the panel is open with nothing loaded.
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
                    .child("Open a file from the file tree"),
            )
    }

    /// Banner shown when the file changed on disk while the buffer is dirty.
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
            .child(div().flex_1().child("File changed on disk"))
            .child(
                Button::new("editor-conflict-reload")
                    .label("Reload")
                    .small()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.editor_reload_from_disk(tab_ix, ix, window, cx);
                    })),
            )
            .child(
                Button::new("editor-conflict-keep")
                    .label("Keep mine")
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
}

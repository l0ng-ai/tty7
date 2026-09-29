//! Getting around the editor: back and forward through where the caret has
//! been, Go to Symbol, and the breadcrumbs over the text.
//!
//! **History.** Every jump — Go to File, Go to Line, Go to Symbol, a file link
//! clicked in the grid, a definition a language server found, a click far down
//! the file — is noticed the same way: [`Tty7App::editor_nav_tick`] samples the
//! caret each time the editor draws, and a caret that changed file, or moved
//! [`JUMP_LINES`] or more without the text changing under it, has jumped. The
//! place it left goes on the back stack. Nothing has to remember to report a
//! jump, so a new way of jumping cannot forget to.
//!
//! One history per tab, like a tab's strip of files: each tab is its own
//! editor group. A place in a file that has since been closed opens the file
//! again; one in an untitled buffer that is gone is skipped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{AnyElement, App, Context, Window, div, px};
use gpui_component::input::{LineEdit, Position};
use gpui_component::{ActiveTheme as _, Icon, IconName, h_flex};
use tty7_core::core::machine::TabId;

use super::outline::{self, Outline, SymbolKind};
use super::{BufferId, CURSOR_SCROLL_ATTEMPTS, TabCode, place_cursor};
use crate::ui::app::Tty7App;
use crate::ui::host_ops::HostId;
use crate::ui::i18n::{L10nKey, t};
use crate::ui::search::{CommandKind, Item, SearchTab};

/// A caret that moves this many lines or more at once has jumped. Arrowing
/// and typing never do; a click, a search hit or Page Down do.
pub(crate) const JUMP_LINES: u32 = 10;

/// How many places the back stack keeps.
pub(crate) const MAX_HISTORY: usize = 50;

/// How long a Back or Forward waits for the file it asked for to open before
/// the caret is watched for jumps again.
const PENDING_TIMEOUT: Duration = Duration::from_secs(3);

/// How often an outline is rebuilt while the text keeps changing. Between
/// rebuilds the breadcrumbs show the last one, a keystroke or two stale.
const OUTLINE_THROTTLE: Duration = Duration::from_millis(250);

/// The key context the editor's chords are bound in: the text field's own.
///
/// Not a context of the editor panel's: gpui ranks a context-free binding as
/// deep as whatever has focus, so a panel-level binding would lose ⌘⇧O to the
/// workspace switcher. At the text field's depth, and listed after the
/// window's table, it wins. Only the editor panel handles these actions, so in
/// any other text field — or the terminal — the chord falls through to the
/// binding next in line.
pub(crate) const KEY_CONTEXT: &str = "Input";

/// One place the caret has been.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NavLocation {
    pub host: HostId,
    /// Empty for an untitled buffer.
    pub path: PathBuf,
    /// Set only for an untitled buffer, which has no path to be found by and
    /// can be returned to only while it is still open.
    pub buffer: Option<BufferId>,
    pub pos: Position,
}

impl NavLocation {
    fn same_file(&self, other: &NavLocation) -> bool {
        self.host == other.host && self.path == other.path && self.buffer == other.buffer
    }

    /// In the same file and within a jump of each other: one place, as far
    /// as history is concerned.
    fn near(&self, other: &NavLocation) -> bool {
        self.same_file(other) && self.pos.line.abs_diff(other.pos.line) < JUMP_LINES
    }
}

/// Back and forward stacks, as a browser keeps them.
#[derive(Clone, Debug, Default)]
pub(crate) struct NavHistory {
    back: Vec<NavLocation>,
    forward: Vec<NavLocation>,
}

impl NavHistory {
    /// The caret jumped away from `from`. A new jump forgets the way forward,
    /// and a place near the last one recorded replaces it rather than
    /// stacking up beside it.
    pub(crate) fn push(&mut self, from: NavLocation) {
        self.forward.clear();
        match self.back.last_mut() {
            Some(last) if last.near(&from) => *last = from,
            _ => self.back.push(from),
        }
        if self.back.len() > MAX_HISTORY {
            self.back.remove(0);
        }
    }

    /// Where Back goes from `current`, which becomes the way forward.
    /// Places near `current` are passed over: going back to where you already
    /// are would look like the key did nothing.
    pub(crate) fn go_back(&mut self, current: Option<NavLocation>) -> Option<NavLocation> {
        let target = pop_past(&mut self.back, current.as_ref())?;
        if let Some(current) = current {
            self.forward.push(current);
        }
        Some(target)
    }

    /// Where Forward goes from `current`, which goes back on the back stack.
    pub(crate) fn go_forward(&mut self, current: Option<NavLocation>) -> Option<NavLocation> {
        let target = pop_past(&mut self.forward, current.as_ref())?;
        if let Some(current) = current {
            match self.back.last_mut() {
                Some(last) if last.near(&current) => *last = current,
                _ => self.back.push(current),
            }
        }
        Some(target)
    }

    /// Moves the places in one file along with edits made to it, so a place
    /// keeps pointing at the same code after lines above it come and go.
    pub(crate) fn shift(&mut self, file: &NavLocation, edits: &[LineEdit]) {
        for loc in self.back.iter_mut().chain(self.forward.iter_mut()) {
            if loc.same_file(file) {
                loc.pos.line = shift_line(loc.pos.line, edits);
            }
        }
    }

    /// Drops the places `keep` turns down, from both stacks.
    pub(crate) fn retain(&mut self, keep: impl Fn(&NavLocation) -> bool) {
        self.back.retain(&keep);
        self.forward.retain(&keep);
    }

    #[cfg(test)]
    fn lens(&self) -> (usize, usize) {
        (self.back.len(), self.forward.len())
    }
}

fn shift_line(line: u32, edits: &[LineEdit]) -> u32 {
    edits
        .iter()
        .fold(line as usize, |line, edit| edit.shift(line))
        .min(u32::MAX as usize) as u32
}

fn pop_past(stack: &mut Vec<NavLocation>, current: Option<&NavLocation>) -> Option<NavLocation> {
    loop {
        let top = stack.pop()?;
        if !current.is_some_and(|c| c.near(&top)) {
            return Some(top);
        }
    }
}

/// Where the caret is, and how long the text is — a text that changed length
/// since the last sample was being edited, and an edit is not a jump.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sample {
    pub at: NavLocation,
    pub text_len: usize,
}

/// One tab's history, and the caret sample the next one is compared against.
#[derive(Default)]
pub(crate) struct TabNav {
    pub(crate) history: NavHistory,
    last: Option<Sample>,
    /// A Back or Forward on its way to a file that is still opening. Until it
    /// lands, the caret that has not got there yet is not a jump.
    pending: Option<(NavLocation, Instant)>,
}

impl TabNav {
    /// Compares a fresh sample against the last one and records a jump.
    /// `quiet` says the caret was moved by a key that pages or pastes, which
    /// can go a long way without anyone having jumped anywhere.
    pub(crate) fn observe(&mut self, now: Option<Sample>, at: Instant, quiet: bool) {
        if let Some((target, since)) = &self.pending {
            let arrived = now.as_ref().is_some_and(|s| s.at.same_file(target));
            if arrived || at.duration_since(*since) > PENDING_TIMEOUT {
                self.pending = None;
                if arrived {
                    self.last = now;
                    return;
                }
            } else {
                return;
            }
        }
        let Some(now) = now else {
            // No buffer in front: nothing to compare the next one against but
            // the last place the caret was, which is kept.
            return;
        };
        if let Some(last) = &self.last {
            let jumped = !last.at.same_file(&now.at)
                || (!quiet
                    && last.text_len == now.text_len
                    && last.at.pos.line.abs_diff(now.at.pos.line) >= JUMP_LINES);
            if jumped {
                self.history.push(last.at.clone());
            }
        }
        self.last = Some(now);
    }

    /// Moves the history, and the last sample, along with edits to a file.
    fn shift(&mut self, file: &NavLocation, edits: &[LineEdit]) {
        self.history.shift(file, edits);
        if let Some(last) = self.last.as_mut().filter(|l| l.at.same_file(file)) {
            last.at.pos.line = shift_line(last.at.pos.line, edits);
        }
    }

    fn expect(&mut self, target: NavLocation, at: Instant) {
        self.pending = Some((target, at));
    }
}

struct OutlineCache {
    text: gpui_component::Rope,
    outline: Arc<Outline>,
    at: Instant,
}

/// Go to Symbol's preview: where the caret was before the picker started
/// moving it, put back if the picker is dismissed.
struct SymbolPreview {
    buffer: BufferId,
    origin: Position,
    scroll: gpui::Point<gpui::Pixels>,
}

/// The window's navigation state, held by `EditorPanelState`.
#[derive(Default)]
pub(crate) struct EditorNav {
    tabs: HashMap<TabId, TabNav>,
    outlines: HashMap<BufferId, OutlineCache>,
    /// Symbols handed in from elsewhere — a language server's
    /// `documentSymbol` — which win over the tree's.
    provided: HashMap<BufferId, Arc<Outline>>,
    preview: Option<SymbolPreview>,
    refresh_scheduled: bool,
    /// How far into each buffer's edit log the history has been moved.
    edits_seen: HashMap<BufferId, u64>,
    /// A key that pages or pastes was just pressed in the editor: the caret
    /// move it makes is not a jump.
    quiet_move: bool,
    _keystrokes: Option<gpui::Subscription>,
}

/// Whether an action moves the caret a long way without jumping anywhere:
/// paging through the file, or pasting, undoing and redoing a large edit.
fn is_quiet_move(action: &dyn gpui::Action) -> bool {
    use gpui_component::input::{MovePageDown, MovePageUp, Paste, Redo, Undo};
    let any = action.as_any();
    any.is::<MovePageUp>()
        || any.is::<MovePageDown>()
        || any.is::<Paste>()
        || any.is::<Undo>()
        || any.is::<Redo>()
}

impl EditorNav {
    pub(crate) fn new(cx: &mut Context<Tty7App>) -> Self {
        let keystrokes = cx.observe_keystrokes(|this, event, window, cx| {
            if event.action.as_deref().is_some_and(is_quiet_move)
                && this.editor_panel_has_focus(window, cx)
            {
                this.editor.nav.quiet_move = true;
            }
        });
        Self {
            _keystrokes: Some(keystrokes),
            ..Self::default()
        }
    }

    pub(crate) fn forget_tab(&mut self, tab: TabId) {
        self.tabs.remove(&tab);
    }

    pub(crate) fn forget_buffer(&mut self, id: BufferId) {
        self.outlines.remove(&id);
        self.edits_seen.remove(&id);
        self.provided.remove(&id);
        for nav in self.tabs.values_mut() {
            nav.history.retain(|l| l.buffer != Some(id));
        }
    }
}

impl Tty7App {
    fn nav_tab(&self) -> Option<TabId> {
        self.tabs.get(self.active).map(|t| t.tree_id.get())
    }

    /// Where the caret is in the buffer in front, if there is one.
    fn nav_here(&self, cx: &gpui::App) -> Option<Sample> {
        let f = self.active_buffer()?;
        let state = f.input.read(cx);
        Some(Sample {
            at: NavLocation {
                host: f.host.id(),
                path: f.path.clone(),
                buffer: f.untitled.map(|_| f.id()),
                pos: state.cursor_position(),
            },
            text_len: state.text().len(),
        })
    }

    /// Samples the caret for jumps. Called whenever the editor draws, which
    /// every caret move and every edit makes it do.
    pub(crate) fn editor_nav_tick(&mut self, cx: &mut Context<Self>) {
        // The picker moving the caret around is looking, not jumping; the
        // jump it ends in is seen on the first draw after it closes.
        if self.editor.nav.preview.is_some() {
            return;
        }
        let Some(tab) = self.nav_tab() else { return };
        let now = self.nav_here(cx);
        let quiet = std::mem::take(&mut self.editor.nav.quiet_move);
        self.editor
            .nav
            .tabs
            .entry(tab)
            .or_default()
            .observe(now, Instant::now(), quiet);
    }

    /// A buffer's text changed: every tab's history of it moves with the
    /// lines. A buffer edited so much the log has forgotten some of it keeps
    /// its places where they were.
    pub(crate) fn editor_nav_note_edit(&mut self, id: BufferId, cx: &mut Context<Self>) {
        let Some(f) = self.buffer(id) else { return };
        let file = NavLocation {
            host: f.host.id(),
            path: f.path.clone(),
            buffer: f.untitled.map(|_| id),
            pos: Position::default(),
        };
        let state = f.input.read(cx);
        let version = state.edit_version();
        let seen = self.editor.nav.edits_seen.insert(id, version).unwrap_or(0);
        let Some(edits) = state.line_edits_since(seen) else {
            return;
        };
        if edits.is_empty() {
            return;
        }
        for nav in self.editor.nav.tabs.values_mut() {
            nav.shift(&file, &edits);
        }
    }

    /// Back (or, with `forward`, Forward) through the active tab's history.
    pub(crate) fn editor_navigate(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.nav_tab() else { return };
        // Settle the caret's latest move first, or a jump made since the
        // last draw would be lost.
        self.editor_nav_tick(cx);
        loop {
            let current = self.nav_here(cx).map(|s| s.at);
            let nav = self.editor.nav.tabs.entry(tab).or_default();
            let target = match forward {
                true => nav.history.go_forward(current),
                false => nav.history.go_back(current),
            };
            let Some(target) = target else { return };
            if self.nav_open(tab, target, window, cx) {
                return;
            }
        }
    }

    /// Shows `target`. `false` when it is gone for good — an untitled buffer
    /// that was closed, a machine no longer connected.
    fn nav_open(
        &mut self,
        tab: TabId,
        target: NavLocation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let at = Instant::now();
        if let Some(id) = target.buffer {
            if self.buffer(id).is_none() {
                return false;
            }
            self.editor
                .nav
                .tabs
                .entry(tab)
                .or_default()
                .expect(target.clone(), at);
            self.editor_show_in_tab(tab, id, true, window, cx);
            if let Some(f) = self.buffer(id) {
                place_cursor(
                    f.input.clone(),
                    target.pos,
                    CURSOR_SCROLL_ATTEMPTS,
                    window,
                    cx,
                );
            }
            return true;
        }
        let Some(host) = crate::ui::host_registry::HostRegistry::lookup(cx, target.host) else {
            return false;
        };
        self.editor
            .nav
            .tabs
            .entry(tab)
            .or_default()
            .expect(target.clone(), at);
        // The same road a file link takes, so a file closed since opens again.
        self.editor.pending_cursor = Some((
            target.path.clone(),
            target.pos.line + 1,
            target.pos.character + 1,
        ));
        self.editor_open_on_host(host, &target.path, window, cx);
        true
    }

    // ---- Outline ----

    /// Replaces the tree's symbols for `buffer` with ones found elsewhere — a
    /// language server's `textDocument/documentSymbol`, flattened with
    /// [`Outline::from_nodes`]. `None` goes back to the tree's. The caller
    /// keeps them current: they are shown as given until replaced.
    pub(crate) fn editor_set_document_symbols(
        &mut self,
        buffer: BufferId,
        symbols: Option<Outline>,
        cx: &mut Context<Self>,
    ) {
        match symbols {
            Some(outline) => self.editor.nav.provided.insert(buffer, Arc::new(outline)),
            None => self.editor.nav.provided.remove(&buffer),
        };
        cx.notify();
    }

    /// A buffer's outline. `fresh` rebuilds a stale one now; otherwise a text
    /// that keeps changing is re-outlined at most every [`OUTLINE_THROTTLE`],
    /// with a redraw scheduled for when it may be.
    fn editor_outline(
        &mut self,
        id: BufferId,
        fresh: bool,
        cx: &mut Context<Self>,
    ) -> Option<Arc<Outline>> {
        if let Some(provided) = self.editor.nav.provided.get(&id) {
            return Some(provided.clone());
        }
        let f = self.buffer(id)?;
        let (language, tree, text) = match f.input.read(cx).syntax_tree() {
            Some((language, tree, text)) => (language.to_string(), Some(tree), text),
            // Not parsed yet — a buffer that has never been drawn. Only worth
            // a parse of our own when someone is waiting on the answer.
            None if fresh => (
                f.language().to_string(),
                None,
                f.input.read(cx).text().clone(),
            ),
            None => return None,
        };
        if !outline::supports(&language) {
            return None;
        }
        if let Some(cache) = self.editor.nav.outlines.get(&id) {
            let current = cache.text.len() == text.len() && cache.text == text;
            if current {
                return Some(cache.outline.clone());
            }
            if !fresh && cache.at.elapsed() < OUTLINE_THROTTLE {
                let stale = cache.outline.clone();
                self.nav_schedule_refresh(cx);
                return Some(stale);
            }
        }
        let outline = Arc::new(match tree {
            Some(tree) => outline::outline_from_tree(&language, &tree, &text),
            None => outline::outline_of_text(&language, &text),
        });
        self.editor.nav.outlines.insert(
            id,
            OutlineCache {
                text,
                outline: outline.clone(),
                at: Instant::now(),
            },
        );
        Some(outline)
    }

    fn nav_schedule_refresh(&mut self, cx: &mut Context<Self>) {
        if self.editor.nav.refresh_scheduled {
            return;
        }
        self.editor.nav.refresh_scheduled = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(OUTLINE_THROTTLE).await;
            let _ = this.update(cx, |this, cx| {
                this.editor.nav.refresh_scheduled = false;
                cx.notify();
            });
        })
        .detach();
    }

    // ---- Go to Symbol ----

    /// Go to Symbol in the file in front: the search, on its Symbols tab.
    /// The same chord puts it away again.
    pub(crate) fn editor_go_to_symbol(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.search.clone() {
            if view.read(cx).tab() == SearchTab::Symbols {
                self.close_search(window, cx);
                return;
            }
            self.close_search(window, cx);
        }
        let Some(f) = self.active_buffer() else {
            return;
        };
        let id = f.id();
        let input = f.input.clone();
        // A symbol is a place in the source; a rendered Markdown file shows
        // the source to be taken to it.
        if let Some(f) = self.buffer_mut(id) {
            f.preview = false;
        }
        let (origin, scroll) = {
            let state = input.read(cx);
            (state.cursor_position(), state.scroll_offset())
        };
        let outline = self.editor_outline(id, true, cx).unwrap_or_default();
        let rows = symbol_rows(&outline);
        let here = outline
            .chain_at(origin)
            .last()
            .map(|&ix| symbol_command(&outline.symbols[ix]));
        self.open_search(SearchTab::Symbols, "", window, cx);
        self.editor.nav.preview = Some(SymbolPreview {
            buffer: id,
            origin,
            scroll,
        });
        let live = self.lsp_project_symbol_query(window, cx);
        if let Some(view) = self.search.clone() {
            view.update(cx, |view, cx| {
                view.set_symbols(rows, here, window, cx);
                if let Some(live) = live {
                    view.set_live_query(live, window, cx);
                }
            });
        }
    }

    /// The editor's search row as this window can fill it: Files always,
    /// Symbols with a file in front.
    pub(crate) fn editor_search_tabs(&self) -> Vec<SearchTab> {
        let mut tabs = vec![SearchTab::Files];
        if self.active_buffer().is_some() {
            tabs.push(SearchTab::Symbols);
        }
        tabs
    }

    /// Shows a symbol the picker is on, leaving the keyboard in the picker.
    pub(crate) fn editor_symbol_preview(&mut self, line: u32, column: u32, cx: &mut Context<Self>) {
        let Some(buffer) = self.editor.nav.preview.as_ref().map(|p| p.buffer) else {
            return;
        };
        let Some(f) = self.buffer(buffer) else { return };
        f.input.update(cx, |state, cx| {
            state.preview_cursor_position(Position::new(line, column), cx)
        });
    }

    /// Begins a preview in the file in front for a picker someone else
    /// filled — Find References (`ui::lsp`) — so arrowing through it moves
    /// the caret and closing it puts the caret back, as Go to Symbol does.
    pub(crate) fn editor_begin_preview(&mut self, cx: &App) {
        let Some(f) = self.active_buffer() else {
            return;
        };
        let (origin, scroll) = {
            let state = f.input.read(cx);
            (state.cursor_position(), state.scroll_offset())
        };
        self.editor.nav.preview = Some(SymbolPreview {
            buffer: f.id(),
            origin,
            scroll,
        });
    }

    /// The buffer a preview is running in, if one is.
    pub(crate) fn editor_preview_buffer(&self) -> Option<BufferId> {
        self.editor.nav.preview.as_ref().map(|p| p.buffer)
    }

    /// The picker's choice is final: the caret stays where it took it.
    pub(crate) fn editor_symbol_preview_commit(&mut self) {
        self.editor.nav.preview = None;
    }

    /// The picker went away without a choice: the caret and the scroll go
    /// back to where they were, and so does the keyboard.
    pub(crate) fn editor_symbol_preview_cancel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(preview) = self.editor.nav.preview.take() else {
            return;
        };
        let Some(f) = self.buffer(preview.buffer) else {
            return;
        };
        f.input.update(cx, |state, cx| {
            state.set_cursor_position(preview.origin, window, cx);
            state.set_scroll_offset(preview.scroll, cx);
        });
    }

    /// Puts the caret on a 0-based position in the buffer in front.
    pub(crate) fn editor_go_to_position(
        &mut self,
        line: u32,
        column: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.nav.preview = None;
        let Some(f) = self.active_buffer() else {
            return;
        };
        place_cursor(
            f.input.clone(),
            Position::new(line, column),
            CURSOR_SCROLL_ATTEMPTS,
            window,
            cx,
        );
    }

    // ---- Chrome ----

    /// The palette's editor rows, offered while a file is in front of it.
    pub(crate) fn editor_palette_items(&self) -> Vec<Item> {
        use crate::ui::search::CommandGroup;
        if !self.code_panel_visible() || self.active_buffer().is_none() {
            return Vec::new();
        }
        [
            (L10nKey::CmdEditorGoToSymbol, CommandKind::EditorGoToSymbol),
            (L10nKey::CmdEditorGoBack, CommandKind::EditorNavigateBack),
            (
                L10nKey::CmdEditorGoForward,
                CommandKind::EditorNavigateForward,
            ),
            (L10nKey::CmdEditorSplitRight, CommandKind::EditorSplitRight),
        ]
        .into_iter()
        .map(|(key, kind)| Item::localized(key, kind).in_group(CommandGroup::View))
        .collect()
    }

    /// The editor's own chords, handled by the element that holds it.
    pub(crate) fn editor_nav_actions<E: InteractiveElement>(
        &self,
        element: E,
        cx: &mut Context<Self>,
    ) -> E {
        use crate::core::actions::{
            EditorFocusLeftGroup, EditorFocusRightGroup, EditorGoToSymbol, EditorNavigateBack,
            EditorNavigateForward, EditorSplitRight,
        };
        element
            .on_action(cx.listener(|this, _: &EditorSplitRight, window, cx| {
                if !this.editor_split(window, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &EditorFocusLeftGroup, window, cx| {
                if !this.editor_focus_group(true, window, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &EditorFocusRightGroup, window, cx| {
                if !this.editor_focus_group(false, window, cx) {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &EditorGoToSymbol, window, cx| {
                this.editor_go_to_symbol(window, cx)
            }))
            .on_action(cx.listener(|this, _: &EditorNavigateBack, window, cx| {
                this.editor_navigate(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &EditorNavigateForward, window, cx| {
                this.editor_navigate(true, window, cx)
            }))
    }

    /// The row over the text: where the file is, then which symbols the caret
    /// is inside. The symbols open Go to Symbol.
    pub(crate) fn render_editor_breadcrumbs(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let id = self.tab_code().and_then(TabCode::active_id)?;
        let slot = self.tab_code().map_or(0, TabCode::focused_slot);
        self.render_editor_breadcrumbs_for(id, slot, true, window, cx)
    }

    /// The breadcrumbs of one group's file. `focused` is whether that group
    /// has the focus; the other group's symbols first bring the focus over.
    pub(crate) fn render_editor_breadcrumbs_for(
        &mut self,
        id: BufferId,
        slot: usize,
        focused: bool,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let f = self.buffer(id)?;
        let caret = f.input.read(cx).cursor_position();
        let segments: Vec<String> = if f.untitled.is_some() {
            vec![f.label().to_string()]
        } else {
            let roots = self.tab_code().map(|c| c.roots.as_slice()).unwrap_or(&[]);
            let local = f.host.id() == self.spawn_host(cx);
            path_segments(&f.path, if local { roots } else { &[] })
        };
        let file = (f.untitled.is_none()).then(|| f.path.clone());
        // A rendered Markdown file keeps the path; its symbols are the source's.
        let supported = !f.preview && f.untitled.is_none() && outline::supports(f.language());
        let chain: Vec<(SymbolKind, String)> = match supported {
            true => self
                .editor_outline(id, false, cx)
                .map(|o| {
                    o.chain_at(caret)
                        .into_iter()
                        .map(|ix| (o.symbols[ix].kind, o.symbols[ix].name.clone()))
                        .collect()
                })
                .unwrap_or_default(),
            false => Vec::new(),
        };

        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        // A chevron a rung fainter than the names, so the path reads as its
        // names and the separators recede.
        let sep = || {
            div().flex_none().px(px(4.)).child(
                Icon::new(IconName::ChevronRight)
                    .size(px(9.))
                    .text_color(muted.opacity(0.6)),
            )
        };
        let last = segments.len().saturating_sub(1);
        let mut path = h_flex()
            .flex_shrink(1.)
            .min_w_0()
            .overflow_hidden()
            .items_center();
        for (i, seg) in segments.into_iter().enumerate() {
            if i > 0 {
                path = path.child(sep());
            }
            path = path.child(
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .text_color(if i == last { fg } else { muted })
                    .child(seg),
            );
        }
        let path = div()
            .id(("editor-breadcrumb-path", slot))
            .flex_shrink(1.)
            .min_w_0()
            .overflow_hidden()
            .cursor_pointer()
            .child(path)
            .on_click(cx.listener(move |this, _, _window, cx| {
                if let Some(file) = &file {
                    this.file_tree_reveal_path(file, cx);
                }
            }));

        let symbols = supported.then(|| {
            let mut row = h_flex().min_w_0().overflow_hidden().items_center();
            if chain.is_empty() {
                row = row.child(sep()).child(div().text_color(muted).child("…"));
            }
            for (kind, name) in chain {
                row = row.child(sep()).child(
                    h_flex()
                        .flex_none()
                        .gap(px(4.))
                        .whitespace_nowrap()
                        // An impl is named by its header, `impl` and all.
                        .when(kind != SymbolKind::Impl, |d| {
                            d.child(div().text_color(muted).child(kind.tag()))
                        })
                        // The symbol is code, so it is set as code, a rung
                        // darker than the path it hangs off.
                        .child(
                            div()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_color(fg.opacity(0.6))
                                .child(name),
                        ),
                );
            }
            div()
                .id(("editor-breadcrumb-symbols", slot))
                .flex_shrink(1.)
                .min_w_0()
                .overflow_hidden()
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|s| s.bg(cx.theme().muted))
                .child(row)
                .tooltip(|window, cx| {
                    gpui_component::tooltip::Tooltip::new(t(L10nKey::EditorGoToSymbolAction))
                        .build(window, cx)
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    if !focused {
                        let left = slot == 0;
                        this.editor_focus_group(left, window, cx);
                    }
                    this.editor_go_to_symbol(window, cx)
                }))
        });

        Some(
            h_flex()
                .id(("editor-breadcrumbs", slot))
                .flex_none()
                .w_full()
                .h(px(22.))
                .items_center()
                .overflow_hidden()
                .px(px(crate::ui::app::CONTENT_INSET))
                .text_size(gpui::rems(crate::ui::right_panel::META))
                .child(path)
                .children(symbols)
                .into_any_element(),
        )
    }
}

/// A path as the breadcrumbs spell it: from inside the project root it is in,
/// with the root's own name first, or whole when it is in none.
pub(crate) fn path_segments(path: &Path, roots: &[PathBuf]) -> Vec<String> {
    let (lead, rel) = match roots
        .iter()
        .find_map(|r| Some((r, path.strip_prefix(r).ok()?)))
    {
        Some((root, rel)) => (
            root.file_name().map(|n| n.to_string_lossy().to_string()),
            rel.to_path_buf(),
        ),
        None => (None, path.to_path_buf()),
    };
    lead.into_iter()
        .chain(rel.components().filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            std::path::Component::RootDir | std::path::Component::Prefix(_) => None,
            _ => None,
        }))
        .collect()
}

fn symbol_command(symbol: &outline::Symbol) -> CommandKind {
    CommandKind::GoToSymbol {
        line: symbol.selection.line,
        column: symbol.selection.character,
    }
}

/// The picker's rows, in document order, each with how deep it sits.
pub(crate) fn symbol_rows(outline: &Outline) -> Vec<(usize, Item)> {
    outline
        .symbols
        .iter()
        .enumerate()
        .map(|(ix, s)| {
            let mut item = Item::new(s.name.clone(), symbol_command(s)).with_note(s.kind.tag());
            let path = outline.container_path(ix);
            if !path.is_empty() {
                item = item.with_subtitle(path.join(" › "));
            }
            (s.depth, item)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(path: &str, line: u32) -> NavLocation {
        NavLocation {
            host: HostId::LOCAL,
            path: PathBuf::from(path),
            buffer: None,
            pos: Position::new(line, 0),
        }
    }

    fn sample(path: &str, line: u32, len: usize) -> Option<Sample> {
        Some(Sample {
            at: at(path, line),
            text_len: len,
        })
    }

    #[test]
    fn back_and_forward_walk_the_places_jumped_from() {
        let mut h = NavHistory::default();
        h.push(at("a.rs", 10));
        h.push(at("b.rs", 40));
        // Now at c.rs:5.
        assert_eq!(h.go_back(Some(at("c.rs", 5))), Some(at("b.rs", 40)));
        assert_eq!(h.go_back(Some(at("b.rs", 40))), Some(at("a.rs", 10)));
        assert_eq!(h.go_back(Some(at("a.rs", 10))), None);
        assert_eq!(h.go_forward(Some(at("a.rs", 10))), Some(at("b.rs", 40)));
        assert_eq!(h.go_forward(Some(at("b.rs", 40))), Some(at("c.rs", 5)));
        assert_eq!(h.go_forward(Some(at("c.rs", 5))), None);
    }

    #[test]
    fn a_new_jump_forgets_the_way_forward() {
        let mut h = NavHistory::default();
        h.push(at("a.rs", 10));
        h.push(at("b.rs", 40));
        h.go_back(Some(at("c.rs", 5)));
        assert_eq!(h.lens(), (1, 1));
        h.push(at("b.rs", 40));
        assert_eq!(h.lens(), (2, 0));
        assert_eq!(h.go_forward(Some(at("d.rs", 0))), None);
    }

    #[test]
    fn places_near_each_other_are_one_entry() {
        let mut h = NavHistory::default();
        h.push(at("a.rs", 10));
        h.push(at("a.rs", 14));
        assert_eq!(h.lens(), (1, 0));
        h.push(at("a.rs", 30));
        h.push(at("b.rs", 14));
        assert_eq!(h.lens(), (3, 0));
        // Back from right beside the last entry passes over it.
        assert_eq!(h.go_back(Some(at("b.rs", 16))), Some(at("a.rs", 30)));
    }

    #[test]
    fn the_back_stack_is_capped() {
        let mut h = NavHistory::default();
        for i in 0..(MAX_HISTORY as u32 + 5) {
            h.push(at("a.rs", i * JUMP_LINES));
        }
        assert_eq!(h.lens().0, MAX_HISTORY);
        let mut oldest = None;
        let mut cur = at("z.rs", 0);
        while let Some(l) = h.go_back(Some(cur.clone())) {
            cur = l.clone();
            oldest = Some(l);
        }
        assert_eq!(oldest, Some(at("a.rs", 5 * JUMP_LINES)));
    }

    #[test]
    fn closed_untitled_buffers_can_be_dropped() {
        let mut h = NavHistory::default();
        let mut untitled = at("", 3);
        untitled.buffer = Some(gpui::EntityId::from(7u64));
        h.push(untitled);
        h.push(at("a.rs", 0));
        h.retain(|l| l.buffer.is_none());
        assert_eq!(h.lens(), (1, 0));
    }

    #[test]
    fn the_tracker_records_jumps_but_not_walks_or_edits() {
        let t0 = Instant::now();
        let mut nav = TabNav::default();
        nav.observe(sample("a.rs", 0, 100), t0, false);
        // Arrowing down a line at a time is not a jump.
        for line in 1..=30 {
            nav.observe(sample("a.rs", line, 100), t0, false);
        }
        assert_eq!(nav.history.lens(), (0, 0));
        // Pasting forty lines moves the caret forty lines, and is an edit.
        nav.observe(sample("a.rs", 70, 900), t0, false);
        assert_eq!(nav.history.lens(), (0, 0));
        // A click far away is.
        nav.observe(sample("a.rs", 300, 900), t0, false);
        assert_eq!(nav.history.back, [at("a.rs", 70)]);
        // So is another file coming forward.
        nav.observe(sample("b.rs", 0, 10), t0, false);
        assert_eq!(nav.history.back, [at("a.rs", 70), at("a.rs", 300)]);
        // No buffer in front keeps the last place for the next one.
        nav.observe(None, t0, false);
        nav.observe(sample("c.rs", 0, 10), t0, false);
        assert_eq!(nav.history.back.last(), Some(&at("b.rs", 0)));
    }

    #[test]
    fn a_pending_back_is_not_mistaken_for_a_jump() {
        let t0 = Instant::now();
        let mut nav = TabNav::default();
        nav.observe(sample("a.rs", 0, 10), t0, false);
        nav.observe(sample("b.rs", 50, 10), t0, false);
        let target = nav.history.go_back(Some(at("b.rs", 50))).unwrap();
        nav.expect(target, t0);
        // The file is still opening: b.rs is on screen, and that is fine.
        nav.observe(sample("b.rs", 50, 10), t0, false);
        nav.observe(sample("a.rs", 0, 10), t0, false);
        assert_eq!(nav.history.lens(), (0, 1));
        // A target that never arrives stops being waited on.
        nav.expect(at("gone.rs", 0), t0);
        nav.observe(
            sample("a.rs", 0, 10),
            t0 + PENDING_TIMEOUT + Duration::from_secs(1),
            false,
        );
        nav.observe(
            sample("b.rs", 0, 10),
            t0 + PENDING_TIMEOUT + Duration::from_secs(1),
            false,
        );
        assert_eq!(nav.history.back, [at("a.rs", 0)]);
    }

    #[test]
    fn paging_pasting_and_undoing_are_the_quiet_moves() {
        use gpui_component::input::{MoveDown, MovePageDown, MovePageUp, Paste, Redo, Undo};
        assert!(is_quiet_move(&MovePageDown));
        assert!(is_quiet_move(&MovePageUp));
        assert!(is_quiet_move(&Paste));
        assert!(is_quiet_move(&Undo));
        assert!(is_quiet_move(&Redo));
        assert!(!is_quiet_move(&MoveDown));
    }

    #[test]
    fn a_page_down_or_a_paste_is_not_a_jump() {
        let t0 = Instant::now();
        let mut nav = TabNav::default();
        nav.observe(sample("a.rs", 0, 100), t0, false);
        nav.observe(sample("a.rs", 60, 100), t0, true);
        nav.observe(sample("a.rs", 120, 100), t0, true);
        assert_eq!(nav.history.lens(), (0, 0));
        // Only the move the key made: the next one is judged on its own.
        nav.observe(sample("a.rs", 400, 100), t0, false);
        assert_eq!(nav.history.back, [at("a.rs", 120)]);
    }

    #[test]
    fn places_move_with_lines_inserted_or_deleted_above_them() {
        let mut nav = TabNav::default();
        let t0 = Instant::now();
        nav.observe(sample("a.rs", 40, 10), t0, false);
        nav.observe(sample("b.rs", 0, 10), t0, false);
        nav.history.push(at("a.rs", 90));
        // Three lines pasted at line 10 of a.rs, then lines 20..=29 deleted.
        let edits = [
            LineEdit {
                start_line: 10,
                end_line: 10,
                new_lines: 3,
                at_line_start: false,
            },
            LineEdit {
                start_line: 20,
                end_line: 30,
                new_lines: 0,
                at_line_start: false,
            },
        ];
        nav.shift(&at("a.rs", 0), &edits);
        // b.rs is untouched; 40 → 43 → 33, 90 → 93 → 83.
        assert_eq!(nav.history.back, [at("a.rs", 33), at("a.rs", 83)]);
        nav.shift(&at("b.rs", 0), &edits[..1]);
        assert_eq!(nav.last.as_ref().unwrap().at, at("b.rs", 0));
    }

    #[test]
    fn breadcrumb_paths_start_at_their_project() {
        let roots = [PathBuf::from("/work/tty7")];
        assert_eq!(
            path_segments(Path::new("/work/tty7/src/ui/code_editor.rs"), &roots),
            ["tty7", "src", "ui", "code_editor.rs"]
        );
        assert_eq!(
            path_segments(Path::new("/etc/hosts"), &roots),
            ["etc", "hosts"]
        );
    }

    #[test]
    fn symbol_rows_carry_their_containers_and_the_breadcrumb_chain_names_them() {
        let src = "impl Tty7App {\n    fn open_file(&self) {\n        let x = 1;\n    }\n}\n";
        let outline = outline::outline_of("rust", src);
        let rows = symbol_rows(&outline);
        let shown: Vec<_> = rows
            .iter()
            .map(|(d, i)| {
                (
                    *d,
                    i.title.as_str(),
                    i.subtitle.as_deref(),
                    i.note.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                (0, "impl Tty7App", None, Some("impl")),
                (1, "open_file", Some("impl Tty7App"), Some("method")),
            ]
        );
        assert_eq!(
            rows[1].1.kind,
            CommandKind::GoToSymbol { line: 1, column: 7 }
        );
        let chain: Vec<_> = outline
            .chain_at(Position::new(2, 8))
            .into_iter()
            .map(|i| {
                format!(
                    "{} {}",
                    outline.symbols[i].kind.tag(),
                    outline.symbols[i].name
                )
            })
            .collect();
        assert_eq!(chain, ["impl impl Tty7App", "method open_file"]);
    }
}

#[cfg(test)]
pub(super) mod gpui_tests {
    use super::*;
    use crate::ui::app::test_window::harness_with_tabs;
    use crate::ui::editor_text::{EditorConfig, TextFormat};
    use gpui::{Entity, TestAppContext, VisualTestContext};

    /// `alpha` on line 0, `beta` on 30, `gamma` on 50.
    pub(crate) fn source() -> String {
        let mut lines = vec!["fn alpha() {".to_string()];
        lines.extend((1..=20).map(|_| "    let x = 1;".to_string()));
        lines.push("}".into());
        lines.extend((22..30).map(|_| String::new()));
        lines.push("fn beta() {".into());
        lines.push("}".into());
        lines.extend((32..50).map(|_| String::new()));
        lines.push("fn gamma() {".into());
        lines.push("}".into());
        lines.join("\n") + "\n"
    }

    pub(crate) fn open_rs(
        app: &Entity<Tty7App>,
        vcx: &mut VisualTestContext,
        path: &str,
    ) -> BufferId {
        let path = PathBuf::from(path);
        app.update_in(vcx, |app, window, cx| {
            let host = app.active_host(cx).expect("a host");
            let tab = app.nav_tab().expect("a tab");
            let indent = crate::ui::editor_text::detect_indent("", "rust");
            let id = app.editor_new_buffer(
                host,
                path,
                None,
                source(),
                TextFormat::default(),
                EditorConfig::default(),
                indent,
                None,
                window,
                cx,
            );
            app.editor_show_in_tab(tab, id, true, window, cx);
            id
        })
    }

    pub(crate) fn caret(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> Position {
        app.read_with(vcx, |app, cx| {
            app.active_buffer()
                .unwrap()
                .input
                .read(cx)
                .cursor_position()
        })
    }

    /// What drawing the editor does: sample the caret.
    pub(crate) fn draw(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) {
        app.update(vcx, |app, cx| app.editor_nav_tick(cx));
    }

    pub(crate) fn put_caret(app: &Entity<Tty7App>, vcx: &mut VisualTestContext, line: u32) {
        app.update_in(vcx, |app, window, cx| {
            let input = app.active_buffer().unwrap().input.clone();
            input.update(cx, |s, cx| {
                s.set_cursor_position(Position::new(line, 4), window, cx)
            });
        });
        draw(app, vcx);
    }

    #[gpui::test]
    fn go_to_symbol_previews_and_escape_puts_the_caret_back(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        open_rs(&app, &mut vcx, "/nav-test/a.rs");
        put_caret(&app, &mut vcx, 5);

        app.update_in(&mut vcx, |app, window, cx| {
            app.editor_go_to_symbol(window, cx)
        });
        vcx.run_until_parked();
        let symbols = app.read_with(&vcx, |app, cx| {
            let view = app.search.clone().expect("the search is open");
            assert_eq!(view.read(cx).tab(), SearchTab::Symbols);
            view.read(cx).symbol_count()
        });
        assert_eq!(symbols, 3);
        // Opening on the symbol around the caret does not move it.
        assert_eq!(caret(&app, &mut vcx), Position::new(5, 4));

        vcx.simulate_keystrokes("down");
        vcx.run_until_parked();
        assert_eq!(
            caret(&app, &mut vcx),
            Position::new(30, 3),
            "beta is previewed"
        );

        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(app.read_with(&vcx, |app, _| app.search.is_none()));
        assert_eq!(caret(&app, &mut vcx), Position::new(5, 4), "and put back");
        draw(&app, &mut vcx);
        app.read_with(&vcx, |app, _| {
            let tab = app.nav_tab().unwrap();
            assert!(
                app.editor.nav.tabs[&tab].history.back.is_empty(),
                "looking is not jumping"
            );
        });
    }

    #[gpui::test]
    fn a_symbol_jump_can_be_walked_back_and_forward(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        open_rs(&app, &mut vcx, "/nav-test/a.rs");
        put_caret(&app, &mut vcx, 5);

        app.update_in(&mut vcx, |app, window, cx| {
            app.editor_go_to_symbol(window, cx)
        });
        vcx.run_until_parked();
        vcx.simulate_keystrokes("g a m m a");
        vcx.run_until_parked();
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        assert!(app.read_with(&vcx, |app, _| app.search.is_none()));
        assert_eq!(caret(&app, &mut vcx), Position::new(50, 3));
        draw(&app, &mut vcx);

        app.update_in(&mut vcx, |app, window, cx| {
            app.editor_navigate(false, window, cx)
        });
        vcx.run_until_parked();
        assert_eq!(
            caret(&app, &mut vcx),
            Position::new(5, 4),
            "back to where it was"
        );
        draw(&app, &mut vcx);

        app.update_in(&mut vcx, |app, window, cx| {
            app.editor_navigate(true, window, cx)
        });
        vcx.run_until_parked();
        assert_eq!(
            caret(&app, &mut vcx),
            Position::new(50, 3),
            "and forward again"
        );
        draw(&app, &mut vcx);

        // Another file coming forward is a jump too, and Back returns to the
        // first one.
        open_rs(&app, &mut vcx, "/nav-test/b.rs");
        draw(&app, &mut vcx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.editor_navigate(false, window, cx)
        });
        vcx.run_until_parked();
        app.read_with(&vcx, |app, _| {
            assert_eq!(
                app.active_buffer().unwrap().path,
                PathBuf::from("/nav-test/a.rs")
            );
        });
        assert_eq!(caret(&app, &mut vcx), Position::new(50, 3));
    }

    #[gpui::test]
    fn history_follows_edits_to_the_file(cx: &mut TestAppContext) {
        use gpui::EntityInputHandler as _;
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        open_rs(&app, &mut vcx, "/nav-test/a.rs");
        put_caret(&app, &mut vcx, 5);
        put_caret(&app, &mut vcx, 50);
        // Three lines typed at the top of the file.
        app.update_in(&mut vcx, |app, window, cx| {
            let input = app.active_buffer().unwrap().input.clone();
            input.update(cx, |s, cx| {
                s.replace_text_in_range(Some(0..0), "//\n//\n//\n", window, cx);
                // Typing took the caret to the top; back to where it was
                // reading, which has moved down with its line.
                s.set_cursor_position(Position::new(53, 4), window, cx);
            });
        });
        vcx.run_until_parked();
        draw(&app, &mut vcx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.editor_navigate(false, window, cx)
        });
        vcx.run_until_parked();
        assert_eq!(
            caret(&app, &mut vcx).line,
            8,
            "the place moved down with its code"
        );
    }

    #[gpui::test]
    fn the_symbol_chord_puts_go_to_symbol_away(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        open_rs(&app, &mut vcx, "/nav-test/a.rs");
        put_caret(&app, &mut vcx, 5);
        app.update_in(&mut vcx, |app, window, cx| {
            app.editor_go_to_symbol(window, cx)
        });
        vcx.run_until_parked();
        assert!(app.read_with(&vcx, |app, _| app.search.is_some()));
        vcx.simulate_keystrokes("secondary-shift-o");
        vcx.run_until_parked();
        assert!(
            app.read_with(&vcx, |app, _| app.search.is_none()),
            "the chord that opened it closes it"
        );
        assert_eq!(caret(&app, &mut vcx), Position::new(5, 4));
    }

    /// Go to File and Go to Symbol share the editor's row: Tab walks from one
    /// to the other, each arriving set up the way its chord sets it up.
    #[gpui::test]
    fn tab_walks_the_editors_row(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        // No file in front: Files is all the row would hold.
        assert_eq!(
            app.read_with(&vcx, |app, _| app.editor_search_tabs()),
            vec![SearchTab::Files]
        );
        open_rs(&app, &mut vcx, "/nav-test/a.rs");
        assert_eq!(
            app.read_with(&vcx, |app, _| app.editor_search_tabs()),
            vec![SearchTab::Files, SearchTab::Symbols]
        );
        app.update_in(&mut vcx, |app, window, cx| app.quick_open_file(window, cx));
        vcx.run_until_parked();
        let tab = |app: &Entity<Tty7App>, vcx: &mut gpui::VisualTestContext| {
            app.read_with(vcx, |app, cx| app.search.as_ref().map(|v| v.read(cx).tab()))
        };
        assert_eq!(tab(&app, &mut vcx), Some(SearchTab::Files));
        vcx.simulate_keystrokes("tab");
        vcx.run_until_parked();
        assert_eq!(tab(&app, &mut vcx), Some(SearchTab::Symbols));
        vcx.simulate_keystrokes("tab");
        vcx.run_until_parked();
        assert_eq!(tab(&app, &mut vcx), Some(SearchTab::Files));
    }

    #[gpui::test]
    fn the_palette_offers_the_editor_commands_only_with_a_file_open(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        assert!(app.read_with(&vcx, |app, _| app.editor_palette_items().is_empty()));
        open_rs(&app, &mut vcx, "/nav-test/a.rs");
        let kinds: Vec<CommandKind> = app.read_with(&vcx, |app, _| {
            app.editor_palette_items()
                .into_iter()
                .map(|i| i.kind)
                .collect()
        });
        assert_eq!(
            kinds,
            [
                CommandKind::EditorGoToSymbol,
                CommandKind::EditorNavigateBack,
                CommandKind::EditorNavigateForward,
                CommandKind::EditorSplitRight,
            ]
        );
    }
}

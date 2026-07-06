//! The command palette: a centered overlay (Cmd+P) that lists runnable
//! commands, fuzzy-filters them as you type, and runs the selected one.
//!
//! This module owns the palette's *data* — the command catalog and the
//! `ListDelegate` that filters it. The heavy lifting (search input, virtual
//! list, keyboard navigation, Enter/Esc handling) is supplied by
//! gpui-component's `list::ListState`, so we don't reimplement any of it.
//! [`PaletteView`] wraps that list with the overlay chrome (scrim + card) and
//! emits a [`PaletteEvent`] on confirm/dismiss; command *execution* lives in
//! `app.rs`, where it can touch `Tty7App`'s tab/pane operations.

use gpui::{
    App, Context, Entity, EventEmitter, MouseButton, MouseDownEvent, Subscription, Task, Window,
    div, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, IndexPath, h_flex,
    list::{List, ListDelegate, ListEvent, ListItem, ListState},
    v_flex,
};

use crate::core::config::Config;

/// What a command actually does. Most variants map to an existing `Tty7App`
/// operation dispatched in `app.rs` (so it can touch tabs/panes); the two
/// exceptions are handled entirely inside [`PaletteView`]: `OpenThemePicker`
/// swaps the palette to the theme sub-list and never reaches the host, and
/// `SetTheme` is emitted from that sub-list.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    NewTab,
    SplitRight,
    SplitDown,
    ClosePane,
    ResetFontSize,
    NextPane,
    PrevPane,
    ToggleMaximizePane,
    FindInTerminal,
    ReopenClosedTab,
    OpenSettings,
    RestartDaemon,
    /// Opens the theme sub-list (a nested palette). Handled in `PaletteView`.
    OpenThemePicker,
    /// Apply the preset at this index in `presets::all()`. Emitted from the
    /// theme sub-list.
    SetTheme(usize),
    /// Switch to the tab at this index in `Tty7App::tabs`.
    ActivateTab(usize),
}

impl CommandKind {
    /// The action whose keystroke should be shown beside this command in the
    /// palette, if any. Commands with no global binding (Change Theme and its
    /// sub-entries, Find, tab switching) return `None` and render without a hint.
    fn binding_action(self) -> Option<&'static str> {
        use CommandKind::*;
        Some(match self {
            NewTab => "NewTab",
            SplitRight => "SplitRight",
            SplitDown => "SplitDown",
            ClosePane => "CloseActiveTab",
            ResetFontSize => "ResetFontSize",
            NextPane => "FocusNextPane",
            PrevPane => "FocusPrevPane",
            ToggleMaximizePane => "ToggleMaximizePane",
            ReopenClosedTab => "ReopenClosedTab",
            OpenSettings => "OpenSettings",
            RestartDaemon => "RestartDaemon",
            FindInTerminal | OpenThemePicker | SetTheme(_) | ActivateTab(_) => return None,
        })
    }
}

/// A single palette entry: a label plus the action it triggers.
#[derive(Clone)]
pub struct Command {
    pub title: String,
    pub kind: CommandKind,
}

impl Command {
    fn new(title: impl Into<String>, kind: CommandKind) -> Self {
        Self {
            title: title.into(),
            kind,
        }
    }

    /// The static commands available regardless of how many tabs exist. The
    /// caller appends the dynamic "Switch to Tab: …" entries (one per tab).
    ///
    /// Trailing "…" flags a command that opens further UI rather than acting
    /// immediately (a sub-list for Change Theme, a search bar for Find). The
    /// held-key font zoom (⌘+/⌘−) is deliberately absent — stepping it needs a
    /// re-open per press, so it makes a poor palette citizen; only the one-shot
    /// Reset is worth a slot.
    pub fn base_commands() -> Vec<Command> {
        use CommandKind::*;
        vec![
            Command::new("New Tab", NewTab),
            Command::new("Split Right", SplitRight),
            Command::new("Split Down", SplitDown),
            Command::new("Close Pane/Tab", ClosePane),
            Command::new("Next Pane", NextPane),
            Command::new("Previous Pane", PrevPane),
            Command::new("Toggle Maximize Pane", ToggleMaximizePane),
            Command::new("Find in Terminal…", FindInTerminal),
            Command::new("Reopen Closed Tab", ReopenClosedTab),
            Command::new("Change Theme…", OpenThemePicker),
            Command::new("Open Settings", OpenSettings),
            Command::new("Reset Font Size", ResetFontSize),
            Command::new("Restart Background Service…", RestartDaemon),
        ]
    }

    /// The theme-picker sub-list: one entry per built-in preset, in the presets'
    /// display order. Confirming one emits `SetTheme(i)`, which applies that
    /// preset. The active theme is marked with a check so the list doubles as a
    /// "which theme am I on?" indicator.
    pub fn theme_commands(cx: &App) -> Vec<Command> {
        let active = cx.global::<Config>().theme_preset.as_str();
        crate::ui::presets::all()
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let title = if p.id == active {
                    format!("{}  ✓", p.name)
                } else {
                    p.name.to_string()
                };
                Command::new(title, CommandKind::SetTheme(i))
            })
            .collect()
    }
}

/// Case-insensitive subsequence match: every character of `query` must appear
/// in `title`, in order (but not necessarily contiguously). An empty query
/// matches everything. This is the simple "fuzzy" rule the palette filters on.
pub fn fuzzy_match(query: &str, title: &str) -> bool {
    let mut needle = query.chars().flat_map(char::to_lowercase).peekable();
    for ch in title.chars().flat_map(char::to_lowercase) {
        if needle.peek() == Some(&ch) {
            needle.next();
        }
    }
    // All needle chars consumed → matched. An empty query trivially satisfies this.
    needle.peek().is_none()
}

/// Feeds the command catalog to gpui-component's `ListState`. It keeps the full
/// catalog plus the subset matching the current query (`matched`), re-filtering
/// in `perform_search` whenever the search input changes.
pub struct PaletteDelegate {
    /// The full catalog: static commands followed by per-tab switch entries.
    commands: Vec<Command>,
    /// The subset matching the current query — exactly what the list renders.
    matched: Vec<Command>,
    /// Index of the highlighted row, mirrored from the list's own selection so
    /// `render_item` can mark it. `None` when nothing matches.
    selected: Option<IndexPath>,
}

impl PaletteDelegate {
    pub fn new(commands: Vec<Command>) -> Self {
        Self {
            matched: commands.clone(),
            commands,
            selected: Some(IndexPath::default()),
        }
    }

    /// The command kind at the given (filtered) index path, if any. Called by
    /// `app.rs` when the list confirms a selection.
    pub fn command_at(&self, ix: IndexPath) -> Option<CommandKind> {
        self.matched.get(ix.row).map(|c| c.kind)
    }
}

impl ListDelegate for PaletteDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.matched.len()
    }

    /// Re-filter the catalog against the live query and reset the highlight to
    /// the first match.
    fn perform_search(
        &mut self,
        query: &str,
        _window: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.matched = self
            .commands
            .iter()
            .filter(|c| fuzzy_match(query, &c.title))
            .cloned()
            .collect();
        self.selected = (!self.matched.is_empty()).then(IndexPath::default);
        Task::ready(())
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let cmd = self.matched.get(ix.row)?;

        // Read the colours we need as Copy values, then release the theme borrow
        // so we can borrow `cx` again for the keybinding lookup below.
        let (kbd_bg, border, muted) = {
            let t = cx.theme();
            (t.secondary.opacity(0.6), t.border, t.muted_foreground)
        };

        // Shortcut hint: the effective keystroke for this command, rendered as
        // small keycaps on the right — the Raycast/VSCode convention that makes a
        // command palette feel professional and teaches the shortcut in passing.
        let keys = cmd
            .kind
            .binding_action()
            .and_then(|action| crate::ui::keymap::effective_key(action, cx))
            .map(|spec| crate::ui::keymap::key_tokens(&spec));

        let mut row = h_flex()
            .w_full()
            .items_center()
            .justify_between()
            .child(cmd.title.clone());
        if let Some(tokens) = keys {
            row = row.child(h_flex().gap_1().children(tokens.into_iter().map(move |t| {
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .min_w(px(20.))
                    .h(px(20.))
                    .px_1()
                    .rounded_md()
                    .bg(kbd_bg)
                    .border_1()
                    .border_color(border)
                    .text_xs()
                    .text_color(muted)
                    .child(t)
            })));
        }

        Some(
            ListItem::new(ix.row)
                .selected(Some(ix) == self.selected)
                // Tighter than the stock row: smaller text + compact vertical
                // padding so the palette reads dense, like the terminal itself.
                .py_1()
                .text_sm()
                .child(row),
        )
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix;
        cx.notify();
    }
}

/// What the palette tells its host (`Tty7App`) when the user acts on it.
pub enum PaletteEvent {
    /// A command was chosen; the host should close the palette and run it.
    Confirm(CommandKind),
    /// The palette was dismissed (Esc or click outside) with nothing chosen.
    Dismiss,
}

/// The command palette as a self-contained view. It owns the `ListState`
/// (search input, fuzzy filter, keyboard nav) and the scrim/card overlay
/// chrome, and emits a [`PaletteEvent`] when the user confirms or dismisses.
/// The host builds the root catalog and executes the chosen command, so this
/// view stays ignorant of what most commands do — the one exception is the
/// theme sub-list, a two-level flow the palette drives internally: picking
/// "Change Theme…" swaps the list to the presets (Esc steps back to the root),
/// and only the final `SetTheme` reaches the host.
pub struct PaletteView {
    list: Entity<ListState<PaletteDelegate>>,
    /// The root catalog, kept so Esc inside the theme sub-list can restore it
    /// instead of dismissing the whole palette.
    root: Vec<Command>,
    /// True while the theme sub-list is showing.
    in_submenu: bool,
    /// Keeps the *current* list's event subscription alive. Replaced on every
    /// [`show`](Self::show) (root ⇄ sub-list) so it always targets the live list.
    _sub: Subscription,
}

impl PaletteView {
    pub fn new(commands: Vec<Command>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let list = Self::build_list(commands.clone(), window, cx);
        let _sub = cx.subscribe_in(&list, window, Self::on_list_event);
        Self {
            list,
            root: commands,
            in_submenu: false,
            _sub,
        }
    }

    /// Build a fresh `ListState` for `commands` and focus its search input.
    /// gpui-component supplies the search box, fuzzy filtering, ↑/↓ navigation
    /// and Enter/Esc; focusing the input keeps keystrokes off the terminal PTY
    /// until the palette closes.
    fn build_list(
        commands: Vec<Command>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ListState<PaletteDelegate>> {
        let delegate = PaletteDelegate::new(commands);
        let list = cx.new(|cx| ListState::new(delegate, window, cx).searchable(true));
        list.update(cx, |state, cx| state.focus(window, cx));
        list
    }

    /// Swap the visible list to `commands` (root ⇄ theme sub-list). Recreating
    /// the `ListState` from scratch — rather than mutating the delegate in
    /// place — hands us a cleared search box, reset selection and fresh row
    /// cache for free, sidestepping the list's internal query/selection caching.
    fn show(&mut self, commands: Vec<Command>, window: &mut Window, cx: &mut Context<Self>) {
        let list = Self::build_list(commands, window, cx);
        self._sub = cx.subscribe_in(&list, window, Self::on_list_event);
        self.list = list;
        cx.notify();
    }

    /// Translate the current list's confirm/cancel into either a host-facing
    /// event or an in-place transition into/out of the theme sub-list.
    fn on_list_event(
        &mut self,
        list: &Entity<ListState<PaletteDelegate>>,
        ev: &ListEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            ListEvent::Confirm(ix) => {
                let kind = list.read(cx).delegate().command_at(*ix);
                match kind {
                    // A submenu opener never reaches the host: it swaps this
                    // palette to the theme sub-list and stays open.
                    Some(CommandKind::OpenThemePicker) => {
                        self.in_submenu = true;
                        let themes = Command::theme_commands(cx);
                        self.show(themes, window, cx);
                    }
                    Some(kind) => cx.emit(PaletteEvent::Confirm(kind)),
                    None => cx.emit(PaletteEvent::Dismiss),
                }
            }
            // Esc: from the sub-list, step back to the root catalog; from the
            // root, dismiss the palette.
            ListEvent::Cancel => {
                if self.in_submenu {
                    self.in_submenu = false;
                    let root = self.root.clone();
                    self.show(root, window, cx);
                } else {
                    cx.emit(PaletteEvent::Dismiss);
                }
            }
            ListEvent::Select(_) => {}
        }
    }
}

impl EventEmitter<PaletteEvent> for PaletteView {}

impl Render for PaletteView {
    /// The centered overlay: a dim full-window scrim plus the command card. The
    /// card just frames gpui-component's `List`, which renders its own search
    /// input and the filtered, scrollable, keyboard-driven rows.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (background, border, popover) = (theme.background, theme.border, theme.popover);

        let card = v_flex()
            .w(px(560.))
            .max_h(px(440.))
            .bg(popover)
            .border_1()
            .border_color(border)
            .rounded_lg()
            .shadow_lg()
            .overflow_hidden()
            .child(List::new(&self.list).p_1().max_h(px(440.)));

        // Full-window scrim; clicking the empty area dismisses the palette (the
        // card itself is occluded so its clicks don't bubble here).
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(120.))
            .bg(background.opacity(0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _: &MouseDownEvent, _window, cx| {
                    cx.emit(PaletteEvent::Dismiss);
                }),
            )
            .child(div().occlude().child(card))
    }
}

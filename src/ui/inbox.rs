//! The attention inbox: one keyboard-driven list of every agent waiting on
//! the user, across every open window and the machines they reach, each
//! answerable where it is listed.
//!
//! The rows are the panes the tray menu lists, read the same way
//! ([`Tty7App::agent_rows`]'s walk), and they queue the way
//! [`crate::ui::tray::next_needing_attention`] does: whoever has waited
//! longest first. A pane whose hook said what it asks carries an
//! [`AgentPrompt`]; its card shows the question and its options, or the
//! command it wants to run, and the answer goes in through
//! [`TerminalView::answer_agent_prompt`] as the keys its TUI takes. Turns
//! that finished unread follow, dimmed, under the waiting ones.
//!
//! The shell is the switcher's: a 760px card 112px from the top, a 48px
//! header with its `esc` cap and a 40px footer of key hints.

use std::collections::HashMap;

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EntityId, FocusHandle, Focusable as _, Hsla,
    KeyDownEvent, MouseButton, MouseDownEvent, SharedString, Subscription, Window, div, prelude::*,
    px, rems,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use crate::core::cli_agent::{AgentPrompt, AgentPromptKind, AgentStatus, CLIAgent};
use crate::terminal::prompt_answer::{AnswerRefused, PromptAnswer, QuestionAnswer, answerable};
use crate::terminal::view::TerminalView;
use crate::ui::app::Tty7App;
use crate::ui::dialog::{self, Tone, keycap};
use crate::ui::i18n::{L10nKey, t, t_plural};
use crate::ui::right_panel::{HEADING, META, META_MONO, TAB_TEXT, TEXT};

/// The switcher's card, so the two overlays stand in the same place.
const CARD_W: f32 = 760.;
const CARD_MARGIN: f32 = 24.;
/// The list's tallest; a shorter window gets what it has.
const BODY_MAX_H: f32 = 520.;
const LIST_PAD: f32 = 8.;
const CARD_PAD: f32 = 12.;
const CARD_ROUND: f32 = 8.;
const AVATAR: f32 = 22.;
/// The most finished turns listed under the waiting ones.
const DONE_MAX: usize = 4;
/// How long a note under a card ("Answer sent.") stays.
const NOTE_FOR: std::time::Duration = std::time::Duration::from_secs(4);
/// How often every window's agent rows are re-read for the others: an agent
/// that starts or stops waiting notifies its own window, not the rest.
const REFRESH: std::time::Duration = std::time::Duration::from_secs(1);

/// Which prompts the list shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Filter {
    #[default]
    All,
    Questions,
    Permissions,
}

impl Filter {
    const ALL: [Filter; 3] = [Filter::All, Filter::Questions, Filter::Permissions];

    fn label(self) -> &'static str {
        match self {
            Filter::All => t(L10nKey::InboxFilterAll),
            Filter::Questions => t(L10nKey::InboxFilterQuestions),
            Filter::Permissions => t(L10nKey::InboxFilterPermissions),
        }
    }

    fn admits(self, row: &InboxRow) -> bool {
        match (self, row.prompt.as_ref().map(|p| &p.kind)) {
            (Filter::All, _) => true,
            (Filter::Questions, Some(AgentPromptKind::Question { .. })) => true,
            (Filter::Permissions, Some(AgentPromptKind::Permission { .. })) => true,
            _ => false,
        }
    }
}

/// One agent pane, as the inbox lists it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InboxRow {
    pub leaf_id: u64,
    pub agent: CLIAgent,
    pub status: AgentStatus,
    pub prompt: Option<AgentPrompt>,
    /// When the pane entered `status`, by its daemon's clock.
    pub since_ms: Option<u64>,
    pub age_secs: Option<u64>,
    pub title: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub machine: String,
    pub unread: bool,
    pub host: crate::ui::host_ops::HostId,
    pub cwd: Option<std::path::PathBuf>,
}

/// The rows to draw: the waiting ones `filter` admits, longest wait first,
/// then the finished turns nobody has read, newest first, under them.
///
/// `rows` come in window, tab and pane order, which breaks ties; a row with
/// no `since_ms` (an older daemon) queues behind the stamped ones, the way
/// the tray's queue does. Finished turns are only shown under All.
pub(crate) fn ordered(rows: &[InboxRow], filter: Filter) -> (Vec<&InboxRow>, Vec<&InboxRow>) {
    let mut waiting: Vec<&InboxRow> = rows
        .iter()
        .filter(|r| r.status == AgentStatus::Waiting && filter.admits(r))
        .collect();
    waiting.sort_by_key(|r| (r.since_ms.is_none(), r.since_ms));
    let mut done: Vec<&InboxRow> = match filter {
        Filter::All => rows
            .iter()
            .filter(|r| r.status == AgentStatus::Done && r.unread)
            .collect(),
        _ => Vec::new(),
    };
    done.sort_by_key(|r| std::cmp::Reverse(r.since_ms));
    done.truncate(DONE_MAX);
    (waiting, done)
}

/// How many agents wait, and on how many machines.
pub(crate) fn waiting_counts(rows: &[InboxRow]) -> (usize, usize) {
    let waiting: Vec<_> = rows
        .iter()
        .filter(|r| r.status == AgentStatus::Waiting)
        .collect();
    let mut machines: Vec<&str> = waiting.iter().map(|r| r.machine.as_str()).collect();
    machines.sort_unstable();
    machines.dedup();
    (waiting.len(), machines.len())
}

/// Every window's agent rows as last polled, in window order, by the app
/// entity of the window holding them.
#[derive(Default)]
pub(crate) struct AttentionBoard {
    windows: Vec<(EntityId, Vec<InboxRow>)>,
}

impl gpui::Global for AttentionBoard {}

/// Start polling every window's agent panes into the [`AttentionBoard`],
/// once per process, and redraw the windows when what they would show
/// changes. The poll runs outside every window's update, the one place all
/// of them can be read.
pub(crate) fn init(cx: &mut App) {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    cx.default_global::<AttentionBoard>();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(REFRESH).await;
            cx.update(poll_board);
        }
    })
    .detach();
}

fn poll_board(cx: &mut App) {
    let apps: Vec<Entity<Tty7App>> = crate::ui::windows::WindowRegistry::open_windows(cx)
        .into_iter()
        .filter_map(|(_, weak)| weak.upgrade())
        .collect();
    let windows: Vec<(EntityId, Vec<InboxRow>)> = apps
        .iter()
        .map(|app| (app.entity_id(), app.read(cx).inbox_rows_here(cx)))
        .collect();
    if cx.global::<AttentionBoard>().windows == windows {
        return;
    }
    cx.global_mut::<AttentionBoard>().windows = windows;
    for app in apps {
        app.update(cx, |_, cx| cx.notify());
    }
}

/// What the user has chosen so far on one card, before it is sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Draft {
    /// The question digits and the Other field apply to.
    pub active: usize,
    /// Per question: the options picked (one at most when single-select).
    pub picks: Vec<Vec<usize>>,
    /// Per question: words of the user's own.
    pub others: Vec<String>,
}

impl Draft {
    fn for_prompt(prompt: &AgentPrompt) -> Draft {
        let n = match &prompt.kind {
            AgentPromptKind::Question { questions } => questions.len(),
            AgentPromptKind::Permission { .. } => 0,
        };
        Draft {
            active: 0,
            picks: vec![Vec::new(); n],
            others: vec![String::new(); n],
        }
    }

    /// Pick option `i` of the active question: toggled on a multi-select
    /// one, chosen on a single-select one, which then hands the cursor on
    /// to the next question. Picking clears the question's own words.
    pub(crate) fn pick(&mut self, prompt: &AgentPrompt, i: usize) -> bool {
        let AgentPromptKind::Question { questions } = &prompt.kind else {
            return false;
        };
        let Some(q) = questions.get(self.active) else {
            return false;
        };
        if i >= q.options.len() {
            return false;
        }
        let picks = &mut self.picks[self.active];
        self.others[self.active].clear();
        if q.multi_select {
            match picks.iter().position(|p| *p == i) {
                Some(at) => {
                    picks.remove(at);
                }
                None => picks.push(i),
            }
        } else {
            *picks = vec![i];
            if self.active + 1 < questions.len() {
                self.active += 1;
            }
        }
        true
    }

    /// The answer the draft makes, once every question has one.
    pub(crate) fn answer(&self, prompt: &AgentPrompt) -> Option<PromptAnswer> {
        let AgentPromptKind::Question { questions } = &prompt.kind else {
            return None;
        };
        questions
            .iter()
            .enumerate()
            .map(|(at, q)| {
                let other = self.others.get(at).map(|s| s.trim()).unwrap_or_default();
                let picks = self.picks.get(at).cloned().unwrap_or_default();
                match (q.multi_select, picks.as_slice()) {
                    (_, []) if !other.is_empty() => Some(QuestionAnswer::Other(other.into())),
                    (true, [_, ..]) => Some(QuestionAnswer::Toggle(picks)),
                    (false, [i]) => Some(QuestionAnswer::Pick(*i)),
                    _ => None,
                }
            })
            .collect::<Option<Vec<_>>>()
            .map(PromptAnswer::Questions)
    }

    /// The first question still without an answer.
    fn first_open(&self) -> Option<usize> {
        (0..self.picks.len())
            .find(|&at| self.picks[at].is_empty() && self.others[at].trim().is_empty())
    }
}

pub(crate) struct Inbox {
    filter: Filter,
    /// The card the cursor is on, by pane: rows come and go under it.
    sel: Option<u64>,
    drafts: HashMap<(u64, String), Draft>,
    /// The Other field, for the active question of the selected card.
    other: Entity<InputState>,
    focus: FocusHandle,
    note: Option<(u64, SharedString)>,
    scroll: gpui::ScrollHandle,
    anchor: gpui::ScrollAnchor,
    _subs: Vec<Subscription>,
}

impl Tty7App {
    pub(crate) fn toggle_attention_inbox(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.inbox.is_some() {
            self.close_inbox(window, cx);
        } else {
            self.open_inbox(window, cx);
        }
    }

    fn open_inbox(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.switcher.is_some() {
            self.close_switcher(window, cx);
        }
        let other =
            cx.new(|cx| InputState::new(window, cx).placeholder(t(L10nKey::InboxOtherPlaceholder)));
        let subs = vec![cx.subscribe_in(
            &other,
            window,
            |this, input, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    let text = input.read(cx).value().to_string();
                    if let Some((key, prompt)) = this.inbox_selected_prompt(cx)
                        && let Some(inbox) = this.inbox.as_mut()
                    {
                        let draft = inbox
                            .drafts
                            .entry(key)
                            .or_insert_with(|| Draft::for_prompt(&prompt));
                        let at = draft.active;
                        if let Some(slot) = draft.others.get_mut(at) {
                            *slot = text;
                            if !slot.trim().is_empty() {
                                draft.picks[at].clear();
                            }
                        }
                    }
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.inbox_answer_selected(window, cx),
                _ => {}
            },
        )];
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let scroll = gpui::ScrollHandle::new();
        self.inbox = Some(Inbox {
            filter: Filter::All,
            sel: None,
            drafts: HashMap::new(),
            other,
            focus,
            note: None,
            anchor: gpui::ScrollAnchor::for_handle(scroll.clone()),
            scroll,
            _subs: subs,
        });
        cx.notify();
    }

    pub(crate) fn close_inbox(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.inbox.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// This window's agent panes, as inbox rows.
    pub(crate) fn inbox_rows_here(&self, cx: &App) -> Vec<InboxRow> {
        let now_ms = crate::core::config::unix_now().saturating_mul(1000);
        let mut rows = Vec::new();
        for (index, tab) in self.tabs.iter().enumerate() {
            let title = self.tab_label(tab, index, None, cx);
            for leaf in tab.pane.terminals() {
                let view = leaf.read(cx);
                let Some(agent) = view.agent() else { continue };
                let session = view.agent_session();
                let status = session.as_ref().map_or(AgentStatus::Idle, |s| s.status);
                let machine = match view.workspace() {
                    Some(w) => crate::ui::remote_connect::target_label(cx, &w.target),
                    None => t(L10nKey::SwitcherThisComputer).to_string(),
                };
                rows.push(InboxRow {
                    leaf_id: leaf.entity_id().as_u64(),
                    agent,
                    status,
                    prompt: session
                        .as_ref()
                        .filter(|s| s.status == AgentStatus::Waiting)
                        .and_then(|s| s.prompt.clone()),
                    since_ms: session.as_ref().and_then(|s| s.since_ms),
                    // In whole minutes, as the chip shows it, so the poll
                    // sees a change once a minute rather than every second.
                    age_secs: session
                        .as_ref()
                        .and_then(|s| s.status_age_secs(now_ms))
                        .map(|s| s - s % 60),
                    title: title.clone(),
                    repo: view
                        .cwd()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
                    branch: view.git_status(cx).map(|g| g.branch),
                    machine,
                    unread: view.agent_result_unread(),
                    host: view.host_id(),
                    cwd: view.host_cwd(),
                });
            }
        }
        rows
    }

    /// Every open window's agent panes, in window order: this window's read
    /// fresh from `self`, the others as [`init`]'s poll last saw them.
    ///
    /// Not read live. This runs in a render, and a window can be drawn
    /// while another window's app is mid-update (a window opened from an
    /// action handler draws its first frame inside it); reading that app
    /// here would be a double lease and a panic.
    fn inbox_rows_everywhere(&self, me: EntityId, cx: &App) -> Vec<InboxRow> {
        let mut rows = Vec::new();
        let mut seen_self = false;
        let board = cx.try_global::<AttentionBoard>();
        for (id, theirs) in board.map(|b| b.windows.as_slice()).unwrap_or_default() {
            if *id == me {
                seen_self = true;
                rows.extend(self.inbox_rows_here(cx));
            } else {
                rows.extend(theirs.iter().cloned());
            }
        }
        if !seen_self {
            rows.extend(self.inbox_rows_here(cx));
        }
        rows
    }

    fn find_terminal(&self, leaf_id: u64) -> Option<Entity<TerminalView>> {
        self.tabs
            .iter()
            .flat_map(|t| t.pane.terminals())
            .find(|l| l.entity_id().as_u64() == leaf_id)
    }

    fn find_terminal_everywhere(
        &self,
        leaf_id: u64,
        me: EntityId,
        cx: &App,
    ) -> Option<Entity<TerminalView>> {
        self.find_terminal(leaf_id).or_else(|| {
            crate::ui::windows::WindowRegistry::window_apps(cx)
                .into_iter()
                .filter_map(|weak| weak.upgrade())
                .filter(|app| app.entity_id() != me)
                .find_map(|app| app.read(cx).find_terminal(leaf_id))
        })
    }

    /// The waiting rows on show, in order, and the finished ones.
    fn inbox_layout(&self, cx: &Context<Self>) -> (Vec<InboxRow>, Vec<InboxRow>) {
        let filter = self.inbox.as_ref().map_or(Filter::All, |i| i.filter);
        let rows = self.inbox_rows_everywhere(cx.entity_id(), cx);
        let (waiting, done) = ordered(&rows, filter);
        (
            waiting.into_iter().cloned().collect(),
            done.into_iter().cloned().collect(),
        )
    }

    /// The selected waiting row, falling back to the head of the list.
    fn inbox_selected(&self, cx: &Context<Self>) -> Option<InboxRow> {
        let sel = self.inbox.as_ref()?.sel;
        let (waiting, _) = self.inbox_layout(cx);
        waiting
            .iter()
            .find(|r| Some(r.leaf_id) == sel)
            .or(waiting.first())
            .cloned()
    }

    fn inbox_selected_prompt(&self, cx: &Context<Self>) -> Option<((u64, String), AgentPrompt)> {
        let row = self.inbox_selected(cx)?;
        let prompt = row.prompt?;
        Some(((row.leaf_id, prompt.id.clone()), prompt))
    }

    fn inbox_step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (waiting, _) = self.inbox_layout(cx);
        let n = waiting.len();
        if n == 0 {
            return;
        }
        let at = self
            .inbox_selected(cx)
            .and_then(|r| waiting.iter().position(|w| w.leaf_id == r.leaf_id))
            .unwrap_or(0);
        let next = match forward {
            true => (at + 1) % n,
            false => (at + n - 1) % n,
        };
        if let Some(inbox) = self.inbox.as_mut() {
            inbox.sel = Some(waiting[next].leaf_id);
            inbox.anchor.scroll_to(window, cx);
        }
        self.inbox_sync_other(window, cx);
        cx.notify();
    }

    /// Put the selected card's active question's words in the Other field.
    fn inbox_sync_other(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .inbox_selected_prompt(cx)
            .and_then(|(key, _)| {
                let inbox = self.inbox.as_ref()?;
                let d = inbox.drafts.get(&key)?;
                d.others.get(d.active).cloned()
            })
            .unwrap_or_default();
        if let Some(inbox) = self.inbox.as_ref() {
            let other = inbox.other.clone();
            if other.read(cx).value() != text.as_str() {
                other.update(cx, |s, cx| s.set_value(text, window, cx));
            }
        }
    }

    fn inbox_select(&mut self, leaf_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(inbox) = self.inbox.as_mut() {
            inbox.sel = Some(leaf_id);
        }
        self.inbox_sync_other(window, cx);
        cx.notify();
    }

    fn inbox_pick(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, prompt)) = self.inbox_selected_prompt(cx) else {
            return;
        };
        let Some(inbox) = self.inbox.as_mut() else {
            return;
        };
        let draft = inbox
            .drafts
            .entry(key)
            .or_insert_with(|| Draft::for_prompt(&prompt));
        if draft.pick(&prompt, i) {
            self.inbox_sync_other(window, cx);
            cx.notify();
        }
    }

    fn inbox_set_active(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((key, prompt)) = self.inbox_selected_prompt(cx) else {
            return;
        };
        if let Some(inbox) = self.inbox.as_mut() {
            let draft = inbox
                .drafts
                .entry(key)
                .or_insert_with(|| Draft::for_prompt(&prompt));
            if at < draft.picks.len() {
                draft.active = at;
            }
        }
        self.inbox_sync_other(window, cx);
        cx.notify();
    }

    fn inbox_note(&mut self, leaf_id: u64, note: &'static str, cx: &mut Context<Self>) {
        let Some(inbox) = self.inbox.as_mut() else {
            return;
        };
        let note: SharedString = note.into();
        inbox.note = Some((leaf_id, note.clone()));
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTE_FOR).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(inbox) = this.inbox.as_mut()
                    && inbox.note.as_ref().is_some_and(|(_, n)| *n == note)
                {
                    inbox.note = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Answer the selected card with what its draft says, or, on a
    /// permission card, allow it once.
    fn inbox_answer_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.inbox_selected(cx) else {
            return;
        };
        let Some(prompt) = row.prompt.clone() else {
            self.inbox_note(row.leaf_id, t(L10nKey::InboxUnsupported), cx);
            return;
        };
        let answer = match &prompt.kind {
            AgentPromptKind::Permission { .. } => Some(PromptAnswer::AllowOnce),
            AgentPromptKind::Question { .. } => {
                let key = (row.leaf_id, prompt.id.clone());
                let draft = self
                    .inbox
                    .as_ref()
                    .and_then(|i| i.drafts.get(&key))
                    .cloned();
                let draft = draft.unwrap_or_else(|| Draft::for_prompt(&prompt));
                match draft.answer(&prompt) {
                    Some(answer) => Some(answer),
                    None => {
                        if let Some(at) = draft.first_open() {
                            self.inbox_set_active(at, window, cx);
                        }
                        None
                    }
                }
            }
        };
        match answer {
            Some(answer) => self.inbox_send(row.leaf_id, &prompt.id, answer, window, cx),
            None => self.inbox_note(row.leaf_id, t(L10nKey::InboxIncomplete), cx),
        }
    }

    fn inbox_send(
        &mut self,
        leaf_id: u64,
        prompt_id: &str,
        answer: PromptAnswer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let me = cx.entity_id();
        let Some(leaf) = self.find_terminal_everywhere(leaf_id, me, cx) else {
            self.inbox_note(leaf_id, t(L10nKey::InboxStale), cx);
            return;
        };
        let sent = leaf.update(cx, |view, cx| {
            view.answer_agent_prompt(prompt_id, &answer, cx)
        });
        let note = match sent {
            Ok(()) => {
                if let Some(inbox) = self.inbox.as_mut() {
                    inbox.drafts.remove(&(leaf_id, prompt_id.to_string()));
                }
                self.inbox_sync_other(window, cx);
                window.focus(
                    &self
                        .inbox
                        .as_ref()
                        .map(|i| i.focus.clone())
                        .unwrap_or(cx.focus_handle()),
                    cx,
                );
                L10nKey::InboxSent
            }
            Err(AnswerRefused::Stale) => {
                if let Some(inbox) = self.inbox.as_mut() {
                    inbox.drafts.remove(&(leaf_id, prompt_id.to_string()));
                }
                L10nKey::InboxStale
            }
            Err(AnswerRefused::InFlight) => L10nKey::InboxInFlight,
            Err(AnswerRefused::Mismatch) => L10nKey::InboxIncomplete,
            Err(AnswerRefused::Unsupported) => L10nKey::InboxUnsupported,
            Err(AnswerRefused::NoInput) => L10nKey::InboxNoInput,
        };
        self.inbox_note(leaf_id, t(note), cx);
    }

    /// Close the inbox and bring the pane up, in whichever window has it.
    fn inbox_open_pane(&mut self, leaf_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.inbox = None;
        cx.notify();
        if self.owns_leaf(leaf_id) {
            self.handle_tray_action(
                crate::ui::tray::TrayAction::RevealPane { leaf_id },
                window,
                cx,
            );
        } else {
            cx.defer(move |cx| crate::ui::tray::reveal_pane(leaf_id, cx));
        }
    }

    /// Open the finished pane and, when this window has it, its diff.
    fn inbox_review_diff(&mut self, row: &InboxRow, window: &mut Window, cx: &mut Context<Self>) {
        let here = self.owns_leaf(row.leaf_id);
        self.inbox_open_pane(row.leaf_id, window, cx);
        if here && let Some(cwd) = row.cwd.clone() {
            self.toggle_diff_overlay(row.host, cwd, window, cx);
        }
    }

    fn on_inbox_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(inbox) = self.inbox.as_ref() else {
            return;
        };
        let (key, mods) = (ev.keystroke.key.as_str(), ev.keystroke.modifiers);
        let typing = inbox.other.read(cx).focus_handle(cx).is_focused(window);
        if key == "escape" {
            cx.stop_propagation();
            // Out of the Other field first; the inbox on a second press.
            if typing {
                let focus = inbox.focus.clone();
                window.focus(&focus, cx);
                cx.notify();
            } else {
                self.close_inbox(window, cx);
            }
            return;
        }
        if mods.alt || mods.function {
            return;
        }
        if key == "enter" && mods.secondary() {
            cx.stop_propagation();
            if let Some(row) = self.inbox_selected(cx) {
                self.inbox_open_pane(row.leaf_id, window, cx);
            }
            return;
        }
        if typing {
            // The field takes its own keys; Enter answers through its
            // PressEnter event.
            return;
        }
        let bare = !mods.control && !mods.secondary() && !mods.shift;
        match key {
            "up" | "down" if bare => {
                cx.stop_propagation();
                self.inbox_step(key == "down", window, cx);
            }
            "left" | "right" if bare => {
                cx.stop_propagation();
                let active = self
                    .inbox_selected_prompt(cx)
                    .and_then(|(k, _)| self.inbox.as_ref()?.drafts.get(&k).map(|d| d.active));
                let at = active.unwrap_or(0);
                let at = match key {
                    "right" => at + 1,
                    _ => at.saturating_sub(1),
                };
                self.inbox_set_active(at, window, cx);
            }
            "enter" if bare => {
                cx.stop_propagation();
                self.inbox_answer_selected(window, cx);
            }
            "backspace" | "delete" if bare => {
                if let Some(row) = self.inbox_selected(cx)
                    && let Some(p) = row.prompt.as_ref()
                    && matches!(p.kind, AgentPromptKind::Permission { .. })
                {
                    cx.stop_propagation();
                    let id = p.id.clone();
                    self.inbox_send(row.leaf_id, &id, PromptAnswer::Deny, window, cx);
                }
            }
            "tab" => {
                cx.stop_propagation();
                if let Some(inbox) = self.inbox.as_ref() {
                    let other = inbox.other.clone();
                    other.update(cx, |s, cx| s.focus(window, cx));
                }
            }
            _ if bare && key.len() == 1 => {
                if let Some(d) = key.chars().next().and_then(|c| c.to_digit(10))
                    && d >= 1
                {
                    cx.stop_propagation();
                    self.inbox_pick(d as usize - 1, window, cx);
                }
            }
            _ => {}
        }
    }

    /// The title-bar button: how many agents wait, on the waiting amber.
    /// Not drawn while none does.
    pub(crate) fn render_inbox_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rows = self.inbox_rows_everywhere(cx.entity_id(), cx);
        let (n, _) = waiting_counts(&rows);
        if n == 0 {
            return None;
        }
        let amber = cx.theme().warning;
        let tip = crate::ui::tab_strip::chord_tooltip(
            t(L10nKey::InboxButtonLabel),
            "ToggleAttentionInbox",
            cx,
        );
        Some(
            div()
                .occlude()
                .flex_shrink_0()
                .child(
                    h_flex()
                        .id("titlebar-attention-inbox")
                        .h(px(24.))
                        .px(px(8.))
                        .gap(px(6.))
                        .items_center()
                        .rounded(px(7.))
                        .bg(amber.opacity(0.14))
                        .hover(move |s| s.bg(amber.opacity(0.22)))
                        .cursor_pointer()
                        .text_size(rems(TAB_TEXT))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(amber)
                        .child(hollow_dot(amber, 7.))
                        .child(n.to_string())
                        .tooltip(move |window, cx| tip(window, cx).into())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_attention_inbox(window, cx)
                        })),
                )
                .into_any_element(),
        )
    }

    pub(crate) fn render_inbox(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let inbox = self.inbox.as_ref()?;
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let rows = self.inbox_rows_everywhere(cx.entity_id(), cx);
        let (agents, machines) = waiting_counts(&rows);
        let (waiting, done) = ordered(&rows, inbox.filter);
        let selected = waiting
            .iter()
            .find(|r| Some(r.leaf_id) == inbox.sel)
            .or(waiting.first())
            .map(|r| r.leaf_id);

        let viewport = window.viewport_size();
        let card_w = CARD_W
            .min(viewport.width.as_f32() - 2. * CARD_MARGIN)
            .max(320.);
        let body_h = BODY_MAX_H
            .min(
                viewport.height.as_f32()
                    - crate::ui::switcher::CARD_TOP
                    - dialog::HEADER_H
                    - dialog::FOOTER_H
                    - 40.
                    - CARD_MARGIN,
            )
            .max(120.);

        let title = format!(
            "{} · {} · {}",
            t(L10nKey::InboxTitle),
            t_plural(L10nKey::InboxAgentCount, agents, &[]),
            t_plural(L10nKey::InboxMachineCount, machines, &[]),
        );

        let mut tabs = h_flex()
            .gap(px(4.))
            .px(px(LIST_PAD + 2.))
            .h(px(40.))
            .items_center()
            .border_b_1()
            .border_color(border);
        let rungs = dialog::popover_rungs(cx);
        for f in Filter::ALL {
            let on = f == inbox.filter;
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("inbox-filter-{f:?}")))
                    .px(px(10.))
                    .h(px(26.))
                    .flex()
                    .items_center()
                    .rounded(px(7.))
                    .cursor_pointer()
                    .text_size(rems(TAB_TEXT))
                    .when(on, |d| {
                        d.bg(gpui::rgb(rungs.selected))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(cx.theme().foreground)
                    })
                    .when(!on, |d| {
                        d.text_color(muted)
                            .hover(move |s| s.bg(gpui::rgb(rungs.hover)))
                    })
                    .child(f.label())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(inbox) = this.inbox.as_mut() {
                            inbox.filter = f;
                            inbox.sel = None;
                        }
                        this.inbox_sync_other(window, cx);
                        cx.notify();
                    })),
            );
        }

        let mut list = v_flex().gap(px(6.)).p(px(LIST_PAD));
        if waiting.is_empty() {
            list = list.child(
                div()
                    .px(px(10.))
                    .py(px(14.))
                    .text_size(rems(13. / 16.))
                    .text_color(muted)
                    .child(t(L10nKey::InboxEmpty)),
            );
        }
        for row in &waiting {
            let picked = Some(row.leaf_id) == selected;
            list = list.child(self.render_inbox_card(row, picked, cx));
        }
        for row in &done {
            list = list.child(self.render_inbox_done(row, cx));
        }
        let scroll = inbox.scroll.clone();
        let body = div()
            .h(px(body_h))
            .child(crate::ui::scrollbar::with_vertical_scrollbar(
                "inbox-scrollbar",
                div()
                    .id("inbox-list")
                    .track_scroll(&scroll)
                    .size_full()
                    .overflow_y_scroll()
                    .child(list),
                &scroll,
            ));

        let card = dialog::card("inbox-card", t(L10nKey::InboxTitle), card_w, cx)
            .child(dialog::header(title, cx))
            .child(tabs)
            .child(body)
            .child(render_footer(
                selected
                    .and_then(|id| waiting.iter().find(|r| r.leaf_id == id))
                    .is_some_and(|r| {
                        r.prompt
                            .as_ref()
                            .is_some_and(|p| matches!(p.kind, AgentPromptKind::Permission { .. }))
                    }),
                cx,
            ));

        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_start()
                .justify_center()
                .pt(px(crate::ui::switcher::CARD_TOP))
                .bg(crate::ui::presets::scrim_fill(cx))
                .key_context("AttentionInbox")
                .track_focus(&inbox.focus)
                .on_key_down(cx.listener(Self::on_inbox_key))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        // Spent on the dismissal, like the switcher's scrim:
                        // the title-bar button under it would reopen it.
                        cx.stop_propagation();
                        this.close_inbox(window, cx)
                    }),
                )
                .child(card)
                .into_any_element(),
        )
    }

    fn render_inbox_card(
        &self,
        row: &InboxRow,
        picked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, amber) = (theme.foreground, theme.muted_foreground, theme.warning);
        let rungs = dialog::popover_rungs(cx);
        let leaf_id = row.leaf_id;
        let inbox = self.inbox.as_ref().expect("only drawn while up");
        let draft = row.prompt.as_ref().map(|p| {
            inbox
                .drafts
                .get(&(leaf_id, p.id.clone()))
                .cloned()
                .unwrap_or_else(|| Draft::for_prompt(p))
        });

        let kind = match row.prompt.as_ref().map(|p| &p.kind) {
            Some(AgentPromptKind::Question { .. }) => t(L10nKey::InboxKindQuestion),
            Some(AgentPromptKind::Permission { .. }) => t(L10nKey::InboxKindPermission),
            None => t(L10nKey::InboxKindWaiting),
        };
        let chip_text = match row.age_secs {
            Some(secs) => format!("{kind} · {}", crate::ui::home::compact_age(secs)),
            None => kind.to_string(),
        };
        let meta = match (&row.repo, &row.branch) {
            (Some(repo), Some(branch)) => format!("{repo} · {branch}"),
            (Some(repo), None) => repo.clone(),
            _ => String::new(),
        };

        let head = h_flex()
            .gap(px(10.))
            .items_center()
            .child(crate::ui::tab_strip::avatar(
                SharedString::from(format!("inbox-avatar-{leaf_id}")),
                crate::ui::search::Avatar {
                    agent: Some(row.agent),
                    status: Some(row.status),
                    unread: 0,
                    ssh: None,
                },
                AVATAR,
                cx,
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .id(SharedString::from(format!("inbox-title-{leaf_id}")))
                            .truncate()
                            .text_size(rems(TEXT))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(fg)
                            .cursor_pointer()
                            .child(row.title.clone())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.inbox_open_pane(leaf_id, window, cx)
                            })),
                    )
                    .when(!meta.is_empty(), |c| {
                        c.child(
                            div()
                                .truncate()
                                .text_size(rems(META))
                                .text_color(muted)
                                .child(meta),
                        )
                    }),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap(px(6.))
                    .items_center()
                    .h(px(22.))
                    .px(px(8.))
                    .rounded_full()
                    .bg(amber.opacity(0.12))
                    .text_size(rems(HEADING))
                    .text_color(amber)
                    .child(hollow_dot(amber, 7.))
                    .child(chip_text),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .max_w(px(140.))
                    .truncate()
                    .text_size(rems(META))
                    .text_color(muted)
                    .child(row.machine.clone()),
            );

        let body: AnyElement = match (row.prompt.as_ref(), draft.as_ref()) {
            (Some(p), Some(d)) => match &p.kind {
                AgentPromptKind::Question { questions } => {
                    self.render_questions(row, p, questions, d, picked, cx)
                }
                AgentPromptKind::Permission {
                    tool,
                    summary,
                    detail,
                } => {
                    let mono = cx.theme().mono_font_family.clone();
                    let id = p.id.clone();
                    let deny_id = p.id.clone();
                    v_flex()
                        .gap(px(8.))
                        .child(
                            dialog::well(cx)
                                .child(
                                    div()
                                        .text_size(rems(HEADING))
                                        .text_color(muted)
                                        .child(tool.clone()),
                                )
                                .child(
                                    div()
                                        .font_family(mono)
                                        .text_size(rems(META_MONO))
                                        .text_color(fg)
                                        .child(summary.clone()),
                                )
                                .children(detail.clone().map(|d| {
                                    div()
                                        .text_size(rems(META))
                                        .text_color(muted)
                                        .line_clamp(3)
                                        .child(d)
                                })),
                        )
                        .child(
                            h_flex()
                                .justify_end()
                                .gap(px(6.))
                                .child(dialog::button(
                                    SharedString::from(format!("inbox-deny-{leaf_id}")),
                                    t(L10nKey::InboxDeny),
                                    Tone::Secondary,
                                    answerable(row.agent),
                                    rungs,
                                    cx,
                                    cx.listener(move |this, _, window, cx| {
                                        this.inbox_send(
                                            leaf_id,
                                            &deny_id,
                                            PromptAnswer::Deny,
                                            window,
                                            cx,
                                        )
                                    }),
                                ))
                                .child(dialog::button(
                                    SharedString::from(format!("inbox-allow-{leaf_id}")),
                                    t(L10nKey::InboxAllowOnce),
                                    Tone::Primary,
                                    answerable(row.agent),
                                    rungs,
                                    cx,
                                    cx.listener(move |this, _, window, cx| {
                                        this.inbox_send(
                                            leaf_id,
                                            &id,
                                            PromptAnswer::AllowOnce,
                                            window,
                                            cx,
                                        )
                                    }),
                                )),
                        )
                        .into_any_element()
                }
            },
            _ => div()
                .text_size(rems(META))
                .text_color(muted)
                .child(t(L10nKey::InboxNoDetail))
                .into_any_element(),
        };

        let note = inbox
            .note
            .as_ref()
            .filter(|(id, _)| *id == leaf_id)
            .map(|(_, n)| n.clone());

        v_flex()
            .id(SharedString::from(format!("inbox-card-{leaf_id}")))
            .anchor_scroll(picked.then(|| inbox.anchor.clone()))
            .gap(px(10.))
            .p(px(CARD_PAD))
            .rounded(px(CARD_ROUND))
            // The cursor's card: the neutral selected step, never the accent.
            .when(picked, |c| c.bg(gpui::rgb(rungs.selected)))
            .when(!picked, |c| c.hover(move |s| s.bg(gpui::rgb(rungs.hover))))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(
                cx.listener(move |this, _, window, cx| this.inbox_select(leaf_id, window, cx)),
            )
            .child(head)
            .child(body)
            .children(note.map(|n| div().text_size(rems(META)).text_color(muted).child(n)))
            .into_any_element()
    }

    fn render_questions(
        &self,
        row: &InboxRow,
        prompt: &AgentPrompt,
        questions: &[crate::core::cli_agent::AgentQuestion],
        draft: &Draft,
        picked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let rungs = dialog::popover_rungs(cx);
        let leaf_id = row.leaf_id;
        let several = questions.len() > 1;
        let mut col = v_flex().gap(px(12.));
        for (at, q) in questions.iter().enumerate() {
            let active = at == draft.active;
            let mut grid = div().grid().grid_cols(2).gap(px(6.));
            for (i, opt) in q.options.iter().enumerate() {
                let on = draft.picks.get(at).is_some_and(|p| p.contains(&i));
                let sole = !several && !q.multi_select;
                let prompt_c = prompt.clone();
                grid = grid.child(
                    h_flex()
                        .id(SharedString::from(format!("inbox-opt-{leaf_id}-{at}-{i}")))
                        .gap(px(8.))
                        .items_start()
                        .p(px(8.))
                        .rounded(px(7.))
                        .cursor_pointer()
                        .bg(gpui::rgb(match on {
                            true => rungs.pressed,
                            false => rungs.hover,
                        }))
                        .hover(move |s| s.bg(gpui::rgb(rungs.pressed)))
                        .when(on, |b| b.border_1().border_color(fg.opacity(0.5)))
                        .when(active && i < 9, |b| {
                            b.child(keycap((i + 1).to_string(), cx))
                        })
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_size(rems(TAB_TEXT))
                                        .text_color(fg)
                                        .when(on, |d| d.font_weight(gpui::FontWeight::MEDIUM))
                                        .child(opt.label.clone()),
                                )
                                .children(opt.description.clone().map(|d| {
                                    div()
                                        .text_size(rems(META))
                                        .text_color(muted)
                                        .line_clamp(2)
                                        .child(d)
                                })),
                        )
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.inbox_select(leaf_id, window, cx);
                            this.inbox_set_active(at, window, cx);
                            this.inbox_pick(i, window, cx);
                            // One single-select question: the click is the
                            // answer, as its digit would be in the agent.
                            if sole {
                                let draft = this
                                    .inbox
                                    .as_ref()
                                    .and_then(|ib| ib.drafts.get(&(leaf_id, prompt_c.id.clone())))
                                    .cloned();
                                if let Some(answer) = draft.and_then(|d| d.answer(&prompt_c)) {
                                    this.inbox_send(leaf_id, &prompt_c.id, answer, window, cx);
                                }
                            }
                        })),
                );
            }
            let other_here = q.allow_other && !q.multi_select;
            col = col.child(
                v_flex()
                    .gap(px(8.))
                    .when(several && !active, |c| c.opacity(0.6))
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .items_baseline()
                            .children(q.header.clone().map(|h| {
                                div()
                                    .flex_shrink_0()
                                    .text_size(rems(HEADING))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(muted)
                                    .child(h)
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(rems(TEXT))
                                    .text_color(fg)
                                    .child(q.text.clone()),
                            ),
                    )
                    .when(!q.options.is_empty(), |c| c.child(grid))
                    .when(other_here, |c| {
                        match picked && active && self.inbox.is_some() {
                            true => c.child(dialog::field(
                                Input::new(&self.inbox.as_ref().expect("checked").other),
                                cx,
                            )),
                            false => {
                                let text = draft.others.get(at).cloned().unwrap_or_default();
                                c.child(
                                    h_flex()
                                        .h(px(dialog::FIELD_H))
                                        .px(px(8.))
                                        .rounded(px(7.))
                                        .bg(dialog::well_fill(cx))
                                        .text_size(rems(TAB_TEXT))
                                        .text_color(match text.is_empty() {
                                            true => muted,
                                            false => fg,
                                        })
                                        .child(match text.is_empty() {
                                            true => t(L10nKey::InboxOtherPlaceholder).to_string(),
                                            false => text,
                                        }),
                                )
                            }
                        }
                    }),
            );
        }
        if !answerable(row.agent) {
            col = col.child(
                div()
                    .text_size(rems(META))
                    .text_color(muted)
                    .child(t(L10nKey::InboxUnsupported)),
            );
        }
        col.into_any_element()
    }

    fn render_inbox_done(&self, row: &InboxRow, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let rungs = dialog::popover_rungs(cx);
        let leaf_id = row.leaf_id;
        let review = row.clone();
        h_flex()
            .id(SharedString::from(format!("inbox-done-{leaf_id}")))
            .gap(px(10.))
            .items_center()
            .px(px(CARD_PAD))
            .py(px(8.))
            .rounded(px(CARD_ROUND))
            .opacity(0.6)
            .hover(move |s| s.opacity(1.).bg(gpui::rgb(rungs.hover)))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(crate::ui::tab_strip::avatar(
                SharedString::from(format!("inbox-done-avatar-{leaf_id}")),
                crate::ui::search::Avatar {
                    agent: Some(row.agent),
                    status: Some(row.status),
                    unread: 0,
                    ssh: None,
                },
                AVATAR,
                cx,
            ))
            .child(
                div()
                    .id(SharedString::from(format!("inbox-done-title-{leaf_id}")))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(rems(TAB_TEXT))
                    .text_color(fg)
                    .cursor_pointer()
                    .child(row.title.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.inbox_open_pane(leaf_id, window, cx)
                    })),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(rems(META))
                    .text_color(muted)
                    .child(t(L10nKey::InboxDone)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .max_w(px(140.))
                    .truncate()
                    .text_size(rems(META))
                    .text_color(muted)
                    .child(row.machine.clone()),
            )
            .child(dialog::button(
                SharedString::from(format!("inbox-review-{leaf_id}")),
                t(L10nKey::InboxReviewDiff),
                Tone::Secondary,
                row.cwd.is_some(),
                rungs,
                cx,
                cx.listener(move |this, _, window, cx| this.inbox_review_diff(&review, window, cx)),
            ))
            .into_any_element()
    }
}

/// The waiting mark: a hollow ring in the status amber, as the avatars wear
/// it for a waiting agent.
fn hollow_dot(color: Hsla, size: f32) -> gpui::Div {
    div()
        .flex_shrink_0()
        .size(px(size))
        .rounded_full()
        .border_2()
        .border_color(color)
}

fn render_footer(permission: bool, cx: &App) -> gpui::Div {
    let hint = |keys: Vec<AnyElement>, label: &str| {
        h_flex()
            .items_center()
            .gap(px(6.))
            .children(keys)
            .child(label.to_string())
            .into_any_element()
    };
    let mut hints = vec![
        hint(
            vec![keycap("↑", cx), keycap("↓", cx)],
            t(L10nKey::InboxHintAgent),
        ),
        hint(vec![keycap("1–9", cx)], t(L10nKey::InboxHintPick)),
        hint(vec![keycap("↵", cx)], t(L10nKey::InboxHintAnswer)),
    ];
    if permission {
        hints.push(hint(vec![keycap("⌫", cx)], t(L10nKey::InboxHintDeny)));
    }
    hints.push(hint(
        vec![
            keycap(crate::ui::keymap::secondary_glyph(), cx),
            keycap("↵", cx),
        ],
        t(L10nKey::InboxHintOpen),
    ));
    h_flex()
        .flex_none()
        .items_center()
        .justify_end()
        .gap(px(16.))
        .h(px(dialog::FOOTER_H))
        .px(px(12.))
        .border_t_1()
        .border_color(cx.theme().border)
        .text_size(rems(11.5 / 16.))
        .text_color(cx.theme().muted_foreground)
        .children(hints)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::cli_agent::{AgentQuestion, AgentQuestionOption};

    fn row(leaf_id: u64, status: AgentStatus, since_ms: Option<u64>) -> InboxRow {
        InboxRow {
            leaf_id,
            agent: CLIAgent::Claude,
            status,
            prompt: None,
            since_ms,
            age_secs: None,
            title: format!("tab {leaf_id}"),
            repo: None,
            branch: None,
            machine: "This Computer".into(),
            unread: false,
            host: crate::ui::host_ops::HostId::LOCAL,
            cwd: None,
        }
    }

    fn with_prompt(mut r: InboxRow, kind: AgentPromptKind) -> InboxRow {
        r.prompt = Some(AgentPrompt {
            id: format!("p{}", r.leaf_id),
            kind,
        });
        r
    }

    fn permission() -> AgentPromptKind {
        AgentPromptKind::Permission {
            tool: "Bash".into(),
            summary: "ls".into(),
            detail: None,
        }
    }

    fn questions(qs: Vec<AgentQuestion>) -> AgentPromptKind {
        AgentPromptKind::Question { questions: qs }
    }

    fn question(n: usize, multi_select: bool) -> AgentQuestion {
        AgentQuestion {
            header: None,
            text: "?".into(),
            options: (0..n)
                .map(|i| AgentQuestionOption {
                    label: format!("o{i}"),
                    description: None,
                })
                .collect(),
            multi_select,
            allow_other: true,
        }
    }

    fn ids(rows: &[&InboxRow]) -> Vec<u64> {
        rows.iter().map(|r| r.leaf_id).collect()
    }

    #[test]
    fn the_longest_wait_leads_and_unread_results_trail() {
        let mut done_old = row(5, AgentStatus::Done, Some(10));
        done_old.unread = true;
        let mut done_new = row(6, AgentStatus::Done, Some(90));
        done_new.unread = true;
        let rows = [
            row(1, AgentStatus::Waiting, Some(50)),
            row(2, AgentStatus::Working, Some(1)),
            row(3, AgentStatus::Waiting, None),
            row(4, AgentStatus::Waiting, Some(20)),
            done_old,
            done_new,
            // Read already: nothing to come back to.
            row(7, AgentStatus::Done, Some(99)),
        ];
        let (waiting, done) = ordered(&rows, Filter::All);
        assert_eq!(ids(&waiting), vec![4, 1, 3]);
        assert_eq!(ids(&done), vec![6, 5]);
    }

    #[test]
    fn the_filter_keeps_only_its_kind_and_drops_finished_turns() {
        let mut done = row(9, AgentStatus::Done, Some(1));
        done.unread = true;
        let rows = [
            with_prompt(row(1, AgentStatus::Waiting, Some(1)), permission()),
            with_prompt(
                row(2, AgentStatus::Waiting, Some(2)),
                questions(vec![question(2, false)]),
            ),
            row(3, AgentStatus::Waiting, Some(3)),
            done,
        ];
        let (w, d) = ordered(&rows, Filter::Permissions);
        assert_eq!((ids(&w), d.len()), (vec![1], 0));
        let (w, d) = ordered(&rows, Filter::Questions);
        assert_eq!((ids(&w), d.len()), (vec![2], 0));
        let (w, d) = ordered(&rows, Filter::All);
        assert_eq!((ids(&w), ids(&d)), (vec![1, 2, 3], vec![9]));
    }

    #[test]
    fn counts_name_agents_and_the_machines_they_are_on() {
        let mut far = row(2, AgentStatus::Waiting, None);
        far.machine = "devbox".into();
        let rows = [
            row(1, AgentStatus::Waiting, None),
            far,
            row(3, AgentStatus::Waiting, None),
            row(4, AgentStatus::Working, None),
        ];
        assert_eq!(waiting_counts(&rows), (3, 2));
        assert_eq!(waiting_counts(&[]), (0, 0));
    }

    #[test]
    fn a_draft_answers_only_once_every_question_has_one() {
        let prompt = AgentPrompt {
            id: "p".into(),
            kind: questions(vec![question(3, false), question(3, true)]),
        };
        let mut d = Draft::for_prompt(&prompt);
        assert_eq!(d.answer(&prompt), None);
        assert!(d.pick(&prompt, 1));
        // A single-select pick hands the cursor on.
        assert_eq!(d.active, 1);
        assert_eq!(d.answer(&prompt), None);
        assert_eq!(d.first_open(), Some(1));
        assert!(d.pick(&prompt, 2));
        assert!(d.pick(&prompt, 0));
        assert!(d.pick(&prompt, 2));
        assert!(!d.pick(&prompt, 7));
        assert_eq!(
            d.answer(&prompt),
            Some(PromptAnswer::Questions(vec![
                QuestionAnswer::Pick(1),
                QuestionAnswer::Toggle(vec![0]),
            ]))
        );
        d.active = 0;
        d.picks[0].clear();
        d.others[0] = " zstd ".into();
        assert_eq!(
            d.answer(&prompt),
            Some(PromptAnswer::Questions(vec![
                QuestionAnswer::Other("zstd".into()),
                QuestionAnswer::Toggle(vec![0]),
            ]))
        );
    }
}

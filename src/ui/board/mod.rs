//! The board: the window's agents and the tasks handed to them, in five
//! columns by where each one stands (#1010).
//!
//! It is the main area's second view — the sidebar stays, the terminal steps
//! aside — and it only ever shows this window's workspace, the same scope as
//! the sidebar beside it.
//!
//! Every card is one of two things. A *task* is kept on the machine tree
//! ([`tty7_core::core::task`]): something the user wrote down, maybe started,
//! maybe finished. Any other tab running an agent is a *loose* card, drawn
//! from the tab itself and kept nowhere, so an agent started by hand shows up
//! without anyone filing it. Where either sits is worked out from the agent
//! on every frame; nothing here stores a column.
//!
//! Moving a card is asking its agent to do something, so only the moves an
//! agent can make are offered: start a queued task, pause a running one,
//! allow what a waiting one asks, send a finished one back with changes, or
//! call it done. A drag, a button on the card and the peek panel all end in
//! [`Tty7App::board_move`].

mod composer;
mod peek;
mod view;

use std::path::PathBuf;

use gpui::{App, Context, Entity, FocusHandle, Focusable as _, SharedString, Subscription, Window};
use gpui_component::WindowExt as _;
use gpui_component::input::InputState;
use tty7_core::core::group_key::GroupId;
use tty7_core::core::machine::TabId;
use tty7_core::core::task::{self, Column, Done, Live, Run, Task, TaskId};

use crate::core::cli_agent::{AgentStatus, CLIAgent};
use crate::core::config::{Config, unix_now};
use crate::ui::app::Tty7App;
use crate::ui::i18n::{L10nKey, t, t_fmt};

pub(crate) use composer::Composer;

/// What the main area is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum MainView {
    #[default]
    Terminal,
    Board,
}

/// The board's own state on the window.
pub(crate) struct Board {
    /// This workspace's tasks, as the machine last said — and as this window
    /// has since changed them, since its own edits never come back to it.
    pub tasks: Vec<Task>,
    /// Only these agents' cards; empty is everyone's.
    pub agent_filter: Vec<CLIAgent>,
    /// Only this pinned group's cards, or every group's.
    pub group_filter: Option<GroupId>,
    pub composer: Option<Composer>,
    /// What the worktree switch last said, so the next task starts there.
    pub last_worktree: bool,
    pub focus: FocusHandle,
    selected: Option<CardRef>,
    /// The panel beside the columns is open on [`Self::selected`].
    peek: bool,
    reply: Option<Entity<InputState>>,
    _reply_sub: Option<Subscription>,
    /// The selected card's changed files, once `git diff` has answered.
    files: Option<(CardRef, Result<Vec<FileChange>, String>)>,
    /// The card being dragged, while one is.
    dragging: Option<CardDrag>,
    toast: Option<Toast>,
    toast_seq: u64,
}

impl Board {
    pub(crate) fn new(cx: &mut App) -> Board {
        Board {
            tasks: Vec::new(),
            agent_filter: Vec::new(),
            group_filter: None,
            composer: None,
            last_worktree: true,
            focus: cx.focus_handle(),
            selected: None,
            peek: false,
            reply: None,
            _reply_sub: None,
            files: None,
            dragging: None,
            toast: None,
            toast_seq: 0,
        }
    }
}

/// Which card a click, a drag or a key is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CardRef {
    Task(TaskId),
    /// A tab running an agent that no task claims.
    Loose(TabId),
}

/// What a dragged card carries.
#[derive(Clone, Debug)]
pub(crate) struct CardDrag {
    key: CardRef,
    from: Column,
    /// It is waiting on a question, which a drop into Running answers with
    /// words rather than a yes.
    question: bool,
    title: SharedString,
    agent: Option<CLIAgent>,
}

/// One changed file, from `git diff --numstat`.
#[derive(Clone, Debug, PartialEq)]
struct FileChange {
    path: String,
    added: u32,
    removed: u32,
}

/// The note at the bottom of the board saying what just happened, with the
/// way back when there is one.
struct Toast {
    text: SharedString,
    undo: Option<Undo>,
}

/// Putting the board back the way it was. Only for what the board itself
/// changed: a keystroke already sent to an agent cannot be taken back.
#[derive(Clone)]
enum Undo {
    Put(Task),
    Remove(TaskId),
}

/// One card, read off a task or a tab for this frame.
struct Card {
    key: CardRef,
    column: Column,
    agent: Option<CLIAgent>,
    status: Option<AgentStatus>,
    title: SharedString,
    /// The task's own words, for the peek panel.
    prompt: Option<SharedString>,
    group: Option<GroupId>,
    repo: Option<String>,
    branch: Option<String>,
    cwd: Option<PathBuf>,
    /// What the agent is stopped on.
    ask: Option<SharedString>,
    question: bool,
    diff: Option<(u32, u32)>,
    /// When this card's current state began, in Unix seconds, if known.
    since: Option<u64>,
    /// The open tab a run of this card lives in.
    tab: Option<TabId>,
    /// A finished run this card could pick back up.
    resumable: bool,
    paused: bool,
}

/// What the board reads off one open tab.
struct TabFacts {
    id: TabId,
    agent: CLIAgent,
    live: Live,
    ask: Option<String>,
    question: bool,
    cwd: Option<PathBuf>,
    branch: Option<String>,
    diff: Option<(u32, u32)>,
    label: Option<String>,
    group: Option<GroupId>,
}

/// The moves a card may make, by the column it is in. Done is where a card
/// rests; reopening it is a deliberate act in the peek panel, not a drag.
pub(crate) fn moves_from(from: Column) -> &'static [Column] {
    match from {
        Column::Queued => &[Column::Running],
        Column::Running => &[Column::Queued],
        Column::NeedsInput => &[Column::Running, Column::Queued],
        Column::Review => &[Column::Running, Column::Done],
        Column::Done => &[],
    }
}

/// What a move is called on the column it drops into.
fn move_verb(from: Column, to: Column, question: bool) -> L10nKey {
    match (from, to) {
        (Column::NeedsInput, Column::Running) if question => L10nKey::BoardVerbReply,
        (Column::NeedsInput, Column::Running) => L10nKey::BoardVerbAllow,
        (Column::Review, Column::Running) => L10nKey::BoardVerbChanges,
        (_, Column::Queued) => L10nKey::BoardVerbPause,
        (_, Column::Done) => L10nKey::BoardVerbDone,
        _ => L10nKey::BoardVerbStart,
    }
}

/// What a paused task's agent is told when it is started again.
const CONTINUE: &str = "Continue where you left off.";

/// How long a pause holds against an agent still reporting work: an
/// interrupted turn can land a last tool result after the ESC that stopped it.
const PAUSE_GRACE_SECS: u64 = 3;

const TOAST_MS: u64 = 4500;

impl Tty7App {
    pub(crate) fn board_open(&self) -> bool {
        self.main_view == MainView::Board
    }

    pub(crate) fn toggle_board(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.main_view {
            MainView::Board => self.close_board(window, cx),
            MainView::Terminal => self.open_board(window, cx),
        }
    }

    pub(crate) fn open_board(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.main_view = MainView::Board;
        self.maximized = None;
        self.focus_active(window, cx);
        cx.notify();
    }

    pub(crate) fn close_board(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.main_view == MainView::Terminal {
            return;
        }
        self.main_view = MainView::Terminal;
        self.board.composer = None;
        self.board.peek = false;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Takes the tasks a tree pull brought in.
    pub(crate) fn adopt_board_tasks(&mut self, tasks: Vec<Task>, cx: &mut Context<Self>) {
        if self.board.tasks != tasks {
            self.board.tasks = tasks;
            cx.notify();
        }
    }

    pub(crate) fn board_task_put(&mut self, task: Task) {
        match self.board.tasks.iter_mut().find(|t| t.id == task.id) {
            Some(slot) => *slot = task,
            None => self.board.tasks.push(task),
        }
    }

    pub(crate) fn board_task_removed(&mut self, task: TaskId) {
        self.board.tasks.retain(|t| t.id != task);
    }

    /// How many cards are stopped on the user — the sidebar row's count.
    pub(crate) fn board_needs_input(&self, cx: &App) -> usize {
        self.board_cards(None, false, cx)
            .iter()
            .filter(|c| c.column == Column::NeedsInput)
            .count()
    }

    fn task(&self, id: TaskId) -> Option<Task> {
        self.board.tasks.iter().find(|t| t.id == id).cloned()
    }

    /// Saves `task` here and on the machine. False, with the reason on
    /// screen, when the machine would not take it.
    fn save_task(&mut self, task: Task, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Err(why) = task.check() {
            window.push_notification(why, cx);
            return false;
        }
        if !crate::ui::tree_sync::push_task(cx, self.workspace, task.clone()) {
            window.push_notification(t(L10nKey::BoardUnavailable), cx);
            return false;
        }
        self.board_task_put(task);
        cx.notify();
        true
    }

    fn delete_task(&mut self, id: TaskId, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !crate::ui::tree_sync::remove_task(cx, self.workspace, id) {
            window.push_notification(t(L10nKey::BoardUnavailable), cx);
            return false;
        }
        self.board_task_removed(id);
        if self.board.selected == Some(CardRef::Task(id)) {
            self.board.selected = None;
            self.board.peek = false;
        }
        cx.notify();
        true
    }

    // ---- reading the board ------------------------------------------------

    /// Everything the board knows about the open tabs that run an agent.
    ///
    /// `full` also reads what only a drawn card shows — the tab's label and
    /// its git counts. The sidebar's count asks every frame and needs neither.
    fn tab_facts(&self, window: Option<&Window>, full: bool, cx: &App) -> Vec<TabFacts> {
        let urgency = |s: AgentStatus| match s {
            AgentStatus::Waiting => 3,
            AgentStatus::Working => 2,
            AgentStatus::Done => 1,
            AgentStatus::Idle => 0,
        };
        self.tabs
            .iter()
            .filter_map(|tab| {
                // The tab's most urgent agent pane speaks for it, the same
                // pane its sidebar row reports.
                let leaf = tab
                    .pane
                    .terminals()
                    .into_iter()
                    .filter(|l| l.read(cx).agent().is_some())
                    .max_by_key(|l| l.read(cx).agent_session().map_or(0, |s| urgency(s.status)))?;
                let view = leaf.read(cx);
                let session = view.agent_session().unwrap_or_default();
                let waiting = session.status == AgentStatus::Waiting;
                let git = full.then(|| view.git_status(cx)).flatten();
                Some(TabFacts {
                    id: tab.tree_id.get(),
                    agent: view.agent()?,
                    live: Live::from(&session),
                    ask: waiting.then(|| session.message.clone()).flatten(),
                    question: waiting && session.question,
                    cwd: view.cwd(),
                    branch: git.as_ref().map(|g| g.branch.clone()),
                    diff: git
                        .as_ref()
                        .map(|g| (g.added, g.removed))
                        .filter(|&(a, r)| a > 0 || r > 0),
                    label: full.then(|| self.full_tab_label(tab, window, cx)).flatten(),
                    group: tab.group.get().filter(|g| self.sidebar_groups.contains(*g)),
                })
            })
            .collect()
    }

    /// Every card on the board this frame, filtered as the header says.
    fn board_cards(&self, window: Option<&Window>, full: bool, cx: &App) -> Vec<Card> {
        let facts = self.tab_facts(window, full, cx);
        let find = |tab: Option<TabId>| tab.and_then(|id| facts.iter().find(|f| f.id == id));
        let mut cards = Vec::new();
        for task in &self.board.tasks {
            let column = task::column(task, |run| find(run.tab).map(|f| f.live));
            // The run this card reports: the newest one still open, else the
            // newest one at all.
            let open = task.runs.iter().rev().find_map(|r| find(r.tab));
            let last = task.runs.last();
            let cwd = open
                .and_then(|f| f.cwd.clone())
                .or_else(|| last.and_then(|r| r.worktree.as_ref().map(PathBuf::from)))
                .or_else(|| task.cwd.as_ref().map(PathBuf::from));
            cards.push(Card {
                key: CardRef::Task(task.id),
                column,
                agent: open
                    .map(|f| f.agent)
                    .or(last.map(|r| r.agent))
                    .or(task.agent),
                status: open.map(|f| f.live.status),
                title: task.title.clone().into(),
                prompt: Some(task.ask().to_string().into()),
                group: task.group.filter(|g| self.sidebar_groups.contains(*g)),
                repo: repo_name(cwd.as_deref()),
                branch: open.and_then(|f| f.branch.clone()),
                cwd,
                ask: open.and_then(|f| f.ask.clone()).map(Into::into),
                question: open.is_some_and(|f| f.question),
                diff: open.and_then(|f| f.diff),
                since: match column {
                    Column::Done => task.done.map(|d| d.at),
                    Column::Queued => task.paused.or(Some(task.created)),
                    _ => last.map(|r| r.started),
                },
                tab: open.map(|f| f.id),
                resumable: open.is_none() && last.is_some_and(|r| r.session_id.is_some()),
                paused: task.paused.is_some(),
            });
        }
        let claimed = |id: TabId| self.board.tasks.iter().any(|t| t.runs_in(id));
        for f in facts.iter().filter(|f| !claimed(f.id)) {
            cards.push(Card {
                key: CardRef::Loose(f.id),
                column: task::live_column(f.live),
                agent: Some(f.agent),
                status: Some(f.live.status),
                title: f
                    .label
                    .clone()
                    .unwrap_or_else(|| f.agent.display_name().to_string())
                    .into(),
                prompt: None,
                group: f.group,
                repo: repo_name(f.cwd.as_deref()),
                branch: f.branch.clone(),
                cwd: f.cwd.clone(),
                ask: f.ask.clone().map(Into::into),
                question: f.question,
                diff: f.diff,
                since: None,
                tab: Some(f.id),
                resumable: false,
                paused: false,
            });
        }
        if !self.board.agent_filter.is_empty() {
            cards.retain(|c| {
                c.agent
                    .is_some_and(|a| self.board.agent_filter.contains(&a))
            });
        }
        if let Some(only) = self.board.group_filter {
            cards.retain(|c| c.group == Some(only));
        }
        cards
    }

    fn card(&self, key: CardRef, window: &Window, cx: &App) -> Option<Card> {
        self.board_cards(Some(window), true, cx)
            .into_iter()
            .find(|c| c.key == key)
    }

    /// The terminal the card's agent runs in, if it is open.
    fn card_view(
        &self,
        tab: Option<TabId>,
        cx: &App,
    ) -> Option<Entity<crate::terminal::view::TerminalView>> {
        let tab = self.tabs.iter().find(|t| Some(t.tree_id.get()) == tab)?;
        tab.pane
            .terminals()
            .into_iter()
            .find(|l| l.read(cx).agent().is_some())
    }

    // ---- selection, peek, keys --------------------------------------------

    fn select_card(
        &mut self,
        key: CardRef,
        peek: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self.board.selected != Some(key);
        self.board.selected = Some(key);
        if peek {
            self.board.peek = true;
            if changed || self.board.files.as_ref().map(|(k, _)| *k) != Some(key) {
                self.load_card_files(key, window, cx);
            }
            self.ensure_reply(window, cx);
            if let Some(card) = self.card(key, window, cx) {
                self.set_reply_placeholder(&card, window, cx);
            }
        }
        cx.notify();
    }

    fn close_peek(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.board.peek = false;
        window.focus(&self.board.focus, cx);
        cx.notify();
    }

    /// Jumps to the tab a card's agent runs in.
    fn open_card_tab(&mut self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|t| t.tree_id.get() == tab) else {
            return;
        };
        self.main_view = MainView::Terminal;
        self.board.composer = None;
        self.board.peek = false;
        self.activate(index, window, cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Enter on a card, or a double click: into its terminal when there is
    /// one.
    fn open_card(&mut self, key: CardRef, window: &mut Window, cx: &mut Context<Self>) {
        match (self.card(key, window, cx).and_then(|c| c.tab), key) {
            (Some(tab), _) => self.open_card_tab(tab, window, cx),
            // Nothing running to look at: a task that has not run yet opens
            // for editing.
            (None, CardRef::Task(id)) => self.open_composer(Some(id), window, cx),
            (None, CardRef::Loose(_)) => self.select_card(key, true, window, cx),
        }
    }

    fn board_key(&mut self, ev: &gpui::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let typing = self
            .board
            .reply
            .as_ref()
            .is_some_and(|r| r.focus_handle(cx).is_focused(window));
        if k.key == "escape" {
            if typing {
                window.focus(&self.board.focus, cx);
            } else if self.board.peek {
                self.close_peek(window, cx);
            } else if self.board.selected.take().is_some() {
                cx.notify();
            } else {
                self.close_board(window, cx);
            }
            cx.stop_propagation();
            return;
        }
        if typing || k.modifiers.modified() {
            return;
        }
        match k.key.as_str() {
            "c" | "n" => self.open_composer(None, window, cx),
            "enter" => match self.board.selected {
                Some(key) => self.open_card(key, window, cx),
                None => return,
            },
            "up" | "down" | "left" | "right" => self.move_selection(&k.key, window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    /// Arrow keys: along a column, or to the nearest card in the next column
    /// that has one.
    fn move_selection(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let cards = self.board_cards(Some(window), false, cx);
        let grid: Vec<Vec<CardRef>> = Column::ALL
            .iter()
            .map(|col| {
                cards
                    .iter()
                    .filter(|c| c.column == *col)
                    .map(|c| c.key)
                    .collect()
            })
            .collect();
        let at = self.board.selected.and_then(|sel| {
            grid.iter()
                .enumerate()
                .find_map(|(c, col)| col.iter().position(|k| *k == sel).map(|r| (c, r)))
        });
        let next = match at {
            None => grid.iter().position(|c| !c.is_empty()).map(|c| (c, 0)),
            Some((c, r)) => match key {
                "down" => Some((c, (r + 1).min(grid[c].len() - 1))),
                "up" => Some((c, r.saturating_sub(1))),
                _ => {
                    let step: isize = if key == "right" { 1 } else { -1 };
                    let mut n = c as isize + step;
                    while n >= 0 && (n as usize) < grid.len() && grid[n as usize].is_empty() {
                        n += step;
                    }
                    match n >= 0 && (n as usize) < grid.len() {
                        true => Some((n as usize, r.min(grid[n as usize].len() - 1))),
                        false => Some((c, r)),
                    }
                }
            },
        };
        if let Some((c, r)) = next {
            let key = grid[c][r];
            let peek = self.board.peek;
            self.select_card(key, peek, window, cx);
        }
    }

    // ---- moves --------------------------------------------------------------

    /// Moves a card to `to`, by asking its agent for whatever that move means.
    pub(crate) fn board_move(
        &mut self,
        key: CardRef,
        to: Column,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(card) = self.card(key, window, cx) else {
            return;
        };
        if !moves_from(card.column).contains(&to) {
            return;
        }
        match (card.column, to) {
            (Column::Queued, Column::Running) => self.start_card(&card, window, cx),
            (_, Column::Queued) => self.pause_card(&card, window, cx),
            (Column::NeedsInput, Column::Running) if !card.question => {
                self.allow_card(&card, window, cx)
            }
            (_, Column::Running) => self.focus_reply(key, window, cx),
            (_, Column::Done) => self.mark_done(key, window, cx),
            _ => {}
        }
    }

    /// Starts a queued card: a paused task's agent is told to carry on, a
    /// task nobody has run gets a run, and an agent open with nothing to do
    /// needs telling what to do first.
    fn start_card(&mut self, card: &Card, window: &mut Window, cx: &mut Context<Self>) {
        match card.key {
            CardRef::Task(id) => match (card.tab, self.task(id)) {
                (Some(_), Some(mut task)) => {
                    if let Some(view) = self.card_view(card.tab, cx) {
                        view.read(cx).send_agent_prompt(CONTINUE);
                    }
                    task.paused = None;
                    task.done = None;
                    let title = task.title.clone();
                    if self.save_task(task, window, cx) {
                        self.flash(
                            t_fmt(L10nKey::BoardToastStarted, &[("title", &title)]),
                            None,
                            cx,
                        );
                    }
                }
                (None, Some(_)) => self.start_task(id, None, window, cx),
                _ => {}
            },
            CardRef::Loose(_) => self.focus_reply(card.key, window, cx),
        }
    }

    /// Interrupts the card's agent and sends the card back to the queue.
    fn pause_card(&mut self, card: &Card, window: &mut Window, cx: &mut Context<Self>) {
        let mut task = match card.key {
            CardRef::Task(id) => self.task(id),
            CardRef::Loose(tab) => self.loose_task(tab, window, cx),
        };
        let Some(task) = task.as_mut() else {
            return;
        };
        if let Some(view) = self.card_view(card.tab, cx) {
            view.read(cx).send_keys(b"\x1b");
        }
        task.paused = Some(unix_now());
        let title = task.title.clone();
        if self.save_task(task.clone(), window, cx) {
            self.flash(
                t_fmt(L10nKey::BoardToastPaused, &[("title", &title)]),
                None,
                cx,
            );
        }
    }

    /// Takes the choice a permission prompt has highlighted, which is "yes"
    /// for every agent the board can start.
    fn allow_card(&mut self, card: &Card, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.card_view(card.tab, cx) else {
            return;
        };
        view.read(cx).send_keys(b"\r");
        self.unpause(card.key, window, cx);
        self.flash(
            t_fmt(L10nKey::BoardToastAllowed, &[("title", &card.title)]),
            None,
            cx,
        );
    }

    /// Sends `text` to the card's agent, as if typed at its prompt.
    fn send_reply(
        &mut self,
        key: CardRef,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let Some(card) = self.card(key, window, cx) else {
            return;
        };
        let Some(view) = self.card_view(card.tab, cx) else {
            return;
        };
        view.read(cx).send_agent_prompt(text);
        self.unpause(key, window, cx);
        let agent = card.agent.map_or("", |a| a.display_name());
        self.flash(
            t_fmt(
                L10nKey::BoardToastSent,
                &[("agent", agent), ("title", &card.title)],
            ),
            None,
            cx,
        );
    }

    fn unpause(&mut self, key: CardRef, window: &mut Window, cx: &mut Context<Self>) {
        if let CardRef::Task(id) = key
            && let Some(mut task) = self.task(id)
            && (task.paused.is_some() || task.done.is_some())
        {
            task.paused = None;
            task.done = None;
            self.save_task(task, window, cx);
        }
    }

    fn mark_done(&mut self, key: CardRef, window: &mut Window, cx: &mut Context<Self>) {
        let facts = self.tab_facts(Some(window), false, cx);
        let live = |run: &Run| {
            run.tab
                .and_then(|id| facts.iter().find(|f| f.id == id))
                .map(|f| f.live)
        };
        let (mut task, undo) = match key {
            CardRef::Task(id) => {
                let Some(task) = self.task(id) else {
                    return;
                };
                (task.clone(), Undo::Put(task))
            }
            CardRef::Loose(tab) => {
                let Some(task) = self.loose_task(tab, window, cx) else {
                    return;
                };
                let id = task.id;
                (task, Undo::Remove(id))
            }
        };
        task.paused = None;
        task.done = Some(Done {
            at: unix_now(),
            turns: task::live_turns(&task, live),
        });
        let title = task.title.clone();
        if self.save_task(task, window, cx) {
            self.flash(
                t_fmt(L10nKey::BoardToastDone, &[("title", &title)]),
                Some(undo),
                cx,
            );
        }
    }

    fn reopen(&mut self, id: TaskId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut task) = self.task(id) else {
            return;
        };
        let before = task.clone();
        task.done = None;
        let title = task.title.clone();
        if self.save_task(task, window, cx) {
            self.flash(
                t_fmt(L10nKey::BoardToastReopened, &[("title", &title)]),
                Some(Undo::Put(before)),
                cx,
            );
        }
    }

    fn remove_task(&mut self, id: TaskId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task) = self.task(id) else {
            return;
        };
        let title = task.title.clone();
        if self.delete_task(id, window, cx) {
            self.flash(
                t_fmt(L10nKey::BoardToastRemoved, &[("title", &title)]),
                Some(Undo::Put(task)),
                cx,
            );
        }
    }

    /// A task for a loose card, holding the tab it runs in.
    fn loose_task(&self, tab: TabId, window: &Window, cx: &App) -> Option<Task> {
        let facts = self.tab_facts(Some(window), true, cx);
        let f = facts.into_iter().find(|f| f.id == tab)?;
        let title = f
            .label
            .clone()
            .unwrap_or_else(|| f.agent.display_name().to_string());
        let title: String = title.chars().take(task::MAX_TITLE_CHARS).collect();
        let mut task = Task::new(title);
        task.cwd = f.cwd.map(|p| p.display().to_string());
        task.agent = Some(f.agent);
        task.group = f.group;
        task.push_run(Run {
            agent: f.agent,
            tab: Some(tab),
            session_id: None,
            started: unix_now(),
            worktree: None,
        });
        Some(task)
    }

    fn resume_task(&mut self, id: TaskId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut task) = self.task(id) else {
            return;
        };
        let Some(run) = task.runs.last().cloned() else {
            return;
        };
        let Some(session) = run.session_id.clone() else {
            return;
        };
        let cwd = run
            .worktree
            .as_ref()
            .or(task.cwd.as_ref())
            .map(PathBuf::from);
        let before = self.tabs.len();
        self.resume_session(run.agent, &session, cwd, false, window, cx);
        // Nothing opened, and the reason is on screen already; the run must
        // not be pinned to whichever tab happened to be active.
        if self.tabs.len() == before {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        task.push_run(Run {
            tab: Some(tab.tree_id.get()),
            started: unix_now(),
            ..run
        });
        task.done = None;
        task.paused = None;
        self.save_task(task, window, cx);
    }

    /// Starts an agent on `task`, told what to do — in a worktree of its own
    /// when the task asks for one.
    fn start_task(
        &mut self,
        id: TaskId,
        agent: Option<CLIAgent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = self.task(id) else {
            return;
        };
        let Some(agent) = agent
            .or(task.agent)
            .or_else(|| self.offered_agents(cx).first().copied())
        else {
            window.push_notification(t(L10nKey::BoardNoAgent), cx);
            return;
        };
        let overrides = &cx.global::<Config>().agent_launch;
        let launch = agent.launch_command(overrides);
        let command = match task::prompt_flag(agent) {
            Some(flag) => {
                let ask = crate::core::shell_quote::quote_for_shell(&one_line(task.ask()), None);
                match flag {
                    Some(flag) => format!("{launch} {flag} {ask}"),
                    None => format!("{launch} {ask}"),
                }
            }
            // The agent takes no prompt it would stay open after answering,
            // so it starts bare and the prompt waits on the clipboard.
            None => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(task.ask().to_string()));
                window.push_notification(
                    t_fmt(
                        L10nKey::BoardPromptCopied,
                        &[("agent", agent.display_name())],
                    ),
                    cx,
                );
                launch
            }
        };
        let cwd = task.cwd.as_ref().map(PathBuf::from);
        if !task.worktree {
            self.launch_run(task, agent, command, cwd, None, window, cx);
            return;
        }
        // The worktree is cut first, off the thread — it is a `git worktree
        // add` on whichever machine the workspace is on — and the run opens
        // in it once it exists.
        let Some(cwd) = cwd else {
            window.push_notification(t(L10nKey::BoardWorktreeNeedsRepo), cx);
            return;
        };
        let host_id = self.spawn_host(cx);
        let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, host_id) else {
            window.push_notification(t(L10nKey::BoardUnavailable), cx);
            return;
        };
        let slug = task::branch_slug(&task.title);
        crate::ui::host_ops::HostOps::run_in(
            host,
            window,
            cx,
            move |h| crate::core::worktree::create_for(h, &cwd, slug.as_deref()),
            move |this, made, window, cx| match made {
                Ok(wt) => {
                    let path = wt.path.clone();
                    this.launch_run(
                        task,
                        agent,
                        command,
                        Some(path.clone()),
                        Some(path),
                        window,
                        cx,
                    )
                }
                Err(e) => window
                    .push_notification(t_fmt(L10nKey::BoardWorktreeFailed, &[("error", &e)]), cx),
            },
        );
    }

    /// Opens the tab a run of `task` works in and starts `agent` there.
    ///
    /// The window stays on the board: the card moving to Running is the
    /// answer, and whoever wants to watch can open it.
    #[allow(clippy::too_many_arguments)]
    fn launch_run(
        &mut self,
        mut task: Task,
        agent: CLIAgent,
        command: String,
        cwd: Option<PathBuf>,
        worktree: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let on_board = self.board_open();
        let Some(slot) = self.new_tab_slot(cwd, None, window, cx) else {
            return;
        };
        // The tab is named for the task, so its sidebar row says what it is
        // for rather than whatever the agent titles itself, and it is filed
        // where the task is.
        let group = task.group.filter(|g| self.sidebar_groups.contains(*g));
        let tab = &mut self.tabs[self.active];
        tab.name = Some(task.title.clone());
        if group.is_some() {
            tab.group.set(group);
        }
        let tab_id = tab.tree_id.get();
        crate::ui::agent_launch::run_when_ready(&slot, command, cx);
        if on_board {
            self.main_view = MainView::Board;
            self.focus_active(window, cx);
        }
        task.push_run(Run {
            agent,
            tab: Some(tab_id),
            session_id: None,
            started: unix_now(),
            worktree: worktree.map(|p| p.display().to_string()),
        });
        task.agent = Some(agent);
        task.done = None;
        task.paused = None;
        let title = task.title.clone();
        if self.save_task(task, window, cx) {
            self.flash(
                t_fmt(L10nKey::BoardToastStarted, &[("title", &title)]),
                None,
                cx,
            );
        }
        self.save_session(cx);
    }

    /// Keeps each open run's session id written down, so a run whose tab
    /// closes can still be resumed, and lifts a pause once its agent is
    /// working again. Called on every frame, board or not; only sends
    /// anything when something changed, and quietly: a window whose tree has
    /// not landed tries again next frame.
    fn note_runs(&mut self, cx: &mut Context<Self>) {
        let now = unix_now();
        let mut changed = Vec::new();
        for task in &self.board.tasks {
            let mut next = task.clone();
            let mut dirty = false;
            for run in next.runs.iter_mut() {
                let Some(view) = self.card_view(run.tab, cx) else {
                    continue;
                };
                let session = view.read(cx).agent_session();
                let id = session.as_ref().and_then(|s| s.session_id.clone());
                if id.is_some() && id != run.session_id {
                    run.session_id = id;
                    dirty = true;
                }
                let working = session.is_some_and(|s| s.status == AgentStatus::Working);
                if working
                    && next
                        .paused
                        .is_some_and(|at| now.saturating_sub(at) > PAUSE_GRACE_SECS)
                {
                    next.paused = None;
                    dirty = true;
                }
            }
            if dirty {
                changed.push(next);
            }
        }
        for task in changed {
            if crate::ui::tree_sync::push_task(cx, self.workspace, task.clone()) {
                self.board_task_put(task);
            }
        }
    }

    // ---- toast --------------------------------------------------------------

    fn flash(&mut self, text: String, undo: Option<Undo>, cx: &mut Context<Self>) {
        self.board.toast_seq += 1;
        let seq = self.board.toast_seq;
        self.board.toast = Some(Toast {
            text: text.into(),
            undo,
        });
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(TOAST_MS))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.board.toast_seq == seq {
                    this.board.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(undo) = self.board.toast.take().and_then(|t| t.undo) else {
            return;
        };
        match undo {
            Undo::Put(task) => {
                self.save_task(task, window, cx);
            }
            Undo::Remove(id) => {
                self.delete_task(id, window, cx);
            }
        }
        cx.notify();
    }

    // ---- files --------------------------------------------------------------

    /// Asks git which files the card's checkout has changed, for the peek
    /// panel. Against HEAD, so a worktree shows what its run has done.
    fn load_card_files(&mut self, key: CardRef, window: &mut Window, cx: &mut Context<Self>) {
        self.board.files = None;
        let Some(cwd) = self.card(key, window, cx).and_then(|c| c.cwd) else {
            return;
        };
        let host_id = self.spawn_host(cx);
        let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, host_id) else {
            return;
        };
        crate::ui::host_ops::HostOps::run_in(
            host,
            window,
            cx,
            move |h| match h.git(&cwd, &["diff", "--numstat", "HEAD"]) {
                Ok(out) if out.success() => {
                    Ok(parse_numstat(&String::from_utf8_lossy(&out.stdout)))
                }
                Ok(out) => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
                Err(e) => Err(e.to_string()),
            },
            move |this, files, _window, cx| {
                if this.board.selected == Some(key) {
                    this.board.files = Some((key, files));
                    cx.notify();
                }
            },
        );
    }
}

fn repo_name(cwd: Option<&std::path::Path>) -> Option<String> {
    cwd.and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
}

/// `git diff --numstat`: `added<TAB>removed<TAB>path` a line, `-` for the
/// counts of a binary file.
fn parse_numstat(text: &str) -> Vec<FileChange> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let added = parts.next()?.parse().unwrap_or(0);
            let removed = parts.next()?.parse().unwrap_or(0);
            let path = parts.next()?.to_string();
            Some(FileChange {
                path,
                added,
                removed,
            })
        })
        .collect()
}

/// A prompt as one command-line argument. The command is typed into a shell,
/// where a newline would submit it half-way; the agent reads the paragraphs
/// joined just as well.
fn one_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// How long ago `since` was, in the board's short form: `now`, `12m`, `3h`,
/// `2d`.
fn ago(since: u64) -> String {
    let secs = unix_now().saturating_sub(since);
    match secs {
        0..60 => t(L10nKey::BoardNow).to_string(),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_goes_on_one_line() {
        assert_eq!(one_line("Fix it.\n\n  Then test.\n"), "Fix it. Then test.");
        assert_eq!(one_line("one"), "one");
    }

    #[test]
    fn numstat_reads_counts_and_binary_files() {
        let files = parse_numstat("3\t1\tsrc/a.rs\n-\t-\tlogo.png\n");
        assert_eq!(
            files,
            vec![
                FileChange {
                    path: "src/a.rs".into(),
                    added: 3,
                    removed: 1
                },
                FileChange {
                    path: "logo.png".into(),
                    added: 0,
                    removed: 0
                },
            ]
        );
    }

    #[test]
    fn only_the_moves_an_agent_can_make_are_offered() {
        assert_eq!(moves_from(Column::Queued), &[Column::Running]);
        assert!(moves_from(Column::Done).is_empty());
        assert!(moves_from(Column::Review).contains(&Column::Done));
        assert!(!moves_from(Column::Running).contains(&Column::Done));
    }

    #[test]
    fn a_waiting_question_is_answered_not_allowed() {
        assert_eq!(
            move_verb(Column::NeedsInput, Column::Running, true),
            L10nKey::BoardVerbReply
        );
        assert_eq!(
            move_verb(Column::NeedsInput, Column::Running, false),
            L10nKey::BoardVerbAllow
        );
    }
}

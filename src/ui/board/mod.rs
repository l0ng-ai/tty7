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
//! on every frame; nothing here stores a column, beyond holding the last one
//! drawn while a tab's agent cannot be read yet ([`Held`]).
//!
//! Moving a card is asking its agent to do something, so only the moves an
//! agent can make are offered: start a queued task, pause a running one,
//! allow what a waiting one asks, send a finished one back with changes, or
//! call it done. A drag, a button on the card and the peek panel all end in
//! [`Tty7App::board_move`].

mod cleanup;
mod composer;
mod peek;
mod view;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::{
    App, Context, Entity, EntityId, FocusHandle, Focusable as _, SharedString, Subscription, Window,
};
use gpui_component::WindowExt as _;
use gpui_component::input::InputState;
use gpui_component::menu::PopupMenu;
use tty7_core::core::group_key::GroupId;
use tty7_core::core::machine::TabId;
use tty7_core::core::task::{self, Column, Done, DoneMarks, Live, Run, Task, TaskId};
use tty7_core::core::worktree::setup;

use crate::core::cli_agent::{AgentSessionState, AgentStatus, CLIAgent};
use crate::core::config::{Config, unix_now};
use crate::terminal::view::TerminalView;
use crate::ui::app::{Tab, Tty7App};
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::pane::PaneSlot;

pub(crate) use composer::Composer;

/// The key context the board declares, for the bindings that only mean
/// something on it.
pub(crate) const KEY_CONTEXT: &str = "Board";

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
    /// The card whose words the reply box holds. A reply goes to it, not to
    /// whichever card happens to be selected when Enter lands.
    reply_for: Option<CardRef>,
    /// What was typed to the other cards, put back when one is selected again.
    drafts: HashMap<CardRef, String>,
    /// The card and column the reply box's placeholder was last worded for.
    reply_shape: Option<(CardRef, Column)>,
    /// The selected card's changed files, once `git diff` has answered.
    files: Option<(CardRef, Result<Vec<FileChange>, String>)>,
    /// The card being dragged, while one is.
    dragging: Option<CardDrag>,
    toast: Option<Toast>,
    toast_seq: u64,
    /// What ⌘Z walks back, newest last.
    undo: UndoStack,
    /// Done cards older than [`DONE_RECENT_SECS`] are on show.
    show_old_done: bool,
    /// Tasks whose worktree is being cut, before their agent has a tab.
    starting: HashSet<TaskId>,
    /// The column each task was last drawn in, held while its tab's agent
    /// has gone quiet — see [`Held`].
    held: RefCell<HashMap<TaskId, Held>>,
    /// Whether each finished run's worktree was still on disk, and when that
    /// was asked — so a drawn board stats each one every few seconds rather
    /// than every frame.
    worktree_seen: RefCell<HashMap<String, (bool, std::time::Instant)>>,
    /// The zoom the active tab had when the board opened, put back when the
    /// board closes.
    zoom: Option<(TabId, Entity<TerminalView>)>,
    /// A card's menu opened from the keyboard, and what closes it.
    menu: Option<(CardRef, Entity<PopupMenu>, Subscription)>,
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
            reply_for: None,
            drafts: HashMap::new(),
            reply_shape: None,
            files: None,
            dragging: None,
            toast: None,
            toast_seq: 0,
            undo: UndoStack::default(),
            show_old_done: false,
            starting: HashSet::new(),
            held: RefCell::new(HashMap::new()),
            worktree_seen: RefCell::new(HashMap::new()),
            zoom: None,
            menu: None,
        }
    }
}

/// Which card a click, a drag or a key is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CardRef {
    Task(TaskId),
    /// A tab running an agent that no task claims.
    Loose(TabId),
}

/// What the user was looking at when they asked for a move: the column the
/// card was drawn in and what it was stopped on. The move is only made while
/// the card still says that — an agent can go on to a different prompt
/// between the frame that drew a button and the click on it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Seen {
    column: Column,
    ask: Option<SharedString>,
}

impl Seen {
    fn of(card: &Card) -> Seen {
        Seen {
            column: card.column,
            ask: card.ask.clone(),
        }
    }
}

/// What a dragged card carries.
#[derive(Clone, Debug)]
pub(crate) struct CardDrag {
    key: CardRef,
    seen: Seen,
    /// It is waiting on a question, which a drop into Running answers with
    /// words rather than a yes.
    question: bool,
    title: SharedString,
    agent: Option<CLIAgent>,
}

impl CardDrag {
    fn from(&self) -> Column {
        self.seen.column
    }
}

/// One changed file, from `git diff --numstat`.
#[derive(Clone, Debug, PartialEq)]
struct FileChange {
    path: String,
    added: u32,
    removed: u32,
}

/// The note at the bottom of the board saying what just happened, with the
/// way back when there is one — the newest entry on [`UndoStack`].
struct Toast {
    text: SharedString,
    undo: bool,
}

/// Putting the board back the way it was. Only for what the board itself
/// changed: a keystroke already sent to an agent cannot be taken back.
///
/// Each one names only what its change touched, and is laid over the card
/// as it is when undone: the card may have picked up a run, a session id or
/// another window's edit since, none of which an undo should take away.
#[derive(Clone, Debug, PartialEq)]
enum Undo {
    /// Take off a card the board just put on.
    Remove(TaskId),
    /// Put back a card the board just took off, as it was.
    Restore(Task),
    /// Put back what an edit changed: the fields the task sheet writes.
    Edit(Task),
    /// Put back a card's done and paused marks.
    Marks(Marks),
}

/// A card's done and paused marks, as they stood.
#[derive(Clone, Debug, PartialEq)]
struct Marks {
    id: TaskId,
    done: Option<Done>,
    done_marks: Option<DoneMarks>,
    paused: Option<u64>,
}

impl Marks {
    fn of(task: &Task) -> Marks {
        Marks {
            id: task.id,
            done: task.done,
            done_marks: task.done_marks.clone(),
            paused: task.paused,
        }
    }
}

/// What an [`Undo`] comes to against the board as it is now.
#[derive(Debug, PartialEq)]
enum UndoStep {
    Save(Box<Task>),
    Delete(TaskId),
}

impl Undo {
    fn task(&self) -> TaskId {
        match self {
            Undo::Remove(id) => *id,
            Undo::Restore(t) | Undo::Edit(t) => t.id,
            Undo::Marks(m) => m.id,
        }
    }

    /// `current` is the card with this undo's id, if the board still has it.
    /// `None` when there is nothing left to put back: the card is gone (or,
    /// for a removal, back already).
    fn step(self, current: Option<&Task>) -> Option<UndoStep> {
        match (self, current) {
            (Undo::Remove(id), Some(_)) => Some(UndoStep::Delete(id)),
            (Undo::Restore(task), None) => Some(UndoStep::Save(Box::new(task))),
            (Undo::Edit(before), Some(now)) => {
                let mut task = now.clone();
                task.title = before.title;
                task.prompt = before.prompt;
                task.agent = before.agent;
                task.cwd = before.cwd;
                task.group = before.group;
                task.worktree = before.worktree;
                task.branch = before.branch;
                Some(UndoStep::Save(Box::new(task)))
            }
            (Undo::Marks(m), Some(now)) => {
                let mut task = now.clone();
                task.done = m.done;
                task.done_marks = m.done_marks;
                task.paused = m.paused;
                Some(UndoStep::Save(Box::new(task)))
            }
            _ => None,
        }
    }
}

/// How many of the board's own changes ⌘Z can walk back.
const UNDO_DEPTH: usize = 10;

#[derive(Default)]
struct UndoStack(Vec<Undo>);

impl UndoStack {
    fn push(&mut self, undo: Undo) {
        self.0.push(undo);
        if self.0.len() > UNDO_DEPTH {
            self.0.remove(0);
        }
    }

    fn pop(&mut self) -> Option<Undo> {
        self.0.pop()
    }
}

/// A task's column as last drawn, and since when its agent has been silent.
///
/// A tab whose terminal is still connecting — the app just started, a link
/// dropped and is coming back — has no agent to read yet, and a run with no
/// agent reads as over. Without this every such card would flash into Review
/// (and the sidebar's count drop to nothing) until the agent's status landed.
/// So a card whose run's tab is open but silent keeps the column it was last
/// drawn in: for as long as the tab is reconnecting, and for [`HOLD_SECS`]
/// after its agent goes quiet otherwise — the moment between a terminal
/// coming up and its agent's status arriving. An agent that really quit is
/// read as over once that passes.
#[derive(Clone, Copy, Debug)]
struct Held {
    column: Column,
    silent_since: Option<u64>,
}

const HOLD_SECS: u64 = 5;

/// How long a worktree's being on disk, or not, is taken as read.
const WORKTREE_RECHECK: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// The answers a waiting question offers, when its tool call named them.
    options: Vec<String>,
    question: bool,
    diff: Option<(u32, u32)>,
    /// When this card's current state began, in Unix seconds, if known.
    since: Option<u64>,
    /// The open tab a run of this card lives in.
    tab: Option<TabId>,
    /// The pane in [`Self::tab`] this card was read from — see
    /// [`speaking_leaf`]. What the card's buttons type goes to this one.
    leaf: Option<EntityId>,
    /// A finished run this card could pick back up.
    resumable: bool,
    paused: bool,
    /// Its last run worked in a worktree that is no longer there: nothing to
    /// reopen or resume, only to start again.
    worktree_gone: bool,
    /// Its worktree is being cut; no move is offered until its agent's tab
    /// opens.
    starting: bool,
}

/// What the board reads off one open tab.
struct TabFacts {
    id: TabId,
    /// The pane that speaks for the tab.
    leaf: EntityId,
    agent: CLIAgent,
    live: Live,
    ask: Option<String>,
    options: Vec<String>,
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
        (Column::NeedsInput, Column::Running) if question => L10nKey::BoardAnswerInTerminal,
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

/// How long a finished card stays on show in Done before it folds away.
const DONE_RECENT_SECS: u64 = 7 * 24 * 3600;

/// Whether a card is folded out of sight: finished more than
/// [`DONE_RECENT_SECS`] before `now`, while the older ones have not been
/// asked for. What the Done column draws and what the arrow keys reach are
/// both this.
fn folded(column: Column, since: Option<u64>, now: u64, show_old: bool) -> bool {
    column == Column::Done
        && !show_old
        && since.is_some_and(|at| at < now.saturating_sub(DONE_RECENT_SECS))
}

/// The group filter still worth keeping: one whose group is still pinned.
/// A filter on a group since unpinned would hide every card with nothing
/// on the header left to undo it.
fn live_group_filter(filter: Option<GroupId>, pinned: impl Fn(GroupId) -> bool) -> Option<GroupId> {
    filter.filter(|g| pinned(*g))
}

/// The pane that speaks for a tab on the board: its most urgent agent pane,
/// the same one its sidebar row reports. A card is read off this pane, and
/// what the card's buttons send goes to it, so a split tab with two agents
/// never shows one and answers the other.
fn speaking_leaf(tab: &Tab, cx: &App) -> Option<Entity<TerminalView>> {
    let urgency = crate::ui::tray::urgency;
    tab.pane
        .terminals()
        .into_iter()
        .filter(|l| l.read(cx).agent().is_some())
        .max_by_key(|l| l.read(cx).agent_session().map_or(0, |s| urgency(s.status)))
}

/// What a waiting agent is stopped on, in words: a question says itself, a
/// permission prompt says what it would run.
fn waiting_ask(session: &AgentSessionState) -> Option<String> {
    session
        .ask
        .as_ref()
        .map(|a| a.question.clone())
        .or_else(|| session.message.clone())
}

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
        // The board covers the panes, so a zoom has nothing to show; it is
        // set aside, not dropped, and comes back with the terminal.
        let tab = self.tabs.get(self.active).map(|t| t.tree_id.get());
        if let (Some(tab), Some(leaf)) = (tab, self.maximized.take()) {
            self.board.zoom = Some((tab, leaf));
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    pub(crate) fn close_board(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.main_view == MainView::Terminal {
            return;
        }
        self.leave_board_view();
        self.board.composer = None;
        self.board.peek = false;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Shows the terminal again, with the zoom the board set aside back on
    /// the tab it came from: in force if that tab is still the active one,
    /// parked on it otherwise, the way `activate` parks a zoom.
    pub(crate) fn leave_board_view(&mut self) {
        self.main_view = MainView::Terminal;
        let Some((tab, leaf)) = self.board.zoom.take() else {
            return;
        };
        let Some(i) = self.tabs.iter().position(|t| t.tree_id.get() == tab) else {
            return;
        };
        let still_there = self.tabs[i]
            .pane
            .leaves()
            .iter()
            .any(|l| l.entity_id() == leaf.entity_id());
        if !still_there {
            return;
        }
        if i == self.active {
            self.maximized.get_or_insert(leaf);
        } else {
            self.tabs[i].zoomed.get_or_insert(leaf);
        }
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
        self.board.held.borrow_mut().remove(&task);
        self.board.tasks.retain(|t| t.id != task);
    }

    /// How many cards are stopped on the user — the sidebar row's count.
    ///
    /// Every card, whatever the board's header is filtering: the row says
    /// what is waiting in this workspace, not what one view of it shows.
    pub(crate) fn board_needs_input(&self, cx: &App) -> usize {
        self.all_cards(None, false, cx)
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
        // A new card on a full board: the machine makes room by dropping
        // the card done longest ago, and says so to every window — or, with
        // nothing done to drop, refuses. Asked here first so the refusal is
        // on screen rather than lost on the way.
        if !self.board.tasks.iter().any(|t| t.id == task.id)
            && let Err(why) = task::make_room(&self.board.tasks, task.id)
        {
            window.push_notification(why, cx);
            return false;
        }
        if !crate::ui::tree_sync::push_task(cx, self.workspace, task.clone()) {
            window.push_notification(t(L10nKey::BoardUnavailable), cx);
            return false;
        }
        // The user just moved it: a column held for it is out of date.
        self.board.held.borrow_mut().remove(&task.id);
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
        self.tabs
            .iter()
            .filter_map(|tab| {
                let leaf = speaking_leaf(tab, cx)?;
                let view = leaf.read(cx);
                let session = view.agent_session().unwrap_or_default();
                let waiting = session.status == AgentStatus::Waiting;
                let git = full.then(|| view.git_status(cx)).flatten();
                Some(TabFacts {
                    id: tab.tree_id.get(),
                    leaf: leaf.entity_id(),
                    agent: view.agent()?,
                    live: Live::from(&session),
                    ask: waiting.then(|| waiting_ask(&session)).flatten(),
                    options: match waiting {
                        true => session
                            .ask
                            .as_ref()
                            .map(|a| a.options.clone())
                            .unwrap_or_default(),
                        false => Vec::new(),
                    },
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

    /// Whether tab `id` is still connecting a pane — the app starting onto
    /// it, or a dropped link coming back — so it cannot yet say what agent
    /// runs there.
    fn tab_connecting(&self, id: TabId, cx: &App) -> bool {
        self.tabs
            .iter()
            .find(|t| t.tree_id.get() == id)
            .is_some_and(|tab| {
                tab.pane.leaves().iter().any(|slot| match slot {
                    PaneSlot::Connecting(_) => true,
                    PaneSlot::Ready(view) => {
                        let term = &view.read(cx).terminal;
                        term.exited && !term.child_exited()
                    }
                })
            })
    }

    /// Every card on the board this frame, filtered as the header says.
    fn board_cards(&self, window: Option<&Window>, full: bool, cx: &App) -> Vec<Card> {
        self.filter_cards(self.all_cards(window, full, cx))
    }

    /// `cards`, less those the header's filters hide.
    fn filter_cards(&self, mut cards: Vec<Card>) -> Vec<Card> {
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

    /// Every card on the board this frame, whatever the header filters.
    ///
    /// `full` is for a drawn board: it reads what only a card on screen
    /// shows, and asks the disk whether each finished run's worktree is still
    /// there — nothing a count or a key press needs every frame.
    fn all_cards(&self, window: Option<&Window>, full: bool, cx: &App) -> Vec<Card> {
        let facts = self.tab_facts(window, full, cx);
        let find = |tab: Option<TabId>| tab.and_then(|id| facts.iter().find(|f| f.id == id));
        let tab_open = |id: TabId| self.tabs.iter().any(|t| t.tree_id.get() == id);
        // Whether a worktree is still there can only be asked of this
        // computer's disk; a remote one is taken to be.
        let local = full && self.can_spawn_locally(cx);
        let now = unix_now();
        // Each tab's agent status as the machine tree last recorded it, for a
        // tab whose terminal cannot say yet — see `Held`. Read only once
        // such a tab turns up.
        let remembered = std::cell::OnceCell::<HashMap<TabId, AgentStatus>>::new();
        let remembered_status = |tab: TabId| {
            remembered
                .get_or_init(|| {
                    crate::ui::machine_mirror::tab_views_for(cx, self.workspace)
                        .map(|(views, _)| {
                            views
                                .into_iter()
                                .filter_map(|v| Some((v.id, v.status?)))
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .get(&tab)
                .copied()
        };
        let mut held = self.board.held.borrow_mut();
        let mut cards = Vec::new();
        for task in &self.board.tasks {
            // A run whose tab is open but whose agent has not shown up yet —
            // the shell is still starting, the command still being typed —
            // is starting, not over. Read as an agent that has not taken its
            // first turn, which `column` counts as Running. Past the grace a
            // run that never started is over after all.
            let starting = |run: &Run| {
                run.tab.is_some_and(|id| {
                    find(Some(id)).is_none() && tab_open(id) && task::just_started(run, now)
                })
            };
            let computed = task::column(task, |run| {
                find(run.tab)
                    .map(|f| f.live)
                    .or_else(|| starting(run).then_some(task::STARTING))
            });
            let starting_tab = task.runs.last().filter(|r| starting(r)).and_then(|r| r.tab);
            // The run this card reports: the newest one still open, else the
            // newest one at all.
            let open = task.runs.iter().rev().find_map(|r| find(r.tab));
            // A run whose tab is open with no agent to read in it: see `Held`.
            let silent = (open.is_none() && starting_tab.is_none())
                .then(|| {
                    task.runs
                        .iter()
                        .rev()
                        .filter_map(|r| r.tab)
                        .find(|id| tab_open(*id))
                })
                .flatten();
            let column = match (silent, held.get_mut(&task.id)) {
                (Some(tab), Some(h)) => {
                    let since = *h.silent_since.get_or_insert(now);
                    if !self.tab_connecting(tab, cx) && now.saturating_sub(since) >= HOLD_SECS {
                        // Quiet for good: what it reads as now is what it is.
                        h.column = computed;
                    }
                    h.column
                }
                // Nothing drawn yet to hold — the app just started onto a tab
                // still connecting. What the machine last heard its agent say
                // stands in until the agent can be read, then is held.
                (Some(tab), None) if self.tab_connecting(tab, cx) => {
                    let column = match remembered_status(tab) {
                        Some(AgentStatus::Waiting) => Column::NeedsInput,
                        Some(AgentStatus::Working) => Column::Running,
                        _ => computed,
                    };
                    held.insert(
                        task.id,
                        Held {
                            column,
                            silent_since: None,
                        },
                    );
                    column
                }
                (Some(_), None) => computed,
                (None, _) => {
                    held.insert(
                        task.id,
                        Held {
                            column: computed,
                            silent_since: None,
                        },
                    );
                    computed
                }
            };
            let starting_now = self.board.starting.contains(&task.id);
            let column = match starting_now {
                true => Column::Running,
                false => column,
            };
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
                options: open.map(|f| f.options.clone()).unwrap_or_default(),
                question: open.is_some_and(|f| f.question),
                diff: open.and_then(|f| f.diff),
                since: match column {
                    Column::Done => task.done.map(|d| d.at),
                    Column::Queued => task.paused.or(Some(task.created)),
                    _ => last.map(|r| r.started),
                },
                tab: open.map(|f| f.id).or(starting_tab),
                leaf: open.map(|f| f.leaf),
                resumable: open.is_none() && last.is_some_and(|r| r.session_id.is_some()),
                paused: task.paused.is_some(),
                worktree_gone: local
                    && open.is_none()
                    && last
                        .and_then(|r| r.worktree.as_deref())
                        .is_some_and(|p| !self.worktree_exists(p)),
                starting: starting_now,
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
                options: f.options.clone(),
                question: f.question,
                diff: f.diff,
                since: None,
                tab: Some(f.id),
                leaf: Some(f.leaf),
                resumable: false,
                paused: false,
                worktree_gone: false,
                starting: false,
            });
        }
        cards
    }

    /// Whether worktree `path` is on this computer's disk, as asked at most
    /// [`WORKTREE_RECHECK`] ago.
    fn worktree_exists(&self, path: &str) -> bool {
        let mut seen = self.board.worktree_seen.borrow_mut();
        match seen.get(path) {
            Some(&(there, at)) if at.elapsed() < WORKTREE_RECHECK => there,
            _ => {
                let there = std::path::Path::new(path).exists();
                seen.insert(path.to_string(), (there, std::time::Instant::now()));
                there
            }
        }
    }

    fn card(&self, key: CardRef, window: &Window, cx: &App) -> Option<Card> {
        self.all_cards(Some(window), true, cx)
            .into_iter()
            .find(|c| c.key == key)
    }

    /// The terminal a card's agent runs in, if it is open: the very pane the
    /// card was read from when it names one, else the one that speaks for
    /// the tab now.
    fn card_view(
        &self,
        tab: Option<TabId>,
        leaf: Option<EntityId>,
        cx: &App,
    ) -> Option<Entity<TerminalView>> {
        let tab = self.tabs.iter().find(|t| Some(t.tree_id.get()) == tab)?;
        match leaf {
            Some(leaf) => tab
                .pane
                .terminals()
                .into_iter()
                .find(|l| l.entity_id() == leaf),
            None => speaking_leaf(tab, cx),
        }
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
        }
        cx.notify();
    }

    /// Points whatever was about card `from` at `to`: a loose card the
    /// board has just filed as a task is the same card to the user, and the
    /// panel open on it, and what was typed to it, stay with it.
    fn repoint(&mut self, from: CardRef, to: CardRef) {
        if self.board.selected == Some(from) {
            self.board.selected = Some(to);
        }
        if self.board.reply_for == Some(from) {
            self.board.reply_for = Some(to);
        }
        if let Some(draft) = self.board.drafts.remove(&from) {
            self.board.drafts.insert(to, draft);
        }
        if let Some((k, _)) = self.board.files.as_mut().filter(|(k, _)| *k == from) {
            *k = to;
        }
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
        self.leave_board_view();
        self.board.composer = None;
        self.board.peek = false;
        self.activate(index, window, cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Enter on a card, or a double click: into its terminal when there is
    /// one. Otherwise a task nobody has run yet opens for editing, a run that
    /// can be picked back up is, and anything else opens in the panel.
    fn open_card(&mut self, key: CardRef, window: &mut Window, cx: &mut Context<Self>) {
        let Some(card) = self.card(key, window, cx) else {
            return;
        };
        if let Some(tab) = card.tab {
            self.open_card_tab(tab, window, cx);
            return;
        }
        let never_ran = match key {
            CardRef::Task(id) => self.task(id).is_some_and(|t| t.runs.is_empty()),
            CardRef::Loose(_) => false,
        };
        match key {
            CardRef::Task(id) if never_ran && !card.starting => {
                self.open_composer(Some(id), window, cx)
            }
            CardRef::Task(id) if card.resumable && !card.worktree_gone => {
                self.resume_task(id, window, cx)
            }
            _ => self.select_card(key, true, window, cx),
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
            if cx.has_active_drag() {
                // Esc lets go of a card, not of the board under it.
                cx.stop_active_drag(window);
                self.board.dragging = None;
                cx.notify();
            } else if typing {
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
        let m = &k.modifiers;
        let menu_key =
            k.key == "menu" || (k.key == "f10" && m.shift && !m.control && !m.alt && !m.platform);
        if !typing && menu_key {
            if let Some(key) = self.board.selected {
                self.open_card_menu(key, window, cx);
            }
            cx.stop_propagation();
            return;
        }
        if typing || m.modified() {
            return;
        }
        match k.key.as_str() {
            "c" | "n" => self.open_composer(None, window, cx),
            "enter" => match self.board.selected {
                Some(key) => self.open_card(key, window, cx),
                None => return,
            },
            "up" | "down" | "left" | "right" => self.move_selection(&k.key, window, cx),
            "a" | "d" | "p" | "r" => match self.board.selected {
                Some(key) => self.card_key(key, &k.key, window, cx),
                None => return,
            },
            _ => return,
        }
        cx.stop_propagation();
    }

    /// A letter on the selected card: A allows what it asks, D marks it
    /// done, P pauses it, R reopens or resumes it — each only where the card
    /// offers that move.
    fn card_key(
        &mut self,
        key: CardRef,
        letter: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(card) = self.card(key, window, cx) else {
            return;
        };
        let seen = Seen::of(&card);
        let can = |to: Column| moves_from(card.column).contains(&to);
        match (letter, key) {
            ("a", _) if card.column == Column::NeedsInput && !card.question => {
                self.board_move(key, Column::Running, &seen, window, cx)
            }
            ("d", _) if can(Column::Done) => self.board_move(key, Column::Done, &seen, window, cx),
            ("p", _) if can(Column::Queued) => {
                self.board_move(key, Column::Queued, &seen, window, cx)
            }
            ("r", CardRef::Task(id)) if card.column == Column::Done => match card.worktree_gone {
                true => self.start_again(id, window, cx),
                false => self.reopen(id, window, cx),
            },
            ("r", CardRef::Task(id)) if card.resumable && !card.worktree_gone => {
                self.resume_task(id, window, cx)
            }
            _ => {}
        }
    }

    /// Opens a card's menu from the keyboard, under the card: the same menu
    /// a right click opens.
    fn open_card_menu(&mut self, key: CardRef, window: &mut Window, cx: &mut Context<Self>) {
        let Some(card) = self.card(key, window, cx) else {
            return;
        };
        let spec = view::CardMenu::of(&card);
        let app = cx.entity().downgrade();
        let menu = PopupMenu::build(window, cx, move |menu, _, _| {
            view::card_menu(menu, &spec, app.clone())
        });
        let sub = cx.subscribe_in(
            &menu,
            window,
            |this, _, _: &gpui::DismissEvent, window, cx| {
                this.board.menu = None;
                if this.board_open() {
                    window.focus(&this.board.focus, cx);
                }
                cx.notify();
            },
        );
        menu.focus_handle(cx).focus(window, cx);
        self.board.menu = Some((key, menu, sub));
        cx.notify();
    }

    /// Arrow keys: along a column, or to the nearest card in the next column
    /// that has one. Only the cards on show: a folded Done card is not one.
    fn move_selection(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let cards = self.board_cards(Some(window), false, cx);
        let now = unix_now();
        let show_old = self.board.show_old_done;
        let grid: Vec<Vec<CardRef>> = Column::ALL
            .iter()
            .map(|col| {
                cards
                    .iter()
                    .filter(|c| c.column == *col && !folded(c.column, c.since, now, show_old))
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

    /// Moves a card to `to`, by asking its agent for whatever that move means
    /// — so long as the card still stands where `seen` saw it.
    pub(crate) fn board_move(
        &mut self,
        key: CardRef,
        to: Column,
        seen: &Seen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(card) = self.card(key, window, cx) else {
            return;
        };
        if card.starting {
            return;
        }
        if card.column != seen.column {
            self.flash(
                t_fmt(L10nKey::BoardCardMoved, &[("title", &card.title)]),
                None,
                cx,
            );
            return;
        }
        if !moves_from(card.column).contains(&to) {
            return;
        }
        match (card.column, to) {
            (Column::Queued, Column::Running) => self.start_card(&card, window, cx),
            (_, Column::Queued) => self.pause_card(&card, window, cx),
            // A question is answered where it is asked: its picker takes
            // arrow keys and a choice, which the board does not fake.
            (Column::NeedsInput, Column::Running) if card.question => {
                if let Some(tab) = card.tab {
                    self.open_card_tab(tab, window, cx)
                }
            }
            (Column::NeedsInput, Column::Running) => {
                self.allow_card(&card, seen.ask.as_ref(), window, cx)
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
                    if let Some(view) = self.card_view(card.tab, card.leaf, cx) {
                        view.read(cx).send_agent_prompt(CONTINUE);
                    }
                    task.paused = None;
                    task::reopen(&mut task);
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
        let task = match card.key {
            CardRef::Task(id) => self.task(id),
            CardRef::Loose(tab) => self.loose_task(tab, window, cx),
        };
        let Some(mut task) = task else {
            return;
        };
        let undo = match card.key {
            CardRef::Task(_) => Undo::Marks(Marks::of(&task)),
            CardRef::Loose(_) => Undo::Remove(task.id),
        };
        if let Some(view) = self.card_view(card.tab, card.leaf, cx) {
            view.read(cx).send_keys(b"\x1b");
        }
        task.paused = Some(unix_now());
        let (id, title) = (task.id, task.title.clone());
        if self.save_task(task, window, cx) {
            self.repoint(card.key, CardRef::Task(id));
            self.flash(
                t_fmt(L10nKey::BoardToastPaused, &[("title", &title)]),
                Some(undo),
                cx,
            );
        }
    }

    /// Takes the choice a permission prompt has highlighted, which is "yes"
    /// for every agent the board can start — but only on the prompt the user
    /// saw, `seen`. Read again off the pane itself: an agent that has moved
    /// on to another prompt since would otherwise be told yes to that one.
    fn allow_card(
        &mut self,
        card: &Card,
        seen: Option<&SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = self.card_view(card.tab, card.leaf, cx);
        let still = view
            .as_ref()
            .and_then(|v| v.read(cx).agent_session())
            .is_some_and(|s| {
                s.status == AgentStatus::Waiting
                    && !s.question
                    && waiting_ask(&s).as_deref() == seen.map(|a| a.as_ref())
            });
        let Some(view) = view.filter(|_| still) else {
            self.flash(
                t_fmt(L10nKey::BoardPromptChanged, &[("title", &card.title)]),
                None,
                cx,
            );
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

    /// Sends `text` to the card's agent, as if typed at its prompt. False,
    /// with the reason on screen, when the agent is not at its prompt — a
    /// permission dialog or a picker came up since the reply box was drawn,
    /// and typed words would land in it.
    fn send_reply(
        &mut self,
        key: CardRef,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return false;
        }
        let Some(card) = self.card(key, window, cx) else {
            return false;
        };
        // Its agent's tab closed since the box was offered: say so rather
        // than swallow what was typed, which stays in the box.
        let Some(view) = self.card_view(card.tab, card.leaf, cx) else {
            self.flash(
                t_fmt(L10nKey::BoardCardMoved, &[("title", &card.title)]),
                None,
                cx,
            );
            return false;
        };
        let agent = card.agent.map_or("", |a| a.display_name());
        let waiting = card.column == Column::NeedsInput
            || card.ask.is_some()
            || view
                .read(cx)
                .agent_session()
                .is_some_and(|s| s.status == AgentStatus::Waiting || s.ask.is_some());
        if waiting {
            self.flash(
                t_fmt(L10nKey::BoardReplyWaiting, &[("agent", agent)]),
                None,
                cx,
            );
            return false;
        }
        view.read(cx).send_agent_prompt(text);
        self.unpause(key, window, cx);
        self.flash(
            t_fmt(
                L10nKey::BoardToastSent,
                &[("agent", agent), ("title", &card.title)],
            ),
            None,
            cx,
        );
        true
    }

    fn unpause(&mut self, key: CardRef, window: &mut Window, cx: &mut Context<Self>) {
        if let CardRef::Task(id) = key
            && let Some(mut task) = self.task(id)
            && (task.paused.is_some() || task.done.is_some())
        {
            task.paused = None;
            task::reopen(&mut task);
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
                let undo = Undo::Marks(Marks::of(&task));
                (task, undo)
            }
            CardRef::Loose(tab) => {
                let Some(task) = self.loose_task(tab, window, cx) else {
                    return;
                };
                let id = task.id;
                (task, Undo::Remove(id))
            }
        };
        task::mark_done(&mut task, live, unix_now());
        let (id, title) = (task.id, task.title.clone());
        if self.save_task(task, window, cx) {
            self.repoint(key, CardRef::Task(id));
            self.flash(
                t_fmt(L10nKey::BoardToastDone, &[("title", &title)]),
                Some(undo),
                cx,
            );
        }
    }

    /// Starts a finished task over: a new run, from where the task says it
    /// runs — for one whose worktree is gone and has nothing to reopen.
    fn start_again(&mut self, id: TaskId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut task) = self.task(id) else {
            return;
        };
        task::reopen(&mut task);
        if self.save_task(task, window, cx) {
            self.start_task(id, None, window, cx);
        }
    }

    fn reopen(&mut self, id: TaskId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut task) = self.task(id) else {
            return;
        };
        let undo = Undo::Marks(Marks::of(&task));
        task::reopen(&mut task);
        let title = task.title.clone();
        if self.save_task(task, window, cx) {
            self.flash(
                t_fmt(L10nKey::BoardToastReopened, &[("title", &title)]),
                Some(undo),
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
                Some(Undo::Restore(task)),
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
            bare: true,
        });
        Some(task)
    }

    /// Files an agent started by hand as a task, so the board keeps it.
    fn keep_loose(&mut self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task) = self.loose_task(tab, window, cx) else {
            return;
        };
        let (id, title) = (task.id, task.title.clone());
        if self.save_task(task, window, cx) {
            self.repoint(CardRef::Loose(tab), CardRef::Task(id));
            self.flash(
                t_fmt(L10nKey::BoardToastKept, &[("title", &title)]),
                Some(Undo::Remove(id)),
                cx,
            );
        }
    }

    /// Picks a finished run back up in a new tab. Like [`Self::launch_run`],
    /// the window stays on the board when that is where it was asked.
    fn resume_task(&mut self, id: TaskId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task) = self.task(id) else {
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
        let on_board = self.board_open();
        let zoom = self.board.zoom.take();
        let before = self.tabs.len();
        self.resume_session(run.agent, &session, cwd, false, window, cx);
        if on_board {
            self.return_to_board(zoom, window, cx);
        }
        // Nothing opened, and the reason is on screen already; the run must
        // not be pinned to whichever tab happened to be active.
        if self.tabs.len() == before {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let tab = tab.tree_id.get();
        // Read again: opening the tab may have taken a while, and the card
        // may have changed meanwhile.
        let Some(mut task) = self.task(id) else {
            return;
        };
        task.push_run(Run {
            tab: Some(tab),
            started: unix_now(),
            bare: true,
            ..run
        });
        task::reopen(&mut task);
        task.paused = None;
        self.save_task(task, window, cx);
    }

    /// Puts the board back after one of its own moves opened a tab — which
    /// shows the terminal, as any new tab does — with the zoom it had set
    /// aside still set aside.
    fn return_to_board(
        &mut self,
        zoom: Option<(TabId, Entity<TerminalView>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.main_view = MainView::Board;
        if self.board.zoom.is_none() {
            self.board.zoom = zoom;
        }
        self.focus_active(window, cx);
        cx.notify();
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
        // A second Start while the worktree is still being cut would cut
        // another.
        if self.board.starting.contains(&id) {
            return;
        }
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
        // The same line the New Worktree dialog types: the agent, told the
        // task when it takes a first message.
        let ask = one_line(task.ask());
        let command = setup::agent_line(agent, &ask, &cx.global::<Config>().agent_launch);
        // One that takes none starts bare, and the task waits on the
        // clipboard.
        if agent.prompt_args(&ask).is_none() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(task.ask().to_string()));
            window.push_notification(
                t_fmt(
                    L10nKey::BoardPromptCopied,
                    &[("agent", agent.display_name())],
                ),
                cx,
            );
        }
        let cwd = task.cwd.as_ref().map(PathBuf::from);
        if !task.worktree {
            self.launch_run(id, agent, Some(command), cwd, None, window, cx);
            return;
        }
        // The worktree is cut first, off the thread — on whichever machine
        // the workspace is on — and then opened the way the New Worktree
        // dialog opens one: its setup, once approved, before the agent.
        let Some(cwd) = cwd else {
            window.push_notification(t(L10nKey::BoardWorktreeNeedsRepo), cx);
            return;
        };
        let host_id = self.spawn_host(cx);
        let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, host_id) else {
            window.push_notification(t(L10nKey::BoardUnavailable), cx);
            return;
        };
        let slug = task
            .branch
            .clone()
            .or_else(|| task::branch_slug(&task.title));
        let title = task.title.clone();
        // Until its tab opens, the card says it is on its way and offers no
        // move: there is no agent yet to ask anything of.
        self.board.starting.insert(id);
        cx.notify();
        crate::ui::host_ops::HostOps::run_in(
            host,
            window,
            cx,
            move |h| {
                crate::core::worktree::create_for(h, &cwd, slug.as_deref())
                    .map(|wt| crate::ui::worktree_prompt::Created::read(h, wt))
            },
            move |this, made, window, cx| match made {
                Ok(created) => this.start_worktree(
                    host_id,
                    created,
                    Some(command),
                    Box::new(move |this, wt, line, window, cx| {
                        this.board.starting.remove(&id);
                        cx.notify();
                        // Removed while its worktree was being cut: there is
                        // no card left to give a run to.
                        if this.task(id).is_none() {
                            window.push_notification(
                                t_fmt(L10nKey::BoardStartGone, &[("title", &title)]),
                                cx,
                            );
                            return;
                        }
                        let path = wt.path.clone();
                        this.launch_run(id, agent, line, Some(path.clone()), Some(path), window, cx)
                    }),
                    window,
                    cx,
                ),
                Err(e) => {
                    this.board.starting.remove(&id);
                    cx.notify();
                    window.push_notification(
                        t_fmt(L10nKey::BoardWorktreeFailed, &[("error", &e)]),
                        cx,
                    )
                }
            },
        );
    }

    /// Opens the tab a run of task `id` works in and starts `agent` there.
    ///
    /// The window stays on the board: the card moving to Running is the
    /// answer, and whoever wants to watch can open it.
    #[allow(clippy::too_many_arguments)]
    fn launch_run(
        &mut self,
        id: TaskId,
        agent: CLIAgent,
        command: Option<String>,
        cwd: Option<PathBuf>,
        worktree: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The task as it is now, not as it was when Start was pressed: a
        // worktree takes a while, and the card may have been edited since.
        let Some(mut task) = self.task(id) else {
            return;
        };
        let on_board = self.board_open();
        let zoom = self.board.zoom.take();
        let Some(slot) = self.new_tab_slot(cwd, None, window, cx) else {
            self.board.zoom = zoom;
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
        if let Some(command) = command {
            crate::ui::agent_launch::run_when_ready(&slot, command, cx);
        }
        if on_board {
            self.return_to_board(zoom, window, cx);
        }
        task.push_run(Run {
            agent,
            tab: Some(tab_id),
            session_id: None,
            started: unix_now(),
            worktree: worktree.map(|p| p.display().to_string()),
            bare: agent.prompt_args(&one_line(task.ask())).is_none(),
        });
        task.agent = Some(agent);
        task::reopen(&mut task);
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
            let mut next: Option<Task> = None;
            for (i, run) in task.runs.iter().enumerate() {
                let Some(view) = self.run_view(run, cx) else {
                    continue;
                };
                let session = view.read(cx).agent_session();
                let id = session.as_ref().and_then(|s| s.session_id.as_ref());
                if id.is_some() && id != run.session_id.as_ref() {
                    next.get_or_insert_with(|| task.clone()).runs[i].session_id = id.cloned();
                }
                let working = session.is_some_and(|s| s.status == AgentStatus::Working);
                let paused = next.as_ref().unwrap_or(task).paused;
                if working && paused.is_some_and(|at| now.saturating_sub(at) > PAUSE_GRACE_SECS) {
                    next.get_or_insert_with(|| task.clone()).paused = None;
                }
            }
            changed.extend(next);
        }
        for task in changed {
            if crate::ui::tree_sync::push_task(cx, self.workspace, task.clone()) {
                self.board_task_put(task);
            }
        }
    }

    /// The pane `run`'s own agent runs in: one in its tab running the agent
    /// it was started with — the one already known by its session, if that
    /// is still there. Another agent started in the same tab, or beside it
    /// in a split, is not this run's, and its session is not the one to
    /// resume.
    fn run_view(&self, run: &Run, cx: &App) -> Option<Entity<TerminalView>> {
        let tab = self
            .tabs
            .iter()
            .find(|t| Some(t.tree_id.get()) == run.tab)?;
        let mine: Vec<_> = tab
            .pane
            .terminals()
            .into_iter()
            .filter(|l| l.read(cx).agent() == Some(run.agent))
            .collect();
        let known = mine.iter().find(|l| {
            run.session_id.is_some()
                && l.read(cx).agent_session().and_then(|s| s.session_id) == run.session_id
        });
        known.or(mine.first()).cloned()
    }

    // ---- toast --------------------------------------------------------------

    /// Says what just happened. With `undo`, the change goes on the stack
    /// ⌘Z walks back, and the note offers to take it back.
    fn flash(&mut self, text: String, undo: Option<Undo>, cx: &mut Context<Self>) {
        self.board.toast_seq += 1;
        let seq = self.board.toast_seq;
        let can_undo = undo.is_some();
        if let Some(undo) = undo {
            self.board.undo.push(undo);
        }
        self.board.toast = Some(Toast {
            text: text.into(),
            undo: can_undo,
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

    /// Walks back the newest change the board made, laid over the card as it
    /// is now. One whose card has gone since has nothing to put back.
    fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(undo) = self.board.undo.pop() else {
            return;
        };
        self.board.toast = None;
        let current = self.task(undo.task());
        match undo.step(current.as_ref()) {
            Some(UndoStep::Save(task)) => {
                self.save_task(*task, window, cx);
            }
            Some(UndoStep::Delete(id)) => {
                self.delete_task(id, window, cx);
            }
            None => {}
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
        60..3600 => t_fmt(L10nKey::BoardAgoMinutes, &[("n", &(secs / 60).to_string())]),
        3600..86400 => t_fmt(L10nKey::BoardAgoHours, &[("n", &(secs / 3600).to_string())]),
        _ => t_fmt(L10nKey::BoardAgoDays, &[("n", &(secs / 86400).to_string())]),
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
            L10nKey::BoardAnswerInTerminal
        );
        assert_eq!(
            move_verb(Column::NeedsInput, Column::Running, false),
            L10nKey::BoardVerbAllow
        );
    }

    #[test]
    fn only_old_done_cards_fold_and_only_until_asked_for() {
        let now = 100 * 24 * 3600;
        let old = Some(now - DONE_RECENT_SECS - 1);
        let recent = Some(now - DONE_RECENT_SECS + 60);
        assert!(folded(Column::Done, old, now, false));
        assert!(
            !folded(Column::Done, old, now, true),
            "shown once asked for"
        );
        assert!(!folded(Column::Done, recent, now, false));
        assert!(!folded(Column::Done, None, now, false), "no date, no fold");
        assert!(!folded(Column::Review, old, now, false), "only Done folds");
    }

    #[test]
    fn a_filter_on_an_unpinned_group_lets_go() {
        let (kept, gone) = (GroupId::new(), GroupId::new());
        let pinned = |g: GroupId| g == kept;
        assert_eq!(live_group_filter(Some(kept), pinned), Some(kept));
        assert_eq!(live_group_filter(Some(gone), pinned), None);
        assert_eq!(live_group_filter(None, pinned), None);
    }

    #[test]
    fn the_undo_stack_keeps_the_newest_few() {
        let mut stack = UndoStack::default();
        let ids: Vec<TaskId> = (0..UNDO_DEPTH + 2).map(|_| TaskId::new()).collect();
        for id in &ids {
            stack.push(Undo::Remove(*id));
        }
        assert_eq!(stack.0.len(), UNDO_DEPTH);
        assert_eq!(stack.pop(), Some(Undo::Remove(ids[UNDO_DEPTH + 1])));
        assert_eq!(stack.pop(), Some(Undo::Remove(ids[UNDO_DEPTH])));
        while stack.pop().is_some() {}
        assert!(!stack.0.contains(&Undo::Remove(ids[0])), "the oldest went");
    }

    /// An undo puts back what its change touched and nothing else: a run
    /// the card picked up since stays.
    #[test]
    fn an_undo_is_laid_over_the_card_as_it_is_now() {
        let mut before = Task::new("t");
        before.paused = Some(5);
        let undo = Undo::Marks(Marks::of(&before));
        let mut now = before.clone();
        task::mark_done(&mut now, |_| None, 9);
        now.push_run(Run {
            agent: CLIAgent::Claude,
            tab: Some(TabId::new()),
            session_id: Some("s".into()),
            started: 10,
            worktree: None,
            bare: false,
        });
        let Some(UndoStep::Save(put)) = undo.clone().step(Some(&now)) else {
            panic!("the card is still there to put back");
        };
        assert_eq!(put.paused, Some(5));
        assert!(put.done.is_none() && put.done_marks.is_none());
        assert_eq!(put.runs, now.runs, "the new run stays");
        assert_eq!(undo.step(None), None, "a card gone since is left gone");

        let mut edited = before.clone();
        edited.title = "new".into();
        edited.paused = None;
        let Some(UndoStep::Save(put)) = Undo::Edit(before.clone()).step(Some(&edited)) else {
            panic!("an edit is undone");
        };
        assert_eq!(put.title, "t");
        assert_eq!(put.paused, None, "an edit's undo leaves the marks alone");
    }

    #[test]
    fn a_removal_is_undone_only_while_the_card_is_still_gone() {
        let task = Task::new("t");
        assert_eq!(
            Undo::Restore(task.clone()).step(None),
            Some(UndoStep::Save(Box::new(task.clone())))
        );
        assert_eq!(Undo::Restore(task.clone()).step(Some(&task)), None);
        assert_eq!(
            Undo::Remove(task.id).step(Some(&task)),
            Some(UndoStep::Delete(task.id))
        );
        assert_eq!(Undo::Remove(task.id).step(None), None);
    }
}

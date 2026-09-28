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

use std::path::PathBuf;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Hsla, SharedString, Subscription, Window, div,
    prelude::*, px, rems,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use tty7_core::core::machine::TabId;
use tty7_core::core::task::{self, Column, Done, Live, Run, Task, TaskId};

use crate::core::cli_agent::{AgentStatus, CLIAgent};
use crate::core::config::{Config, unix_now};
use crate::ui::app::Tty7App;
use crate::ui::dialog::{self, Tone};
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::right_panel::{HEADING, META, META_MONO, TAB_TEXT, TEXT};

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
    /// Only this agent's cards, or everyone's.
    pub agent_filter: Option<CLIAgent>,
    pub composer: Option<Composer>,
    pub focus: FocusHandle,
}

impl Board {
    pub(crate) fn new(cx: &mut App) -> Board {
        Board {
            tasks: Vec::new(),
            agent_filter: None,
            composer: None,
            focus: cx.focus_handle(),
        }
    }
}

/// The New task card, or the same card open on a task being edited.
pub(crate) struct Composer {
    editing: Option<TaskId>,
    title: Entity<InputState>,
    prompt: Entity<InputState>,
    agent: Option<CLIAgent>,
    cwd: Option<PathBuf>,
    _subs: Vec<Subscription>,
}

/// Board geometry, from the v7 board comp.
const HEADER_H: f32 = 48.;
const PAD_X: f32 = 20.;
const COLUMN_GAP: f32 = 12.;
const COLUMN_HEAD_H: f32 = 28.;
const CARD_GAP: f32 = 8.;
const CARD_RADIUS: f32 = 9.;
const AVATAR: f32 = 16.;
const COMPOSER_W: f32 = 560.;
const PROMPT_H: f32 = 132.;

/// Which card a click or a menu is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CardRef {
    Task(TaskId),
    /// A tab running an agent that no task claims.
    Loose(TabId),
}

/// One card, read off a task or a tab for this frame.
struct Card {
    key: CardRef,
    column: Column,
    agent: Option<CLIAgent>,
    status: Option<AgentStatus>,
    title: SharedString,
    /// `repo · branch`, or just the repo.
    place: Option<SharedString>,
    /// What the agent says it is doing, from its terminal title.
    doing: Option<SharedString>,
    /// What the agent is stopped on.
    ask: Option<SharedString>,
    diff: Option<(u32, u32)>,
    /// When this card's current state began, in Unix seconds, if known.
    since: Option<u64>,
    /// The open tab a run of this card lives in.
    tab: Option<TabId>,
    /// A finished run this card could pick back up.
    resumable: bool,
}

/// What the board reads off one open tab.
struct TabFacts {
    id: TabId,
    agent: CLIAgent,
    live: Live,
    ask: Option<String>,
    doing: Option<String>,
    cwd: Option<PathBuf>,
    branch: Option<String>,
    diff: Option<(u32, u32)>,
    label: Option<String>,
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

    fn delete_task(&mut self, id: TaskId, window: &mut Window, cx: &mut Context<Self>) {
        if !crate::ui::tree_sync::remove_task(cx, self.workspace, id) {
            window.push_notification(t(L10nKey::BoardUnavailable), cx);
            return;
        }
        self.board_task_removed(id);
        cx.notify();
    }

    /// Everything the board knows about the open tabs that run an agent.
    ///
    /// `full` also reads what only a drawn card shows — the tab's label, its
    /// git counts, its terminal title. The sidebar's count asks every frame
    /// and needs none of that.
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
                let git = full.then(|| view.git_status(cx)).flatten();
                Some(TabFacts {
                    id: tab.tree_id.get(),
                    agent: view.agent()?,
                    live: Live::from(&session),
                    ask: (session.status == AgentStatus::Waiting)
                        .then(|| session.message.clone())
                        .flatten(),
                    doing: full
                        .then(|| view.stated_title().map(str::to_string))
                        .flatten(),
                    cwd: view.cwd(),
                    branch: git.as_ref().map(|g| g.branch.clone()),
                    diff: git
                        .as_ref()
                        .map(|g| (g.added, g.removed))
                        .filter(|&(a, r)| a > 0 || r > 0),
                    label: full.then(|| self.full_tab_label(tab, window, cx)).flatten(),
                })
            })
            .collect()
    }

    /// Every card on the board this frame, filtered by agent.
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
            let agent = open
                .map(|f| f.agent)
                .or(last.map(|r| r.agent))
                .or(task.agent);
            let cwd = open
                .and_then(|f| f.cwd.clone())
                .or_else(|| task.cwd.as_ref().map(PathBuf::from));
            cards.push(Card {
                key: CardRef::Task(task.id),
                column,
                agent,
                status: open.map(|f| f.live.status),
                title: task.title.clone().into(),
                place: place(cwd.as_deref(), open.and_then(|f| f.branch.as_deref())),
                doing: open
                    .filter(|_| column == Column::Running)
                    .and_then(|f| f.doing.clone())
                    .map(Into::into),
                ask: open.and_then(|f| f.ask.clone()).map(Into::into),
                diff: open.and_then(|f| f.diff),
                since: match column {
                    Column::Done => task.done.map(|d| d.at),
                    Column::Queued => Some(task.created),
                    _ => last.map(|r| r.started),
                },
                tab: open.map(|f| f.id),
                resumable: open.is_none() && last.is_some_and(|r| r.session_id.is_some()),
            });
        }
        let claimed = |id: TabId| self.board.tasks.iter().any(|t| t.runs_in(id));
        for f in facts.iter().filter(|f| !claimed(f.id)) {
            let column = task::live_column(f.live);
            cards.push(Card {
                key: CardRef::Loose(f.id),
                column,
                agent: Some(f.agent),
                status: Some(f.live.status),
                title: f
                    .label
                    .clone()
                    .unwrap_or_else(|| f.agent.display_name().to_string())
                    .into(),
                place: place(f.cwd.as_deref(), f.branch.as_deref()),
                doing: (column == Column::Running)
                    .then(|| f.doing.clone())
                    .flatten()
                    .filter(|d| Some(d) != f.label.as_ref())
                    .map(Into::into),
                ask: f.ask.clone().map(Into::into),
                diff: f.diff,
                since: None,
                tab: Some(f.id),
                resumable: false,
            });
        }
        if let Some(only) = self.board.agent_filter {
            cards.retain(|c| c.agent == Some(only));
        }
        cards
    }

    /// Jumps to the tab a card's agent runs in.
    fn open_card_tab(&mut self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|t| t.tree_id.get() == tab) else {
            return;
        };
        self.main_view = MainView::Terminal;
        self.board.composer = None;
        self.activate(index, window, cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    fn click_card(&mut self, key: CardRef, window: &mut Window, cx: &mut Context<Self>) {
        let cards = self.board_cards(Some(window), false, cx);
        let Some(card) = cards.iter().find(|c| c.key == key) else {
            return;
        };
        match (card.tab, key) {
            (Some(tab), _) => self.open_card_tab(tab, window, cx),
            (None, CardRef::Task(id)) => self.open_composer(Some(id), window, cx),
            (None, CardRef::Loose(_)) => {}
        }
    }

    /// Opens a new tab for `task` and starts its agent there, told what to do.
    ///
    /// The window stays on the board: the card moving to Running is the
    /// answer, and whoever wants to watch can click it.
    fn start_task(
        &mut self,
        id: TaskId,
        agent: Option<CLIAgent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut task) = self.board.tasks.iter().find(|t| t.id == id).cloned() else {
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
        let Some(slot) = self.new_tab_slot(cwd, None, window, cx) else {
            return;
        };
        // The tab is named for the task, so its sidebar row says what it is
        // for rather than whatever the agent titles itself.
        let tab = &mut self.tabs[self.active];
        tab.name = Some(task.title.clone());
        let tab_id = tab.tree_id.get();
        crate::ui::agent_launch::run_when_ready(&slot, command, cx);
        self.main_view = MainView::Board;
        self.focus_active(window, cx);
        task.push_run(Run {
            agent,
            tab: Some(tab_id),
            session_id: None,
            started: unix_now(),
        });
        task.agent = Some(agent);
        task.done = None;
        self.save_task(task, window, cx);
        self.save_session(cx);
    }

    fn mark_done(&mut self, key: CardRef, window: &mut Window, cx: &mut Context<Self>) {
        let facts = self.tab_facts(Some(window), false, cx);
        let live = |run: &Run| {
            run.tab
                .and_then(|id| facts.iter().find(|f| f.id == id))
                .map(|f| f.live)
        };
        let task = match key {
            CardRef::Task(id) => {
                let Some(mut task) = self.board.tasks.iter().find(|t| t.id == id).cloned() else {
                    return;
                };
                task.done = Some(Done {
                    at: unix_now(),
                    turns: task::live_turns(&task, live),
                });
                task
            }
            CardRef::Loose(tab) => {
                let Some(mut task) = self.loose_task(tab, window, cx) else {
                    return;
                };
                task.done = Some(Done {
                    at: unix_now(),
                    turns: task::live_turns(&task, live),
                });
                task
            }
        };
        self.save_task(task, window, cx);
    }

    fn reopen(&mut self, id: TaskId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut task) = self.board.tasks.iter().find(|t| t.id == id).cloned() else {
            return;
        };
        task.done = None;
        self.save_task(task, window, cx);
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
        task.push_run(Run {
            agent: f.agent,
            tab: Some(tab),
            session_id: None,
            started: unix_now(),
        });
        Some(task)
    }

    fn keep_loose(&mut self, tab: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(task) = self.loose_task(tab, window, cx) {
            self.save_task(task, window, cx);
        }
    }

    fn resume_task(&mut self, id: TaskId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task) = self.board.tasks.iter().find(|t| t.id == id).cloned() else {
            return;
        };
        let Some(run) = task.runs.last().cloned() else {
            return;
        };
        let Some(session) = run.session_id.clone() else {
            return;
        };
        let cwd = task.cwd.as_ref().map(PathBuf::from);
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
        let mut task = task;
        task.push_run(Run {
            tab: Some(tab.tree_id.get()),
            started: unix_now(),
            ..run
        });
        task.done = None;
        self.save_task(task, window, cx);
    }

    /// Writes down the session each open run's agent reported, so a run whose
    /// tab closes can still be resumed. Called on every frame, board or not —
    /// a tab can close while the terminal is showing — and only sends
    /// anything when a session is new. Quiet on failure: a window whose tree
    /// has not landed tries again next frame, and saying so every frame
    /// would bury the screen in toasts.
    fn note_run_sessions(&mut self, cx: &mut Context<Self>) {
        let mut changed = Vec::new();
        for task in &self.board.tasks {
            let mut next = task.clone();
            let mut dirty = false;
            for run in next.runs.iter_mut() {
                let Some(tab) = run
                    .tab
                    .and_then(|id| self.tabs.iter().find(|t| t.tree_id.get() == id))
                else {
                    continue;
                };
                let session = tab
                    .pane
                    .terminals()
                    .into_iter()
                    .find_map(|l| l.read(cx).agent_session().and_then(|s| s.session_id));
                if session.is_some() && session != run.session_id {
                    run.session_id = session;
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

    // ---- composer ---------------------------------------------------------

    pub(crate) fn open_composer(
        &mut self,
        editing: Option<TaskId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = editing.and_then(|id| self.board.tasks.iter().find(|t| t.id == id).cloned());
        let (title, prompt) = existing
            .as_ref()
            .map(|t| (t.title.clone(), t.prompt.clone()))
            .unwrap_or_default();
        let title = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t(L10nKey::BoardTitlePlaceholder))
                .default_value(title)
        });
        let prompt = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder(t(L10nKey::BoardPromptPlaceholder))
                .default_value(prompt)
        });
        title.update(cx, |state, cx| state.focus(window, cx));
        let subs = vec![
            cx.subscribe_in(
                &title,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter {
                        secondary: true, ..
                    } => this.submit_composer(true, window, cx),
                    InputEvent::PressEnter { .. } => this.submit_composer(false, window, cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
            cx.subscribe_in(&prompt, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter {
                    secondary: true, ..
                } = ev
                {
                    this.submit_composer(true, window, cx);
                }
            }),
        ];
        let cwd = match &existing {
            Some(t) => t.cwd.as_ref().map(PathBuf::from),
            None => self.tabs.get(self.active).and_then(|t| {
                t.pane
                    .focused_or_first(window, cx)
                    .and_then(|leaf| leaf.read(cx).spawnable_cwd())
            }),
        };
        let agent = existing
            .as_ref()
            .and_then(|t| t.agent)
            .or_else(|| self.offered_agents(cx).first().copied());
        self.board.composer = Some(Composer {
            editing,
            title,
            prompt,
            agent,
            cwd,
            _subs: subs,
        });
        cx.notify();
    }

    fn close_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.board.composer.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// Saves the composer's task, and with `start`, starts it too.
    fn submit_composer(&mut self, start: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.board.composer.as_ref() else {
            return;
        };
        let title = c.title.read(cx).value().trim().to_string();
        if title.is_empty() {
            window.push_notification(t(L10nKey::BoardNeedsTitle), cx);
            return;
        }
        let prompt = c.prompt.read(cx).value().trim().to_string();
        let mut task = c
            .editing
            .and_then(|id| self.board.tasks.iter().find(|t| t.id == id).cloned())
            .unwrap_or_else(|| Task::new(String::new()));
        task.title = title;
        task.prompt = prompt;
        task.agent = c.agent;
        task.cwd = c.cwd.as_ref().map(|p| p.display().to_string());
        let (id, agent) = (task.id, task.agent);
        if !self.save_task(task, window, cx) {
            return;
        }
        self.board.composer = None;
        if start {
            self.start_task(id, agent, window, cx);
        } else {
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    fn set_composer_agent(&mut self, agent: CLIAgent, cx: &mut Context<Self>) {
        if let Some(c) = self.board.composer.as_mut() {
            c.agent = Some(agent);
            cx.notify();
        }
    }

    // ---- drawing ----------------------------------------------------------

    /// The board over the terminal area, or `None` while the terminal shows.
    pub(crate) fn render_board(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.note_run_sessions(cx);
        if self.main_view != MainView::Board {
            return None;
        }
        let cards = self.board_cards(Some(window), true, cx);
        let header = self.render_board_header(&cards, cx);
        let body = match cards.is_empty() && self.board.agent_filter.is_none() {
            true => self.render_board_empty(cx),
            false => h_flex()
                .flex_1()
                .min_h_0()
                .items_start()
                .gap(px(COLUMN_GAP))
                .px(px(PAD_X))
                .pt(px(4.))
                .pb(px(PAD_X))
                .children(Column::ALL.into_iter().map(|col| {
                    let in_col: Vec<&Card> = cards.iter().filter(|c| c.column == col).collect();
                    self.render_column(col, &in_col, cx)
                }))
                .into_any_element(),
        };
        Some(
            v_flex()
                .id("board")
                .track_focus(&self.board.focus)
                .key_context("Board")
                .absolute()
                .inset_0()
                .occlude()
                .bg(crate::ui::theme::overlay_background(cx))
                .children(crate::ui::app::overlay_surface_layers(cx))
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                    let k = &ev.keystroke;
                    if this.board.composer.is_some() {
                        if k.key == "escape" {
                            this.close_composer(window, cx);
                        }
                        return;
                    }
                    let bare = !k.modifiers.modified();
                    match k.key.as_str() {
                        "escape" => this.close_board(window, cx),
                        "n" if bare => this.open_composer(None, window, cx),
                        _ => return,
                    }
                    cx.stop_propagation();
                }))
                .child(header)
                .child(body)
                .when_some(self.render_composer(cx), |this, el| this.child(el))
                .into_any_element(),
        )
    }

    fn render_board_header(&self, cards: &[Card], cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let count = |col: Column| cards.iter().filter(|c| c.column == col).count();
        let summary = t_fmt(
            L10nKey::BoardSummary,
            &[
                ("running", &count(Column::Running).to_string()),
                ("input", &count(Column::NeedsInput).to_string()),
                ("review", &count(Column::Review).to_string()),
            ],
        );
        let filter_label: SharedString = match self.board.agent_filter {
            Some(agent) => agent.display_name().into(),
            None => t(L10nKey::BoardAllAgents).into(),
        };
        let mut agents: Vec<CLIAgent> = self
            .board
            .tasks
            .iter()
            .filter_map(|t| t.agent)
            .chain(self.tab_facts(None, false, cx).iter().map(|f| f.agent))
            .collect();
        agents.sort_by_key(|a| a.display_name());
        agents.dedup();
        let app = cx.entity().downgrade();
        let rungs = dialog::popover_rungs(cx);
        h_flex()
            .flex_none()
            .h(px(HEADER_H))
            .px(px(PAD_X))
            .items_center()
            .gap(px(12.))
            .child(
                div()
                    .text_size(rems(TEXT))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(t(L10nKey::BoardTitle)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(rems(META))
                    .text_color(muted)
                    .child(summary),
            )
            .child(
                Button::new("board-agent-filter")
                    .label(filter_label)
                    .ghost()
                    .small()
                    .dropdown_caret(true)
                    .dropdown_menu(move |menu: PopupMenu, _window, _cx| {
                        let a0 = app.clone();
                        let mut menu =
                            menu.item(PopupMenuItem::new(t(L10nKey::BoardAllAgents)).on_click(
                                move |_, _, cx| {
                                    let _ = a0.update(cx, |this, cx| {
                                        this.board.agent_filter = None;
                                        cx.notify();
                                    });
                                },
                            ));
                        for agent in agents.clone() {
                            let a = app.clone();
                            menu = menu.item(PopupMenuItem::new(agent.display_name()).on_click(
                                move |_, _, cx| {
                                    let _ = a.update(cx, |this, cx| {
                                        this.board.agent_filter = Some(agent);
                                        cx.notify();
                                    });
                                },
                            ));
                        }
                        menu
                    }),
            )
            .child(dialog::button(
                "board-new-task",
                t(L10nKey::BoardNewTask),
                Tone::Primary,
                true,
                rungs,
                cx,
                cx.listener(|this, _, window, cx| this.open_composer(None, window, cx)),
            ))
            .into_any_element()
    }

    fn render_board_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .child(
                div()
                    .text_size(rems(TEXT))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(t(L10nKey::BoardEmpty)),
            )
            .child(
                div()
                    .max_w(px(420.))
                    .text_center()
                    .text_size(rems(META))
                    .text_color(muted)
                    .child(t(L10nKey::BoardEmptyHint)),
            )
            .into_any_element()
    }

    fn render_column(&self, col: Column, cards: &[&Card], cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let (dot, hollow) = column_dot(col, cx);
        let label = t(match col {
            Column::Queued => L10nKey::BoardColQueued,
            Column::Running => L10nKey::BoardColRunning,
            Column::NeedsInput => L10nKey::BoardColNeedsInput,
            Column::Review => L10nKey::BoardColReview,
            Column::Done => L10nKey::BoardColDone,
        });
        let head = h_flex()
            .flex_none()
            .h(px(COLUMN_HEAD_H))
            .px(px(4.))
            .items_center()
            .gap(px(8.))
            .text_size(rems(META))
            .child(
                div()
                    .size(px(7.))
                    .rounded_full()
                    .when(hollow, |d| d.border_1().border_color(dot))
                    .when(!hollow, |d| d.bg(dot)),
            )
            .child(
                div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(fg)
                    .child(label),
            )
            .child(
                div()
                    .text_color(muted)
                    .font_features(crate::ui::tab_sidebar::tabular())
                    .child(cards.len().to_string()),
            );
        let list = v_flex()
            .id(("board-column", col as usize))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap(px(CARD_GAP))
            .pb(px(4.))
            .children(cards.iter().map(|c| self.render_card(c, cx)));
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .gap(px(CARD_GAP))
            .child(head)
            .child(list)
            .into_any_element()
    }

    fn render_card(&self, card: &Card, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, border) = (theme.foreground, theme.muted_foreground, theme.border);
        let surface = theme.popover;
        let well: Hsla = gpui::rgb(dialog::popover_rungs(cx).hover).into();
        let mono = theme.mono_font_family.clone();
        let key = card.key;
        let id = card_element_id(key);
        let avatar = crate::ui::tab_strip::avatar(
            SharedString::from(format!("{id}-avatar")),
            crate::ui::search::Avatar {
                agent: card.agent,
                status: card.status,
                unread: 0,
                ssh: None,
            },
            AVATAR,
            cx,
        );
        let title_row = h_flex()
            .items_start()
            .gap(px(9.))
            .child(div().mt(px(1.)).child(avatar))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(rems(TAB_TEXT))
                    .line_height(rems(TAB_TEXT * 1.4))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(fg)
                    .line_clamp(3)
                    .child(card.title.clone()),
            );
        let place = card.place.clone().map(|p| {
            div()
                .truncate()
                .text_size(rems(HEADING))
                .text_color(muted)
                .child(p)
        });
        let doing = card.doing.clone().map(|d| {
            div()
                .truncate()
                .px(px(7.))
                .py(px(4.))
                .rounded(px(5.))
                .bg(well)
                .font_family(mono.clone())
                .text_size(rems(META_MONO))
                .text_color(muted)
                .child(d)
        });
        let ask = (card.column == Column::NeedsInput).then(|| {
            let tab = card.tab;
            v_flex()
                .gap(px(8.))
                .when_some(card.ask.clone(), |v, ask| {
                    v.child(
                        div()
                            .px(px(8.))
                            .py(px(6.))
                            .rounded(px(5.))
                            .bg(well)
                            .font_family(mono.clone())
                            .text_size(rems(META_MONO))
                            .line_height(rems(META_MONO * 1.5))
                            .text_color(fg)
                            .line_clamp(4)
                            .child(ask),
                    )
                })
                .when_some(tab, |v, tab| {
                    v.child(
                        dialog::button(
                            SharedString::from(format!("{id}-open")),
                            t(L10nKey::BoardOpen),
                            Tone::Primary,
                            true,
                            dialog::popover_rungs(cx),
                            cx,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.open_card_tab(tab, window, cx)
                            }),
                        )
                        .w_full()
                        .h(px(24.)),
                    )
                })
        });
        let footer = {
            let mut row = h_flex()
                .items_center()
                .gap(px(8.))
                .text_size(rems(META_MONO))
                .text_color(muted)
                .font_features(crate::ui::tab_sidebar::tabular());
            if let Some((added, removed)) = card.diff {
                row = row.child(
                    h_flex()
                        .gap(px(4.))
                        .child(div().text_color(theme.success).child(format!("+{added}")))
                        .child(div().text_color(theme.danger).child(format!("−{removed}"))),
                );
            }
            if card.resumable {
                row = row.child(t(L10nKey::BoardResumable));
            }
            row = row.child(div().flex_1());
            if let Some(since) = card.since {
                row = row.child(ago(since));
            }
            row
        };
        let done = card.column == Column::Done;
        let app = cx.entity().downgrade();
        let (column, tab, resumable) = (card.column, card.tab, card.resumable);
        v_flex()
            .id(id)
            .gap(px(8.))
            .pt(px(11.))
            .px(px(12.))
            .pb(px(10.))
            .rounded(px(CARD_RADIUS))
            .bg(surface)
            .border_1()
            .border_color(border.opacity(0.6))
            .hover(|s| s.border_color(border))
            .when(done, |c| c.opacity(0.6))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| this.click_card(key, window, cx)))
            .child(title_row)
            .children(place)
            .children(doing)
            .children(ask)
            .child(footer)
            .context_menu(move |menu, _window, _cx| {
                card_menu(menu, key, column, tab, resumable, app.clone())
            })
            .into_any_element()
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let c = self.board.composer.as_ref()?;
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let rungs = dialog::popover_rungs(cx);
        let agents = self.offered_agents(cx);
        let agent_label: SharedString = c
            .agent
            .map(|a| a.display_name().into())
            .unwrap_or_else(|| t(L10nKey::BoardNoAgent).into());
        let app = cx.entity().downgrade();
        let where_ = c
            .cwd
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let has_title = !c.title.read(cx).value().trim().is_empty();
        let editing = c.editing.is_some();
        let well = dialog::well_fill(cx);
        let prompt_field = div()
            .h(px(PROMPT_H))
            .rounded(crate::ui::rounding::ROW_RADIUS)
            .bg(well)
            .py(px(2.))
            .child(Input::new(&c.prompt).appearance(false).small().h_full());
        let agent_picker = Button::new("board-composer-agent")
            .label(agent_label)
            .small()
            .ghost()
            .dropdown_caret(true)
            .when_some(c.agent, |b, agent| {
                b.icon(Icon::default().path(agent.icon_path()).size(px(12.)))
            })
            .dropdown_menu(move |menu: PopupMenu, _window, _cx| {
                let mut menu = menu;
                for agent in agents.clone() {
                    let a = app.clone();
                    menu = menu.item(PopupMenuItem::new(agent.display_name()).on_click(
                        move |_, _, cx| {
                            let _ = a.update(cx, |this, cx| this.set_composer_agent(agent, cx));
                        },
                    ));
                }
                menu
            });
        let card = dialog::card(COMPOSER_W, cx)
            .child(dialog::header(
                t(if editing {
                    L10nKey::BoardEditTask
                } else {
                    L10nKey::BoardNewTask
                }),
                cx,
            ))
            .child(
                dialog::body()
                    .child(dialog::labelled(
                        t(L10nKey::BoardFieldTitle),
                        Input::new(&c.title),
                        cx,
                    ))
                    .child(
                        v_flex()
                            .gap(px(6.))
                            .child(dialog::label(t(L10nKey::BoardFieldPrompt), cx))
                            .child(prompt_field),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(10.))
                            .child(dialog::label(t(L10nKey::BoardFieldAgent), cx))
                            .child(agent_picker)
                            .child(div().flex_1())
                            .child(
                                div()
                                    .min_w_0()
                                    .max_w(px(260.))
                                    .truncate()
                                    .font_family(theme.mono_font_family.clone())
                                    .text_size(rems(META_MONO))
                                    .text_color(muted)
                                    .child(where_),
                            ),
                    ),
            )
            .child(
                dialog::footer(cx)
                    .child(dialog::button(
                        "board-composer-cancel",
                        t(L10nKey::Cancel),
                        Tone::Secondary,
                        true,
                        rungs,
                        cx,
                        cx.listener(|this, _, window, cx| this.close_composer(window, cx)),
                    ))
                    .child(dialog::button(
                        "board-composer-save",
                        t(if editing {
                            L10nKey::BoardSave
                        } else {
                            L10nKey::BoardAddToQueue
                        }),
                        Tone::Secondary,
                        has_title,
                        rungs,
                        cx,
                        cx.listener(|this, _, window, cx| this.submit_composer(false, window, cx)),
                    ))
                    .child(dialog::button(
                        "board-composer-start",
                        t(L10nKey::BoardStartNow),
                        Tone::Primary,
                        has_title && c.agent.is_some(),
                        rungs,
                        cx,
                        cx.listener(|this, _, window, cx| this.submit_composer(true, window, cx)),
                    )),
            );
        Some(
            div()
                .absolute()
                .inset_0()
                .bg(crate::ui::presets::scrim_fill(cx))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                        this.close_composer(window, cx)
                    }),
                )
                .flex()
                .flex_col()
                .items_center()
                .justify_start()
                .pt(px(crate::ui::switcher::CARD_TOP - HEADER_H))
                .child(
                    card.on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation()),
                )
                .into_any_element(),
        )
    }

    /// The sidebar's Board row, under the search box.
    pub(crate) fn render_board_nav(&self, cx: &mut Context<Self>) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().rail;
        let active = self.board_open();
        let theme = cx.theme();
        let needs = self.board_needs_input(cx);
        let warn = theme.warning;
        h_flex()
            .id("sidebar-board")
            .w_full()
            .flex_shrink_0()
            .h(px(30.))
            .px(px(8.))
            .gap(px(10.))
            .items_center()
            .rounded(crate::ui::rounding::ROW_RADIUS)
            .cursor_pointer()
            .when(active, |s| {
                s.bg(gpui::rgb(sf.selected))
                    .text_color(gpui::rgb(sf.text_selected))
                    .font_weight(gpui::FontWeight::MEDIUM)
            })
            .when(!active, |s| {
                s.text_color(gpui::rgb(sf.text_resting))
                    .hover(|s| s.bg(gpui::rgb(sf.hover)))
            })
            .child(
                Icon::empty()
                    .path("icons/board.svg")
                    .size(px(14.))
                    .text_color(theme.muted_foreground),
            )
            .child(div().flex_1().child(t(L10nKey::BoardTitle)))
            .when(needs > 0, |row| {
                row.child(
                    div()
                        .min_w(px(18.))
                        .h(px(18.))
                        .px(px(5.))
                        .rounded_full()
                        .bg(warn)
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(rems(11. / 16.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(gpui::white())
                        .child(needs.min(99).to_string()),
                )
            })
            .on_click(cx.listener(|this, _, window, cx| this.toggle_board(window, cx)))
            .into_any_element()
    }
}

fn card_element_id(key: CardRef) -> SharedString {
    match key {
        CardRef::Task(id) => format!("board-task-{id}").into(),
        CardRef::Loose(id) => format!("board-tab-{id}").into(),
    }
}

fn card_menu(
    menu: PopupMenu,
    key: CardRef,
    column: Column,
    tab: Option<TabId>,
    resumable: bool,
    app: gpui::WeakEntity<Tty7App>,
) -> PopupMenu {
    let item = |label: L10nKey,
                f: Box<dyn Fn(&mut Tty7App, &mut Window, &mut Context<Tty7App>)>| {
        let app = app.clone();
        PopupMenuItem::new(t(label)).on_click(move |_, window, cx| {
            let _ = app.update(cx, |this, cx| f(this, window, cx));
        })
    };
    let mut menu = menu;
    if let Some(tab) = tab {
        menu = menu.item(item(
            L10nKey::BoardOpen,
            Box::new(move |this, window, cx| this.open_card_tab(tab, window, cx)),
        ));
    }
    match key {
        CardRef::Task(id) => {
            if tab.is_none() {
                menu = menu.item(item(
                    L10nKey::BoardStart,
                    Box::new(move |this, window, cx| this.start_task(id, None, window, cx)),
                ));
            }
            if resumable {
                menu = menu.item(item(
                    L10nKey::BoardResume,
                    Box::new(move |this, window, cx| this.resume_task(id, window, cx)),
                ));
            }
            menu = match column {
                Column::Done => menu.item(item(
                    L10nKey::BoardReopen,
                    Box::new(move |this, window, cx| this.reopen(id, window, cx)),
                )),
                _ => menu.item(item(
                    L10nKey::BoardMarkDone,
                    Box::new(move |this, window, cx| this.mark_done(key, window, cx)),
                )),
            };
            menu.separator()
                .item(item(
                    L10nKey::BoardEdit,
                    Box::new(move |this, window, cx| this.open_composer(Some(id), window, cx)),
                ))
                .item(item(
                    L10nKey::BoardDelete,
                    Box::new(move |this, window, cx| this.delete_task(id, window, cx)),
                ))
        }
        CardRef::Loose(tab) => menu
            .item(item(
                L10nKey::BoardKeep,
                Box::new(move |this, window, cx| this.keep_loose(tab, window, cx)),
            ))
            .item(item(
                L10nKey::BoardMarkDone,
                Box::new(move |this, window, cx| this.mark_done(key, window, cx)),
            )),
    }
}

/// The dot a column heads itself with, and whether it is drawn as a ring.
/// The colours are the agent status dots the sidebar already uses, so a card
/// and its tab's row say a state the same way.
fn column_dot(col: Column, cx: &App) -> (Hsla, bool) {
    let muted = cx.theme().muted_foreground;
    let status = |s: AgentStatus| -> Hsla { gpui::rgb(s.dot_rgb().unwrap_or(0x888888)).into() };
    match col {
        Column::Queued => (muted, true),
        Column::Running => (status(AgentStatus::Working), false),
        Column::NeedsInput => (status(AgentStatus::Waiting), true),
        Column::Review => (status(AgentStatus::Done), false),
        Column::Done => (muted.opacity(0.6), false),
    }
}

/// `repo · branch` for a card, from where its agent runs.
fn place(cwd: Option<&std::path::Path>, branch: Option<&str>) -> Option<SharedString> {
    let repo = cwd
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned());
    match (repo, branch) {
        (Some(repo), Some(branch)) => Some(format!("{repo}  ·  {branch}").into()),
        (Some(repo), None) => Some(repo.into()),
        (None, Some(branch)) => Some(branch.to_string().into()),
        (None, None) => None,
    }
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
    fn a_place_names_what_it_knows() {
        let p = std::path::Path::new("/src/tty7");
        assert_eq!(
            place(Some(p), Some("main")).as_deref(),
            Some("tty7  ·  main")
        );
        assert_eq!(place(Some(p), None).as_deref(), Some("tty7"));
        assert_eq!(place(None, None), None);
    }
}

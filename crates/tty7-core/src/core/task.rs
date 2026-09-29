//! The board: work handed to coding agents, and where each piece of it stands.
//!
//! A [`Task`] is what the user asked for — a title, a prompt, where to run it —
//! plus the [`Run`]s that have been started for it. It lives on the workspace,
//! beside the tabs and groups, so it is kept by the same machine tree, moves
//! over the same deltas and dies with the workspace that holds it.
//!
//! What a task does *not* store is its column. Where a card sits is read off
//! the agents running it every time the board is drawn ([`column`]): a turn
//! starting, a permission prompt, a turn finishing are all things the agent
//! already reports, and a column written down beside them would only ever be
//! a second, staler copy of that. The one thing only a person can say — "this
//! is finished" — is the only verdict kept ([`Task::done`]), and even that
//! gives way when the agent is put back to work.

use serde::{Deserialize, Serialize};

use crate::core::cli_agent::{AgentSessionState, AgentStatus, CLIAgent};
use crate::core::group_key::GroupId;
use crate::core::machine::TabId;

/// How many tasks one workspace keeps. The board is a working surface rather
/// than an archive, and every one of these rides in the machine tree, which
/// is rewritten whole on every mutation.
pub const MAX_TASKS: usize = 512;

/// How long a title may be, in characters. A card shows two lines of it.
pub const MAX_TITLE_CHARS: usize = 200;

/// How long a prompt may be, in bytes. It is typed onto a command line, and
/// the whole task travels in every delta that touches it.
pub const MAX_PROMPT_BYTES: usize = 16 * 1024;

/// How many runs one task keeps. Each is a pane someone started for it; past
/// this the oldest are dropped, since a finished run is only kept to be
/// resumed and nobody resumes the tenth attempt at the same thing.
pub const MAX_RUNS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(uuid::Uuid);

impl TaskId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    pub fn parse(text: &str) -> Option<Self> {
        uuid::Uuid::parse_str(text).ok().map(Self)
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    /// What the agent is told. Empty means the title is the whole ask.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    /// Where a run starts, on the workspace's own host. `None` for a task
    /// written down before anyone said where it belongs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Which agent a run starts by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<CLIAgent>,
    /// A pinned sidebar group the card is filed under. `None` leaves it to the
    /// same automatic grouping a tab gets, worked out from [`Self::cwd`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<GroupId>,
    /// Each run gets a git worktree of its own, cut from [`Self::cwd`]'s
    /// repository — so two agents on the same repo, or one agent and the
    /// user, never write into the same checkout.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub worktree: bool,
    /// Unix seconds.
    #[serde(default)]
    pub created: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<Run>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<Done>,
    /// The user paused it — interrupted its agent and sent the card back to
    /// the queue — at this Unix second. It holds while no agent on it is in
    /// the middle of a turn; whoever puts one back to work clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<u64>,
}

/// One agent started for a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub agent: CLIAgent,
    /// The tab it was opened in. A tab rather than a pane: the tab exists the
    /// moment a run starts, while a pane on a remote machine has no id until
    /// it lands. A tab that has gone leaves the run behind as history,
    /// resumable by [`Self::session_id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<TabId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Unix seconds.
    #[serde(default)]
    pub started: u64,
    /// The worktree this run was given, when its task asked for one — where
    /// its work is, and what to clean up once it is merged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
}

/// The user said the task is finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {
    /// Unix seconds.
    pub at: u64,
    /// How many turns its live runs had finished between them when it was
    /// marked. A later turn means someone put an agent back on it, and a card
    /// that stayed in Done while its agent worked would be lying.
    #[serde(default)]
    pub turns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Column {
    /// Nothing has started working on it.
    Queued,
    /// An agent is in the middle of a turn.
    Running,
    /// An agent is stopped on a question or a permission prompt.
    NeedsInput,
    /// Every agent on it has stopped, and nobody has said it is finished.
    Review,
    Done,
}

impl Column {
    pub const ALL: [Column; 5] = [
        Column::Queued,
        Column::Running,
        Column::NeedsInput,
        Column::Review,
        Column::Done,
    ];
}

/// What the board needs to know about the agent a run's tab is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Live {
    pub status: AgentStatus,
    /// Turns finished, as [`AgentSessionState::turns`].
    pub turns: u64,
}

impl From<&AgentSessionState> for Live {
    fn from(state: &AgentSessionState) -> Live {
        Live {
            status: state.status,
            turns: state.turns,
        }
    }
}

/// Where one live agent session puts its card.
///
/// An agent that is open but has never been given anything to do is Queued,
/// not Running: nothing is happening, and the one thing that would move it on
/// is someone typing a prompt — which is what Queued means.
pub fn live_column(state: Live) -> Column {
    match state.status {
        AgentStatus::Waiting => Column::NeedsInput,
        AgentStatus::Working => Column::Running,
        AgentStatus::Done => Column::Review,
        AgentStatus::Idle if state.turns > 0 => Column::Review,
        AgentStatus::Idle => Column::Queued,
    }
}

/// Where a task's card sits, given what the agents in its runs' tabs are
/// doing now.
///
/// `live` answers for a run whose tab is still open and running an agent; a
/// run it has no answer for is taken to be over. The most urgent run wins: one
/// agent asking a question is the card's news even while another is busy,
/// and any agent still busy outranks the user's say-so that it is finished.
pub fn column(task: &Task, live: impl Fn(&Run) -> Option<Live>) -> Column {
    let states: Vec<Live> = task.runs.iter().filter_map(live).collect();
    if states.iter().any(|s| s.status == AgentStatus::Waiting) {
        return Column::NeedsInput;
    }
    if states.iter().any(|s| s.status == AgentStatus::Working) {
        return Column::Running;
    }
    if task.paused.is_some() {
        return Column::Queued;
    }
    if let Some(done) = task.done
        && turns(&states) <= done.turns
    {
        return Column::Done;
    }
    if task.runs.is_empty() {
        return Column::Queued;
    }
    // An agent open on a run that has not taken a turn yet was just started
    // for this task and is picking its prompt up.
    if states.iter().any(|&s| live_column(s) == Column::Queued) {
        return Column::Running;
    }
    Column::Review
}

/// The turns a task's live runs have finished between them, which is what a
/// [`Done`] mark is measured against.
pub fn live_turns(task: &Task, live: impl Fn(&Run) -> Option<Live>) -> u64 {
    let states: Vec<Live> = task.runs.iter().filter_map(live).collect();
    turns(&states)
}

fn turns(states: &[Live]) -> u64 {
    states
        .iter()
        .fold(0u64, |sum, s| sum.saturating_add(s.turns))
}

impl Task {
    pub fn new(title: impl Into<String>) -> Task {
        Task {
            id: TaskId::new(),
            title: title.into(),
            prompt: String::new(),
            cwd: None,
            agent: None,
            group: None,
            worktree: false,
            created: crate::core::machine::unix_now(),
            runs: Vec::new(),
            done: None,
            paused: None,
        }
    }

    /// What the agent is told when a run starts: the prompt, or the title when
    /// there is none.
    pub fn ask(&self) -> &str {
        let prompt = self.prompt.trim();
        if prompt.is_empty() {
            self.title.trim()
        } else {
            prompt
        }
    }

    /// Whether any run of this task was opened in `tab`.
    pub fn runs_in(&self, tab: TabId) -> bool {
        self.runs.iter().any(|r| r.tab == Some(tab))
    }

    /// Records a run, dropping the oldest past [`MAX_RUNS`].
    pub fn push_run(&mut self, run: Run) {
        self.runs.push(run);
        if self.runs.len() > MAX_RUNS {
            let excess = self.runs.len() - MAX_RUNS;
            self.runs.drain(..excess);
        }
    }

    /// Whether the task is fit to keep, and if not, why — in words for
    /// whoever sent it.
    pub fn check(&self) -> Result<(), String> {
        if self.title.trim().is_empty() {
            return Err("a task needs a title".into());
        }
        if self.title.chars().count() > MAX_TITLE_CHARS {
            return Err(format!(
                "a task title is at most {MAX_TITLE_CHARS} characters"
            ));
        }
        if self.prompt.len() > MAX_PROMPT_BYTES {
            return Err(format!(
                "a task prompt is at most {} KiB",
                MAX_PROMPT_BYTES / 1024
            ));
        }
        if self.runs.len() > MAX_RUNS {
            return Err(format!("a task keeps at most {MAX_RUNS} runs"));
        }
        Ok(())
    }
}

/// A branch and worktree name made from a task's title: lowercase ASCII
/// words joined by `-`, at most 40 characters. `None` for a title with no
/// ASCII word in it (a Chinese one, say), which gets a generated name instead.
pub fn branch_slug(title: &str) -> Option<String> {
    let mut slug = String::new();
    for word in title
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let word = word.to_ascii_lowercase();
        let sep = usize::from(!slug.is_empty());
        if slug.len() + sep + word.len() > 40 {
            break;
        }
        if sep == 1 {
            slug.push('-');
        }
        slug.push_str(&word);
    }
    (!slug.is_empty()).then_some(slug)
}

/// The argument that hands `agent` its first prompt while leaving it open for
/// the conversation that follows, or `None` for an agent whose command line
/// cannot do that — one that takes a prompt only to answer it and exit, or
/// takes none at all.
///
/// The flag, when there is one, comes before the prompt: `gemini -i "…"`.
pub fn prompt_flag(agent: CLIAgent) -> Option<Option<&'static str>> {
    match agent {
        CLIAgent::Claude | CLIAgent::Codex | CLIAgent::Cursor => Some(None),
        CLIAgent::Gemini | CLIAgent::Qwen | CLIAgent::Copilot => Some(Some("-i")),
        CLIAgent::OpenCode => Some(Some("--prompt")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(status: AgentStatus, turns: u64) -> Live {
        Live { status, turns }
    }

    /// A task with one run per entry, each in a tab of its own.
    fn task_with(runs: usize) -> (Task, Vec<TabId>) {
        let mut t = Task::new("fix it");
        let tabs: Vec<TabId> = (0..runs).map(|_| TabId::new()).collect();
        for &tab in &tabs {
            t.push_run(Run {
                agent: CLIAgent::Claude,
                tab: Some(tab),
                session_id: None,
                started: 0,
                worktree: None,
            });
        }
        (t, tabs)
    }

    fn col(task: &Task, states: &[(TabId, Live)]) -> Column {
        column(task, |r| {
            states
                .iter()
                .find(|(tab, _)| Some(*tab) == r.tab)
                .map(|(_, s)| *s)
        })
    }

    #[test]
    fn a_task_nobody_started_is_queued() {
        assert_eq!(col(&task_with(0).0, &[]), Column::Queued);
    }

    #[test]
    fn each_live_status_has_its_column() {
        let (t, tabs) = task_with(1);
        let at = |l| [(tabs[0], l)];
        assert_eq!(col(&t, &at(live(AgentStatus::Working, 0))), Column::Running);
        assert_eq!(
            col(&t, &at(live(AgentStatus::Waiting, 0))),
            Column::NeedsInput
        );
        assert_eq!(col(&t, &at(live(AgentStatus::Done, 1))), Column::Review);
        assert_eq!(col(&t, &at(live(AgentStatus::Idle, 2))), Column::Review);
    }

    #[test]
    fn a_run_that_has_not_picked_up_its_prompt_yet_counts_as_running() {
        let (t, tabs) = task_with(1);
        assert_eq!(
            col(&t, &[(tabs[0], live(AgentStatus::Idle, 0))]),
            Column::Running
        );
    }

    #[test]
    fn a_run_whose_tab_is_gone_leaves_the_task_to_review() {
        assert_eq!(col(&task_with(1).0, &[]), Column::Review);
    }

    #[test]
    fn the_most_urgent_run_wins() {
        let (t, tabs) = task_with(2);
        let states = [
            (tabs[0], live(AgentStatus::Working, 0)),
            (tabs[1], live(AgentStatus::Waiting, 0)),
        ];
        assert_eq!(col(&t, &states), Column::NeedsInput);
        let states = [
            (tabs[0], live(AgentStatus::Working, 0)),
            (tabs[1], live(AgentStatus::Done, 1)),
        ];
        assert_eq!(col(&t, &states), Column::Running);
    }

    #[test]
    fn done_holds_until_an_agent_takes_another_turn() {
        let (mut t, tabs) = task_with(1);
        t.done = Some(Done { at: 1, turns: 1 });
        let at = |l| [(tabs[0], l)];
        assert_eq!(col(&t, &at(live(AgentStatus::Done, 1))), Column::Done);
        assert_eq!(col(&t, &[]), Column::Done);
        assert_eq!(col(&t, &at(live(AgentStatus::Working, 1))), Column::Running);
        assert_eq!(col(&t, &at(live(AgentStatus::Done, 2))), Column::Review);
    }

    #[test]
    fn a_paused_task_waits_in_the_queue_until_its_agent_works_again() {
        let (mut t, tabs) = task_with(1);
        t.paused = Some(1);
        assert_eq!(
            col(&t, &[(tabs[0], live(AgentStatus::Done, 3))]),
            Column::Queued
        );
        assert_eq!(col(&t, &[]), Column::Queued);
        assert_eq!(
            col(&t, &[(tabs[0], live(AgentStatus::Working, 3))]),
            Column::Running
        );
    }

    #[test]
    fn a_task_marked_done_before_it_ever_ran_is_done() {
        let (mut t, _) = task_with(0);
        t.done = Some(Done { at: 1, turns: 0 });
        assert_eq!(col(&t, &[]), Column::Done);
    }

    #[test]
    fn a_live_agent_nobody_has_asked_anything_is_queued() {
        assert_eq!(live_column(live(AgentStatus::Idle, 0)), Column::Queued);
        assert_eq!(live_column(live(AgentStatus::Idle, 1)), Column::Review);
    }

    #[test]
    fn runs_past_the_cap_drop_the_oldest() {
        let (t, tabs) = task_with(MAX_RUNS + 3);
        assert_eq!(t.runs.len(), MAX_RUNS);
        assert_eq!(t.runs[0].tab, Some(tabs[3]));
    }

    #[test]
    fn check_refuses_what_the_tree_should_not_keep() {
        assert!(Task::new("  ").check().is_err());
        assert!(Task::new("x".repeat(MAX_TITLE_CHARS + 1)).check().is_err());
        let mut t = Task::new("ok");
        t.prompt = "p".repeat(MAX_PROMPT_BYTES + 1);
        assert!(t.check().is_err());
        assert!(Task::new("ok").check().is_ok());
    }

    #[test]
    fn a_branch_slug_keeps_the_ascii_words_of_a_title() {
        assert_eq!(
            branch_slug("Fix the flaky restore test!").as_deref(),
            Some("fix-the-flaky-restore-test")
        );
        assert_eq!(
            branch_slug("修复 #203 的 emoji 宽度").as_deref(),
            Some("203-emoji")
        );
        assert_eq!(branch_slug("列出最大的文件"), None);
        let long = branch_slug(&"word ".repeat(20)).unwrap();
        assert!(long.len() <= 40 && !long.ends_with('-'), "{long}");
    }

    #[test]
    fn the_ask_falls_back_to_the_title() {
        let mut t = Task::new(" Fix the flaky test ");
        assert_eq!(t.ask(), "Fix the flaky test");
        t.prompt = "  details\n".into();
        assert_eq!(t.ask(), "details");
    }

    #[test]
    fn a_task_round_trips_and_an_old_one_reads() {
        let (mut t, _) = task_with(1);
        t.prompt = "go".into();
        t.done = Some(Done { at: 9, turns: 2 });
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<Task>(&json).unwrap(), t);
        let bare: Task =
            serde_json::from_str(r#"{"id":"6f1c0b52-6a7e-4d53-9d1e-0a1b2c3d4e5f","title":"t"}"#)
                .unwrap();
        assert!(bare.runs.is_empty() && bare.done.is_none() && bare.prompt.is_empty());
    }
}

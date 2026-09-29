//! The New task sheet: where it runs, what to do, which agent, on which
//! branch.
//!
//! One box for the words — the first line is the card's title, all of it is
//! what the agent is told — so writing a task is writing a message, not
//! filling a form. Everything else is one line under it: the agent, whether
//! the run gets a worktree of its own, and the branch it lands on.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Hsla, SharedString, Subscription, Window, div,
    prelude::*, px, rems,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use tty7_core::core::group_key::GroupId;
use tty7_core::core::session::WorkspaceId;
use tty7_core::core::task::{self, Task, TaskId};

use super::{CardRef, Undo};
use crate::core::cli_agent::CLIAgent;
use crate::ui::app::Tty7App;
use crate::ui::dialog::{self, Tone};
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::right_panel::{META, META_MONO, TAB_TEXT};
use crate::ui::tab_sidebar::TaskPlace;
use crate::ui::worktree_prompt::branch_problem;

/// The New task card, or the same card open on a task being edited.
pub(crate) struct Composer {
    editing: Option<TaskId>,
    text: Entity<InputState>,
    /// What the words and the branch opened as, so a draft is only kept
    /// once they differ.
    seed: (String, String),
    /// The branch a worktree run is cut on; empty takes the name made from
    /// the title.
    branch: Entity<InputState>,
    /// The place picker's filter.
    search: Entity<InputState>,
    agent: Option<CLIAgent>,
    cwd: Option<PathBuf>,
    /// The pinned group the task is filed under, when its place is one.
    group: Option<GroupId>,
    worktree: bool,
    pop: Option<Pop>,
    /// The row of the open list that Enter picks.
    row: usize,
    /// The branch the run would land on already exists: that name, and the
    /// one it will get instead.
    taken: Option<(String, String)>,
    /// Bumped on every edit that changes the branch, so an older answer to
    /// "is it taken" that lands late is dropped.
    taken_seq: u64,
    /// Why the path typed into the place list was not taken.
    place_error: Option<String>,
    /// Holds the keys while the agent list is open, so 1–9 pick from it.
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

/// Which of the sheet's two lists is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pop {
    Place,
    Agent,
}

const WIDTH: f32 = 640.;
const TOP: f32 = 112.;
const HEAD_H: f32 = 46.;
const TEXT_H: f32 = 150.;
const FOOT_H: f32 = 52.;
const CONTROL_H: f32 = 28.;
const PLACE_POP_W: f32 = 300.;
const AGENT_POP_W: f32 = 232.;
const ROW_H: f32 = 30.;
/// A title is the first line, cut to what a card shows in two lines.
const TITLE_CHARS: usize = 48;

/// What a sheet closed without saving held, by the workspace and the task it
/// was editing (`None`: a new one). Written on every keystroke, so however
/// the sheet goes away — Esc, a click outside, the board closing under it —
/// opening it again brings the words back. Saving is what clears it.
#[derive(Default)]
struct Drafts(HashMap<(WorkspaceId, Option<TaskId>), (String, String)>);

impl gpui::Global for Drafts {}

/// How long the branch box sits still before git is asked whether the name
/// is taken.
const TAKEN_DEBOUNCE_MS: u64 = 300;

/// The card's title and the agent's prompt, out of what was typed.
fn split(text: &str) -> Option<(String, String)> {
    let text = text.trim();
    let first = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let cut = first.chars().count() > TITLE_CHARS;
    let title = match cut {
        true => format!(
            "{}…",
            first
                .chars()
                .take(TITLE_CHARS - 1)
                .collect::<String>()
                .trim_end()
        ),
        false => first.to_string(),
    };
    // A one-line task is all title: the agent is told the title. Unless
    // the title had to be cut — then the agent is told all of it.
    let prompt = match text == first && !cut {
        true => String::new(),
        false => text.to_string(),
    };
    Some((title, prompt))
}

/// What the sheet opens on for a task being edited: the words [`split`] made
/// its title and prompt from, whole.
fn seed(title: &str, prompt: &str) -> String {
    if prompt.is_empty() {
        title.to_string()
    } else if prompt.starts_with(title.trim_end_matches('…')) {
        prompt.to_string()
    } else {
        format!("{title}\n\n{prompt}")
    }
}

/// The name `name` would get from [`worktree::create_for`] — itself when no
/// branch has it, else the first `-2`, `-3`, … free — or `None` when it is
/// free. Only asks after branches: a retired name or a leftover folder also
/// moves `create_for` on, and this does not see those.
///
/// [`worktree::create_for`]: tty7_core::core::worktree::create_for
fn next_free_branch(h: &dyn crate::ui::host_ops::Host, cwd: &Path, name: &str) -> Option<String> {
    let exists = |branch: &str| {
        h.git(
            cwd,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .is_ok_and(|out| out.success())
    };
    if !exists(name) {
        return None;
    }
    (2..100)
        .map(|n| format!("{name}-{n}"))
        .find(|next| !exists(next))
}

/// The places whose name or folder has `query` in it, and — for a query that
/// is a path — that path itself, as a place of its own. `home` is where `~`
/// goes; a remote workspace has none this side knows, so there `~` is no
/// path at all.
fn matching_places(places: Vec<TaskPlace>, query: &str, home: Option<&Path>) -> Vec<TaskPlace> {
    let q = query.trim();
    let needle = q.to_lowercase();
    let mut out: Vec<TaskPlace> = places
        .into_iter()
        .filter(|p| {
            needle.is_empty()
                || p.name.to_lowercase().contains(&needle)
                || p.cwd.to_string_lossy().to_lowercase().contains(&needle)
        })
        .collect();
    // `~` and `~/…` are home; `~bob` is someone else's, which there is no
    // telling from here.
    let path = match q.strip_prefix('~') {
        Some("") => home.map(Path::to_path_buf),
        Some(rest) if rest.starts_with(['/', '\\']) => {
            home.map(|home| PathBuf::from(format!("{}{rest}", home.display())))
        }
        Some(_) => None,
        None => (q.starts_with('/') || q.contains(":\\")).then(|| PathBuf::from(q)),
    };
    if let Some(path) = path
        && !out.iter().any(|p| p.cwd == path)
    {
        out.push(TaskPlace {
            group: None,
            name: q.to_string(),
            cwd: path,
            grouped: false,
            branch: None,
        });
    }
    out
}

impl Tty7App {
    pub(crate) fn open_composer(
        &mut self,
        editing: Option<TaskId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = editing.and_then(|id| self.task(id));
        let text_seed = existing
            .as_ref()
            .map_or(String::new(), |t| seed(&t.title, &t.prompt));
        let branch_seed = existing
            .as_ref()
            .and_then(|t| t.branch.clone())
            .unwrap_or_default();
        // A sheet closed on unsaved words opens on them again.
        let (text_now, branch_now) = cx
            .default_global::<Drafts>()
            .0
            .get(&(self.workspace, editing))
            .cloned()
            .unwrap_or_else(|| (text_seed.clone(), branch_seed.clone()));
        let text = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder(t(L10nKey::BoardComposerPlaceholder))
                .default_value(text_now)
        });
        let branch = cx.new(|cx| InputState::new(window, cx).default_value(branch_now));
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(t(L10nKey::BoardPlaceSearch)));
        text.update(cx, |state, cx| state.focus(window, cx));
        let subs = vec![
            cx.subscribe_in(
                &text,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter {
                        secondary: true, ..
                    } => this.submit_composer(true, window, cx),
                    InputEvent::Change => {
                        this.keep_composer_draft(cx);
                        this.refresh_branch_placeholder(window, cx);
                    }
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &branch,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter {
                        secondary: true, ..
                    } => this.submit_composer(true, window, cx),
                    InputEvent::Change => {
                        this.keep_composer_draft(cx);
                        this.check_composer_branch(window, cx);
                    }
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &search,
                window,
                |this, input, ev: &InputEvent, window, cx| match ev {
                    // Enter takes the row picked with the arrows — the first
                    // one left after filtering, unless moved.
                    InputEvent::PressEnter { .. } => {
                        let query = input.read(cx).value().to_string();
                        let row = this.board.composer.as_ref().map_or(0, |c| c.row);
                        if let Some(place) = this.composer_places(&query, cx).into_iter().nth(row) {
                            this.choose_composer_place(place, window, cx);
                        }
                    }
                    InputEvent::Change => {
                        if let Some(c) = this.board.composer.as_mut() {
                            c.row = 0;
                            c.place_error = None;
                        }
                        cx.notify();
                    }
                    _ => {}
                },
            ),
        ];
        let (places, here) = self.task_places(cx);
        let here = here.and_then(|i| places.get(i));
        // A new task goes to the group the board is filtered to, else to the
        // place the active tab is in.
        let (group, cwd) = match &existing {
            Some(t) => (t.group, t.cwd.as_ref().map(PathBuf::from)),
            None => {
                let filtered = self
                    .board
                    .group_filter
                    .and_then(|g| places.iter().find(|p| p.group == Some(g)));
                match filtered.or(here) {
                    Some(p) => (p.group, Some(p.cwd.clone())),
                    None => (None, None),
                }
            }
        };
        let agent = existing
            .as_ref()
            .and_then(|t| t.agent)
            .or_else(|| self.offered_agents(cx).first().copied());
        let worktree = existing
            .as_ref()
            .map_or(self.board.last_worktree, |t| t.worktree);
        self.board.composer = Some(Composer {
            editing,
            text,
            seed: (text_seed, branch_seed),
            branch,
            search,
            agent,
            cwd,
            group: group.filter(|g| self.sidebar_groups.contains(*g)),
            worktree,
            pop: None,
            row: 0,
            taken: None,
            taken_seq: 0,
            place_error: None,
            focus: cx.focus_handle(),
            _subs: subs,
        });
        self.refresh_branch_placeholder(window, cx);
    }

    /// Writes down what the sheet holds, or forgets it once it is back to
    /// what the sheet opened on.
    fn keep_composer_draft(&mut self, cx: &mut Context<Self>) {
        let Some(c) = self.board.composer.as_ref() else {
            return;
        };
        let now = (
            c.text.read(cx).value().to_string(),
            c.branch.read(cx).value().to_string(),
        );
        let key = (self.workspace, c.editing);
        let unchanged = now == c.seed || (now.0.trim().is_empty() && now.1.trim().is_empty());
        let drafts = &mut cx.default_global::<Drafts>().0;
        match unchanged {
            true => drafts.remove(&key),
            false => drafts.insert(key, now),
        };
    }

    /// The branch a worktree run would be cut on: the one typed, else the
    /// one the title makes. `None` in place, or with nothing to name it by.
    fn composer_branch(&self, cx: &App) -> Option<String> {
        let c = self.board.composer.as_ref()?;
        if !c.worktree {
            return None;
        }
        let typed = c.branch.read(cx).value().trim().to_string();
        match typed.is_empty() {
            true => {
                split(&c.text.read(cx).value()).and_then(|(title, _)| task::branch_slug(&title))
            }
            false => Some(typed),
        }
    }

    /// What is wrong with the branch typed, in words for under the box.
    fn composer_branch_problem(&self, cx: &App) -> Option<String> {
        let c = self.board.composer.as_ref()?;
        if !c.worktree {
            return None;
        }
        branch_problem(c.branch.read(cx).value().trim()).map(|p| p.message())
    }

    /// Asks the workspace's machine, once typing settles, whether the branch
    /// the run would land on is already there — `create_for` moves on to
    /// `-2` then, and the sheet should say so before, not after.
    fn check_composer_branch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self
            .composer_branch(cx)
            .filter(|_| self.composer_branch_problem(cx).is_none());
        let host_id = self.spawn_host(cx);
        let host = crate::ui::host_registry::HostRegistry::get(cx, host_id);
        let Some(c) = self.board.composer.as_mut() else {
            return;
        };
        c.taken_seq += 1;
        let seq = c.taken_seq;
        let (Some(name), Some(cwd), Some(host)) = (name, c.cwd.clone(), host) else {
            c.taken = None;
            cx.notify();
            return;
        };
        if c.taken.as_ref().is_some_and(|(was, _)| *was != name) {
            c.taken = None;
        }
        let current = move |this: &Tty7App| {
            this.board
                .composer
                .as_ref()
                .is_some_and(|c| c.taken_seq == seq)
        };
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(TAKEN_DEBOUNCE_MS))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !current(this) {
                    return;
                }
                crate::ui::host_ops::HostOps::run_in(
                    host,
                    window,
                    cx,
                    {
                        let name = name.clone();
                        move |h| next_free_branch(h, &cwd, &name)
                    },
                    move |this, next, _, cx| {
                        if !current(this) {
                            return;
                        }
                        if let Some(c) = this.board.composer.as_mut() {
                            c.taken = next.map(|next| (name, next));
                            cx.notify();
                        }
                    },
                );
            });
        })
        .detach();
        cx.notify();
    }

    /// Shows the branch the title would name, in the branch box, until the
    /// user types one of their own.
    fn refresh_branch_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.board.composer.as_ref() else {
            return;
        };
        let auto = split(&c.text.read(cx).value())
            .and_then(|(title, _)| task::branch_slug(&title))
            .unwrap_or_else(|| t(L10nKey::BoardBranchAuto).to_string());
        let branch = c.branch.clone();
        branch.update(cx, |s, cx| s.set_placeholder(auto, window, cx));
        self.check_composer_branch(window, cx);
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
        let Some((title, prompt)) = split(&c.text.read(cx).value()) else {
            window.push_notification(t(L10nKey::BoardNeedsTitle), cx);
            return;
        };
        // The same as Start being greyed out: ⌘↵ must not save and close
        // only to say there is nothing to start it with.
        if start && c.agent.is_none() {
            window.push_notification(t(L10nKey::BoardNoAgent), cx);
            return;
        }
        if let Some(problem) = self.composer_branch_problem(cx) {
            window.push_notification(problem, cx);
            return;
        }
        let Some(c) = self.board.composer.as_ref() else {
            return;
        };
        let branch = c.branch.read(cx).value().trim().to_string();
        let editing = c.editing;
        let before = editing.and_then(|id| self.task(id));
        // Saving an edit to a task someone else has since removed would put
        // it back; the sheet stays open, so the words can still be copied.
        if editing.is_some() && before.is_none() {
            window.push_notification(t(L10nKey::BoardTaskGone), cx);
            return;
        }
        let mut task = before.clone().unwrap_or_else(|| Task::new(String::new()));
        task.title = title.clone();
        task.prompt = prompt;
        task.agent = c.agent;
        task.cwd = c.cwd.as_ref().map(|p| p.display().to_string());
        task.group = c.group;
        task.worktree = c.worktree;
        task.branch = (c.worktree && !branch.is_empty()).then_some(branch);
        self.board.last_worktree = c.worktree;
        let (id, agent) = (task.id, task.agent);
        if !self.save_task(task, window, cx) {
            return;
        }
        cx.default_global::<Drafts>()
            .0
            .remove(&(self.workspace, editing));
        self.board.composer = None;
        self.board.selected = Some(CardRef::Task(id));
        if start {
            self.start_task(id, agent, window, cx);
        } else {
            let undo = match before {
                Some(before) => Undo::Edit(before),
                None => Undo::Remove(id),
            };
            self.flash(
                t_fmt(L10nKey::BoardToastQueued, &[("title", &title)]),
                Some(undo),
                cx,
            );
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    /// Opens one of the sheet's lists, or closes it if it is the one open.
    /// The place list takes the keys for its filter, the agent list for
    /// 1–9 and the arrows; closing hands them back to the task's words.
    fn toggle_composer_pop(&mut self, pop: Pop, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.board.composer.as_mut() else {
            return;
        };
        c.pop = (c.pop != Some(pop)).then_some(pop);
        c.row = 0;
        c.place_error = None;
        match c.pop {
            Some(Pop::Place) => {
                let search = c.search.clone();
                search.update(cx, |s, cx| {
                    s.set_value("", window, cx);
                    s.focus(window, cx);
                });
            }
            Some(Pop::Agent) => window.focus(&c.focus.clone(), cx),
            None => {
                let text = c.text.clone();
                text.update(cx, |s, cx| s.focus(window, cx));
            }
        }
        cx.notify();
    }

    fn close_composer_pop(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match self.board.composer.as_ref().and_then(|c| c.pop) {
            Some(pop) => {
                self.toggle_composer_pop(pop, window, cx);
                true
            }
            None => false,
        }
    }

    /// Where `~` goes for the place list: this computer's home, and nowhere
    /// for a workspace on another machine.
    fn composer_home(&self, cx: &App) -> Option<PathBuf> {
        self.spawn_host(cx)
            .is_local()
            .then(|| std::env::var_os("HOME").map(PathBuf::from))
            .flatten()
    }

    /// The place list's rows for `query`.
    fn composer_places(&self, query: &str, cx: &App) -> Vec<TaskPlace> {
        let (places, _) = self.task_places(cx);
        matching_places(places, query, self.composer_home(cx).as_deref())
    }

    /// Moves the open list's pick by `by` rows, within the rows it has.
    fn move_composer_row(&mut self, by: isize, cx: &mut Context<Self>) {
        let Some(c) = self.board.composer.as_ref() else {
            return;
        };
        let rows = match c.pop {
            Some(Pop::Place) => {
                let query = c.search.read(cx).value().to_string();
                self.composer_places(&query, cx).len()
            }
            Some(Pop::Agent) => self.offered_agents(cx).len(),
            None => return,
        };
        if let Some(c) = self.board.composer.as_mut()
            && rows > 0
        {
            c.row = c.row.saturating_add_signed(by).min(rows - 1);
            cx.notify();
        }
    }

    /// Takes a place from the list. One of the board's own places is taken
    /// as it is; a path typed in is looked for first, on the machine the
    /// workspace is on, so a task is never filed under a folder that is not
    /// there.
    fn choose_composer_place(
        &mut self,
        place: TaskPlace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (known, _) = self.task_places(cx);
        if known.iter().any(|p| p.cwd == place.cwd) {
            self.set_composer_place(place, window, cx);
            return;
        }
        let host_id = self.spawn_host(cx);
        let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, host_id) else {
            window.push_notification(t(L10nKey::BoardUnavailable), cx);
            return;
        };
        let path = place.cwd.clone();
        crate::ui::host_ops::HostOps::run_in(
            host,
            window,
            cx,
            move |h| h.stat(&path).is_ok_and(|m| m.is_dir),
            move |this, found, window, cx| match found {
                true => this.set_composer_place(place, window, cx),
                false => {
                    if let Some(c) = this.board.composer.as_mut() {
                        c.place_error = Some(t_fmt(
                            L10nKey::BoardPlaceMissing,
                            &[("path", &place.cwd.display().to_string())],
                        ));
                        cx.notify();
                    }
                }
            },
        );
    }

    fn set_composer_place(
        &mut self,
        place: TaskPlace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(c) = self.board.composer.as_mut() {
            c.group = place.group;
            c.cwd = Some(place.cwd);
        }
        self.close_composer_pop(window, cx);
        self.check_composer_branch(window, cx);
    }

    fn set_composer_agent(&mut self, agent: CLIAgent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(c) = self.board.composer.as_mut() {
            c.agent = Some(agent);
        }
        self.close_composer_pop(window, cx);
    }

    fn set_composer_worktree(
        &mut self,
        worktree: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(c) = self.board.composer.as_mut() {
            c.worktree = worktree;
        }
        self.check_composer_branch(window, cx);
    }

    /// The New task sheet, over the whole window like every other sheet.
    pub(crate) fn render_composer(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let c = self.board.composer.as_ref()?;
        let theme = cx.theme();
        let (fg, muted, border) = (theme.foreground, theme.muted_foreground, theme.border);
        let mono = theme.mono_font_family.clone();
        let rungs = dialog::popover_rungs(cx);
        let hover: Hsla = gpui::rgb(rungs.hover).into();
        let well = dialog::well_fill(cx);
        let surface = theme.popover;
        let text = c.text.read(cx).value().to_string();
        let problem = self.composer_branch_problem(cx);
        let ready = split(&text).is_some() && problem.is_none();
        let editing = c.editing.is_some();
        let row_now = c.row;
        let danger = theme.danger;

        // ---- where ----------------------------------------------------------
        let (places, _) = self.task_places(cx);
        let chosen = places
            .iter()
            .find(|p| p.group.is_some() && p.group == c.group)
            .or_else(|| places.iter().find(|p| Some(&p.cwd) == c.cwd.as_ref()))
            .cloned();
        let place_name: SharedString = match (&chosen, &c.cwd) {
            (Some(p), _) => p.name.clone().into(),
            (None, Some(cwd)) => cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| cwd.display().to_string())
                .into(),
            (None, None) => t(L10nKey::BoardNoPlace).into(),
        };
        let place_branch = chosen.as_ref().and_then(|p| p.branch.clone());
        let pop = c.pop;
        let place_button = h_flex()
            .id("board-composer-place")
            .h(px(26.))
            .px(px(8.))
            .gap(px(7.))
            .items_center()
            .rounded(px(6.))
            .when(pop == Some(Pop::Place), |d| d.bg(hover))
            .hover(move |s| s.bg(hover))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.toggle_composer_pop(Pop::Place, window, cx)
            }))
            .child(
                Icon::empty()
                    .path("icons/folder-closed.svg")
                    .size(px(12.))
                    .text_color(muted),
            )
            .child(
                div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(place_name),
            )
            .when_some(place_branch.clone(), |d, b| {
                d.child(
                    div()
                        .font_family(mono.clone())
                        .text_size(rems(META_MONO))
                        .text_color(muted)
                        .child(b),
                )
            })
            .child(
                Icon::new(IconName::ChevronDown)
                    .size(px(10.))
                    .text_color(muted),
            );
        let place_pop = (pop == Some(Pop::Place)).then(|| {
            let query = c.search.read(cx).value().to_string();
            let rows = self.composer_places(&query, cx);
            let empty = rows.is_empty();
            let current = chosen.clone();
            // A `~` path means nothing to a workspace on another machine;
            // say so rather than show no row for it.
            let error = c.place_error.clone().or_else(|| {
                (query.trim().starts_with('~') && self.composer_home(cx).is_none())
                    .then(|| t(L10nKey::BoardPlaceRemoteHome).to_string())
            });
            v_flex()
                .absolute()
                .top(px(32.))
                .left_0()
                .w(px(PLACE_POP_W))
                .p(px(5.))
                .gap(px(1.))
                .occlude()
                .map(|panel| crate::ui::theme::floating_surface(panel, cx))
                .rounded(px(10.))
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    h_flex()
                        .h(px(30.))
                        .px(px(8.))
                        .mb(px(3.))
                        .gap(px(7.))
                        .items_center()
                        .border_b_1()
                        .border_color(border)
                        .child(Icon::new(IconName::Search).size(px(11.)).text_color(muted))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&c.search).appearance(false).small()),
                        ),
                )
                .children(rows.into_iter().enumerate().map(|(i, place)| {
                    let on = current
                        .as_ref()
                        .is_some_and(|p| p.cwd == place.cwd && p.group == place.group);
                    let (name, branch) = (place.name.clone(), place.branch.clone());
                    let tip = place.cwd.display().to_string();
                    h_flex()
                        .id(("board-place-row", i))
                        .h(px(32.))
                        .px(px(8.))
                        .gap(px(8.))
                        .items_center()
                        .rounded(px(6.))
                        .when(i == row_now, |d| d.bg(hover))
                        .hover(move |s| s.bg(hover))
                        .cursor_pointer()
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.choose_composer_place(place.clone(), window, cx)
                        }))
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .when_some(branch, |d, b| {
                            d.child(
                                div()
                                    .font_family(mono.clone())
                                    .text_size(rems(META_MONO))
                                    .text_color(muted)
                                    .child(b),
                            )
                        })
                        .child(div().w(px(12.)).when(on, |d| {
                            d.child(Icon::new(IconName::Check).size(px(11.)).text_color(fg))
                        }))
                }))
                .when(empty && error.is_none(), |v| {
                    v.child(
                        div()
                            .px(px(8.))
                            .py(px(10.))
                            .text_size(rems(META))
                            .text_color(muted)
                            .child(t(L10nKey::BoardNoMatches)),
                    )
                })
                .when_some(error, |v, error| {
                    v.child(
                        div()
                            .px(px(8.))
                            .py(px(8.))
                            .text_size(rems(META))
                            .text_color(danger)
                            .child(error),
                    )
                })
        });
        let head = h_flex()
            .flex_none()
            .h(px(HEAD_H))
            .pl(px(18.))
            .pr(px(10.))
            .gap(px(6.))
            .items_center()
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child(t(if editing {
                        L10nKey::BoardEditTask
                    } else {
                        L10nKey::BoardNewTask
                    })),
            )
            .child(div().text_color(muted).child(t(L10nKey::BoardComposerIn)))
            // Deferred: the list drops over the text box, which is drawn
            // after it and would otherwise cover it.
            .child(
                div()
                    .relative()
                    .child(place_button)
                    .children(place_pop.map(|p| gpui::deferred(p).with_priority(1))),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("board-composer-close")
                    .size(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .hover(move |s| s.bg(hover))
                    .cursor_pointer()
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new(t(L10nKey::BoardPeekClose))
                            .build(window, cx)
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.close_composer(window, cx)))
                    .child(Icon::new(IconName::Close).size(px(12.)).text_color(muted)),
            );

        // ---- what -------------------------------------------------------------
        let body = div()
            .h(px(TEXT_H))
            .pt(px(2.))
            .px(px(10.))
            .text_size(rems(15. / 16.))
            .child(Input::new(&c.text).appearance(false).h_full());

        // ---- who, and on which branch -----------------------------------------
        let agents = self.offered_agents(cx);
        let agent_now = c.agent;
        let avatar = |agent: Option<CLIAgent>, id: String, cx: &mut Context<Self>| {
            crate::ui::tab_strip::avatar(
                SharedString::from(id),
                crate::ui::search::Avatar {
                    agent,
                    ..Default::default()
                },
                18.,
                cx,
            )
        };
        let agent_button = h_flex()
            .id("board-composer-agent")
            .h(px(CONTROL_H))
            .pl(px(6.))
            .pr(px(8.))
            .gap(px(7.))
            .items_center()
            .rounded(px(7.))
            .when(pop == Some(Pop::Agent), |d| d.bg(hover))
            .hover(move |s| s.bg(hover))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.toggle_composer_pop(Pop::Agent, window, cx)
            }))
            .child(avatar(agent_now, "board-composer-agent-avatar".into(), cx))
            .child(
                div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(agent_now.map_or(t(L10nKey::BoardNoAgent), short_name)),
            )
            .child(
                Icon::new(IconName::ChevronDown)
                    .size(px(10.))
                    .text_color(muted),
            );
        let agent_pop = (pop == Some(Pop::Agent)).then(|| {
            // Down from the footer: the sheet sits near the window's top, so
            // a list opened upward runs off it once a few agents are
            // installed. Past ten rows it scrolls.
            v_flex()
                .id("board-agent-pop")
                .absolute()
                .top(px(34.))
                .left_0()
                .w(px(AGENT_POP_W))
                .max_h(px(ROW_H * 10. + 10.))
                .overflow_y_scroll()
                .p(px(5.))
                .gap(px(1.))
                .occlude()
                .map(|panel| crate::ui::theme::floating_surface(panel, cx))
                .rounded(px(10.))
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .children(agents.iter().copied().enumerate().map(|(i, agent)| {
                    let on = agent_now == Some(agent);
                    h_flex()
                        .id(("board-agent-row", i))
                        .h(px(ROW_H))
                        .pl(px(6.))
                        .pr(px(8.))
                        .gap(px(8.))
                        .items_center()
                        .rounded(px(6.))
                        .when(i == row_now, |d| d.bg(hover))
                        .hover(move |s| s.bg(hover))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_composer_agent(agent, window, cx)
                        }))
                        .child(avatar(
                            Some(agent),
                            format!("board-agent-row-{}", agent.slug()),
                            cx,
                        ))
                        .child(div().flex_1().child(agent.display_name()))
                        .child(div().w(px(12.)).when(on, |d| {
                            d.child(Icon::new(IconName::Check).size(px(11.)).text_color(fg))
                        }))
                        .child(
                            div()
                                .w(px(14.))
                                .text_right()
                                .text_size(rems(META_MONO))
                                .text_color(muted)
                                .when(i < 9, |d| d.child((i + 1).to_string())),
                        )
                }))
        });

        let worktree = c.worktree;
        let segment = |id: &'static str,
                       label: L10nKey,
                       tip: L10nKey,
                       on_worktree: bool,
                       cx: &mut Context<Self>| {
            let on = worktree == on_worktree;
            div()
                .id(id)
                .h(px(24.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(px(5.))
                .text_size(rems(META))
                .text_color(if on { fg } else { muted })
                .when(on, |d| {
                    d.bg(surface)
                        .border_1()
                        .border_color(border)
                        .font_weight(gpui::FontWeight::MEDIUM)
                })
                .when(!on, |d| d.cursor_pointer().hover(move |s| s.text_color(fg)))
                .tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(t(tip)).build(window, cx)
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_composer_worktree(on_worktree, window, cx)
                }))
                .child(t(label))
        };
        let modes = h_flex()
            .flex_none()
            .h(px(CONTROL_H))
            .p(px(2.))
            .rounded(px(7.))
            .bg(well)
            .child(segment(
                "board-mode-worktree",
                L10nKey::BoardModeWorktree,
                L10nKey::BoardModeWorktreeTip,
                true,
                cx,
            ))
            .child(segment(
                "board-mode-in-place",
                L10nKey::BoardModeInPlace,
                L10nKey::BoardModeInPlaceTip,
                false,
                cx,
            ));
        // The branch the run lands on: one the user can name, with the name
        // made from the title shown until they do — or, in place, the branch
        // already checked out, which is not theirs to rename here.
        let branch_field = h_flex()
            .id("board-composer-branch")
            .flex_1()
            .min_w_0()
            .h(px(CONTROL_H))
            .px(px(8.))
            .gap(px(7.))
            .items_center()
            .rounded(px(7.))
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(t(if worktree {
                    L10nKey::BoardBranchEditTip
                } else {
                    L10nKey::BoardBranchInPlaceTip
                }))
                .build(window, cx)
            })
            .child(
                Icon::empty()
                    .path("icons/git-branch.svg")
                    .size(px(12.))
                    .text_color(muted),
            )
            .child(match worktree {
                true => div()
                    .flex_1()
                    .min_w_0()
                    .font_family(mono.clone())
                    .text_size(rems(META_MONO))
                    .child(Input::new(&c.branch).appearance(false).small())
                    .into_any_element(),
                false => div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(mono.clone())
                    .text_size(rems(META_MONO))
                    .text_color(muted)
                    .child(
                        place_branch.unwrap_or_else(|| t(L10nKey::BoardCurrentBranch).to_string()),
                    )
                    .into_any_element(),
            });
        let start_label = h_flex().gap(px(7.)).child(t(L10nKey::BoardStartNow)).child(
            div()
                .opacity(0.6)
                .text_size(rems(11. / 16.))
                .child(crate::ui::keymap::key_tokens("secondary-enter").concat()),
        );
        // Under the controls, what is wrong with the branch typed — or, for a
        // good one git already has, the name the run will get instead.
        let branch_note = match (&problem, &c.taken) {
            (Some(problem), _) => Some((problem.clone(), danger)),
            (None, Some((branch, next))) if worktree => Some((
                t_fmt(
                    L10nKey::BoardBranchTaken,
                    &[("branch", branch), ("next", next)],
                ),
                muted,
            )),
            _ => None,
        };
        let foot = h_flex()
            .flex_none()
            .h(px(FOOT_H))
            .pl(px(12.))
            .pr(px(10.))
            .gap(px(6.))
            .items_center()
            .border_t_1()
            .border_color(border)
            .text_size(rems(TAB_TEXT))
            .child(
                div()
                    .relative()
                    .child(agent_button)
                    .children(agent_pop.map(|p| gpui::deferred(p).with_priority(1))),
            )
            .child(div().w(px(0.5)).h(px(16.)).mx(px(4.)).bg(border))
            .child(modes)
            .child(branch_field)
            .child(
                dialog::button(
                    "board-composer-queue",
                    t(if editing {
                        L10nKey::BoardSave
                    } else {
                        L10nKey::BoardAddToQueue
                    }),
                    Tone::Secondary,
                    ready,
                    rungs,
                    cx,
                    cx.listener(|this, _, window, cx| this.submit_composer(false, window, cx)),
                )
                .h(px(CONTROL_H)),
            )
            .child(
                dialog::button(
                    "board-composer-start",
                    "",
                    Tone::Primary,
                    ready && c.agent.is_some(),
                    rungs,
                    cx,
                    cx.listener(|this, _, window, cx| this.submit_composer(true, window, cx)),
                )
                .h(px(CONTROL_H))
                .child(start_label),
            );

        let agent_keys = agents.clone();
        let card = v_flex()
            .id("board-composer")
            .track_focus(&c.focus)
            .occlude()
            .w(px(WIDTH))
            .map(|panel| crate::ui::theme::floating_surface(panel, cx))
            .rounded(px(dialog::CARD_RADIUS))
            .text_size(rems(TAB_TEXT))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    // A click anywhere else on the sheet closes an open list.
                    this.close_composer_pop(window, cx);
                }),
            )
            // The arrows move through the place list while its filter has
            // the keys; the box would only swallow them.
            .capture_action(
                cx.listener(|this, _: &gpui_component::input::MoveUp, _, cx| {
                    if this.board.composer.as_ref().and_then(|c| c.pop) == Some(Pop::Place) {
                        cx.stop_propagation();
                        this.move_composer_row(-1, cx);
                    }
                }),
            )
            .capture_action(
                cx.listener(|this, _: &gpui_component::input::MoveDown, _, cx| {
                    if this.board.composer.as_ref().and_then(|c| c.pop) == Some(Pop::Place) {
                        cx.stop_propagation();
                        this.move_composer_row(1, cx);
                    }
                }),
            )
            .on_key_down(
                cx.listener(move |this, ev: &gpui::KeyDownEvent, window, cx| {
                    let k = &ev.keystroke;
                    if this.board.composer.as_ref().and_then(|c| c.pop) != Some(Pop::Agent)
                        || k.modifiers.modified()
                    {
                        return;
                    }
                    // 1–9 pick from the open agent list, as do the arrows
                    // and Enter.
                    let pick = match k.key.as_str() {
                        "up" | "down" => {
                            cx.stop_propagation();
                            this.move_composer_row(if k.key == "up" { -1 } else { 1 }, cx);
                            return;
                        }
                        "enter" => this.board.composer.as_ref().map(|c| c.row),
                        key => key
                            .parse::<usize>()
                            .ok()
                            .filter(|n| (1..=9).contains(n))
                            .map(|n| n - 1),
                    };
                    if let Some(agent) = pick.and_then(|i| agent_keys.get(i).copied()) {
                        cx.stop_propagation();
                        this.set_composer_agent(agent, window, cx);
                    }
                }),
            )
            .child(head)
            .child(body)
            .children(branch_note.map(|(note, color)| {
                div()
                    .px(px(18.))
                    .pb(px(6.))
                    .text_size(rems(META))
                    .text_color(color)
                    .child(note)
            }))
            .child(foot);
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .bg(crate::ui::presets::scrim_fill(cx))
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                    if ev.keystroke.key == "escape" {
                        cx.stop_propagation();
                        // Esc closes an open list first, then the sheet.
                        if !this.close_composer_pop(window, cx) {
                            this.close_composer(window, cx);
                        }
                    }
                }))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                        this.close_composer(window, cx)
                    }),
                )
                .flex()
                .flex_col()
                .items_center()
                .pt(px(TOP))
                .child(card)
                .into_any_element(),
        )
    }
}

/// An agent's name short enough for a button: `Claude Code` is `Claude`.
fn short_name(agent: CLIAgent) -> &'static str {
    let name = agent.display_name();
    match name.split_once(' ') {
        Some((first, "Code" | "CLI" | "Agent")) => first,
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_line_is_the_title_and_the_rest_the_prompt() {
        assert_eq!(split("  \n"), None);
        assert_eq!(
            split("Fix the build"),
            Some(("Fix the build".into(), String::new()))
        );
        let (title, prompt) = split("Fix the build\n\nIt fails on Windows.").unwrap();
        assert_eq!(title, "Fix the build");
        assert_eq!(prompt, "Fix the build\n\nIt fails on Windows.");
        let (title, _) = split(&"x".repeat(80)).unwrap();
        assert_eq!(title.chars().count(), TITLE_CHARS);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn a_line_too_long_for_a_title_is_all_told_to_the_agent() {
        let long = "Make the settings page remember which section was open last time";
        let (title, prompt) = split(long).unwrap();
        assert!(title.ends_with('…'));
        assert_eq!(prompt, long, "nothing typed is lost to the cut");
        // Editing it opens on all of it, and saving again changes nothing.
        let again = seed(&title, &prompt);
        assert_eq!(again, long);
        assert_eq!(split(&again), Some((title, prompt)));
    }

    #[test]
    fn editing_opens_on_what_was_typed() {
        assert_eq!(seed("Fix the build", ""), "Fix the build");
        assert_eq!(
            seed("Fix the build", "Fix the build\n\nIt fails on Windows."),
            "Fix the build\n\nIt fails on Windows."
        );
        // A task written elsewhere, whose prompt does not repeat its title.
        assert_eq!(seed("Fix it", "It fails."), "Fix it\n\nIt fails.");
    }

    #[test]
    fn a_button_drops_the_product_suffix() {
        assert_eq!(short_name(CLIAgent::Claude), "Claude");
    }

    fn place(name: &str, cwd: &str) -> TaskPlace {
        TaskPlace {
            group: None,
            name: name.into(),
            cwd: cwd.into(),
            grouped: true,
            branch: None,
        }
    }

    #[test]
    fn the_place_filter_matches_names_and_folders_and_takes_a_path() {
        let places = vec![place("tty7", "/src/tty7"), place("orbit", "/work/orbit")];
        let names = |q: &str| -> Vec<String> {
            matching_places(places.clone(), q, Some(Path::new("/home/me")))
                .into_iter()
                .map(|p| p.name)
                .collect()
        };
        assert_eq!(names(""), vec!["tty7", "orbit"]);
        assert_eq!(names("ORB"), vec!["orbit"]);
        assert_eq!(names("/work"), vec!["orbit", "/work"]);
        assert_eq!(
            names("/src/tty7"),
            vec!["tty7"],
            "a known path is not offered twice"
        );
        assert!(names("nothing").is_empty());
        let home = matching_places(places.clone(), "~/src", Some(Path::new("/home/me")));
        assert_eq!(home.last().unwrap().cwd, PathBuf::from("/home/me/src"));
        assert!(
            matching_places(places.clone(), "~/src", None).is_empty(),
            "no `~` without a home to put it in"
        );
        let bob = matching_places(places.clone(), "~bob", Some(Path::new("/home/me")));
        assert!(
            bob.iter().all(|p| !p.cwd.starts_with("/home/me")),
            "`~bob` is not under my home"
        );
    }
}

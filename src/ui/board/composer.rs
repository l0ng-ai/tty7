//! The New task sheet: where it runs, what to do, which agent, on which
//! branch.
//!
//! One box for the words — the first line is the card's title, all of it is
//! what the agent is told — so writing a task is writing a message, not
//! filling a form. Everything else is one line under it: the agent, whether
//! the run gets a worktree of its own, and the branch it lands on.

use std::path::PathBuf;

use gpui::{
    AnyElement, Context, Entity, FocusHandle, Hsla, SharedString, Subscription, Window, div,
    prelude::*, px, rems,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use tty7_core::core::group_key::GroupId;
use tty7_core::core::task::{self, Task, TaskId};

use super::{CardRef, Undo};
use crate::core::cli_agent::CLIAgent;
use crate::ui::app::Tty7App;
use crate::ui::dialog::{self, Tone};
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::right_panel::{META, META_MONO, TAB_TEXT};
use crate::ui::tab_sidebar::TaskPlace;

/// The New task card, or the same card open on a task being edited.
pub(crate) struct Composer {
    editing: Option<TaskId>,
    text: Entity<InputState>,
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

/// The card's title and the agent's prompt, out of what was typed.
fn split(text: &str) -> Option<(String, String)> {
    let text = text.trim();
    let first = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let title = match first.chars().count() > TITLE_CHARS {
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
    // A one-line task is all title: the agent is told the title.
    let prompt = match text == first {
        true => String::new(),
        false => text.to_string(),
    };
    Some((title, prompt))
}

/// The places whose name or folder has `query` in it, and — for a query that
/// is a path — that path itself, as a place of its own.
fn matching_places(places: Vec<TaskPlace>, query: &str) -> Vec<TaskPlace> {
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
    if q.starts_with(['/', '~']) || q.contains(":\\") {
        let path = match q.strip_prefix('~') {
            Some(rest) => std::env::var_os("HOME")
                .map(|home| PathBuf::from(format!("{}{rest}", home.to_string_lossy())))
                .unwrap_or_else(|| PathBuf::from(q)),
            None => PathBuf::from(q),
        };
        if !out.iter().any(|p| p.cwd == path) {
            out.push(TaskPlace {
                group: None,
                name: q.to_string(),
                cwd: path,
                grouped: false,
                branch: None,
            });
        }
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
        let seed = existing.as_ref().map_or(String::new(), |t| {
            if t.prompt.is_empty() {
                t.title.clone()
            } else if t.prompt.starts_with(t.title.trim_end_matches('…')) {
                t.prompt.clone()
            } else {
                format!("{}\n\n{}", t.title, t.prompt)
            }
        });
        let text = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder(t(L10nKey::BoardComposerPlaceholder))
                .default_value(seed)
        });
        let branch_seed = existing
            .as_ref()
            .and_then(|t| t.branch.clone())
            .unwrap_or_default();
        let branch = cx.new(|cx| InputState::new(window, cx).default_value(branch_seed));
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
                    InputEvent::Change => this.refresh_branch_placeholder(window, cx),
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
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &search,
                window,
                |this, input, ev: &InputEvent, window, cx| match ev {
                    // Enter takes the first place left after filtering.
                    InputEvent::PressEnter { .. } => {
                        let query = input.read(cx).value().to_string();
                        let (places, _) = this.task_places(cx);
                        if let Some(place) = matching_places(places, &query).into_iter().next() {
                            this.set_composer_place(place, window, cx);
                        }
                    }
                    InputEvent::Change => cx.notify(),
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
            branch,
            search,
            agent,
            cwd,
            group: group.filter(|g| self.sidebar_groups.contains(*g)),
            worktree,
            pop: None,
            focus: cx.focus_handle(),
            _subs: subs,
        });
        self.refresh_branch_placeholder(window, cx);
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
        let Some((title, prompt)) = split(&c.text.read(cx).value()) else {
            window.push_notification(t(L10nKey::BoardNeedsTitle), cx);
            return;
        };
        let branch = c.branch.read(cx).value().trim().to_string();
        let editing = c.editing;
        let mut task = editing
            .and_then(|id| self.task(id))
            .unwrap_or_else(|| Task::new(String::new()));
        let before = editing.and_then(|id| self.task(id));
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
        self.board.composer = None;
        self.board.selected = Some(CardRef::Task(id));
        if start {
            self.start_task(id, agent, window, cx);
        } else {
            let undo = match before {
                Some(before) => Undo::Put(before),
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
    /// 1–9; closing hands them back to the task's words.
    fn toggle_composer_pop(&mut self, pop: Pop, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.board.composer.as_mut() else {
            return;
        };
        c.pop = (c.pop != Some(pop)).then_some(pop);
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
    }

    fn set_composer_agent(&mut self, agent: CLIAgent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(c) = self.board.composer.as_mut() {
            c.agent = Some(agent);
        }
        self.close_composer_pop(window, cx);
    }

    fn set_composer_worktree(&mut self, worktree: bool, cx: &mut Context<Self>) {
        if let Some(c) = self.board.composer.as_mut() {
            c.worktree = worktree;
            cx.notify();
        }
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
        let ready = split(&text).is_some();
        let editing = c.editing.is_some();

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
            let rows = matching_places(places.clone(), &query);
            let empty = rows.is_empty();
            let current = chosen.clone();
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
                        .hover(move |s| s.bg(hover))
                        .cursor_pointer()
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.set_composer_place(place.clone(), window, cx)
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
                .when(empty, |v| {
                    v.child(
                        div()
                            .px(px(8.))
                            .py(px(10.))
                            .text_size(rems(META))
                            .text_color(muted)
                            .child(t(L10nKey::BoardNoMatches)),
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
            v_flex()
                .absolute()
                .bottom(px(34.))
                .left_0()
                .w(px(AGENT_POP_W))
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
                .on_click(
                    cx.listener(move |this, _, _, cx| this.set_composer_worktree(on_worktree, cx)),
                )
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
        let start_label = h_flex()
            .gap(px(7.))
            .child(t(L10nKey::BoardStartNow))
            .child(div().opacity(0.6).text_size(rems(11. / 16.)).child("⌘↵"));
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
            .on_key_down(
                cx.listener(move |this, ev: &gpui::KeyDownEvent, window, cx| {
                    let k = &ev.keystroke;
                    // 1–9 pick from the open agent list.
                    if this.board.composer.as_ref().and_then(|c| c.pop) == Some(Pop::Agent)
                        && !k.modifiers.modified()
                        && let Some(n) = k.key.parse::<usize>().ok().filter(|n| (1..=9).contains(n))
                        && let Some(agent) = agent_keys.get(n - 1).copied()
                    {
                        cx.stop_propagation();
                        this.set_composer_agent(agent, window, cx);
                    }
                }),
            )
            .child(head)
            .child(body)
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
            matching_places(places.clone(), q)
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
    }
}

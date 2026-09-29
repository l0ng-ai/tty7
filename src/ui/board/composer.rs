//! The New task sheet: where it runs, what to do, which agent.
//!
//! One box for the words — the first line is the card's title, all of it is
//! what the agent is told — so writing a task is writing a message, not
//! filling a form.

use std::path::PathBuf;

use gpui::{
    AnyElement, Context, Entity, SharedString, Subscription, Window, div, prelude::*, px, rems,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Icon, WindowExt as _, h_flex, v_flex};
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
    agent: Option<CLIAgent>,
    cwd: Option<PathBuf>,
    /// The pinned group the task is filed under, when its place is one.
    group: Option<GroupId>,
    worktree: bool,
    _subs: Vec<Subscription>,
}

const WIDTH: f32 = 600.;
const TOP: f32 = 120.;
const HEAD_H: f32 = 44.;
const TEXT_H: f32 = 112.;
const FOOT_H: f32 = 48.;
const PILL_H: f32 = 26.;
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
        text.update(cx, |state, cx| state.focus(window, cx));
        let subs =
            vec![cx.subscribe_in(
                &text,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter {
                        secondary: true, ..
                    } => this.submit_composer(true, window, cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            )];
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
            agent,
            cwd,
            group: group.filter(|g| self.sidebar_groups.contains(*g)),
            worktree,
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
        let Some((title, prompt)) = split(&c.text.read(cx).value()) else {
            window.push_notification(t(L10nKey::BoardNeedsTitle), cx);
            return;
        };
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

    fn set_composer_place(&mut self, place: TaskPlace, cx: &mut Context<Self>) {
        if let Some(c) = self.board.composer.as_mut() {
            c.group = place.group;
            c.cwd = Some(place.cwd);
            cx.notify();
        }
    }

    fn set_composer_agent(&mut self, agent: CLIAgent, cx: &mut Context<Self>) {
        if let Some(c) = self.board.composer.as_mut() {
            c.agent = Some(agent);
            cx.notify();
        }
    }

    fn toggle_composer_worktree(&mut self, cx: &mut Context<Self>) {
        if let Some(c) = self.board.composer.as_mut() {
            c.worktree = !c.worktree;
            cx.notify();
        }
    }

    /// The New task sheet, over the whole window like every other sheet.
    pub(crate) fn render_composer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let c = self.board.composer.as_ref()?;
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let rungs = dialog::popover_rungs(cx);
        let (hover, picked) = (gpui::rgb(rungs.hover), gpui::rgb(rungs.selected));
        let text = c.text.read(cx).value().to_string();
        let ready = split(&text).is_some();
        let editing = c.editing.is_some();

        // Where: the sidebar's places as chips, the chosen one lit.
        let (places, _) = self.task_places(cx);
        let chosen = places
            .iter()
            .position(|p| p.group.is_some() && p.group == c.group)
            .or_else(|| {
                places
                    .iter()
                    .position(|p| p.group.is_none() && Some(&p.cwd) == c.cwd.as_ref())
            });
        let chips = places.into_iter().enumerate().map(|(i, place)| {
            let on = chosen == Some(i);
            let tip = place.cwd.display().to_string();
            let name = place.name.clone();
            div()
                .id(("board-place", i))
                .flex_none()
                .h(px(22.))
                .px(px(8.))
                .flex()
                .items_center()
                .rounded(px(5.))
                .text_size(rems(META))
                .text_color(if on { fg } else { muted })
                .when(on, |d| d.bg(picked))
                .when(!on, |d| d.hover(move |s| s.bg(hover)))
                .cursor_pointer()
                .tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                })
                .on_click(
                    cx.listener(move |this, _, _, cx| this.set_composer_place(place.clone(), cx)),
                )
                .child(name)
        });
        let head = h_flex()
            .flex_none()
            .min_h(px(HEAD_H))
            .py(px(8.))
            .px(px(16.))
            .gap(px(6.))
            .flex_wrap()
            .items_center()
            .text_size(rems(META))
            .child(
                div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(fg)
                    .child(t(if editing {
                        L10nKey::BoardEditTask
                    } else {
                        L10nKey::BoardNewTask
                    })),
            )
            .child(div().text_color(muted).child(t(L10nKey::BoardComposerIn)))
            .children(chips);

        let body = div()
            .h(px(TEXT_H))
            .px(px(8.))
            .text_size(rems(15. / 16.))
            .child(Input::new(&c.text).appearance(false).h_full());

        // Which agent: one pill each, the chosen one raised.
        let pills = self.offered_agents(cx).into_iter().map(|agent| {
            let on = c.agent == Some(agent);
            let avatar = crate::ui::tab_strip::avatar(
                SharedString::from(format!("board-pick-{}", agent.slug())),
                crate::ui::search::Avatar {
                    agent: Some(agent),
                    ..Default::default()
                },
                16.,
                cx,
            );
            div()
                .id(SharedString::from(format!("board-agent-{}", agent.slug())))
                .flex_none()
                .h(px(PILL_H))
                .pl(px(5.))
                .pr(px(9.))
                .flex()
                .items_center()
                .gap(px(6.))
                .rounded(px(PILL_H / 2.))
                .text_size(rems(META))
                .text_color(if on { fg } else { muted })
                .when(on, |d| d.bg(picked).border_1().border_color(theme.border))
                .when(!on, |d| d.hover(move |s| s.bg(hover)))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.set_composer_agent(agent, cx)))
                .child(avatar)
                .child(short_name(agent))
        });
        let agents = h_flex()
            .flex_wrap()
            .gap(px(6.))
            .px(px(16.))
            .pt(px(6.))
            .pb(px(14.))
            .children(pills);

        // Where the work lands: a new branch in its own worktree, or the
        // checkout it runs in. Clicking the line switches between the two.
        let first_line = split(&text).map(|(title, _)| title).unwrap_or_default();
        let lands: SharedString = match c.worktree {
            true => task::branch_slug(&first_line)
                .unwrap_or_else(|| t(L10nKey::BoardBranchAuto).to_string())
                .into(),
            false => t_fmt(
                L10nKey::BoardInPlace,
                &[(
                    "cwd",
                    &c.cwd
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                )],
            )
            .into(),
        };
        let lands_line = h_flex()
            .id("board-composer-worktree")
            .flex_1()
            .min_w_0()
            .h(px(28.))
            .px(px(6.))
            .gap(px(8.))
            .items_center()
            .rounded(px(6.))
            .hover(move |s| s.bg(hover))
            .cursor_pointer()
            .tooltip(|window, cx| {
                gpui_component::tooltip::Tooltip::new(t(L10nKey::BoardWorktreeTip))
                    .build(window, cx)
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle_composer_worktree(cx)))
            .child(
                Icon::empty()
                    .path("icons/git-branch.svg")
                    .size(px(12.))
                    .text_color(muted),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.mono_font_family.clone())
                    .text_size(rems(META_MONO))
                    .text_color(muted)
                    .child(lands),
            );
        let foot = h_flex()
            .flex_none()
            .h(px(FOOT_H))
            .pl(px(10.))
            .pr(px(12.))
            .gap(px(8.))
            .items_center()
            .border_t_1()
            .border_color(theme.border)
            .child(lands_line)
            .child(dialog::button(
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
            ))
            .child(dialog::button(
                "board-composer-start",
                t(L10nKey::BoardStartNow),
                Tone::Primary,
                ready && c.agent.is_some(),
                rungs,
                cx,
                cx.listener(|this, _, window, cx| this.submit_composer(true, window, cx)),
            ));

        let card = v_flex()
            .occlude()
            .w(px(WIDTH))
            .map(|panel| crate::ui::theme::floating_surface(panel, cx))
            .rounded(px(dialog::CARD_RADIUS))
            .overflow_hidden()
            .text_size(rems(TAB_TEXT))
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(head)
            .child(body)
            .child(agents)
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
                        this.close_composer(window, cx);
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

/// An agent's name short enough for a pill: `Claude Code` is `Claude`.
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
    fn a_pill_drops_the_product_suffix() {
        assert_eq!(short_name(CLIAgent::Claude), "Claude");
    }
}

//! Drawing the board: the header, five columns of cards, the drop targets a
//! drag lights up, and the note at the bottom saying what just happened.

use gpui::{
    AnyElement, App, Context, Hsla, IntoElement, Render, SharedString, Window, div, prelude::*, px,
    rems,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use tty7_core::core::group_key::{GroupId, PinnedGroup};
use tty7_core::core::machine::TabId;
use tty7_core::core::task::{Column, TaskId};

use super::{Card, CardDrag, CardRef, MainView, ago, move_verb, moves_from};
use crate::core::cli_agent::{AgentStatus, CLIAgent};
use crate::ui::app::Tty7App;
use crate::ui::dialog::{self, Tone};
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::right_panel::{HEADING, META, META_MONO, TAB_TEXT, TEXT};

pub(super) const HEADER_H: f32 = 48.;
const PAD_X: f32 = 20.;
const COLUMN_GAP: f32 = 12.;
const COLUMN_HEAD_H: f32 = 28.;
const CARD_GAP: f32 = 8.;
const CARD_RADIUS: f32 = 9.;
const AVATAR: f32 = 16.;
const DROP_H: f32 = 44.;
const EMPTY_H: f32 = 64.;

impl Tty7App {
    /// The board over the terminal area, or `None` while the terminal shows.
    pub(crate) fn render_board(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.note_runs(cx);
        if self.main_view != MainView::Board {
            return None;
        }
        // A drag that ended anywhere — a drop, a release outside every
        // target, the pointer leaving the window — leaves no drag behind.
        if !cx.has_active_drag() {
            self.board.dragging = None;
        }
        let cards = self.board_cards(Some(window), true, cx);
        // A selection whose card is gone (removed, filtered out) goes too.
        if let Some(sel) = self.board.selected
            && !cards.iter().any(|c| c.key == sel)
        {
            self.board.selected = None;
            self.board.peek = false;
        }
        let header = self.render_board_header(&cards, cx);
        let filtered = !self.board.agent_filter.is_empty() || self.board.group_filter.is_some();
        let body = match cards.is_empty() && !filtered && self.board.dragging.is_none() {
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
        let peek = self
            .board
            .selected
            .filter(|_| self.board.peek)
            .and_then(|sel| cards.iter().find(|c| c.key == sel))
            .map(|card| self.render_peek(card, cx));
        let toast = self.render_toast(cx);
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
                    this.board_key(ev, window, cx)
                }))
                // A click on the board's own ground closes the panel. Cards
                // and the panel keep their clicks to themselves.
                .on_click(cx.listener(|this, _: &gpui::ClickEvent, window, cx| {
                    if this.board.peek {
                        this.close_peek(window, cx);
                    } else {
                        window.focus(&this.board.focus, cx);
                    }
                }))
                .child(header)
                .child(body)
                .children(peek)
                .children(toast)
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
        let app = cx.entity().downgrade();

        // Agents: a set to pick from, each with how many cards it has.
        let everyone = self.board_cards(None, false, cx);
        let mut agents: Vec<(CLIAgent, usize)> = Vec::new();
        for agent in everyone.iter().filter_map(|c| c.agent) {
            match agents.iter_mut().find(|(a, _)| *a == agent) {
                Some((_, n)) => *n += 1,
                None => agents.push((agent, 1)),
            }
        }
        agents.sort_by_key(|(a, _)| a.display_name());
        let picked = self.board.agent_filter.clone();
        let agent_label: SharedString = match picked.as_slice() {
            [] => t(L10nKey::BoardAllAgents).into(),
            [one] => one.display_name().into(),
            many => t_fmt(L10nKey::BoardAgentsCount, &[("n", &many.len().to_string())]).into(),
        };
        let agent_app = app.clone();
        let agent_filter = Button::new("board-agent-filter")
            .label(agent_label)
            .ghost()
            .small()
            .dropdown_caret(true)
            .dropdown_menu(move |menu: PopupMenu, _window, _cx| {
                let mut menu = menu;
                for (agent, n) in agents.clone() {
                    let a = agent_app.clone();
                    let on = picked.is_empty() || picked.contains(&agent);
                    menu = menu.item(
                        PopupMenuItem::new(format!("{}    {n}", agent.display_name()))
                            .checked(on)
                            .on_click(move |_, _, cx| {
                                let _ =
                                    a.update(cx, |this, cx| this.toggle_agent_filter(agent, cx));
                            }),
                    );
                }
                let a = agent_app.clone();
                menu.separator()
                    .item(PopupMenuItem::new(t(L10nKey::BoardShowAllAgents)).on_click(
                        move |_, _, cx| {
                            let _ = a.update(cx, |this, cx| {
                                this.board.agent_filter.clear();
                                cx.notify();
                            });
                        },
                    ))
            });

        // Groups: only once the sidebar has pinned one.
        let groups: Vec<(GroupId, String)> = self
            .sidebar_groups
            .pinned
            .iter()
            .map(|g| (g.id, group_label(g)))
            .collect();
        let group_now: SharedString = self
            .board
            .group_filter
            .and_then(|id| groups.iter().find(|(g, _)| *g == id))
            .map(|(_, name)| name.clone().into())
            .unwrap_or_else(|| t(L10nKey::BoardAllGroups).into());
        let group_app = app.clone();
        let group_filter = (!groups.is_empty()).then(|| {
            Button::new("board-group-filter")
                .label(group_now)
                .ghost()
                .small()
                .dropdown_caret(true)
                .dropdown_menu(move |menu: PopupMenu, _window, _cx| {
                    let pick = |label: SharedString, g: Option<GroupId>| {
                        let a = group_app.clone();
                        PopupMenuItem::new(label).on_click(move |_, _, cx| {
                            let _ = a.update(cx, |this, cx| {
                                this.board.group_filter = g;
                                cx.notify();
                            });
                        })
                    };
                    let mut menu = menu.item(pick(t(L10nKey::BoardAllGroups).into(), None));
                    for (id, name) in groups.clone() {
                        menu = menu.item(pick(name.into(), Some(id)));
                    }
                    menu
                })
        });

        let rungs = dialog::popover_rungs(cx);
        let new_task = h_flex().gap(px(7.)).child(t(L10nKey::BoardNewTask)).child(
            div()
                .px(px(3.))
                .min_w(px(16.))
                .h(px(16.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .border_1()
                .border_color(theme.background.opacity(0.45))
                .text_size(rems(10.5 / 16.))
                .opacity(0.7)
                .child("C"),
        );
        h_flex()
            .flex_none()
            .h(px(HEADER_H))
            .px(px(PAD_X))
            .items_center()
            .gap(px(8.))
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
                    .ml(px(4.))
                    .truncate()
                    .text_size(rems(META))
                    .text_color(muted)
                    .child(summary),
            )
            .children(group_filter)
            .child(agent_filter)
            .child(
                dialog::button(
                    "board-new-task",
                    "",
                    Tone::Primary,
                    true,
                    rungs,
                    cx,
                    cx.listener(|this, _, window, cx| this.open_composer(None, window, cx)),
                )
                .h(px(26.))
                .px(px(10.))
                .child(new_task),
            )
            .into_any_element()
    }

    fn toggle_agent_filter(&mut self, agent: CLIAgent, cx: &mut Context<Self>) {
        let everyone: Vec<CLIAgent> = {
            let mut all: Vec<CLIAgent> = self
                .board_cards(None, false, cx)
                .iter()
                .filter_map(|c| c.agent)
                .collect();
            all.sort_by_key(|a| a.display_name());
            all.dedup();
            all
        };
        let filter = &mut self.board.agent_filter;
        if filter.is_empty() {
            *filter = everyone.clone();
        }
        match filter.iter().position(|a| *a == agent) {
            Some(i) => {
                filter.remove(i);
            }
            None => filter.push(agent),
        }
        // Everyone, or no one, is the same as no filter at all.
        if filter.is_empty() || everyone.iter().all(|a| filter.contains(a)) {
            filter.clear();
        }
        cx.notify();
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
        let (fg, muted, border) = (theme.foreground, theme.muted_foreground, theme.border);
        let (dot, hollow) = column_dot(col, cx);
        let label = t(match col {
            Column::Queued => L10nKey::BoardColQueued,
            Column::Running => L10nKey::BoardColRunning,
            Column::NeedsInput => L10nKey::BoardColNeedsInput,
            Column::Review => L10nKey::BoardColReview,
            Column::Done => L10nKey::BoardColDone,
        });
        // While a card is dragged: where it may go says what the drop would
        // do; where it may not fades back.
        let drag = self.board.dragging.as_ref();
        let valid = drag.is_some_and(|d| moves_from(d.from).contains(&col));
        let home = drag.is_some_and(|d| d.from == col);
        let zone = drag.filter(|_| valid).map(|d| {
            div()
                .flex_none()
                .h(px(DROP_H))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .border_1()
                .border_color(border)
                .text_size(rems(META))
                .text_color(muted)
                .child(t(move_verb(d.from, col, d.question)))
        });
        let empty = (cards.is_empty() && !valid).then(|| {
            div()
                .flex_none()
                .h(px(EMPTY_H))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(CARD_RADIUS))
                .border_1()
                .border_color(border.opacity(0.6))
                .text_size(rems(META))
                .text_color(muted.opacity(0.8))
                .child(t(match col {
                    Column::Queued => L10nKey::BoardEmptyQueued,
                    Column::Running => L10nKey::BoardEmptyRunning,
                    Column::NeedsInput => L10nKey::BoardEmptyNeedsInput,
                    Column::Review => L10nKey::BoardEmptyReview,
                    Column::Done => L10nKey::BoardEmptyDone,
                }))
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
            )
            .when(col == Column::Done && !cards.is_empty(), |head| {
                let ids: Vec<TaskId> = cards
                    .iter()
                    .filter_map(|c| match c.key {
                        CardRef::Task(id) => Some(id),
                        CardRef::Loose(_) => None,
                    })
                    .collect();
                head.child(div().flex_1()).child(
                    div()
                        .id("board-clean-all")
                        .px(px(6.))
                        .h(px(20.))
                        .flex()
                        .items_center()
                        .rounded(px(5.))
                        .text_color(muted)
                        .cursor_pointer()
                        .hover(move |s| s.text_color(fg))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.clean_up(ids.clone(), window, cx)
                        }))
                        .child(t(L10nKey::BoardCleanUpAll)),
                )
            });
        // Done keeps the last week on show; older cards fold into one row
        // until asked for.
        let (shown, folded): (Vec<&Card>, usize) = match col {
            Column::Done if !self.board.show_old_done => {
                let cutoff =
                    crate::core::config::unix_now().saturating_sub(super::DONE_RECENT_SECS);
                let recent: Vec<&Card> = cards
                    .iter()
                    .copied()
                    .filter(|c| c.since.is_none_or(|at| at >= cutoff))
                    .collect();
                let folded = cards.len() - recent.len();
                (recent, folded)
            }
            _ => (cards.to_vec(), 0),
        };
        let fold = (folded > 0).then(|| {
            div()
                .id("board-done-older")
                .flex_none()
                .h(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(7.))
                .text_size(rems(META))
                .text_color(muted)
                .cursor_pointer()
                .hover(move |s| s.text_color(fg))
                .on_click(cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.board.show_old_done = true;
                    cx.notify();
                }))
                .child(t_fmt(
                    L10nKey::BoardOlderDone,
                    &[("n", &folded.to_string())],
                ))
        });
        let list = v_flex()
            .id(("board-column-list", col as usize))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap(px(CARD_GAP))
            .pb(px(4.))
            .children(zone)
            .children(shown.iter().map(|c| self.render_card(c, cx)))
            .children(fold)
            .children(empty);
        let over: Hsla = gpui::rgb(dialog::popover_rungs(cx).hover).into();
        v_flex()
            .id(("board-column", col as usize))
            .flex_1()
            .min_w_0()
            .h_full()
            .gap(px(CARD_GAP))
            .px(px(4.))
            .mx(px(-4.))
            .pb(px(4.))
            .rounded(px(10.))
            .when(drag.is_some() && !valid && !home, |c| c.opacity(0.4))
            .can_drop(move |value, _, _| {
                value
                    .downcast_ref::<CardDrag>()
                    .is_some_and(|d| moves_from(d.from).contains(&col))
            })
            .drag_over::<CardDrag>(move |style, _, _, _| style.bg(over))
            .on_drop(cx.listener(move |this, drag: &CardDrag, window, cx| {
                this.board.dragging = None;
                this.board_move(drag.key, col, window, cx);
            }))
            .child(head)
            .child(list)
            .into_any_element()
    }

    fn render_card(&self, card: &Card, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, border) = (theme.foreground, theme.muted_foreground, theme.border);
        let (surface, success, danger) = (theme.popover, theme.success, theme.danger);
        let mono = theme.mono_font_family.clone();
        let well: Hsla = gpui::rgb(dialog::popover_rungs(cx).hover).into();
        let rungs = dialog::popover_rungs(cx);
        let key = card.key;
        let id = card_element_id(key);
        let selected = self.board.selected == Some(key);
        let dragged = self.board.dragging.as_ref().is_some_and(|d| d.key == key);

        let title_row = h_flex()
            .items_start()
            .gap(px(9.))
            .child(div().mt(px(1.)).child(crate::ui::tab_strip::avatar(
                SharedString::from(format!("{id}-avatar")),
                crate::ui::search::Avatar {
                    agent: card.agent,
                    status: card.status,
                    unread: 0,
                    ssh: None,
                },
                AVATAR,
                cx,
            )))
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
        let group = card
            .group
            .and_then(|g| self.sidebar_groups.get(g))
            .map(group_label);
        let place: Vec<String> = [group, card.repo.clone(), card.branch.clone()]
            .into_iter()
            .flatten()
            .collect();
        let place = (!place.is_empty()).then(|| {
            div()
                .truncate()
                .text_size(rems(HEADING))
                .text_color(muted)
                .child(place.join("  ·  "))
        });
        // A running agent's newest line of output.
        let output = (card.column == Column::Running)
            .then(|| self.card_view(card.tab, cx))
            .flatten()
            .and_then(|v| v.read(cx).screen_tail(1).pop())
            .map(|line| {
                div()
                    .truncate()
                    .px(px(7.))
                    .py(px(5.))
                    .rounded(px(5.))
                    .bg(well)
                    .font_family(mono.clone())
                    .text_size(rems(META_MONO))
                    .text_color(muted)
                    .child(line)
            });
        // A waiting agent's question, and the two ways to answer it.
        let ask = (card.column == Column::NeedsInput).then(|| {
            let (first, second) = match card.question {
                // A question takes words; a permission prompt takes a yes.
                true => ((L10nKey::BoardReply, true), (L10nKey::BoardOpen, false)),
                false => ((L10nKey::BoardAllow, false), (L10nKey::BoardReply, true)),
            };
            let tab = card.tab;
            let action = |which: usize,
                          (label, reply): (L10nKey, bool),
                          tone: Tone,
                          cx: &mut Context<Self>| {
                dialog::button(
                    SharedString::from(format!("{id}-act-{which}")),
                    t(label),
                    tone,
                    true,
                    rungs,
                    cx,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        match (label, reply, tab) {
                            (_, true, _) => this.focus_reply(key, window, cx),
                            (L10nKey::BoardAllow, _, _) => {
                                this.board_move(key, Column::Running, window, cx)
                            }
                            (_, _, Some(tab)) => this.open_card_tab(tab, window, cx),
                            _ => {}
                        }
                    }),
                )
                .flex_1()
                .h(px(24.))
            };
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
                .child(
                    h_flex()
                        .gap(px(6.))
                        .child(action(0, first, Tone::Primary, cx))
                        .child(action(1, second, Tone::Secondary, cx)),
                )
        });
        let footer = {
            let mut row = h_flex()
                .items_center()
                .gap(px(8.))
                .text_size(rems(META_MONO))
                .text_color(muted)
                .font_features(crate::ui::tab_sidebar::tabular());
            if let Some((added, removed)) = card.diff.filter(|_| card.column != Column::Running) {
                row = row.child(
                    h_flex()
                        .gap(px(4.))
                        .child(div().text_color(success).child(format!("+{added}")))
                        .child(div().text_color(danger).child(format!("−{removed}"))),
                );
            }
            if card.worktree_gone {
                row = row.child(t(L10nKey::BoardWorktreeGone));
            } else if card.paused {
                row = row.child(t(L10nKey::BoardStatusPaused));
            } else if card.resumable {
                row = row.child(t(L10nKey::BoardResumable));
            }
            row = row.child(div().flex_1());
            if let Some(since) = card.since {
                row = row.child(ago(since));
            }
            row
        };
        let drag = CardDrag {
            key,
            from: card.column,
            question: card.question,
            title: card.title.clone(),
            agent: card.agent,
        };
        let app = cx.entity().downgrade();
        v_flex()
            .id(SharedString::from(id.clone()))
            .flex_none()
            .gap(px(8.))
            .pt(px(11.))
            .px(px(12.))
            .pb(px(10.))
            .rounded(px(CARD_RADIUS))
            .bg(surface)
            .border_1()
            .when(selected, |c| c.border_color(fg))
            .when(!selected, |c| {
                c.border_color(border.opacity(0.6))
                    .hover(move |s| s.border_color(border))
            })
            .when(card.column == Column::Done, |c| c.opacity(0.6))
            .when(dragged, |c| c.opacity(0.35))
            .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                cx.stop_propagation();
                if ev.click_count() >= 2 {
                    this.open_card(key, window, cx);
                } else {
                    this.select_card(key, true, window, cx);
                    window.focus(&this.board.focus, cx);
                }
            }))
            .when(card.column != Column::Done, |c| {
                c.on_drag(drag, move |drag, _, _, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.board.dragging = Some(drag.clone());
                        this.board.selected = Some(drag.key);
                        cx.notify();
                    });
                    let ghost = CardGhost {
                        title: drag.title.clone(),
                        agent: drag.agent,
                    };
                    cx.new(|_| ghost)
                })
            })
            .context_menu({
                let menu_card = CardMenu {
                    key,
                    column: card.column,
                    tab: card.tab,
                    question: card.question,
                    resumable: card.resumable,
                    gone: card.worktree_gone,
                };
                let app = cx.entity().downgrade();
                move |menu, _window, _cx| card_menu(menu, &menu_card, app.clone())
            })
            .child(title_row)
            .children(place)
            .children(output)
            .children(ask)
            .child(footer)
            .into_any_element()
    }

    fn render_toast(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let toast = self.board.toast.as_ref()?;
        let rungs = dialog::popover_rungs(cx);
        let undo = toast.undo.is_some().then(|| {
            dialog::button(
                "board-toast-undo",
                t(L10nKey::BoardUndo),
                Tone::Secondary,
                true,
                rungs,
                cx,
                cx.listener(|this, _, window, cx| this.undo(window, cx)),
            )
            .h(px(24.))
            .px(px(9.))
        });
        Some(
            h_flex()
                .absolute()
                .bottom(px(24.))
                .left_0()
                .right_0()
                .justify_center()
                .child(
                    h_flex()
                        .occlude()
                        .h(px(36.))
                        .pl(px(14.))
                        .pr(px(8.))
                        .gap(px(14.))
                        .items_center()
                        .map(|panel| crate::ui::theme::floating_surface(panel, cx))
                        .rounded(px(9.))
                        .text_size(rems(TAB_TEXT))
                        .child(toast.text.clone())
                        .children(undo),
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

/// What a card's right-click menu needs to know about it.
struct CardMenu {
    key: CardRef,
    column: Column,
    tab: Option<TabId>,
    question: bool,
    resumable: bool,
    gone: bool,
}

type Act = Box<dyn Fn(&mut Tty7App, &mut Window, &mut Context<Tty7App>)>;

/// The card's everyday moves, the same ones its panel and a drag offer, by
/// the column it is in — then editing and removing, for a task.
fn card_menu(menu: PopupMenu, c: &CardMenu, app: gpui::WeakEntity<Tty7App>) -> PopupMenu {
    let item = |label: L10nKey, act: Act| {
        let app = app.clone();
        PopupMenuItem::new(t(label)).on_click(move |_, window, cx| {
            let _ = app.update(cx, |this, cx| act(this, window, cx));
        })
    };
    let key = c.key;
    let to = |col: Column| -> Act {
        Box::new(move |this, window, cx| this.board_move(key, col, window, cx))
    };
    let reply: Act = Box::new(move |this, window, cx| this.focus_reply(key, window, cx));
    let mut menu = menu;
    if let Some(tab) = c.tab {
        menu = menu.item(item(
            L10nKey::BoardOpenTerminal,
            Box::new(move |this, window, cx| this.open_card_tab(tab, window, cx)),
        ));
    }
    let task = match key {
        CardRef::Task(id) => Some(id),
        CardRef::Loose(_) => None,
    };
    menu = match c.column {
        Column::Queued => menu.item(item(L10nKey::BoardVerbStart, to(Column::Running))),
        Column::Running => menu
            .item(item(L10nKey::BoardReply, reply))
            .item(item(L10nKey::BoardVerbPause, to(Column::Queued))),
        Column::NeedsInput => {
            let menu = match c.question {
                true => menu,
                false => menu.item(item(L10nKey::BoardAllow, to(Column::Running))),
            };
            menu.item(item(L10nKey::BoardReply, reply))
                .item(item(L10nKey::BoardVerbPause, to(Column::Queued)))
        }
        Column::Review => menu
            .item(item(L10nKey::BoardVerbChanges, reply))
            .item(item(L10nKey::BoardVerbDone, to(Column::Done))),
        Column::Done => match task {
            Some(id) => {
                let menu = match c.gone {
                    true => menu.item(item(
                        L10nKey::BoardStartAgain,
                        Box::new(move |this, window, cx| this.start_again(id, window, cx)),
                    )),
                    false => menu.item(item(
                        L10nKey::BoardReopen,
                        Box::new(move |this, window, cx| this.reopen(id, window, cx)),
                    )),
                };
                menu.item(item(
                    L10nKey::BoardCleanUp,
                    Box::new(move |this, window, cx| this.clean_up(vec![id], window, cx)),
                ))
            }
            None => menu,
        },
    };
    if c.resumable
        && !c.gone
        && c.column != Column::Done
        && let Some(id) = task
    {
        menu = menu.item(item(
            L10nKey::BoardResume,
            Box::new(move |this, window, cx| this.resume_task(id, window, cx)),
        ));
    }
    match (key, task) {
        (CardRef::Loose(tab), _) => menu.separator().item(item(
            L10nKey::BoardKeep,
            Box::new(move |this, window, cx| this.keep_loose(tab, window, cx)),
        )),
        (_, Some(id)) if c.column != Column::Done => menu
            .separator()
            .item(item(
                L10nKey::BoardEdit,
                Box::new(move |this, window, cx| this.open_composer(Some(id), window, cx)),
            ))
            .item(item(
                L10nKey::BoardRemove,
                Box::new(move |this, window, cx| this.remove_task(id, window, cx)),
            )),
        _ => menu,
    }
}

/// What follows the pointer while a card is dragged: its title on a card.
struct CardGhost {
    title: SharedString,
    agent: Option<CLIAgent>,
}

impl Render for CardGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w(px(240.))
            .gap(px(9.))
            .px(px(12.))
            .py(px(10.))
            .rounded(px(CARD_RADIUS))
            .map(|panel| crate::ui::theme::floating_surface(panel, cx))
            .text_size(rems(TAB_TEXT))
            .font_weight(gpui::FontWeight::MEDIUM)
            .child(crate::ui::tab_strip::avatar(
                "board-ghost-avatar",
                crate::ui::search::Avatar {
                    agent: self.agent,
                    ..Default::default()
                },
                AVATAR,
                cx,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(self.title.clone()),
            )
    }
}

fn card_element_id(key: CardRef) -> String {
    match key {
        CardRef::Task(id) => format!("board-task-{id}"),
        CardRef::Loose(id) => format!("board-tab-{id}"),
    }
}

/// What a pinned group is called: its given name, else its folder's last
/// component — the name its sidebar header shows.
pub(super) fn group_label(group: &PinnedGroup) -> String {
    group
        .given_name()
        .map(str::to_string)
        .or_else(|| {
            group
                .folder_path()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| t(L10nKey::BoardFieldGroup).to_string())
}

/// The dot a column heads itself with, and whether it is drawn as a ring.
/// The colours are the agent status dots the sidebar already uses, so a card
/// and its tab's row say a state the same way.
pub(super) fn column_dot(col: Column, cx: &App) -> (Hsla, bool) {
    let muted = cx.theme().muted_foreground;
    let status = |s: AgentStatus| -> Hsla { gpui::rgb(s.dot_rgb().unwrap_or(0x888888)).into() };
    match col {
        Column::Queued => (muted, true),
        Column::Running => (status(AgentStatus::Working), false),
        Column::NeedsInput => (status(AgentStatus::Waiting), false),
        Column::Review => (cx.theme().foreground, true),
        Column::Done => (muted.opacity(0.6), false),
    }
}

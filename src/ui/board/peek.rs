//! The panel beside the columns: one card up close, without leaving the
//! board — what it was asked, what its agent is printing, what it is waiting
//! on, which files it touched — and a line to answer it.

use gpui::{AnyElement, Context, Hsla, SharedString, Window, div, prelude::*, px, rems};
use gpui_component::button::ButtonVariants as _;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use tty7_core::core::task::Column;

use super::{Card, CardRef, ago};
use crate::ui::app::Tty7App;
use crate::ui::dialog::{self, Tone};
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::right_panel::{HEADING, META, META_MONO, TAB_TEXT, TEXT};

const WIDTH: f32 = 360.;
const INSET: f32 = 12.;
const RADIUS: f32 = 11.;
/// Lines of the agent's screen the Output block shows.
const OUTPUT_LINES: usize = 12;

impl Tty7App {
    /// The reply box, made once per window the first time the panel opens.
    pub(super) fn ensure_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.board.reply.is_some() {
            return;
        }
        let reply = cx.new(|cx| InputState::new(window, cx));
        let sub = cx.subscribe_in(
            &reply,
            window,
            |this, input, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev
                    && let Some(key) = this.board.selected
                {
                    let text = input.read(cx).value().to_string();
                    if text.trim().is_empty() {
                        return;
                    }
                    input.update(cx, |s, cx| s.set_value("", window, cx));
                    this.send_reply(key, &text, window, cx);
                }
            },
        );
        self.board.reply = Some(reply);
        self._set_reply_sub(sub);
    }

    fn _set_reply_sub(&mut self, sub: gpui::Subscription) {
        self.board._reply_sub = Some(sub);
    }

    /// Words the reply box greets `card` with: an answer, a change request,
    /// a follow-up, or — for an agent sitting idle — its first instruction.
    pub(super) fn set_reply_placeholder(
        &mut self,
        card: &Card,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(reply) = self.board.reply.clone() else {
            return;
        };
        let agent = card.agent.map_or("", |a| a.display_name());
        let placeholder: SharedString = match card.column {
            Column::NeedsInput => t_fmt(L10nKey::BoardReplyTo, &[("agent", agent)]).into(),
            Column::Review => t(L10nKey::BoardReplyChanges).into(),
            Column::Queued => t_fmt(L10nKey::BoardReplyStart, &[("agent", agent)]).into(),
            _ => t(L10nKey::BoardReplyFollowUp).into(),
        };
        reply.update(cx, |s, cx| s.set_placeholder(placeholder, window, cx));
    }

    /// Opens the panel on `key` with the reply box focused: Reply on a card,
    /// or a drag that needs words (sending a finished run back with changes).
    pub(super) fn focus_reply(
        &mut self,
        key: CardRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_card(key, true, window, cx);
        if let Some(reply) = self.board.reply.clone() {
            reply.update(cx, |s, cx| s.focus(window, cx));
        }
    }

    pub(super) fn render_peek(&self, card: &Card, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted, border) = (theme.foreground, theme.muted_foreground, theme.border);
        let well: Hsla = gpui::rgb(dialog::popover_rungs(cx).hover).into();
        let mono = theme.mono_font_family.clone();
        let rungs = dialog::popover_rungs(cx);
        let key = card.key;
        let (dot, hollow) = super::view::column_dot(card.column, cx);
        let status = t(match (card.column, card.paused) {
            (Column::Queued, true) => L10nKey::BoardStatusPaused,
            (Column::Queued, false) => L10nKey::BoardColQueued,
            (Column::Running, _) => L10nKey::BoardColRunning,
            (Column::NeedsInput, _) => L10nKey::BoardColNeedsInput,
            (Column::Review, _) => L10nKey::BoardStatusReview,
            (Column::Done, _) => L10nKey::BoardColDone,
        });
        let head = h_flex()
            .flex_none()
            .items_start()
            .gap(px(10.))
            .pt(px(14.))
            .pr(px(12.))
            .pb(px(12.))
            .pl(px(16.))
            .border_b_1()
            .border_color(border)
            .child(crate::ui::tab_strip::avatar(
                "board-peek-avatar",
                crate::ui::search::Avatar {
                    agent: card.agent,
                    ..Default::default()
                },
                20.,
                cx,
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(5.))
                    .child(
                        div()
                            .text_size(rems(TEXT))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .line_height(rems(TEXT * 1.35))
                            .child(card.title.clone()),
                    )
                    .child(
                        h_flex()
                            .gap(px(6.))
                            .items_center()
                            .text_size(rems(HEADING))
                            .text_color(muted)
                            .child(
                                div()
                                    .size(px(7.))
                                    .rounded_full()
                                    .when(hollow, |d| d.border_1().border_color(dot))
                                    .when(!hollow, |d| d.bg(dot)),
                            )
                            .child(status)
                            .when_some(card.since, |row, since| row.child("·").child(ago(since))),
                    ),
            )
            .child(
                gpui_component::button::Button::new("board-peek-close")
                    .icon(IconName::Close)
                    .ghost()
                    .xsmall()
                    .tooltip(t(L10nKey::BoardPeekClose))
                    .on_click(cx.listener(|this, _, window, cx| this.close_peek(window, cx))),
            );

        let field = |name: L10nKey, value: String| {
            h_flex()
                .h(px(26.))
                .items_center()
                .child(div().w(px(72.)).text_color(muted).child(t(name)))
                .child(div().flex_1().min_w_0().truncate().child(value))
        };
        let changes = card
            .diff
            .map_or("—".to_string(), |(a, r)| format!("+{a}  −{r}"));
        let facts = v_flex()
            .px(px(4.))
            .child(field(
                L10nKey::BoardPeekAgent,
                card.agent
                    .map_or("—".into(), |a| a.display_name().to_string()),
            ))
            .child(field(
                L10nKey::BoardPeekRepo,
                card.repo.clone().unwrap_or_else(|| "—".into()),
            ))
            .child(field(
                L10nKey::BoardPeekBranch,
                card.branch.clone().unwrap_or_else(|| "—".into()),
            ))
            .child(field(L10nKey::BoardPeekChanges, changes));

        let section = |name: SharedString, name_color: Hsla, body: AnyElement| {
            v_flex()
                .gap(px(6.))
                .child(
                    div()
                        .px(px(4.))
                        .text_size(rems(HEADING))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(name_color)
                        .child(name),
                )
                .child(body)
        };
        let block = || div().px(px(10.)).py(px(8.)).rounded(px(7.)).bg(well);

        let prompt = card.prompt.clone().map(|p| {
            section(
                t(L10nKey::BoardPeekPrompt).into(),
                muted,
                block()
                    .line_height(rems(TAB_TEXT * 1.5))
                    .child(p)
                    .into_any_element(),
            )
        });
        let output = self
            .card_view(card.tab, cx)
            .map(|v| v.read(cx).screen_tail(OUTPUT_LINES))
            .filter(|lines| !lines.is_empty())
            .map(|lines| {
                section(
                    t(L10nKey::BoardPeekOutput).into(),
                    muted,
                    block()
                        .font_family(mono.clone())
                        .text_size(rems(META_MONO))
                        .line_height(rems(META_MONO * 1.65))
                        .text_color(muted)
                        .children(lines.into_iter().map(|l| div().truncate().child(l)))
                        .into_any_element(),
                )
            });
        let waiting = (card.column == Column::NeedsInput).then(|| {
            section(
                t(L10nKey::BoardPeekWaiting).into(),
                theme.warning,
                block()
                    .font_family(mono.clone())
                    .text_size(rems(META_MONO))
                    .line_height(rems(META_MONO * 1.5))
                    .text_color(fg)
                    .child(
                        card.ask
                            .clone()
                            .unwrap_or_else(|| t(L10nKey::BoardColNeedsInput).into()),
                    )
                    .into_any_element(),
            )
        });
        let files = match &self.board.files {
            Some((k, Ok(files))) if *k == key && !files.is_empty() => Some(section(
                t_fmt(L10nKey::BoardPeekFiles, &[("n", &files.len().to_string())]).into(),
                muted,
                v_flex()
                    .children(files.iter().map(|f| {
                        h_flex()
                            .h(px(26.))
                            .px(px(4.))
                            .gap(px(8.))
                            .items_center()
                            .rounded(px(6.))
                            .child(div().flex_1().min_w_0().truncate().child(f.path.clone()))
                            .child(
                                h_flex()
                                    .gap(px(4.))
                                    .text_size(rems(META_MONO))
                                    .font_features(crate::ui::tab_sidebar::tabular())
                                    .child(
                                        div()
                                            .text_color(theme.success)
                                            .child(format!("+{}", f.added)),
                                    )
                                    .child(
                                        div()
                                            .text_color(theme.danger)
                                            .child(format!("−{}", f.removed)),
                                    ),
                            )
                    }))
                    .into_any_element(),
            )),
            _ => None,
        };
        let body = v_flex()
            .id("board-peek-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .pt(px(12.))
            .px(px(12.))
            .pb(px(16.))
            .gap(px(16.))
            .text_size(rems(TAB_TEXT))
            .child(facts)
            .children(prompt)
            .children(output)
            .children(waiting)
            .children(files);

        // A line to the agent, while there is one open to hear it.
        let can_reply = card.tab.is_some()
            && matches!(
                card.column,
                Column::Running | Column::NeedsInput | Column::Review | Column::Queued
            );
        let reply = self
            .board
            .reply
            .as_ref()
            .filter(|_| can_reply)
            .map(|reply| {
                div().px(px(12.)).pb(px(10.)).child(
                    h_flex()
                        .items_center()
                        .gap(px(6.))
                        .pl(px(4.))
                        .pr(px(6.))
                        .py(px(4.))
                        .rounded(px(8.))
                        .bg(well)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(reply).appearance(false).small()),
                        )
                        .child(
                            div()
                                .id("board-peek-send")
                                .size(px(26.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(6.))
                                .bg(fg)
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let Some(reply) = this.board.reply.clone() else {
                                        return;
                                    };
                                    let text = reply.read(cx).value().to_string();
                                    reply.update(cx, |s, cx| s.set_value("", window, cx));
                                    this.send_reply(key, &text, window, cx);
                                }))
                                .child(
                                    Icon::new(IconName::ArrowUp)
                                        .size(px(12.))
                                        .text_color(theme.background),
                                ),
                        ),
                )
            });

        // What can be done from here, the move it would be first.
        let (tab, question, resumable, column) =
            (card.tab, card.question, card.resumable, card.column);
        let button =
            |id: &'static str,
             label: L10nKey,
             tone: Tone,
             cx: &mut Context<Self>,
             run: Box<dyn Fn(&mut Tty7App, &mut Window, &mut Context<Tty7App>)>| {
                dialog::button(
                    id,
                    t(label),
                    tone,
                    true,
                    rungs,
                    cx,
                    cx.listener(move |this, _, window, cx| run(this, window, cx)),
                )
                .flex_1()
            };
        let open_terminal = |cx: &mut Context<Self>| {
            tab.map(|tab| {
                button(
                    "board-peek-open",
                    L10nKey::BoardOpenTerminal,
                    Tone::Secondary,
                    cx,
                    Box::new(move |this, window, cx| this.open_card_tab(tab, window, cx)),
                )
            })
        };
        let move_to =
            |id: &'static str, label: L10nKey, tone: Tone, to: Column, cx: &mut Context<Self>| {
                button(
                    id,
                    label,
                    tone,
                    cx,
                    Box::new(move |this, window, cx| this.board_move(key, to, window, cx)),
                )
            };
        let mut actions: Vec<gpui::Stateful<gpui::Div>> = Vec::new();
        match column {
            Column::Queued => {
                if let CardRef::Task(id) = key {
                    actions.push(button(
                        "board-peek-remove",
                        L10nKey::BoardRemove,
                        Tone::Secondary,
                        cx,
                        Box::new(move |this, window, cx| this.remove_task(id, window, cx)),
                    ));
                }
                actions.push(move_to(
                    "board-peek-start",
                    L10nKey::BoardPeekStart,
                    Tone::Primary,
                    Column::Running,
                    cx,
                ));
            }
            Column::Running => {
                actions.push(move_to(
                    "board-peek-pause",
                    L10nKey::BoardVerbPause,
                    Tone::Secondary,
                    Column::Queued,
                    cx,
                ));
                actions.extend(open_terminal(cx));
            }
            Column::NeedsInput => {
                actions.extend(open_terminal(cx));
                if !question {
                    actions.push(move_to(
                        "board-peek-allow",
                        L10nKey::BoardAllow,
                        Tone::Primary,
                        Column::Running,
                        cx,
                    ));
                }
            }
            Column::Review => {
                actions.extend(open_terminal(cx));
                actions.push(move_to(
                    "board-peek-done",
                    L10nKey::BoardVerbDone,
                    Tone::Primary,
                    Column::Done,
                    cx,
                ));
            }
            Column::Done => {
                if let CardRef::Task(id) = key {
                    actions.push(button(
                        "board-peek-reopen",
                        L10nKey::BoardReopen,
                        Tone::Secondary,
                        cx,
                        Box::new(move |this, window, cx| this.reopen(id, window, cx)),
                    ));
                    if resumable {
                        actions.push(button(
                            "board-peek-resume",
                            L10nKey::BoardResume,
                            Tone::Primary,
                            cx,
                            Box::new(move |this, window, cx| this.resume_task(id, window, cx)),
                        ));
                    }
                }
                actions.extend(open_terminal(cx));
            }
        }
        if resumable
            && column != Column::Done
            && let CardRef::Task(id) = key
        {
            actions.insert(
                0,
                button(
                    "board-peek-resume",
                    L10nKey::BoardResume,
                    Tone::Secondary,
                    cx,
                    Box::new(move |this, window, cx| this.resume_task(id, window, cx)),
                ),
            );
        }
        let foot = h_flex()
            .flex_none()
            .gap(px(6.))
            .pt(px(10.))
            .px(px(12.))
            .pb(px(12.))
            .border_t_1()
            .border_color(border)
            .children(actions);

        v_flex()
            .id("board-peek")
            .occlude()
            .absolute()
            .top(px(super::view::HEADER_H))
            .right(px(INSET))
            .bottom(px(INSET))
            .w(px(WIDTH))
            .map(|panel| crate::ui::theme::floating_surface(panel, cx))
            .rounded(px(RADIUS))
            .overflow_hidden()
            .text_size(rems(META))
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(head)
            .child(body)
            .children(reply)
            .child(foot)
            .into_any_element()
    }
}

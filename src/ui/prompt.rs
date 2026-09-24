//! The confirmation dialog on platforms with no native one.
//!
//! macOS and Windows answer `window.prompt` with NSAlert and TaskDialog. Linux
//! has neither, so gpui falls back to a renderer of its own — and that one sets
//! the message and the detail as single unbreakable lines inside a fixed-width
//! box that clips its overflow. Any sentence longer than the box is cut off
//! mid-word, which for the quit-and-stop warning is exactly the part that says
//! what will be lost (#920). This is the same dialog laid out as a card that
//! wraps its text, in the app's own surfaces, with Return and Escape answering
//! the way they do on the native dialogs.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, PromptButton, PromptHandle, PromptLevel,
    PromptResponse, RenderablePromptHandle, Window, div, prelude::*, px, relative,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};

/// Route every `window.prompt` through [`TextPrompt`] where the platform has
/// no dialog of its own to show.
pub(crate) fn install(cx: &mut App) {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        return;
    }
    cx.set_prompt_builder(build);
}

fn build(
    _level: PromptLevel,
    message: &str,
    detail: Option<&str>,
    answers: &[PromptButton],
    handle: PromptHandle,
    window: &mut Window,
    cx: &mut App,
) -> RenderablePromptHandle {
    let prompt = cx.new(|cx| TextPrompt::new(message, detail, answers, cx));
    handle.with_view(prompt, window, cx)
}

pub(crate) struct TextPrompt {
    message: String,
    detail: Option<String>,
    answers: Vec<PromptButton>,
    focus: FocusHandle,
}

impl TextPrompt {
    fn new(
        message: &str,
        detail: Option<&str>,
        answers: &[PromptButton],
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            message: message.to_string(),
            detail: detail.map(str::to_string),
            answers: answers.to_vec(),
            focus: cx.focus_handle(),
        }
    }

    /// The answer a key picks. Return is answer 0 — `confirm_answers` puts the
    /// action there for exactly this — and Escape is the one marked cancel;
    /// a prompt with no cancel answer has nothing Escape can safely mean.
    fn answer_for_key(&self, key: &str) -> Option<usize> {
        match key {
            "enter" if !self.answers.is_empty() => Some(0),
            "escape" => self.answers.iter().position(PromptButton::is_cancel),
            _ => None,
        }
    }
}

impl EventEmitter<PromptResponse> for TextPrompt {}

impl Focusable for TextPrompt {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TextPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        // Answer 0 goes rightmost, where the native dialogs put it and where
        // `confirm_answers` expects it to land.
        let buttons = self.answers.iter().enumerate().rev().map(|(ix, answer)| {
            Button::new(("prompt-answer", ix))
                .label(answer.label().clone())
                .small()
                .when(ix == 0 && !answer.is_cancel(), |b| b.primary())
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.emit(PromptResponse(ix));
                    cx.stop_propagation();
                }))
        });

        let card = v_flex()
            .occlude()
            .w(px(420.))
            .max_w(relative(0.9))
            .gap_3()
            .p_5()
            .map(|card| crate::ui::theme::floating_surface(card, cx))
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(self.message.clone()),
            )
            .children(
                self.detail
                    .clone()
                    .map(|detail| div().text_sm().text_color(muted).child(detail)),
            )
            .child(
                h_flex()
                    .pt_2()
                    .justify_end()
                    .flex_wrap()
                    .gap_2()
                    .children(buttons),
            );

        div()
            .id("text-prompt")
            .track_focus(&self.focus)
            .size_full()
            .cursor_default()
            .bg(crate::ui::presets::scrim_fill(cx))
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                if let Some(ix) = this.answer_for_key(&ev.keystroke.key) {
                    cx.emit(PromptResponse(ix));
                    cx.stop_propagation();
                }
            }))
            .flex()
            .items_center()
            .justify_center()
            .child(card)
    }
}

#[cfg(test)]
mod tests {
    use super::TextPrompt;
    use gpui::{AppContext as _, PromptButton, TestAppContext};

    fn prompt(cx: &mut TestAppContext, answers: &[PromptButton]) -> gpui::Entity<TextPrompt> {
        cx.update(|cx| {
            cx.new(|cx| {
                TextPrompt::new(
                    "Quit and stop tty7 server?",
                    Some("a detail long enough to need more than one line"),
                    answers,
                    cx,
                )
            })
        })
    }

    #[gpui::test]
    fn return_confirms_and_escape_takes_the_cancel_answer(cx: &mut TestAppContext) {
        let p = prompt(cx, &crate::ui::confirm_answers("Quit", "Cancel"));
        p.read_with(cx, |p, _| {
            assert_eq!(p.answer_for_key("enter"), Some(0));
            assert_eq!(p.answer_for_key("escape"), Some(1));
            assert_eq!(p.answer_for_key("a"), None);
        });
    }

    #[gpui::test]
    fn escape_does_nothing_without_a_cancel_answer(cx: &mut TestAppContext) {
        let p = prompt(cx, &[PromptButton::ok("OK"), PromptButton::new("Retry")]);
        p.read_with(cx, |p, _| {
            assert_eq!(p.answer_for_key("escape"), None);
        });
    }
}

//! Every confirmation the app asks — `window.prompt` — drawn as one of the
//! app's own cards.
//!
//! gpui answers `window.prompt` with the platform's dialog where there is one:
//! NSAlert on macOS, TaskDialog on Windows, and on Linux a fallback of its own
//! that clipped any sentence longer than its box (#920). The native two were
//! correct and still wrong for this app. Every other question tty7 asks — the
//! SSH sheet, the worktree prompt, New Workspace — is a card built from
//! [`crate::ui::dialog`]: the switcher's 12px corner, a title row with an
//! `esc` cap, a hairline footer, an ink-filled primary. Closing a busy tab
//! instead threw up a system alert with a centred app icon and grey stacked
//! buttons, so the same app had two ideas of what a dialog looks like, and
//! which one you got depended on which question it was.
//!
//! So all three platforms use this. It is the other cards' surface, corner,
//! scrim and buttons in v5's alert shape — title and detail as one paragraph,
//! answers beneath, no header row — wraps its text, and answers Return and
//! Escape the way the native dialogs did, which the call sites were written
//! against.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, PromptButton, PromptHandle, PromptLevel,
    PromptResponse, RenderablePromptHandle, Window, div, prelude::*, px, rems,
};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use crate::ui::dialog::{self, Tone};
use crate::ui::right_panel::{TAB_TEXT, TEXT};

/// Route every `window.prompt` through [`TextPrompt`].
pub(crate) fn install(cx: &mut App) {
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

/// A confirmation card is a sentence and two buttons — v5's alert width. The
/// worktree form, with three fields, is 440.
const ALERT_W: f32 = 360.;

/// A third answer does not fit beside the other two at [`ALERT_W`]: the
/// update prompt's "Update and relaunch / Install on Next Launch / Later"
/// needs about 400 of button row, and the card clipped it.
const ALERT_W_WIDE: f32 = 460.;

/// How far down the alert sits, at most.
const ALERT_TOP: f32 = 180.;

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
            detail: detail.map(str::to_string).filter(|d| !d.trim().is_empty()),
            answers: answers.to_vec(),
            focus: cx.focus_handle(),
        }
    }

    /// The answer a key picks. Return is answer 0 — `confirm_answers` puts the
    /// action there for exactly this — and Escape is [`Self::cancel_answer`];
    /// a prompt with no way out has nothing Escape can safely mean.
    fn answer_for_key(&self, key: &str) -> Option<usize> {
        match key {
            "enter" if !self.answers.is_empty() => Some(0),
            "escape" => self.cancel_answer(),
            _ => None,
        }
    }

    /// The answer that backs out: the one marked cancel, or a lone
    /// acknowledgement — an `OK` with nothing else to pick is also the way
    /// out, and a card Escape cannot close is a trap.
    fn cancel_answer(&self) -> Option<usize> {
        match self.answers.as_slice() {
            [_] => Some(0),
            answers => answers.iter().position(PromptButton::is_cancel),
        }
    }

    /// Whether answer `ix` sits apart, at the footer's far left. Answers
    /// listed after the cancel in a prompt of three or more are the ones a
    /// caller wants away from Return — Discard beside Save / Cancel — and
    /// packed in with the rest it would sit one button from the default.
    fn stands_apart(&self, ix: usize) -> bool {
        self.answers.len() >= 3
            && self
                .answers
                .iter()
                .position(PromptButton::is_cancel)
                .is_some_and(|cancel| ix > cancel)
    }

    /// How answer `ix` is painted.
    ///
    /// Answer 0 is the one Return picks, so it is the one filled in — unless
    /// it is the cancel. A caller only puts Cancel first when the action is
    /// one Return must never reach by accident (discarding changes, a hard
    /// reset); that action is the one the card warns about, so it goes red,
    /// and the safe answer keeps the quiet paint.
    fn tone(&self, ix: usize) -> Tone {
        let answer = &self.answers[ix];
        let cancel_first = self.answers.first().is_some_and(PromptButton::is_cancel);
        match (ix, answer.is_cancel()) {
            (_, true) => Tone::Secondary,
            (0, false) => Tone::Primary,
            (_, false) if cancel_first => Tone::Danger,
            _ => Tone::Secondary,
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        // gpui paints the prompt as a root of its own, beside the window's
        // `Root` rather than inside it, so nothing it sets is inherited here:
        // left alone the card is set in gpui's `.SystemUIFont`, not the
        // Interface font the rest of the chrome uses (#920).
        let font_family = theme.font_family.clone();
        let rungs = dialog::popover_rungs(cx);

        // Answer 0 goes rightmost, where the native dialogs put it and where
        // `confirm_answers` expects it to land.
        let mut apart = Vec::new();
        let mut packed = Vec::new();
        for ix in (0..self.answers.len()).rev() {
            let button = dialog::button(
                ("prompt-answer", ix),
                self.answers[ix].label().clone(),
                self.tone(ix),
                true,
                rungs,
                cx,
                cx.listener(move |_, _, _, cx| {
                    cx.emit(PromptResponse(ix));
                    cx.stop_propagation();
                }),
            );
            let button = button.debug_selector(move || format!("prompt-answer-{ix}"));
            match self.stands_apart(ix) {
                true => apart.push(button),
                false => packed.push(button),
            }
        }

        // The v5 confirm card: no title row and no footer rule — a question
        // is one thought, not a form, so it is set as a paragraph with its
        // answers under it. The title wraps: cut short, a question no longer
        // asks anything.
        let text = v_flex()
            .gap(px(4.))
            .min_w_0()
            .child(
                div()
                    .text_size(rems(TEXT))
                    .line_height(rems(TEXT * 1.4))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(self.message.clone()),
            )
            .children(self.detail.clone().map(|detail| {
                div()
                    .text_size(rems(TAB_TEXT))
                    .line_height(rems(TAB_TEXT * 1.45))
                    .text_color(muted)
                    .child(detail)
            }));

        // Buttons never shrink, so a row too long for the card — a locale
        // with longer labels than the width was sized for — wraps onto a
        // second line, still flush right, rather than running off the edge.
        let answers = h_flex()
            .flex_wrap()
            .justify_end()
            .items_center()
            .gap(px(8.))
            .children(apart)
            .child(div().flex_1())
            .children(packed);

        let width = match self.answers.len() {
            0..=2 => ALERT_W,
            _ => ALERT_W_WIDE,
        };
        let card = dialog::card("prompt-card", self.message.clone(), width, cx)
            .max_w(gpui::relative(0.9))
            .gap(px(16.))
            .pt(px(20.))
            .px(px(20.))
            .pb(px(16.))
            .child(text)
            .child(answers);

        // Lower than the switcher's drop: an alert is read, not typed into,
        // and v5 sets it at eye height. A short window pulls it up rather
        // than pushing the buttons off the bottom.
        let top = (window.viewport_size().height.as_f32() * 0.22).clamp(16., ALERT_TOP);

        div()
            .id("text-prompt")
            .track_focus(&self.focus)
            .size_full()
            .font_family(font_family)
            // Nothing under the scrim answers the pointer while the question
            // is up: the prompt is painted over the window, not into it, and
            // gpui hands a click to every hitbox under it that is not
            // occluded — the dimmed window would still take it.
            .occlude()
            .cursor_default()
            .bg(crate::ui::presets::scrim_fill(cx))
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                if let Some(ix) = this.answer_for_key(&ev.keystroke.key) {
                    cx.emit(PromptResponse(ix));
                    cx.stop_propagation();
                }
            }))
            // Clicking the scrim backs out, as it does around the other
            // cards — when there is a safe answer to back out to.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                    if let Some(ix) = this.cancel_answer() {
                        cx.emit(PromptResponse(ix));
                    }
                }),
            )
            .flex()
            .flex_col()
            .items_center()
            .justify_start()
            .pt(px(top))
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
    fn the_action_is_filled_and_the_safe_answer_stays_quiet(cx: &mut TestAppContext) {
        use crate::ui::dialog::Tone;
        let p = prompt(cx, &crate::ui::confirm_answers("Close", "Keep"));
        p.read_with(cx, |p, _| {
            assert_eq!(p.tone(0), Tone::Primary);
            assert_eq!(p.tone(1), Tone::Secondary);
        });
    }

    #[gpui::test]
    fn cancel_first_marks_the_other_answer_as_the_dangerous_one(cx: &mut TestAppContext) {
        // Discard puts Cancel at 0 so Return cannot throw work away; the card
        // has to say which button does.
        use crate::ui::dialog::Tone;
        let p = prompt(
            cx,
            &[PromptButton::cancel("Cancel"), PromptButton::new("Discard")],
        );
        p.read_with(cx, |p, _| {
            assert_eq!(p.answer_for_key("enter"), Some(0));
            assert_eq!(p.answer_for_key("escape"), Some(0));
            assert_eq!(p.tone(0), Tone::Secondary);
            assert_eq!(p.tone(1), Tone::Danger);
        });
    }

    #[gpui::test]
    fn discard_stands_apart_from_save_and_cancel(cx: &mut TestAppContext) {
        let p = prompt(
            cx,
            &[
                PromptButton::ok("Save"),
                PromptButton::cancel("Cancel"),
                PromptButton::ok("Discard"),
            ],
        );
        p.read_with(cx, |p, _| {
            assert!(!p.stands_apart(0));
            assert!(!p.stands_apart(1));
            assert!(p.stands_apart(2));
            assert_eq!(p.answer_for_key("escape"), Some(1));
        });
    }

    /// #920. The quit-and-stop question is asked through `window.prompt`, and
    /// with the builder installed it must never reach the platform — on Linux
    /// that meant gpui's fallback, a white box that clipped the text to one
    /// line and ignored the theme. Return still answers it.
    #[gpui::test]
    fn the_quit_prompt_is_drawn_by_the_app_not_the_platform(cx: &mut TestAppContext) {
        use crate::ui::i18n::{L10nKey, t};

        let (_app, mut vcx) = crate::ui::app::test_window::harness(cx);
        let mut answer = vcx.update(|window, cx| {
            super::install(cx);
            window.prompt(
                gpui::PromptLevel::Warning,
                t(L10nKey::QuitStopServerTitle),
                Some(t(L10nKey::QuitStopServerBody)),
                &crate::ui::confirm_answers(t(L10nKey::QuitAndStop), t(L10nKey::Cancel)),
                cx,
            )
        });
        vcx.run_until_parked();
        assert!(
            !vcx.has_pending_prompt(),
            "the quit prompt fell through to the platform dialog"
        );

        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        assert_eq!(answer.try_recv().ok().flatten(), Some(0));
    }

    #[gpui::test]
    fn a_lone_ok_answers_escape_too(cx: &mut TestAppContext) {
        let p = prompt(cx, &[PromptButton::new("OK")]);
        p.read_with(cx, |p, _| {
            assert_eq!(p.answer_for_key("escape"), Some(0));
        });
    }

    #[gpui::test]
    fn escape_does_nothing_without_a_cancel_answer(cx: &mut TestAppContext) {
        let p = prompt(cx, &[PromptButton::ok("OK"), PromptButton::new("Retry")]);
        p.read_with(cx, |p, _| {
            assert_eq!(p.answer_for_key("escape"), None);
        });
    }
    /// The answers are buttons, so a click has to answer — in the window the
    /// prompt is really drawn over, the app's own, not a bare test view.
    #[gpui::test]
    async fn a_click_on_an_answer_answers(cx: &mut TestAppContext) {
        use crate::core::config::Config;
        use crate::core::session::Session;
        use crate::ui::app::Tty7App;
        use crate::ui::windows::WindowRegistry;
        use gpui::{Modifiers, PromptLevel, VisualTestContext};

        crate::core::config::pin_test_config_dir();
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
            crate::ui::keymap::init(cx);
            WindowRegistry::init(cx);
            super::install(cx);
        });
        let window = cx.add_window(|window, cx| {
            let app =
                cx.new(|cx| Tty7App::with_session(None, Some(Session::default()), window, cx));
            gpui_component::Root::new(app, window, cx)
        });
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.run_until_parked();

        let answers = [PromptButton::cancel("Cancel"), PromptButton::new("Discard")];
        let answer = vcx.update(|window, cx| {
            window.prompt(PromptLevel::Warning, "Discard?", None, &answers, cx)
        });
        vcx.run_until_parked();
        let discard = vcx
            .debug_bounds("prompt-answer-1")
            .expect("the Discard button is drawn");
        vcx.simulate_click(discard.center(), Modifiers::none());
        vcx.run_until_parked();
        assert_eq!(answer.await.ok(), Some(1));
    }
}

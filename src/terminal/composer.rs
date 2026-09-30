//! The message composer: a real text box docked under a pane that is running a
//! coding agent, for writing a prompt the way a chat box lets you — mouse
//! selection, the platform's own editing keys, an IME with its candidates in
//! place, pasting a screenshot — and handing it over whole.
//!
//! The agent's own TUI stays exactly where it was. The box docks *below* the
//! grid rather than floating over it, so the grid gives up the rows the box
//! takes and the agent reflows into what is left: nothing it draws is hidden,
//! including the permission prompts that have to be answered in the TUI itself.
//!
//! Sending is typing, not an API. What leaves the box is written to the pane's
//! pty as the text and then Enter, as two writes — see [`submit_plan`] for why
//! each agent needs to be spoken to slightly differently.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use alacritty_terminal::term::TermMode;
use gpui::{
    Context, Entity, Focusable as _, MouseButton, MouseDownEvent, Subscription, Window, div,
    prelude::*, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, h_flex};

use super::view::{TerminalView, types_cleanly};
use crate::core::cli_agent::{AgentStatus, CLIAgent};
use crate::ui::host_ops::HostId;
use crate::ui::i18n::{L10nKey, t, t_fmt};

/// How tall the box may grow, in lines of text, before it scrolls instead.
/// Every row it grows is a row the agent loses, and a resize the agent has to
/// redraw for, so this stays well short of a page.
const MAX_ROWS: usize = 8;

/// The pause between two writes that must not arrive as one read.
///
/// An agent's input layer tells typing from pasting by how bytes arrive, and a
/// CR landing in the same read as the text before it is taken as part of that
/// text — a newline in the message rather than the key that sends it. Long
/// enough to put the two in separate reads on a loaded machine, short enough
/// to be inside the time a key press takes to feel instant.
const SETTLE: Duration = Duration::from_millis(50);

/// Copilot's input treats a CR that follows a paste too closely as part of it.
const SETTLE_AFTER_PASTE_SLOW: Duration = Duration::from_millis(300);

/// One write of a submission, and how long to wait before making it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Step {
    pub delay: Duration,
    pub bytes: Vec<u8>,
}

/// The writes that hand `text` to `agent` and press Enter on it.
///
/// - **The text and the Enter are separate writes**, [`SETTLE`] apart. In one
///   write, the CR is part of the text as far as the agent can tell.
/// - **A single plain line is typed**, not pasted: it goes in the way the
///   keyboard would have sent it, so the agent treats it like typing — a `/`
///   command opens its menu, nothing is folded into a "pasted text"
///   placeholder. Anything with a line break, a tab or other control
///   character, or past [`types_cleanly`]'s bound goes as one bracketed paste,
///   so the lines are the message's rather than a series of Enters. That is
///   the same line the prompt editor draws for the shell.
/// - **Codex is always pasted.** It watches for bursts of fast keystrokes to
///   spot pastes from terminals that do not bracket them, and the Enter after
///   a typed burst is swallowed into it.
/// - **A leading `!` goes to Claude Code on its own.** It switches the input
///   into shell mode only when it is typed into an empty box as a key of its
///   own; arriving with the rest of the line it is just a character.
///
/// Without bracketed paste switched on, line breaks go as LF: to every agent
/// input that is Ctrl+J, a newline in the message, where a CR would send each
/// line as a message of its own.
pub(super) fn submit_plan(agent: CLIAgent, text: &str, bracketed: bool) -> Vec<Step> {
    let clean: String = text
        .replace("\r\n", "\n")
        .chars()
        .filter(|&c| c != '\x1b')
        .map(|c| if c == '\r' { '\n' } else { c })
        .collect();
    let mut body = clean.trim_end();
    let mut steps = Vec::new();
    let mut delay = Duration::ZERO;

    if agent == CLIAgent::Claude
        && let Some(rest) = body.strip_prefix('!')
        && !rest.is_empty()
    {
        steps.push(Step {
            delay,
            bytes: b"!".to_vec(),
        });
        body = rest;
        delay = SETTLE;
    }

    let mut pasted = false;
    if !body.is_empty() {
        pasted = bracketed && (agent == CLIAgent::Codex || !types_cleanly(body));
        let bytes = match pasted {
            true => tty7_core::core::paste::bracket(body.as_bytes()),
            false => body.as_bytes().to_vec(),
        };
        steps.push(Step { delay, bytes });
    }

    let enter_delay = match (steps.is_empty(), agent) {
        (true, _) => Duration::ZERO,
        (false, CLIAgent::Copilot) if pasted => SETTLE_AFTER_PASTE_SLOW,
        (false, _) => SETTLE,
    };
    steps.push(Step {
        delay: enter_delay,
        bytes: b"\r".to_vec(),
    });
    steps
}

/// What a pane's composer holds that outlives the view drawing it.
///
/// A view is rebuilt over the same daemon pane whenever its workspace is
/// switched out and back, and a half-written prompt must not be the price of
/// looking at another workspace. Kept by pane, for the life of the app.
#[derive(Default)]
struct ComposerMemory(HashMap<(HostId, u64), Remembered>);

impl gpui::Global for ComposerMemory {}

#[derive(Default, Clone)]
struct Remembered {
    draft: String,
    open: bool,
}

pub(super) struct Composer {
    pub(super) input: Entity<InputState>,
    /// Whether the user wants the box on this pane. It is only *shown* while
    /// an agent is in the foreground, so a pane that goes back to its shell
    /// hides it and the next agent started there gets it back.
    open: bool,
    /// Writes waiting their turn. Submissions queue rather than interleave, so
    /// a second message sent inside the first one's settle time cannot land
    /// its text between the first one's text and its Enter.
    queue: VecDeque<Step>,
    pumping: bool,
    /// Text for the box from paths that cannot reach it directly — a paste
    /// resolved on a background task, a file upload — taken in at the next
    /// draw, which is the first place with a window to edit it in.
    pending: Vec<String>,
    /// Whose name the placeholder carries. A pane can run one agent after
    /// another, and "Message Codex…" over Claude Code would be a lie.
    named: Option<CLIAgent>,
    _subs: Vec<Subscription>,
}

impl TerminalView {
    fn composer_key(&self) -> (HostId, u64) {
        (self.host_id(), self.pane_id)
    }

    fn remember_composer(&self, cx: &mut Context<Self>) {
        let Some(c) = self.composer.as_ref() else {
            return;
        };
        let entry = Remembered {
            draft: c.input.read(cx).value().to_string(),
            open: c.open,
        };
        let key = self.composer_key();
        let memory = cx.default_global::<ComposerMemory>();
        match entry.draft.is_empty() && !entry.open {
            true => memory.0.remove(&key),
            false => memory.0.insert(key, entry),
        };
    }

    fn ensure_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.composer.is_some() {
            return;
        }
        let remembered = cx
            .try_global::<ComposerMemory>()
            .and_then(|m| m.0.get(&self.composer_key()))
            .cloned()
            .unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(1, MAX_ROWS)
                .submit_on_enter(true)
                .default_value(remembered.draft)
        });
        let subs = vec![cx.subscribe_in(&input, window, Self::on_composer_event)];
        self.composer = Some(Composer {
            input,
            open: remembered.open,
            queue: VecDeque::new(),
            pumping: false,
            pending: Vec::new(),
            named: None,
            _subs: subs,
        });
    }

    /// Whether the box is on screen: wanted, and an agent to talk to.
    pub(super) fn composer_shown(&self) -> bool {
        self.composer.as_ref().is_some_and(|c| c.open) && self.agent().is_some()
    }

    /// Open the box and put the caret in it; from inside it, close it; with
    /// it open but the terminal focused, go back into it.
    ///
    /// A pane with no agent in the foreground has nobody to compose for, so
    /// the chord does nothing there.
    pub fn toggle_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.agent().is_none() {
            return;
        }
        self.ensure_composer(window, cx);
        let Some(c) = self.composer.as_mut() else {
            return;
        };
        let focus = c.input.read(cx).focus_handle(cx);
        match (c.open, focus.is_focused(window)) {
            (true, true) => {
                c.open = false;
                window.focus(&self.focus_handle, cx);
            }
            _ => {
                c.open = true;
                window.focus(&focus, cx);
            }
        }
        self.remember_composer(cx);
        cx.notify();
    }

    /// Esc in the box: hand the keyboard back to the terminal and leave the
    /// box where it is. Esc is also what interrupts an agent, and a box that
    /// closed on it would put the next Esc — the one meant for the agent — a
    /// keystroke further away than the user expects.
    pub(super) fn leave_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// Text arriving for the box by way of the terminal's paste path — files
    /// dropped on it, a pasted screenshot saved to disk, an upload to a remote
    /// pane — so every route the terminal already knows lands in the box
    /// instead when the box has the keyboard.
    ///
    /// Hands `text` back when the box is not the one taking it.
    pub(super) fn composer_takes_paste(
        &mut self,
        text: String,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if !self.composer_focused || !self.composer_shown() {
            return Some(text);
        }
        self.composer.as_mut()?.pending.push(text);
        cx.notify();
        None
    }

    /// Per-frame upkeep: restore a box the pane had open before this view
    /// existed, take in pending text, and give the keyboard back to the
    /// terminal when the agent the box was for has gone.
    pub(super) fn sync_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.composer.is_none()
            && self.agent().is_some()
            && cx
                .try_global::<ComposerMemory>()
                .is_some_and(|m| m.0.contains_key(&self.composer_key()))
        {
            self.ensure_composer(window, cx);
        }
        let shown = self.composer_shown();
        let agent = self.agent();
        let Some(c) = self.composer.as_mut() else {
            return;
        };
        if let Some(agent) = agent
            && c.named != Some(agent)
        {
            c.named = Some(agent);
            let placeholder = t_fmt(
                L10nKey::ComposerPlaceholder,
                &[("agent", agent.display_name())],
            );
            c.input.update(cx, |state, cx| {
                state.set_placeholder(placeholder, window, cx)
            });
        }
        if !c.pending.is_empty() {
            let text = std::mem::take(&mut c.pending).concat();
            c.input
                .update(cx, |state, cx| state.insert(text, window, cx));
        }
        if !shown && self.composer_focused {
            self.composer_focused = false;
            window.focus(&self.focus_handle, cx);
        }
    }

    fn on_composer_event(
        &mut self,
        _input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => self.remember_composer(cx),
            InputEvent::PressEnter { shift: false, .. } => self.submit_composer(window, cx),
            InputEvent::PressEnter { .. } => {}
            InputEvent::Focus => {
                self.composer_focused = true;
                cx.notify();
            }
            InputEvent::Blur => {
                self.composer_focused = false;
                cx.notify();
            }
        }
    }

    /// The agent is asking a question only its TUI can put — a permission
    /// prompt, a choice. Whatever the box sent would be taken as the answer,
    /// so Enter holds the message until the question is gone.
    fn agent_is_asking(&self) -> bool {
        self.agent_session()
            .is_some_and(|s| s.status == AgentStatus::Waiting)
    }

    pub(super) fn submit_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent) = self.agent() else {
            return;
        };
        if self.agent_is_asking() {
            return;
        }
        let Some(c) = self.composer.as_ref() else {
            return;
        };
        let text = c.input.read(cx).value().to_string();
        let bracketed = self
            .terminal
            .term
            .lock()
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        let steps = submit_plan(agent, &text, bracketed);
        c.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.remember_composer(cx);
        if let Some(c) = self.composer.as_mut() {
            c.queue.extend(steps);
        }
        self.pump_composer(agent, cx);
    }

    /// Write queued steps one at a time, each after its own delay.
    ///
    /// Every write after a wait checks that `agent` is still what the pane is
    /// running: an agent that quit inside the wait has handed the pty back to
    /// the shell, and the rest of the message — its Enter above all — would
    /// run there as a command.
    fn pump_composer(&mut self, agent: CLIAgent, cx: &mut Context<Self>) {
        let Some(c) = self.composer.as_mut() else {
            return;
        };
        if c.pumping {
            return;
        }
        c.pumping = true;
        cx.spawn(async move |this, cx| {
            loop {
                // Popping the last step and standing the pump down are one
                // update, so a submission can never find it still "pumping"
                // after it has stopped looking at the queue.
                let next = this
                    .update(cx, |view, _| {
                        let c = view.composer.as_mut()?;
                        let step = c.queue.pop_front();
                        c.pumping = step.is_some();
                        step
                    })
                    .ok()
                    .flatten();
                let Some(step) = next else { return };
                if !step.delay.is_zero() {
                    cx.background_executor().timer(step.delay).await;
                }
                let sent = this.update(cx, |view, cx| {
                    if view.agent() != Some(agent) {
                        if let Some(c) = view.composer.as_mut() {
                            c.queue.clear();
                        }
                        return;
                    }
                    view.send_to_pty(&step.bytes, cx);
                });
                if sent.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    pub(super) fn render_composer(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.composer_shown() {
            return None;
        }
        let c = self.composer.as_ref()?;
        let agent = self.agent()?;
        let theme = cx.theme();
        let focused = c.input.read(cx).focus_handle(cx).is_focused(window);
        let asking = self.agent_is_asking();
        let border = match (asking, focused) {
            (true, _) => theme.warning,
            (false, true) => theme.ring,
            (false, false) => theme.border,
        };
        let hint = match asking {
            true => t_fmt(
                L10nKey::ComposerAgentAsking,
                &[("agent", agent.display_name())],
            ),
            false => t(L10nKey::ComposerKeys).to_string(),
        };
        let hint_color = match asking {
            true => theme.warning,
            false => theme.muted_foreground,
        };
        Some(
            div()
                .id("composer")
                .flex_none()
                .w_full()
                .pt(px(6.))
                // The terminal surface this sits in focuses the grid on any
                // click and opens its own context menu on a right one. Neither
                // is right for a click on the box.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        window.prevent_default();
                        // A click on the frame around the text, not only on
                        // the text, is a click on the box.
                        if let Some(c) = this.composer.as_ref()
                            && !this.composer_focused
                        {
                            let focus = c.input.read(cx).focus_handle(cx);
                            window.focus(&focus, cx);
                        }
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _: &MouseDownEvent, _w, cx| {
                        this.context_menu_allowed = false;
                        cx.stop_propagation();
                    }),
                )
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .capture_action(
                    cx.listener(|this, _: &gpui_component::input::Paste, _w, cx| {
                        // Text pastes are the box's own. A copied file or a
                        // screenshot has no text to paste, and the terminal
                        // already knows how to turn those into a path.
                        let Some(item) = cx.read_from_clipboard() else {
                            return;
                        };
                        if item.text().is_some() && !super::view::clipboard_has_paths(&item) {
                            return;
                        }
                        cx.stop_propagation();
                        this.paste_from_clipboard(cx);
                    }),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .border_1()
                        .border_color(border)
                        .bg(theme.popover)
                        .child(Input::new(&c.input).appearance(false))
                        .child(
                            h_flex()
                                .justify_end()
                                .text_xs()
                                .text_color(hint_color)
                                .child(hint),
                        ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(steps: &[Step]) -> Vec<&[u8]> {
        steps.iter().map(|s| s.bytes.as_slice()).collect()
    }

    #[test]
    fn a_plain_line_is_typed_then_entered_separately() {
        let steps = submit_plan(CLIAgent::Claude, "fix the tests", true);
        assert_eq!(bytes(&steps), [&b"fix the tests"[..], b"\r"]);
        assert_eq!(steps[0].delay, Duration::ZERO);
        assert_eq!(steps[1].delay, SETTLE);
    }

    #[test]
    fn several_lines_go_as_one_paste() {
        let steps = submit_plan(CLIAgent::Claude, "one\r\ntwo\n", true);
        assert_eq!(bytes(&steps), [&b"\x1b[200~one\ntwo\x1b[201~"[..], b"\r"]);
    }

    #[test]
    fn without_bracketed_paste_line_breaks_stay_newlines_not_enters() {
        let steps = submit_plan(CLIAgent::Gemini, "one\ntwo", false);
        assert_eq!(bytes(&steps), [&b"one\ntwo"[..], b"\r"]);
    }

    #[test]
    fn codex_is_always_pasted() {
        let steps = submit_plan(CLIAgent::Codex, "hi", true);
        assert_eq!(bytes(&steps), [&b"\x1b[200~hi\x1b[201~"[..], b"\r"]);
    }

    #[test]
    fn a_leading_bang_reaches_claude_as_a_key_of_its_own() {
        let steps = submit_plan(CLIAgent::Claude, "!git status", true);
        assert_eq!(bytes(&steps), [&b"!"[..], b"git status", b"\r"]);
        assert_eq!(steps[1].delay, SETTLE);
        // Anyone else gets the line as written.
        let steps = submit_plan(CLIAgent::Gemini, "!git status", true);
        assert_eq!(bytes(&steps), [&b"!git status"[..], b"\r"]);
    }

    #[test]
    fn an_empty_box_sends_enter_alone() {
        let steps = submit_plan(CLIAgent::Claude, "  \n", true);
        assert_eq!(
            steps,
            [Step {
                delay: Duration::ZERO,
                bytes: b"\r".to_vec()
            }]
        );
    }

    #[test]
    fn escapes_cannot_close_the_paste_early() {
        let steps = submit_plan(CLIAgent::Claude, "a\n\x1b[201~b", true);
        assert_eq!(bytes(&steps)[0], b"\x1b[200~a\n[201~b\x1b[201~");
    }

    #[test]
    fn copilot_waits_longer_after_a_paste() {
        let steps = submit_plan(CLIAgent::Copilot, "a\nb", true);
        assert_eq!(steps[1].delay, SETTLE_AFTER_PASTE_SLOW);
        let steps = submit_plan(CLIAgent::Copilot, "ab", true);
        assert_eq!(steps[1].delay, SETTLE);
    }
}

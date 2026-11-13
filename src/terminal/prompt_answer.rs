//! Answering a waiting agent's prompt from outside its pane.
//!
//! The attention inbox shows what an agent asks (an [`AgentPrompt`]) and
//! takes the answer there. The agent only reads its own TUI, though, so the
//! answer goes in as the keys a user at that pane would press. This module
//! turns an answer into those keys, per agent, and refuses one meant for a
//! prompt that is no longer the one on screen.
//!
//! # The keys
//!
//! Claude Code draws every question and permission ask with one select
//! component. A digit `1`–`9` picks the option with that number outright
//! (single-select), toggles it (multi-select), or moves onto the free-text
//! "Other" row, which then takes typing and Enter. A single single-select
//! question is submitted by its pick; anything else (more than one
//! question, or a multi-select one) ends on a Submit tab whose first
//! button, "Submit answers", Enter presses. → leaves a multi-select
//! question for the next tab. Esc on a permission ask rejects it.
//!
//! Codex lists its choices with the selection on the first and takes ↓ and
//! Enter; its approval list offers `y` for "Yes, proceed" and Esc for "No".
//! It has no multi-select.
//!
//! No "always allow" answer exists here on purpose: an approval given from
//! a list of agents is one approval, never a standing rule.

use std::time::Duration;

use super::composer::{KEY_GAP, SETTLE, Step};
use crate::core::cli_agent::{
    AgentPrompt, AgentPromptKind, AgentQuestion, AgentSessionState, AgentStatus, CLIAgent,
};

/// What the user chose for one question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuestionAnswer {
    /// One option of a single-select question, by index.
    Pick(usize),
    /// The options ticked on a multi-select question, by index.
    Toggle(Vec<usize>),
    /// Words of the user's own, for a question that takes them.
    Other(String),
}

/// An answer to a whole prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptAnswer {
    /// One answer per question, in the prompt's order.
    Questions(Vec<QuestionAnswer>),
    /// Run the tool, this once.
    AllowOnce,
    Deny,
}

/// Why an answer was not typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnswerRefused {
    /// The prompt the answer was for is gone or was replaced by another.
    Stale,
    /// An answer to this prompt is still being typed.
    InFlight,
    /// The answer does not fit the prompt: the wrong kind, a question left
    /// unanswered, an option that is not there.
    Mismatch,
    /// This agent's prompts cannot be answered from here; its pane can.
    Unsupported,
    /// The pane does not take input: exited, or a remote client holds it.
    NoInput,
}

/// The pane's open prompt, if it is still the one called `id`.
///
/// The inbox draws a prompt and the user answers it a moment later; in
/// between the agent can have moved on, or asked something else. Typing the
/// old answer into the new prompt would answer a question nobody read, so
/// anything but the same id, still waiting, is refused.
pub fn current_prompt<'a>(
    session: Option<&'a AgentSessionState>,
    id: &str,
) -> Result<&'a AgentPrompt, AnswerRefused> {
    session
        .filter(|s| s.status == AgentStatus::Waiting)
        .and_then(|s| s.prompt.as_ref())
        .filter(|p| p.id == id)
        .ok_or(AnswerRefused::Stale)
}

/// Whether the inbox can answer this agent's prompts at all.
pub fn answerable(agent: CLIAgent) -> bool {
    matches!(agent, CLIAgent::Claude | CLIAgent::Codex)
}

/// The writes that give `answer` to `prompt` in `agent`'s TUI.
///
/// `bracketed` is whether the pane has bracketed paste on; Codex's free
/// text goes as a paste when it does, the way the composer sends it.
pub(super) fn answer_keys(
    agent: CLIAgent,
    prompt: &AgentPrompt,
    answer: &PromptAnswer,
    bracketed: bool,
) -> Result<Vec<Step>, AnswerRefused> {
    if !answerable(agent) {
        return Err(AnswerRefused::Unsupported);
    }
    let mut keys = Keys::default();
    match (&prompt.kind, answer) {
        (AgentPromptKind::Permission { .. }, PromptAnswer::AllowOnce) => match agent {
            CLIAgent::Claude => keys.press(Duration::ZERO, b"1"),
            _ => keys.press(Duration::ZERO, b"y"),
        },
        (AgentPromptKind::Permission { .. }, PromptAnswer::Deny) => {
            keys.press(Duration::ZERO, b"\x1b")
        }
        (AgentPromptKind::Question { questions }, PromptAnswer::Questions(answers)) => {
            if questions.len() != answers.len() {
                return Err(AnswerRefused::Mismatch);
            }
            for (q, a) in questions.iter().zip(answers) {
                check(q, a)?;
            }
            match agent {
                CLIAgent::Claude => claude_questions(&mut keys, questions, answers),
                _ => codex_questions(&mut keys, questions, answers, bracketed)?,
            }
        }
        _ => return Err(AnswerRefused::Mismatch),
    }
    Ok(keys.steps)
}

/// Whether `a` is an answer `q` can take.
fn check(q: &AgentQuestion, a: &QuestionAnswer) -> Result<(), AnswerRefused> {
    let n = q.options.len();
    let ok = match a {
        QuestionAnswer::Pick(i) => !q.multi_select && *i < n,
        QuestionAnswer::Toggle(picks) => {
            q.multi_select && !picks.is_empty() && picks.iter().all(|i| *i < n)
        }
        QuestionAnswer::Other(text) => {
            q.allow_other && !q.multi_select && !one_line(text).is_empty()
        }
    };
    ok.then_some(()).ok_or(AnswerRefused::Mismatch)
}

fn claude_questions(keys: &mut Keys, questions: &[AgentQuestion], answers: &[QuestionAnswer]) {
    // The Submit tab is left out for one single-select question alone, which
    // its pick submits (Claude Code's own condition).
    let submit_tab = questions.len() > 1 || questions.iter().any(|q| q.multi_select);
    for (q, a) in questions.iter().zip(answers) {
        match a {
            QuestionAnswer::Pick(i) => claude_row(keys, *i),
            QuestionAnswer::Toggle(picks) => {
                let mut picks = picks.clone();
                picks.sort_unstable();
                picks.dedup();
                for i in picks {
                    // The row is toggled in place; the cursor stays where it
                    // was, so each tick is one more digit.
                    keys.digit_or_walk(i, KEY_GAP, false);
                }
                // On to the next question's tab, or the Submit tab.
                keys.press(SETTLE, b"\x1b[C");
            }
            QuestionAnswer::Other(text) => {
                // The Other row comes right after the options. Its digit moves
                // onto it while it is empty; then it is a text field.
                claude_row(keys, q.options.len());
                keys.press(SETTLE, one_line(text).as_bytes());
                keys.press(SETTLE, b"\r");
            }
        }
    }
    if submit_tab {
        keys.press(SETTLE, b"\r");
    }
}

/// Pick row `i` of a Claude select: its digit, or, past nine, the arrows.
fn claude_row(keys: &mut Keys, i: usize) {
    keys.digit_or_walk(i, SETTLE, true);
}

fn codex_questions(
    keys: &mut Keys,
    questions: &[AgentQuestion],
    answers: &[QuestionAnswer],
    bracketed: bool,
) -> Result<(), AnswerRefused> {
    for (q, a) in questions.iter().zip(answers) {
        match a {
            QuestionAnswer::Pick(i) => {
                for _ in 0..*i {
                    keys.press(KEY_GAP, b"\x1b[B");
                }
                keys.press(SETTLE, b"\r");
            }
            // Only a question with no options is a plain text field in
            // Codex; where its "other" row sits among options is not
            // something to guess at.
            QuestionAnswer::Other(text) if q.options.is_empty() => {
                let text = one_line(text);
                let bytes = match bracketed {
                    true => tty7_core::core::paste::bracket(text.as_bytes()),
                    false => text.into_bytes(),
                };
                keys.press(SETTLE, &bytes);
                keys.press(SETTLE, b"\r");
            }
            _ => return Err(AnswerRefused::Unsupported),
        }
    }
    Ok(())
}

/// The text as one line with no control characters: a line break would be
/// an Enter to the field it lands in.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// The writes so far, with the first one going out at once.
#[derive(Default)]
struct Keys {
    steps: Vec<Step>,
}

impl Keys {
    fn press(&mut self, delay: Duration, bytes: &[u8]) {
        let delay = match self.steps.is_empty() {
            true => Duration::ZERO,
            false => delay,
        };
        self.steps.push(Step {
            delay,
            bytes: bytes.to_vec(),
        });
    }

    /// Row `i` by its digit when it has one. Past nine, walk down to it from
    /// the top and, when `enter`, press Enter on it.
    fn digit_or_walk(&mut self, i: usize, delay: Duration, enter: bool) {
        if i < 9 {
            self.press(delay, (i + 1).to_string().as_bytes());
            return;
        }
        for _ in 0..i {
            self.press(KEY_GAP, b"\x1b[B");
        }
        match enter {
            true => self.press(SETTLE, b"\r"),
            false => self.press(KEY_GAP, b" "),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::cli_agent::AgentQuestionOption;

    fn question(options: &[&str], multi_select: bool, allow_other: bool) -> AgentQuestion {
        AgentQuestion {
            header: None,
            text: "Which?".into(),
            options: options
                .iter()
                .map(|l| AgentQuestionOption {
                    label: (*l).into(),
                    description: None,
                })
                .collect(),
            multi_select,
            allow_other,
        }
    }

    fn asking(questions: Vec<AgentQuestion>) -> AgentPrompt {
        AgentPrompt {
            id: "p1".into(),
            kind: AgentPromptKind::Question { questions },
        }
    }

    fn permission() -> AgentPrompt {
        AgentPrompt {
            id: "p1".into(),
            kind: AgentPromptKind::Permission {
                tool: "Bash".into(),
                summary: "rm -rf build".into(),
                detail: None,
            },
        }
    }

    fn bytes(steps: &[Step]) -> Vec<&[u8]> {
        steps.iter().map(|s| s.bytes.as_slice()).collect()
    }

    #[test]
    fn one_claude_question_is_answered_by_its_digit_alone() {
        let p = asking(vec![question(&["a", "b", "c"], false, true)]);
        let steps = answer_keys(
            CLIAgent::Claude,
            &p,
            &PromptAnswer::Questions(vec![QuestionAnswer::Pick(1)]),
            true,
        )
        .unwrap();
        assert_eq!(bytes(&steps), vec![b"2".as_slice()]);
        assert_eq!(steps[0].delay, Duration::ZERO);
    }

    #[test]
    fn several_claude_questions_end_on_the_submit_tab() {
        let p = asking(vec![
            question(&["a", "b"], false, true),
            question(&["x", "y", "z"], true, true),
        ]);
        let steps = answer_keys(
            CLIAgent::Claude,
            &p,
            &PromptAnswer::Questions(vec![
                QuestionAnswer::Pick(0),
                QuestionAnswer::Toggle(vec![2, 0, 2]),
            ]),
            true,
        )
        .unwrap();
        assert_eq!(
            bytes(&steps),
            vec![
                b"1".as_slice(),
                b"1".as_slice(),
                b"3".as_slice(),
                b"\x1b[C".as_slice(),
                b"\r".as_slice(),
            ]
        );
        assert!(steps[1..].iter().all(|s| s.delay >= KEY_GAP));
    }

    #[test]
    fn claudes_other_row_is_reached_by_digit_then_typed_into() {
        let p = asking(vec![question(&["a", "b"], false, true)]);
        let steps = answer_keys(
            CLIAgent::Claude,
            &p,
            &PromptAnswer::Questions(vec![QuestionAnswer::Other("use\nzstd ".into())]),
            true,
        )
        .unwrap();
        assert_eq!(
            bytes(&steps),
            vec![b"3".as_slice(), b"use zstd".as_slice(), b"\r".as_slice()]
        );
        assert!(steps[1..].iter().all(|s| s.delay == SETTLE));
    }

    #[test]
    fn codex_walks_down_to_its_pick_and_pastes_free_text() {
        let p = asking(vec![
            question(&["a", "b", "c"], false, false),
            question(&[], false, true),
        ]);
        let steps = answer_keys(
            CLIAgent::Codex,
            &p,
            &PromptAnswer::Questions(vec![
                QuestionAnswer::Pick(2),
                QuestionAnswer::Other("hi".into()),
            ]),
            true,
        )
        .unwrap();
        assert_eq!(
            bytes(&steps),
            vec![
                b"\x1b[B".as_slice(),
                b"\x1b[B".as_slice(),
                b"\r".as_slice(),
                b"\x1b[200~hi\x1b[201~".as_slice(),
                b"\r".as_slice(),
            ]
        );
    }

    #[test]
    fn permissions_are_allowed_once_or_denied_and_never_more() {
        let p = permission();
        let keys = |agent, a| bytes(&answer_keys(agent, &p, &a, true).unwrap()).concat();
        assert_eq!(keys(CLIAgent::Claude, PromptAnswer::AllowOnce), b"1");
        assert_eq!(keys(CLIAgent::Codex, PromptAnswer::AllowOnce), b"y");
        assert_eq!(keys(CLIAgent::Claude, PromptAnswer::Deny), b"\x1b");
        assert_eq!(keys(CLIAgent::Codex, PromptAnswer::Deny), b"\x1b");
    }

    #[test]
    fn answers_that_do_not_fit_are_refused() {
        let single = asking(vec![question(&["a", "b"], false, false)]);
        let multi = asking(vec![question(&["a", "b"], true, true)]);
        let refused = |agent, p: &AgentPrompt, a| answer_keys(agent, p, &a, true).unwrap_err();
        let qs = PromptAnswer::Questions;
        use QuestionAnswer::*;
        assert_eq!(
            refused(CLIAgent::Claude, &single, qs(vec![Pick(2)])),
            AnswerRefused::Mismatch
        );
        assert_eq!(
            refused(CLIAgent::Claude, &single, qs(vec![Other("x".into())])),
            AnswerRefused::Mismatch
        );
        assert_eq!(
            refused(CLIAgent::Claude, &single, qs(vec![])),
            AnswerRefused::Mismatch
        );
        assert_eq!(
            refused(CLIAgent::Claude, &multi, qs(vec![Toggle(vec![])])),
            AnswerRefused::Mismatch
        );
        assert_eq!(
            refused(CLIAgent::Claude, &single, PromptAnswer::AllowOnce),
            AnswerRefused::Mismatch
        );
        assert_eq!(
            refused(CLIAgent::Codex, &multi, qs(vec![Toggle(vec![0])])),
            AnswerRefused::Unsupported
        );
        assert_eq!(
            refused(CLIAgent::Gemini, &permission(), PromptAnswer::AllowOnce),
            AnswerRefused::Unsupported
        );
    }

    #[test]
    fn an_answer_for_a_prompt_that_moved_on_is_refused() {
        let mut s = AgentSessionState {
            status: AgentStatus::Waiting,
            prompt: Some(permission()),
            ..Default::default()
        };
        assert_eq!(
            current_prompt(Some(&s), "p1").map(|p| &p.id),
            Ok(&"p1".into())
        );
        assert_eq!(current_prompt(Some(&s), "p0"), Err(AnswerRefused::Stale));
        s.status = AgentStatus::Working;
        assert_eq!(current_prompt(Some(&s), "p1"), Err(AnswerRefused::Stale));
        s.status = AgentStatus::Waiting;
        s.prompt = None;
        assert_eq!(current_prompt(Some(&s), "p1"), Err(AnswerRefused::Stale));
        assert_eq!(current_prompt(None, "p1"), Err(AnswerRefused::Stale));
    }
}

use std::path::Path;

use gpui::{
    AnyElement, Context, Entity, PromptLevel, Subscription, Window, div, prelude::*, px, rems,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, WindowExt as _};

use crate::core::cli_agent::CLIAgent;
use crate::core::config::Config;
use crate::core::shell_quote::quote_for_shell;
use crate::core::worktree::{NewWorktree, WorktreeDefaults, WorktreeRequest, setup};
use crate::ui::app::Tty7App;
use crate::ui::dialog::{self, Tone};
use crate::ui::host_ops::HostId;
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::right_panel::META_MONO;

/// How many agents the Start switch offers next to Shell.
const AGENTS_OFFERED: usize = 3;

pub(crate) struct WorktreePrompt {
    host: crate::ui::host_ops::SharedHost,
    cwd: std::path::PathBuf,
    dir: std::path::PathBuf,
    name: Entity<InputState>,
    branch: Entity<InputState>,
    base: Entity<InputState>,
    task: Entity<InputState>,
    agents: Vec<CLIAgent>,
    /// Which of `agents` the first pane starts; `None` is a plain shell.
    agent: Option<usize>,
    has_setup: bool,
    setup_hint: Option<&'static str>,
    busy: bool,
    _subs: Vec<Subscription>,
}

/// What the new worktree's first pane was asked to start.
struct Start {
    agent: Option<CLIAgent>,
    task: String,
}

/// A created worktree, and the setup script its checkout carries.
struct Created {
    wt: NewWorktree,
    setup: Option<(setup::Setup, Vec<(&'static str, String)>)>,
}

/// The line the first pane types: the setup script with its variables, then
/// the agent — chained with `&&`, so a failed setup stops there, on screen.
fn first_line(
    setup: Option<(&Path, &[(&'static str, String)])>,
    agent: Option<String>,
) -> Option<String> {
    let q = |s: &str| quote_for_shell(s, None);
    let setup = setup.map(|(script, env)| {
        let mut parts = vec!["env".to_string()];
        parts.extend(env.iter().map(|(k, v)| format!("{k}={}", q(v))));
        parts.push(q(&script.to_string_lossy()));
        parts.join(" ")
    });
    match (setup, agent) {
        (Some(setup), Some(agent)) => Some(format!("{setup} && {agent}")),
        (setup, agent) => setup.or(agent),
    }
}

/// `agent`'s launch line, opening on `task` when it takes a first message.
fn agent_line(agent: CLIAgent, task: &str, cfg: &Config) -> String {
    let mut line = agent.launch_command(&cfg.agent_launch);
    let task = task.trim();
    if let Some(args) = (!task.is_empty())
        .then(|| agent.prompt_args(task))
        .flatten()
    {
        for arg in args {
            line.push(' ');
            line.push_str(&quote_for_shell(&arg, None));
        }
    }
    line
}

impl Tty7App {
    pub(crate) fn open_worktree_prompt(
        &mut self,
        host: crate::ui::host_ops::SharedHost,
        cwd: std::path::PathBuf,
        defaults: WorktreeDefaults,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // All three open on a suggestion, which is a value to accept or type
        // over — not a prefix. `default_value` parked the caret in front of it,
        // so naming a worktree "login" over the suggested "feature" produced
        // "loginfeature".
        let prefill = crate::ui::prefill::filled_box;
        let name = prefill(defaults.name.clone(), window, cx);
        let branch = prefill(defaults.name, window, cx);
        let base = prefill(defaults.base, window, cx);
        let task = cx.new(|cx| InputState::new(window, cx));
        name.update(cx, |state, cx| state.focus(window, cx));
        let subs = [&name, &branch, &base, &task]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(
                    input,
                    window,
                    |this, _, ev: &InputEvent, window, cx| match ev {
                        InputEvent::PressEnter { .. } => this.submit_worktree_prompt(window, cx),
                        InputEvent::Change => cx.notify(),
                        _ => {}
                    },
                )
            })
            .collect();
        let mut agents = self.offered_agents(cx);
        agents.truncate(AGENTS_OFFERED);
        // A worktree is usually for a task, so it opens on the agent last
        // used; Shell is one click away.
        let agent =
            crate::ui::agent_launch::most_recent(&agents, &cx.global::<Config>().agent_frecency)
                .and_then(|a| agents.iter().position(|x| *x == a));
        self.worktree_prompt = Some(WorktreePrompt {
            host,
            cwd,
            dir: defaults.dir,
            name,
            branch,
            base,
            task,
            agents,
            agent,
            has_setup: defaults.has_setup,
            setup_hint: defaults.setup_hint,
            busy: false,
            _subs: subs,
        });
        cx.notify();
    }

    fn cancel_worktree_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.worktree_prompt.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn submit_worktree_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.worktree_prompt.as_ref() else {
            return;
        };
        if p.busy {
            return;
        }
        let name = p.name.read(cx).value().trim().to_string();
        let branch = p.branch.read(cx).value().trim().to_string();
        let base = p.base.read(cx).value().trim().to_string();
        let (name, branch) = match (name.is_empty(), branch.is_empty()) {
            (true, true) => {
                window.push_notification(t(L10nKey::WorktreePromptNeedsName), cx);
                return;
            }
            (true, false) => (branch.clone(), branch),
            (false, true) => (name.clone(), name),
            (false, false) => (name, branch),
        };
        let req = WorktreeRequest {
            name,
            branch,
            base: if base.is_empty() {
                "HEAD".to_string()
            } else {
                base
            },
        };
        let start = Start {
            agent: p.agent.map(|i| p.agents[i]),
            task: p.task.read(cx).value().to_string(),
        };
        let p = self.worktree_prompt.as_mut().expect("checked above");
        p.busy = true;
        let cwd = p.cwd.clone();
        let host = p.host.clone();
        let host_id = host.id();
        cx.notify();
        crate::ui::host_ops::HostOps::run_in(
            host,
            window,
            cx,
            move |h| {
                let wt = crate::core::worktree::create(h, &cwd, &req)?;
                // The script is a POSIX executable run through `env`; a
                // Windows host has neither.
                let setup = (h.separator() == '/')
                    .then(|| setup::find(h, &wt.path))
                    .flatten()
                    .zip(setup::env(h, &wt.main_root, &wt.path));
                Ok::<_, String>(Created { wt, setup })
            },
            move |this, result, window, cx| match result {
                Ok(created) => {
                    this.worktree_prompt = None;
                    this.start_worktree(host_id, created, start, window, cx);
                }
                Err(e) => {
                    if let Some(p) = this.worktree_prompt.as_mut() {
                        p.busy = false;
                    }
                    window.push_notification(
                        crate::ui::host_ops::failure(
                            t_fmt(L10nKey::AppNewWorktreeFailed, &[("error", &e.to_string())]),
                            &e,
                        ),
                        cx,
                    );
                    cx.notify();
                }
            },
        );
    }

    /// Open the worktree's tab, running its setup first — once the script's
    /// content has been approved for this repo.
    fn start_worktree(
        &mut self,
        host: HostId,
        created: Created,
        start: Start,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Created { wt, setup } = created;
        if !wt.carried.skipped.is_empty() {
            let paths = wt
                .carried
                .skipped
                .iter()
                .map(|(p, why)| format!("{p} ({why})"))
                .collect::<Vec<_>>()
                .join(", ");
            window.push_notification(
                t_fmt(L10nKey::AppWorktreeNotCarried, &[("paths", &paths)]),
                cx,
            );
        }
        let agent = start
            .agent
            .map(|a| agent_line(a, &start.task, cx.global::<Config>()));
        let Some((script, env)) = setup else {
            self.open_worktree_tab(wt, first_line(None, agent), window, cx);
            return;
        };
        let key = setup::trust_key(host, &wt.main_root);
        if cx.global::<Config>().worktree_setup_trust.get(&key) == Some(&script.digest) {
            let line = first_line(Some((&script.script, &env)), agent);
            self.open_worktree_tab(wt, line, window, cx);
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            t(L10nKey::AppWorktreeSetupTitle),
            Some(&t_fmt(
                L10nKey::AppWorktreeSetupDetail,
                &[("path", &script.script.display().to_string())],
            )),
            &crate::ui::confirm_answers(
                t(L10nKey::AppWorktreeSetupRun),
                t(L10nKey::AppWorktreeSetupSkip),
            ),
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let run = matches!(answer.await, Ok(0));
            let _ = this.update_in(cx, |this, window, cx| {
                if run {
                    this.update_config(cx, |cfg| {
                        cfg.worktree_setup_trust.insert(key, script.digest.clone());
                    });
                }
                let setup = run.then_some((script.script.as_path(), env.as_slice()));
                this.open_worktree_tab(wt, first_line(setup, agent), window, cx);
            });
        })
        .detach();
    }

    pub(crate) fn render_worktree_prompt_overlay(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let p = self.worktree_prompt.as_ref()?;
        let muted = cx.theme().muted_foreground;
        let name_now = p.name.read(cx).value().trim().to_string();
        let branch_now = p.branch.read(cx).value().trim().to_string();
        // Submitting falls back from one field to the other, so either alone
        // is enough; only both blank has nothing to name a worktree after.
        // Offering Create there is offering a click that can only fail.
        let nothing_to_name = name_now.is_empty() && branch_now.is_empty();
        // Preview what submitting would actually make, which is the same
        // fallback: with only a branch typed, the worktree takes its name, and
        // showing "…" there described a path that would never be created.
        let effective = match name_now.is_empty() {
            true => branch_now.as_str(),
            false => name_now.as_str(),
        };
        let preview = p
            .dir
            .join(if effective.is_empty() {
                "…"
            } else {
                effective
            })
            .display()
            .to_string();
        let mono = cx.theme().mono_font_family.clone();
        let meta = move |text: String| {
            div()
                .truncate()
                .text_size(rems(META_MONO))
                .font_family(mono.clone())
                .text_color(muted)
                .child(text)
        };
        let setup_note = match (p.has_setup, p.setup_hint) {
            (true, _) => Some(t(L10nKey::WorktreePromptSetup).to_string()),
            (false, Some(command)) => Some(t_fmt(
                L10nKey::WorktreePromptSetupHint,
                &[("command", command)],
            )),
            (false, None) => None,
        };
        // Only an agent that can open on a first message gets the Task box;
        // for the rest it would be a field that does nothing.
        let takes_task = p
            .agent
            .is_some_and(|i| p.agents[i].prompt_args("").is_some());
        let start_labels: Vec<&str> = std::iter::once(t(L10nKey::WorktreePromptShell))
            .chain(p.agents.iter().map(|a| a.display_name()))
            .collect();
        let start = (!p.agents.is_empty()).then(|| {
            dialog::label(t(L10nKey::WorktreePromptAgent), cx).child(self.segmented(
                "worktree-start",
                &start_labels,
                p.agent.map_or(0, |i| i + 1),
                cx,
                |this, ix, _window, cx| {
                    if let Some(p) = this.worktree_prompt.as_mut() {
                        p.agent = ix.checked_sub(1);
                        cx.notify();
                    }
                },
            ))
        });

        let rungs = dialog::popover_rungs(cx);
        let card = dialog::card(440., cx)
            .child(dialog::header(t(L10nKey::WorktreePromptTitle), cx))
            .child(
                dialog::body()
                    // The path preview hangs off the Name field it follows,
                    // closer to it than the next field is.
                    .child(
                        dialog::labelled(t(L10nKey::WorktreePromptName), Input::new(&p.name), cx)
                            .child(meta(preview)),
                    )
                    .child(dialog::labelled(
                        t(L10nKey::WorktreePromptBranch),
                        Input::new(&p.branch),
                        cx,
                    ))
                    .child(
                        dialog::labelled(t(L10nKey::WorktreePromptBase), Input::new(&p.base), cx)
                            .children(setup_note.map(meta)),
                    )
                    .children(start)
                    .when(takes_task, |body| {
                        body.child(dialog::labelled(
                            t(L10nKey::WorktreePromptTask),
                            Input::new(&p.task),
                            cx,
                        ))
                    }),
            )
            .child(
                dialog::footer(cx)
                    .child(dialog::button(
                        "worktree-cancel",
                        t(L10nKey::Cancel),
                        Tone::Secondary,
                        true,
                        rungs,
                        cx,
                        cx.listener(|this, _, window, cx| this.cancel_worktree_prompt(window, cx)),
                    ))
                    .child(dialog::button(
                        "worktree-create",
                        if p.busy {
                            t(L10nKey::WorktreePromptCreating)
                        } else {
                            t(L10nKey::WorktreePromptCreate)
                        },
                        Tone::Primary,
                        !(p.busy || nothing_to_name),
                        rungs,
                        cx,
                        cx.listener(|this, _, window, cx| this.submit_worktree_prompt(window, cx)),
                    )),
            );

        Some(
            div()
                .absolute()
                .inset_0()
                // The backdrop already swallowed every click in the window; it
                // just did not look like it did. A scrim says the app is
                // waiting, and clicking it backs out — the same gesture the
                // palette and the switcher already answer to.
                .bg(crate::ui::presets::scrim_fill(cx))
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, window, cx| {
                    if ev.keystroke.key == "escape" {
                        this.cancel_worktree_prompt(window, cx);
                    }
                }))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                        this.cancel_worktree_prompt(window, cx)
                    }),
                )
                .flex()
                .flex_col()
                .items_center()
                .justify_start()
                .pt(px(crate::ui::switcher::CARD_TOP))
                .child(card)
                .into_any_element(),
        )
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn first_line_chains_setup_before_the_agent_and_quotes_values() {
        let env = vec![
            ("TTY7_ROOT_PATH", "/r/my repo".to_string()),
            ("TTY7_PORT", "20010".to_string()),
        ];
        let script = Path::new("/r/my repo/.tty7/worktrees/w/.tty7/setup");
        assert_eq!(
            first_line(Some((script, &env)), Some("claude 'fix it'".into())).unwrap(),
            "env TTY7_ROOT_PATH='/r/my repo' TTY7_PORT=20010 \
             '/r/my repo/.tty7/worktrees/w/.tty7/setup' && claude 'fix it'"
        );
        assert_eq!(first_line(None, Some("codex".into())).unwrap(), "codex");
        assert_eq!(first_line(None, None), None);
    }

    #[test]
    fn agent_line_passes_the_task_only_to_agents_that_take_one() {
        let cfg = Config::default();
        assert_eq!(
            agent_line(CLIAgent::Claude, " it's done ", &cfg),
            r"claude 'it'\''s done'"
        );
        assert_eq!(agent_line(CLIAgent::Gemini, "go", &cfg), "gemini -i go");
        assert_eq!(agent_line(CLIAgent::Claude, "  ", &cfg), "claude");
        assert_eq!(agent_line(CLIAgent::Aider, "go", &cfg), "aider");
    }
}

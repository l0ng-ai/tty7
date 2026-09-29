use gpui::{
    AnyElement, Context, Div, Entity, PromptLevel, Subscription, Window, div, prelude::*, px, rems,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, WindowExt as _};

use crate::core::cli_agent::CLIAgent;
use crate::core::config::Config;
use crate::core::worktree::{NewWorktree, WorktreeDefaults, WorktreeRequest, setup};
use crate::ui::app::Tty7App;
use crate::ui::dialog::{self, Tone};
use crate::ui::host_ops::HostId;
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::right_panel::META_MONO;

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
    /// The Start dropdown is open.
    agent_menu: bool,
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
pub(crate) struct Created {
    pub wt: NewWorktree,
    setup: Option<(setup::Setup, Vec<(&'static str, String)>)>,
}

impl Created {
    /// Reads `wt`'s setup script, on the thread that made it. The script is a
    /// POSIX executable run through `env`; a Windows host has neither.
    pub(crate) fn read(h: &dyn crate::ui::host_ops::Host, wt: NewWorktree) -> Created {
        let setup = (h.separator() == '/')
            .then(|| setup::find(h, &wt.path))
            .flatten()
            .zip(setup::env(h, &wt.main_root, &wt.path));
        Created { wt, setup }
    }
}

/// What to do with a worktree's first line once it is settled.
pub(crate) type OpenWith =
    Box<dyn FnOnce(&mut Tty7App, NewWorktree, Option<String>, &mut Window, &mut Context<Tty7App>)>;

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
        let agents = self.offered_agents(cx);
        // A worktree is usually for a task, so it opens on the agent last
        // used; Shell is one pick away.
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
            agent_menu: false,
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
                Ok::<_, String>(Created::read(h, wt))
            },
            move |this, result, window, cx| match result {
                Ok(created) => {
                    this.worktree_prompt = None;
                    let agent = start.agent.map(|a| {
                        setup::agent_line(a, &start.task, &cx.global::<Config>().agent_launch)
                    });
                    this.start_worktree(
                        host_id,
                        created,
                        agent,
                        Box::new(|this, wt, line, window, cx| {
                            this.open_worktree_tab(wt, line, window, cx)
                        }),
                        window,
                        cx,
                    );
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

    /// Settle a new worktree's first line — its setup, once the script's
    /// content has been approved for this repo, then `agent` — and hand it to
    /// `open`. The New Worktree dialog opens a tab with it; the board opens
    /// a task's run.
    pub(crate) fn start_worktree(
        &mut self,
        host: HostId,
        created: Created,
        agent: Option<String>,
        open: OpenWith,
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
        let Some((script, env)) = setup else {
            let line = setup::launch_line(&wt.path, None, agent);
            open(self, wt, line, window, cx);
            return;
        };
        let key = setup::trust_key(host, &wt.main_root);
        if cx.global::<Config>().worktree_setup_trust.get(&key) == Some(&script.digest) {
            let line = setup::launch_line(&wt.path, Some((&script.script, &env)), agent);
            open(self, wt, line, window, cx);
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
                let line = setup::launch_line(&wt.path, setup, agent);
                open(this, wt, line, window, cx);
            });
        })
        .detach();
    }

    fn close_worktree_start_menu(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = self.worktree_prompt.as_mut().filter(|p| p.agent_menu) {
            p.agent_menu = false;
            cx.notify();
        }
    }

    /// The Start dropdown: Shell, then every agent this machine offers.
    fn render_worktree_start(&self, p: &WorktreePrompt, cx: &mut Context<Self>) -> Div {
        use crate::ui::settings::kit;
        let tk = kit::Tk::of(cx);
        let current = p.agent.map_or(t(L10nKey::WorktreePromptShell), |i| {
            p.agents[i].display_name()
        });
        let trigger = dialog::select_well("worktree-start", current, cx)
            // Not a click on the form, which closes the menu this reopens.
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(p) = this.worktree_prompt.as_mut() {
                    p.agent_menu = !p.agent_menu;
                    cx.notify();
                }
            }));
        let menu = p.agent_menu.then(|| {
            let choices = std::iter::once((None, t(L10nKey::WorktreePromptShell))).chain(
                p.agents
                    .iter()
                    .enumerate()
                    .map(|(i, a)| (Some(i), a.display_name())),
            );
            let mut panel = kit::menu_panel(&tk).min_w(px(220.));
            for (row, (choice, label)) in choices.enumerate() {
                panel = panel.child(
                    kit::menu_row(
                        gpui::ElementId::NamedInteger("worktree-start-item".into(), row as u64),
                        false,
                        &tk,
                    )
                    .child(kit::check_mark(choice == p.agent, &tk))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(p) = this.worktree_prompt.as_mut() {
                            p.agent = choice;
                            p.agent_menu = false;
                            cx.notify();
                        }
                    })),
                );
            }
            // `popover_below` measures from a 26px settings trigger.
            kit::popover_below(false, dialog::FIELD_H - 26. + 4., panel)
        });
        dialog::labelled_control(
            t(L10nKey::WorktreePromptAgent),
            div().relative().child(trigger).children(menu),
            cx,
        )
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
        let start = (!p.agents.is_empty()).then(|| self.render_worktree_start(p, cx));

        let rungs = dialog::popover_rungs(cx);
        let card = dialog::card(440., cx)
            .child(dialog::header(t(L10nKey::WorktreePromptTitle), cx))
            .child(
                dialog::body()
                    // A click anywhere else in the form closes the Start menu.
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                            this.close_worktree_start_menu(cx)
                        }),
                    )
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
                    if ev.keystroke.key != "escape" {
                        return;
                    }
                    // Escape backs out one level: an open menu first.
                    if this.worktree_prompt.as_ref().is_some_and(|p| p.agent_menu) {
                        this.close_worktree_start_menu(cx);
                    } else {
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

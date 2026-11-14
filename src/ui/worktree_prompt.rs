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
use crate::ui::github::issue_agent::{
    Invalid, IssueLink, IssueText, PrOffer, agent_prompt, branch_name, pr_base, validate,
};
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
    /// Opened from an issue in the GitHub tab: the Name and Task fields give
    /// way to the issue, and only agents that take a first message are
    /// offered.
    issue: Option<IssueStart>,
    /// "Open a draft PR when the agent finishes".
    draft_pr: bool,
    _subs: Vec<Subscription>,
}

/// The issue a prompt was opened on, and where its pull request would go.
pub(crate) struct IssueStart {
    pub(crate) text: IssueText,
    pub(crate) slug: tty7_core::core::github::RepoSlug,
    /// Every git remote's name, to read `origin/main` as `main`.
    pub(crate) remotes: Vec<String>,
    pub(crate) push_remote: String,
    pub(crate) head_owner: String,
}

/// What the new worktree's first pane was asked to start.
struct Start {
    agent: Option<CLIAgent>,
    task: String,
    /// The tab's issue, still missing what only creating the worktree knows.
    issue: Option<IssueLink>,
}

fn invalid_message(why: Invalid) -> L10nKey {
    match why {
        Invalid::NoBranch => L10nKey::IssueAgentNeedsBranch,
        Invalid::BadBranch => L10nKey::IssueAgentBadBranch,
        Invalid::NoAgent => L10nKey::IssueAgentNoAgent,
    }
}

/// A created worktree, and the setup script its checkout carries.
struct Created {
    wt: NewWorktree,
    setup: Option<(setup::Setup, Vec<(&'static str, String)>)>,
    /// The commit it starts at, for an issue's tab to count commits from.
    base_sha: Option<String>,
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
        self.open_worktree_prompt_for(host, cwd, defaults, None, window, cx);
    }

    /// The same dialog, opened on a GitHub issue.
    pub(crate) fn open_issue_agent_prompt(
        &mut self,
        host: crate::ui::host_ops::SharedHost,
        cwd: std::path::PathBuf,
        defaults: WorktreeDefaults,
        issue: IssueStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_worktree_prompt_for(host, cwd, defaults, Some(issue), window, cx);
    }

    fn open_worktree_prompt_for(
        &mut self,
        host: crate::ui::host_ops::SharedHost,
        cwd: std::path::PathBuf,
        defaults: WorktreeDefaults,
        issue: Option<IssueStart>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // All three open on a suggestion, which is a value to accept or type
        // over — not a prefix. `default_value` parked the caret in front of it,
        // so naming a worktree "login" over the suggested "feature" produced
        // "loginfeature".
        let prefill = crate::ui::prefill::filled_box;
        let suggested = match &issue {
            Some(issue) => branch_name(issue.text.number, &issue.text.title),
            None => defaults.name.clone(),
        };
        let name = prefill(suggested.clone(), window, cx);
        let branch = prefill(suggested, window, cx);
        let base = prefill(defaults.base, window, cx);
        let task = cx.new(|cx| InputState::new(window, cx));
        // An issue's dialog has no Name field: the branch names the worktree.
        let first = if issue.is_some() { &branch } else { &name };
        first.update(cx, |state, cx| state.focus(window, cx));
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
        // The issue reaches the agent as its first message, so an agent that
        // cannot open on one would start knowing nothing about it.
        if issue.is_some() {
            agents.retain(|a| a.prompt_args("").is_some());
        }
        // A worktree is usually for a task, so it opens on the agent last
        // used; Shell is one pick away.
        let agent =
            crate::ui::agent_launch::most_recent(&agents, &cx.global::<Config>().agent_frecency)
                .and_then(|a| agents.iter().position(|x| *x == a))
                .or_else(|| (issue.is_some() && !agents.is_empty()).then_some(0));
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
            issue,
            draft_pr: false,
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
        let (name, branch) = match &p.issue {
            Some(_) => {
                if let Err(why) = validate(&branch, p.agent.is_some()) {
                    window.push_notification(t(invalid_message(why)), cx);
                    return;
                }
                // A branch may hold a `/`; the directory it names may not.
                (branch.replace('/', "-"), branch)
            }
            None => match (name.is_empty(), branch.is_empty()) {
                (true, true) => {
                    window.push_notification(t(L10nKey::WorktreePromptNeedsName), cx);
                    return;
                }
                (true, false) => (branch.clone(), branch),
                (false, true) => (name.clone(), name),
                (false, false) => (name, branch),
            },
        };
        let start = Start {
            agent: p.agent.map(|i| p.agents[i]),
            task: match &p.issue {
                Some(issue) => agent_prompt(&issue.text),
                None => p.task.read(cx).value().to_string(),
            },
            issue: p.issue.as_ref().map(|issue| IssueLink {
                slug: issue.slug.clone(),
                number: issue.text.number,
                title: issue.text.title.clone(),
                branch: branch.clone(),
                worktree: Default::default(),
                base_sha: String::new(),
                pr_base: pr_base(&base, &issue.remotes),
                push_remote: issue.push_remote.clone(),
                head_owner: issue.head_owner.clone(),
                offer: match p.draft_pr {
                    true => PrOffer::Watching { checked: None },
                    false => PrOffer::Off,
                },
            }),
        };
        let want_base_sha = start.issue.is_some();
        let req = WorktreeRequest {
            name,
            branch,
            base: if base.is_empty() {
                "HEAD".to_string()
            } else {
                base
            },
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
                let base_sha = want_base_sha
                    .then(|| crate::core::git::git(h, &wt.path, &["rev-parse", "HEAD"]))
                    .flatten()
                    .map(|s| s.trim().to_string());
                Ok::<_, String>(Created {
                    wt,
                    setup,
                    base_sha,
                })
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
        let Created {
            wt,
            setup,
            base_sha,
        } = created;
        let issue = start.issue.map(|link| IssueLink {
            worktree: wt.path.clone(),
            branch: wt.branch.clone(),
            base_sha: base_sha.unwrap_or_default(),
            ..link
        });
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
            .map(|a| setup::agent_line(a, &start.task, &cx.global::<Config>().agent_launch));
        let Some((script, env)) = setup else {
            let line = setup::launch_line(&wt.path, None, agent);
            self.open_worktree_tab(wt, line, issue, window, cx);
            return;
        };
        let key = setup::trust_key(host, &wt.main_root);
        if cx.global::<Config>().worktree_setup_trust.get(&key) == Some(&script.digest) {
            let line = setup::launch_line(&wt.path, Some((&script.script, &env)), agent);
            self.open_worktree_tab(wt, line, issue, window, cx);
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
                this.open_worktree_tab(wt, line, issue, window, cx);
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
            // An issue is for an agent; a shell would never read it.
            let shell = p
                .issue
                .is_none()
                .then(|| (None, t(L10nKey::WorktreePromptShell)));
            let choices = shell.into_iter().chain(
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
        let nothing_to_name = match &p.issue {
            Some(_) => validate(&branch_now, p.agent.is_some()).is_err(),
            None => name_now.is_empty() && branch_now.is_empty(),
        };
        let name_now = match &p.issue {
            Some(_) => branch_now.replace('/', "-"),
            None => name_now,
        };
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
        let preview = crate::ui::path_display::readable_path(&preview).into_owned();
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
        let takes_task = p.issue.is_none()
            && p.agent
                .is_some_and(|i| p.agents[i].prompt_args("").is_some());
        let start = (!p.agents.is_empty()).then(|| self.render_worktree_start(p, cx));
        let title: gpui::SharedString = match &p.issue {
            Some(issue) => t_fmt(
                L10nKey::IssueAgentTitle,
                &[("number", &issue.text.number.to_string())],
            )
            .into(),
            None => t(L10nKey::WorktreePromptTitle).into(),
        };
        let (create, creating) = match p.issue {
            Some(_) => (L10nKey::IssueAgentStart, L10nKey::IssueAgentStarting),
            None => (
                L10nKey::WorktreePromptCreate,
                L10nKey::WorktreePromptCreating,
            ),
        };
        // What the issue's dialog shows in place of Name and Task: the issue
        // the agent will be told about. With no agent to tell, it says so.
        let issue_well = p.issue.as_ref().map(|issue| {
            dialog::well(cx)
                .child(
                    div()
                        .truncate()
                        .text_color(cx.theme().foreground)
                        .child(format!("#{} {}", issue.text.number, issue.text.title)),
                )
                .when(p.agents.is_empty(), |w| {
                    w.child(meta(t(L10nKey::IssueAgentNoAgent).to_string()))
                })
        });
        // An on/off choice is a switch here, as everywhere else in tty7.
        // Nothing is pushed by ticking it: it only lets the tab offer a
        // button once the agent is done.
        let draft_switch = p.issue.as_ref().map(|_| {
            gpui_component::h_flex()
                .gap(px(10.))
                .items_center()
                .child(
                    crate::ui::settings::kit::switch("issue-agent-draft-pr")
                        .small()
                        .checked(p.draft_pr)
                        .label(t(L10nKey::IssueAgentDraftPr))
                        .on_click(cx.listener(|this, on: &bool, _, cx| {
                            if let Some(p) = this.worktree_prompt.as_mut() {
                                p.draft_pr = *on;
                                cx.notify();
                            }
                        })),
                )
                .child(div().child(t(L10nKey::IssueAgentDraftPr)))
        });

        let rungs = dialog::popover_rungs(cx);
        let card = dialog::card("worktree-prompt-card", title.clone(), 440., cx)
            .child(dialog::header(title, cx))
            .child(
                dialog::body()
                    // A click anywhere else in the form closes the Start menu.
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                            this.close_worktree_start_menu(cx)
                        }),
                    )
                    .children(issue_well)
                    // The path preview hangs off the Name field it follows,
                    // closer to it than the next field is.
                    .when(p.issue.is_none(), |body| {
                        body.child(
                            dialog::labelled(
                                t(L10nKey::WorktreePromptName),
                                Input::new(&p.name),
                                cx,
                            )
                            .child(meta(preview.clone())),
                        )
                    })
                    .child(
                        dialog::labelled(
                            t(L10nKey::WorktreePromptBranch),
                            Input::new(&p.branch),
                            cx,
                        )
                        .when(p.issue.is_some(), |field| field.child(meta(preview))),
                    )
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
                    })
                    .children(draft_switch),
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
                        if p.busy { t(creating) } else { t(create) },
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

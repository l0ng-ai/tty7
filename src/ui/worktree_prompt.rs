use gpui::{AnyElement, Context, Entity, Subscription, Window, div, prelude::*, px, rems};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, WindowExt as _};

use crate::core::worktree::{WorktreeDefaults, WorktreeRequest};
use crate::ui::app::Tty7App;
use crate::ui::dialog::{self, Tone};
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::right_panel::META_MONO;

pub(crate) struct WorktreePrompt {
    host: crate::ui::host_ops::SharedHost,
    cwd: std::path::PathBuf,
    dir: std::path::PathBuf,
    name: Entity<InputState>,
    branch: Entity<InputState>,
    base: Entity<InputState>,
    busy: bool,
    _subs: Vec<Subscription>,
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
        name.update(cx, |state, cx| state.focus(window, cx));
        let subs = [&name, &branch, &base]
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
        self.worktree_prompt = Some(WorktreePrompt {
            host,
            cwd,
            dir: defaults.dir,
            name,
            branch,
            base,
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
        let p = self.worktree_prompt.as_mut().expect("checked above");
        p.busy = true;
        let cwd = p.cwd.clone();
        let host = p.host.clone();
        cx.notify();
        crate::ui::host_ops::HostOps::run_in(
            host,
            window,
            cx,
            move |h| crate::core::worktree::create(h, &cwd, &req),
            move |this, result, window, cx| match result {
                Ok(wt) => {
                    this.worktree_prompt = None;
                    this.open_worktree_tab(wt, window, cx);
                }
                Err(e) => {
                    if let Some(p) = this.worktree_prompt.as_mut() {
                        p.busy = false;
                    }
                    window.push_notification(
                        t_fmt(L10nKey::AppNewWorktreeFailed, &[("error", &e.to_string())]),
                        cx,
                    );
                    cx.notify();
                }
            },
        );
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

        let rungs = dialog::popover_rungs(cx);
        let card = dialog::card(440., cx)
            .child(dialog::header(t(L10nKey::WorktreePromptTitle), cx))
            .child(
                dialog::body()
                    // The path preview hangs off the Name field it follows,
                    // closer to it than the next field is.
                    .child(
                        dialog::labelled(t(L10nKey::WorktreePromptName), Input::new(&p.name), cx)
                            .child(
                                div()
                                    .truncate()
                                    .text_size(rems(META_MONO))
                                    .font_family("monospace")
                                    .text_color(muted)
                                    .child(preview),
                            ),
                    )
                    .child(dialog::labelled(
                        t(L10nKey::WorktreePromptBranch),
                        Input::new(&p.branch),
                        cx,
                    ))
                    .child(dialog::labelled(
                        t(L10nKey::WorktreePromptBase),
                        Input::new(&p.base),
                        cx,
                    )),
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

//! Review Repo Approval Policy: show the active tab's `.tty7/policy` and
//! approve, or revoke, exactly that content (see `core::approval_policy`).
//!
//! The desktop is where approving is safest: a dialog an agent's keystrokes
//! cannot reach. The bytes shown are the bytes whose digest is approved — the
//! file is read once, and an edit after this moment is a different digest.

use gpui::{Context, PromptLevel, Window};
use gpui_component::WindowExt as _;

use crate::core::approval_policy::{self, Approvals, Policy, PolicyFile, PolicyState};
use crate::ui::app::Tty7App;
use crate::ui::i18n::{L10nKey, t, t_fmt};

/// The rules as the dialog lists them.
fn rules_text(policy: &Policy) -> String {
    let list = |rules: Vec<&str>| {
        if rules.is_empty() {
            "  (none)".to_string()
        } else {
            rules
                .iter()
                .map(|r| format!("  {r}"))
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    format!(
        "allow bash:\n{}\nask bash:\n{}",
        list(
            policy
                .allow_bash
                .iter()
                .map(|r| r.pattern.as_str())
                .collect()
        ),
        list(policy.ask_bash.iter().map(|r| r.pattern.as_str()).collect()),
    )
}

impl Tty7App {
    pub(crate) fn review_approval_policy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A remote pane's policy is answered by the hook on that machine,
        // against that machine's approvals; this window cannot file one there.
        let Some(cwd) = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.pane.focused_or_first(window, cx))
            .and_then(|leaf| leaf.read(cx).local_cwd())
        else {
            window.push_notification(t(L10nKey::AppPolicyNotLocal), cx);
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    let file = approval_policy::find(&cwd);
                    let state = file.as_ref().map(|f| f.state(&Approvals::load()));
                    (file, state)
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| match found {
                (Some(file), Some(state)) => this.show_policy(file, state, window, cx),
                _ => window.push_notification(t(L10nKey::AppPolicyNone), cx),
            });
        })
        .detach();
    }

    fn show_policy(
        &mut self,
        file: PolicyFile,
        state: PolicyState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = file.path.display().to_string();
        let digest = file.digest.get(..12).unwrap_or(&file.digest).to_string();
        let policy = match &state {
            PolicyState::Invalid(e) => {
                window.push_notification(
                    t_fmt(
                        L10nKey::AppPolicyInvalid,
                        &[("path", &path), ("error", &e.to_string())],
                    ),
                    cx,
                );
                return;
            }
            PolicyState::Active(policy) => policy.clone(),
            PolicyState::Unapproved | PolicyState::Changed => match Policy::parse(&file.text) {
                Ok(policy) => policy,
                Err(_) => return,
            },
        };
        let active = matches!(state, PolicyState::Active(_));
        let detail = t_fmt(
            if active {
                L10nKey::AppPolicyActiveDetail
            } else {
                L10nKey::AppPolicyReviewDetail
            },
            &[
                ("path", &path),
                ("sha", &digest),
                ("rules", &rules_text(&policy)),
            ],
        );
        let (title, act, keep) = if active {
            (
                t(L10nKey::AppPolicyActiveTitle),
                t(L10nKey::AppPolicyRevoke),
                t(L10nKey::AppPolicyKeep),
            )
        } else {
            (
                t(L10nKey::AppPolicyReviewTitle),
                t(L10nKey::AppPolicyApprove),
                t(L10nKey::AppPolicyNotNow),
            )
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            title,
            Some(&detail),
            &crate::ui::confirm_answers(act, keep),
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if !matches!(answer.await, Ok(0)) {
                return;
            }
            let saved = cx
                .background_executor()
                .spawn(async move {
                    if active {
                        approval_policy::revoke(&file.root).map(|_| ())
                    } else {
                        approval_policy::approve(&file)
                    }
                })
                .await;
            let _ = this.update_in(cx, |_, window, cx| {
                let note = match saved {
                    Ok(()) if active => t(L10nKey::AppPolicyRevoked).to_string(),
                    Ok(()) => t(L10nKey::AppPolicyApproved).to_string(),
                    Err(e) => t_fmt(L10nKey::AppPolicySaveFailed, &[("error", &e.to_string())]),
                };
                window.push_notification(note, cx);
            });
        })
        .detach();
    }
}

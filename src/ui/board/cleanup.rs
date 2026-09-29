//! Clean up: where a finished task leaves the board for good.
//!
//! Done is still a card you can take back; cleaning up is not. It closes the
//! tabs a task's runs opened in their worktrees, deletes those worktrees and
//! their branches, and takes the card off the board. Nothing is lost without
//! a word: when a worktree holds uncommitted changes, or its branch commits
//! that never reached the main checkout, the user is told which and asked
//! first. Otherwise it just happens — there is nothing to ask about.

use std::path::PathBuf;

use gpui::{Context, PromptLevel, Window};
use gpui_component::WindowExt as _;
use tty7_core::core::task::TaskId;
use tty7_core::core::worktree::Discard;

use crate::ui::app::Tty7App;
use crate::ui::i18n::{L10nKey, t, t_fmt};

impl Tty7App {
    /// Cleans up `ids`: checks what deleting their worktrees would lose, asks
    /// if anything, then closes, deletes and removes.
    pub(super) fn clean_up(
        &mut self,
        ids: Vec<TaskId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut worktrees: Vec<PathBuf> = ids
            .iter()
            .filter_map(|id| self.task(*id))
            .flat_map(|task| task.runs.into_iter().filter_map(|r| r.worktree))
            .map(PathBuf::from)
            .collect();
        worktrees.sort();
        worktrees.dedup();
        if worktrees.is_empty() {
            self.finish_clean_up(ids, Vec::new(), window, cx);
            return;
        }
        let host_id = self.spawn_host(cx);
        let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, host_id) else {
            window.push_notification(t(L10nKey::BoardUnavailable), cx);
            return;
        };
        crate::ui::host_ops::HostOps::run_in(
            host,
            window,
            cx,
            move |h| {
                worktrees
                    .iter()
                    .filter_map(|p| tty7_core::core::worktree::discard_check(h, p))
                    .collect::<Vec<Discard>>()
            },
            move |this, checks, window, cx| {
                let losing: Vec<String> = checks
                    .iter()
                    .filter(|d| d.loses_work())
                    .map(|d| {
                        let mut why = Vec::new();
                        if d.worktree.dirty {
                            why.push(t(L10nKey::BoardCleanupDirty).to_string());
                        }
                        if d.unmerged {
                            why.push(t_fmt(L10nKey::BoardCleanupUnmerged, &[("base", &d.base)]));
                        }
                        format!("{} — {}", d.worktree.branch, why.join(", "))
                    })
                    .collect();
                if losing.is_empty() {
                    this.finish_clean_up(ids, checks, window, cx);
                    return;
                }
                let title = t_fmt(L10nKey::BoardCleanupTitle, &[("n", &ids.len().to_string())]);
                let detail = t_fmt(L10nKey::BoardCleanupDetail, &[("list", &losing.join("\n"))]);
                let answer = window.prompt(
                    PromptLevel::Warning,
                    &title,
                    Some(&detail),
                    &crate::ui::confirm_answers(t(L10nKey::BoardCleanupDelete), t(L10nKey::Cancel)),
                    cx,
                );
                cx.spawn_in(window, async move |this, cx| {
                    if matches!(answer.await, Ok(0)) {
                        let _ = this.update_in(cx, |this, window, cx| {
                            this.finish_clean_up(ids, checks, window, cx)
                        });
                    }
                })
                .detach();
            },
        );
    }

    /// The part after any question: close the runs' tabs, delete their
    /// worktrees, take the cards off the board.
    fn finish_clean_up(
        &mut self,
        ids: Vec<TaskId>,
        checks: Vec<Discard>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A tab working inside a worktree holds it open; it closes first.
        // Tabs of runs that worked in place are the user's own and stay.
        let tabs: Vec<_> = ids
            .iter()
            .filter_map(|id| self.task(*id))
            .flat_map(|task| task.runs)
            .filter(|r| r.worktree.is_some())
            .filter_map(|r| r.tab)
            .collect();
        for tab in tabs {
            if let Some(index) = self.tabs.iter().position(|t| t.tree_id.get() == tab) {
                self.close_tab_now(index, false, window, cx);
            }
        }
        let mut removed = 0;
        for id in &ids {
            if self.delete_task(*id, window, cx) {
                removed += 1;
            }
        }
        if !checks.is_empty() {
            let host_id = self.spawn_host(cx);
            if let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, host_id) {
                crate::ui::host_ops::HostOps::run_in(
                    host,
                    window,
                    cx,
                    move |h| {
                        checks
                            .iter()
                            .filter_map(|d| {
                                tty7_core::core::worktree::discard(h, d)
                                    .err()
                                    .map(|e| (d.worktree.path.display().to_string(), e))
                            })
                            .collect::<Vec<_>>()
                    },
                    |_this, failed, window, cx| {
                        for (path, error) in failed {
                            window.push_notification(
                                t_fmt(
                                    L10nKey::BoardCleanupFailed,
                                    &[("path", &path), ("error", &error)],
                                ),
                                cx,
                            );
                        }
                    },
                );
            }
        }
        if removed > 0 {
            self.flash(
                t_fmt(L10nKey::BoardToastCleaned, &[("n", &removed.to_string())]),
                None,
                cx,
            );
        }
    }
}

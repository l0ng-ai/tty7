//! Clean up: where a finished task leaves the board for good.
//!
//! Done is still a card you can take back; cleaning up is not. It takes the
//! cards off the board, closes every tab working inside their runs'
//! worktrees, and removes those worktrees.
//!
//! The worktrees themselves lose little: removal goes through
//! [`worktree::remove`], which files the checkout — uncommitted work
//! included, files git ignores not — under `refs/tty7/trash/<name>` before
//! deleting it, and keeps a branch with commits nothing else has. The note
//! that follows names both. What it cannot keep is a process still running
//! in one of those tabs, so that — like a tab the user did not open for the
//! task, and like cleaning up a whole column — is asked about first.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{Context, Entity, PromptLevel, Window};
use gpui_component::WindowExt as _;
use tty7_core::core::machine::TabId;
use tty7_core::core::task::{Task, TaskId};
use tty7_core::core::worktree;

use crate::terminal::view::{PaneBusy, TerminalView};
use crate::ui::app::{CloseReason, Tty7App};
use crate::ui::i18n::{L10nKey, t, t_fmt};

/// How long the closed tabs' panes get to exit before their worktrees are
/// removed, so the snapshot has the last thing an agent wrote.
const EXIT_WAIT: Duration = Duration::from_secs(3);
const EXIT_POLL: Duration = Duration::from_millis(100);

/// What cleaning up some tasks would touch.
struct Plan {
    worktrees: Vec<PathBuf>,
    /// Every tab to close: the runs' own, then any other with a pane in one
    /// of the worktrees, which is about to be deleted under it.
    tabs: Vec<TabId>,
    /// How many of [`Self::tabs`] are not the runs' own.
    others: usize,
    /// What is still going on in those tabs, in words.
    running: Vec<String>,
}

impl Tty7App {
    /// Cleans up `ids`, asking first only when that would end something
    /// still running or close a tab the task did not open.
    pub(super) fn clean_up(
        &mut self,
        ids: Vec<TaskId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ask_clean_up(ids, false, window, cx);
    }

    /// Cleans up a column's worth of tasks — some of them, maybe, folded out
    /// of sight — after saying how many.
    pub(super) fn confirm_clean_up_all(
        &mut self,
        ids: Vec<TaskId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ask_clean_up(ids, true, window, cx);
    }

    fn ask_clean_up(
        &mut self,
        ids: Vec<TaskId>,
        always: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tasks: Vec<Task> = ids.iter().filter_map(|id| self.task(*id)).collect();
        if tasks.is_empty() {
            return;
        }
        let plan = self.clean_up_plan(&tasks, cx);
        if !always && plan.running.is_empty() && plan.others == 0 {
            self.clean_up_now(ids, window, cx);
            return;
        }
        let title = match tasks.as_slice() {
            [task] => t_fmt(L10nKey::BoardCleanUpOneTitle, &[("title", &task.title)]),
            _ => t_fmt(
                L10nKey::BoardCleanUpTitle,
                &[("n", &tasks.len().to_string())],
            ),
        };
        let mut body = t(L10nKey::BoardCleanUpBody).to_string();
        if plan.others > 0 {
            body.push_str("\n\n");
            body.push_str(&t_fmt(
                L10nKey::BoardCleanUpOtherTabs,
                &[("n", &plan.others.to_string())],
            ));
        }
        if !plan.running.is_empty() {
            body.push_str("\n\n");
            body.push_str(&t_fmt(
                L10nKey::BoardCleanUpRunning,
                &[("what", &plan.running.join(", "))],
            ));
        }
        let level = match plan.running.is_empty() {
            true => PromptLevel::Info,
            false => PromptLevel::Warning,
        };
        let answer = window.prompt(
            level,
            &title,
            Some(&body),
            &crate::ui::confirm_answers(t(L10nKey::BoardCleanUp), t(L10nKey::Keep)),
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if matches!(answer.await, Ok(0)) {
                let _ = this.update_in(cx, |this, window, cx| this.clean_up_now(ids, window, cx));
            }
        })
        .detach();
    }

    fn clean_up_plan(&self, tasks: &[Task], cx: &gpui::App) -> Plan {
        let mut worktrees: Vec<PathBuf> = tasks
            .iter()
            .flat_map(|task| task.runs.iter().filter_map(|r| r.worktree.as_ref()))
            .map(PathBuf::from)
            .collect();
        worktrees.sort();
        worktrees.dedup();
        // Tabs of runs that worked in place are the user's own and stay.
        let mut tabs: Vec<TabId> = Vec::new();
        for tab in tasks
            .iter()
            .flat_map(|task| task.runs.iter())
            .filter(|r| r.worktree.is_some())
            .filter_map(|r| r.tab)
        {
            if !tabs.contains(&tab) {
                tabs.push(tab);
            }
        }
        let own = tabs.len();
        let host = self.spawn_host(cx);
        for tab in &self.tabs {
            let id = tab.tree_id.get();
            let inside = tab.pane.terminals().iter().any(|leaf| {
                let view = leaf.read(cx);
                view.host_id() == host
                    && view
                        .host_cwd()
                        .is_some_and(|cwd| worktrees.iter().any(|wt| cwd.starts_with(wt)))
            });
            if inside && !tabs.contains(&id) {
                tabs.push(id);
            }
        }
        let running = tabs
            .iter()
            .filter_map(|id| self.tabs.iter().find(|t| t.tree_id.get() == *id))
            .flat_map(|tab| tab.pane.terminals())
            .filter_map(|leaf| match self.leaf_close_reason(&leaf, cx)? {
                CloseReason::Busy(PaneBusy::Command(what)) => Some(what),
                CloseReason::Busy(PaneBusy::Agent(name)) => Some(name.to_string()),
                CloseReason::LiveSsh => Some(leaf.read(cx).title.clone()),
            })
            .collect();
        Plan {
            worktrees,
            others: tabs.len() - own,
            tabs,
            running,
        }
    }

    /// Takes the cards off the board, then closes their tabs, then — once
    /// the panes in them have exited — removes their worktrees. In that
    /// order: a machine that will not let a card go stops it before anything
    /// is closed.
    fn clean_up_now(&mut self, ids: Vec<TaskId>, window: &mut Window, cx: &mut Context<Self>) {
        let mut gone = Vec::new();
        for id in ids {
            let Some(task) = self.task(id) else {
                continue;
            };
            if !self.delete_task(id, window, cx) {
                break;
            }
            gone.push(task);
        }
        if gone.is_empty() {
            return;
        }
        let removed = gone.len();
        let plan = self.clean_up_plan(&gone, cx);
        let mut panes: Vec<Entity<TerminalView>> = Vec::new();
        for tab in &plan.tabs {
            if let Some(index) = self.tabs.iter().position(|t| t.tree_id.get() == *tab) {
                panes.extend(self.tabs[index].pane.terminals());
                self.close_tab_now(index, false, window, cx);
            }
        }
        let worktrees = plan.worktrees;
        let done = t_fmt(L10nKey::BoardToastCleaned, &[("n", &removed.to_string())]);
        if worktrees.is_empty() {
            self.flash(done, None, cx);
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let started = std::time::Instant::now();
            while started.elapsed() < EXIT_WAIT {
                let exited = cx
                    .update(|_, cx| {
                        panes.iter().all(|pane| {
                            pane.update(cx, |view, _| {
                                view.terminal.poll_exited();
                                view.terminal.exited || view.terminal.child_exited()
                            })
                        })
                    })
                    .unwrap_or(true);
                if exited {
                    break;
                }
                cx.background_executor().timer(EXIT_POLL).await;
            }
            drop(panes);
            let _ = this.update_in(cx, |this, window, cx| {
                this.remove_worktrees(worktrees, removed, done, window, cx)
            });
        })
        .detach();
    }

    fn remove_worktrees(
        &mut self,
        worktrees: Vec<PathBuf>,
        removed: usize,
        done: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host_id = self.spawn_host(cx);
        let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, host_id) else {
            self.flash(done, None, cx);
            return;
        };
        crate::ui::host_ops::HostOps::run_in(
            host,
            window,
            cx,
            move |h| {
                let mut kept = Vec::new();
                let mut saved = Vec::new();
                let mut failed = Vec::new();
                for path in &worktrees {
                    // Already gone — removed by hand, or by an earlier clean
                    // up — is what was asked for.
                    let Some(wt) = worktree::managed(h, path) else {
                        continue;
                    };
                    match worktree::remove(h, &wt, true) {
                        Ok(r) => {
                            if r.branch_kept {
                                kept.push(wt.branch);
                            }
                            saved.extend(r.snapshot);
                        }
                        Err(e) => failed.push((path.display().to_string(), e)),
                    }
                }
                (kept, saved, failed)
            },
            move |this, (kept, saved, failed), window, cx| {
                for (path, error) in failed {
                    window.push_notification(
                        t_fmt(
                            L10nKey::BoardCleanupFailed,
                            &[("path", &path), ("error", &error)],
                        ),
                        cx,
                    );
                }
                let mut note = match kept.is_empty() {
                    true => done,
                    false => t_fmt(
                        L10nKey::BoardToastCleanedKept,
                        &[("n", &removed.to_string()), ("branches", &kept.join(", "))],
                    ),
                };
                if !saved.is_empty() {
                    note.push_str(" · ");
                    note.push_str(&t_fmt(
                        L10nKey::BoardToastSaved,
                        &[("refs", &saved.join(", "))],
                    ));
                }
                this.flash(note, None, cx);
            },
        );
    }
}

//! Clean up: where a finished task leaves the board for good.
//!
//! Done is still a card you can take back; cleaning up is not. It closes the
//! tabs a task's runs opened in their worktrees, removes those worktrees, and
//! takes the card off the board.
//!
//! It asks nothing, because it loses nothing: removal goes through
//! [`worktree::remove`], which files the checkout — uncommitted work
//! included — under `refs/tty7/trash/<name>` before deleting it, and keeps a
//! branch with commits nothing else has. The note that follows says when a
//! branch was kept.

use std::path::PathBuf;

use gpui::{Context, Window};
use gpui_component::WindowExt as _;
use tty7_core::core::task::TaskId;
use tty7_core::core::worktree;

use crate::ui::app::Tty7App;
use crate::ui::i18n::{L10nKey, t_fmt};

impl Tty7App {
    /// Cleans up `ids`: closes their worktree tabs, removes their worktrees,
    /// takes the cards off the board.
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
        // A tab working inside a worktree holds it open, so it closes first.
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
        if removed == 0 {
            return;
        }
        let done = t_fmt(L10nKey::BoardToastCleaned, &[("n", &removed.to_string())]);
        if worktrees.is_empty() {
            self.flash(done, None, cx);
            return;
        }
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
                let mut failed = Vec::new();
                for path in &worktrees {
                    // Already gone — removed by hand, or by an earlier clean
                    // up — is what was asked for.
                    let Some(wt) = worktree::managed(h, path) else {
                        continue;
                    };
                    match worktree::remove(h, &wt, true) {
                        Ok(r) if r.branch_kept => kept.push(wt.branch),
                        Ok(_) => {}
                        Err(e) => failed.push((path.display().to_string(), e)),
                    }
                }
                (kept, failed)
            },
            move |this, (kept, failed), window, cx| {
                for (path, error) in failed {
                    window.push_notification(
                        t_fmt(
                            L10nKey::BoardCleanupFailed,
                            &[("path", &path), ("error", &error)],
                        ),
                        cx,
                    );
                }
                let note = match kept.is_empty() {
                    true => done,
                    false => t_fmt(
                        L10nKey::BoardToastCleanedKept,
                        &[("n", &removed.to_string()), ("branches", &kept.join(", "))],
                    ),
                };
                this.flash(note, None, cx);
            },
        );
    }
}

//! Where the source control actions and palette commands land.
//!
//! One `ScmIntent` match, so the four ways of asking for a verb — the action,
//! the key binding, the palette entry and the button on the row — cannot drift
//! into meaning different things.

use gpui::{Context, PromptLevel, Window};

use tty7_core::core::git::ops::{Destructive, GitOp, PullMode};

use crate::core::config::DiffViewMode;
use crate::ui::app::Tty7App;
use crate::ui::host_registry::HostRegistry;
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::scm::state::RepoKey;

/// One entry point for every source control verb.
///
/// A single enum rather than fourteen methods: the actions, the palette and
/// the row buttons all want the same behaviour, and routing them through one
/// match is what keeps the three from drifting apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ScmIntent {
    Commit,
    CommitAmend,
    /// Commit, then send it on. Two operations rather than one, so the commit
    /// still stands if the network half fails — and strictly in that order:
    /// the send rides in the commit's [`ScmFollowUp`], because a push
    /// dispatched alongside the commit resolves the branch tip whenever the
    /// pool gets to it, and pushing the *old* tip reports success while
    /// sending nothing.
    CommitAndPush,
    CommitAndSync,
    StageAll,
    UnstageAll,
    DiscardAll,
    Refresh,
    Sync,
    Push,
    Pull,
    Fetch,
    CheckoutBranch,
    CreateBranch,
}

/// What to run once an operation has landed *successfully*.
///
/// Compound verbs — commit-and-push, pull-then-push, discard-all's two halves
/// — are sequences, not bundles: the second operation only makes sense against
/// the repository the first one produced. Dispatching both into the worker
/// pool at once lets them race, so the second rides here and is started from
/// the first one's landing closure instead. A failed or cancelled first half
/// drops the follow-up.
#[derive(Clone, Debug)]
pub(crate) enum ScmFollowUp {
    /// Push the current branch, re-reading the (by then updated) status.
    Push,
    /// Pull, then push — the whole sync sequence.
    Sync,
    /// One more operation, run without a second confirmation: the prompt that
    /// approved the first half covered this one too.
    Op(GitOp),
}

impl Tty7App {
    /// Fold the history section open or shut and remember it.
    pub(crate) fn scm_toggle_graph(&mut self, cx: &mut Context<Self>) {
        let next = !self.scm.graph.expanded;
        self.scm.graph.expanded = next;
        self.update_config(cx, |cfg| cfg.scm_graph_expanded = next);
        cx.notify();
    }

    /// Flip the diff overlay between side-by-side and unified.
    ///
    /// Global rather than per-overlay, matching `diffEditor.renderSideBySide`:
    /// someone who prefers unified prefers it for every file.
    pub(crate) fn toggle_diff_view_mode(&mut self, cx: &mut Context<Self>) {
        let next = match cx.global::<crate::core::config::Config>().diff_view {
            DiffViewMode::Split => DiffViewMode::Unified,
            DiffViewMode::Unified => DiffViewMode::Split,
        };
        self.update_config(cx, |cfg| cfg.diff_view = next);
        cx.notify();
    }

    /// Run one operation against the panel's repository, asking first when it
    /// can lose work.
    ///
    /// The gate lives here rather than in `run_git_op` because
    /// [`GitOp::destructive`] is advice about what the user stands to lose,
    /// and only a window can ask them. `window.prompt` is the project's one
    /// confirmation mechanism — deleting a file in the tree already uses it —
    /// so no modal component is introduced for this.
    pub(crate) fn scm_op(
        &mut self,
        repo: RepoKey,
        op: GitOp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scm_op_then(repo, op, None, window, cx);
    }

    /// [`scm_op`], with something to run once this operation has succeeded.
    /// Cancelling the confirmation drops the follow-up along with the op.
    pub(crate) fn scm_op_then(
        &mut self,
        repo: RepoKey,
        op: GitOp,
        then: Option<ScmFollowUp>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = HostRegistry::get(cx, repo.host) else {
            return;
        };
        let Some(loss) = op.destructive() else {
            self.run_git_op(host, repo.root, op, then, window, cx);
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &confirm_question(&op, loss),
            None,
            &[t(L10nKey::Cancel), confirm_verb(&op, loss)],
            cx,
        );
        cx.spawn_in(window, async move |app, cx| {
            let Ok(1) = answer.await else { return };
            let _ = app.update_in(cx, |app, window, cx| {
                app.run_git_op(host, repo.root, op, then, window, cx)
            });
        })
        .detach();
    }

    pub(crate) fn run_scm_action(
        &mut self,
        intent: ScmIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.scm.active_repo().cloned() else {
            return;
        };
        match intent {
            ScmIntent::Refresh => {
                self.scm_invalidate(repo.host, &repo.root, cx);
                cx.notify();
            }
            ScmIntent::StageAll => self.scm_op(repo, GitOp::StageAll, window, cx),
            ScmIntent::UnstageAll => self.scm_op(repo, GitOp::UnstageAll, window, cx),
            ScmIntent::DiscardAll => self.scm_discard_all(repo, window, cx),
            ScmIntent::Commit => {
                let amend = self.scm.amend;
                self.scm_commit(repo, amend, None, window, cx);
            }
            ScmIntent::CommitAmend => self.scm_commit(repo, true, None, window, cx),
            ScmIntent::CommitAndPush => {
                let amend = self.scm.amend;
                self.scm_commit(repo, amend, Some(ScmFollowUp::Push), window, cx);
            }
            ScmIntent::CommitAndSync => {
                let amend = self.scm.amend;
                self.scm_commit(repo, amend, Some(ScmFollowUp::Sync), window, cx);
            }
            ScmIntent::Sync => self.scm_sync(repo, window, cx),
            ScmIntent::Push => self.scm_push(repo, false, window, cx),
            ScmIntent::Pull => self.scm_op(
                repo,
                GitOp::Pull {
                    mode: PullMode::FfOnly,
                },
                window,
                cx,
            ),
            ScmIntent::Fetch => self.scm_op(
                repo,
                GitOp::Fetch {
                    remote: None,
                    prune: false,
                },
                window,
                cx,
            ),
            ScmIntent::CreateBranch => self.scm_begin_create_branch(window, cx),
            // Checking out is a pick, not a verb: the switcher hangs off the
            // branch name, which is where the list of branches already is.
            ScmIntent::CheckoutBranch => {}
        }
    }

    /// Commit whatever the message box holds.
    ///
    /// The message comes from the box when it is the one on screen and from
    /// the saved draft otherwise, so the key binding and the palette entry
    /// commit the same text the user can see.
    pub(crate) fn scm_commit(
        &mut self,
        repo: RepoKey,
        amend: bool,
        then: Option<ScmFollowUp>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(status) = crate::terminal::git_data::status_of(cx, repo.host, &repo.root) else {
            return;
        };
        let message = self.scm_message(&repo, cx);
        let plan = crate::ui::scm::panel::commit_plan(&status, amend, &message);
        if !plan.enabled {
            // The panel's own button exposes this through disabled + tooltip;
            // the palette and the key binding have no tooltip to hover, so
            // the toast has to carry the reason itself — "write a message
            // first" and "nothing to commit" call for opposite actions, and
            // answering the first with the second sends the user staging
            // files they already staged (#546).
            gpui_component::WindowExt::push_notification(window, t(plan.reason).to_string(), cx);
            // The follow-up dies with the commit: "commit and push" with
            // nothing to commit must not push whatever the branch holds.
            return;
        }
        let all = crate::ui::scm::panel::commit_stages_everything(&status, amend);
        self.scm.amend = false;
        // `run_git_op` arms `scm.committing` when the commit is actually
        // dispatched — after the amend confirmation, not before it — so a
        // cancelled prompt leaves nothing armed. See `scm_commit_landed`.
        self.scm_op_then(
            repo,
            GitOp::Commit {
                message,
                amend,
                signoff: false,
                no_verify: false,
                all,
            },
            then,
            window,
            cx,
        );
    }

    /// What the commit box holds for a repository, whether or not it is the
    /// one currently on screen.
    fn scm_message(&self, repo: &RepoKey, cx: &gpui::App) -> String {
        match (&self.scm.commit_input, &self.scm.commit_repo) {
            (Some(input), Some(showing)) if showing == repo => input.read(cx).value().to_string(),
            _ => self.scm.draft(repo).to_string(),
        }
    }

    /// Park everything, including the files git does not track yet.
    ///
    /// `-u`, because a stash that silently leaves new files behind is a stash
    /// that did not do what "stash all" says.
    pub(crate) fn scm_stash_all(
        &mut self,
        repo: RepoKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let message = match self.scm_message(&repo, cx) {
            m if m.trim().is_empty() => None,
            m => Some(m),
        };
        self.scm_op(
            repo,
            GitOp::Stash {
                message,
                include_untracked: true,
            },
            window,
            cx,
        );
    }

    /// Throw away every change in the worktree: unstaged edits and untracked
    /// files alike.
    ///
    /// Two operations, because git has no single command for it —
    /// `checkout --` cannot touch a file it has never heard of, and `clean`
    /// cannot touch one it has. One confirmation and one sequence, though:
    /// the second half rides in the first one's [`ScmFollowUp`], so the user
    /// answers a single dialog and the two gits never race each other.
    ///
    /// Only *unstaged* paths go to `checkout --`: it restores from the index,
    /// so a staged edit would survive it anyway, and a staged deletion — a
    /// path in neither index nor worktree — would make git reject the whole
    /// batch as an unmatched pathspec. What is staged stays staged, which is
    /// also what the button's own group implies.
    fn scm_discard_all(&mut self, repo: RepoKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = crate::terminal::git_data::status_of(cx, repo.host, &repo.root) else {
            return;
        };
        let (first, second) = match &discard_all_ops(&status)[..] {
            [] => return,
            [one] => (one.clone(), None),
            [a, b, ..] => (a.clone(), Some(b.clone())),
        };
        let Some(host) = HostRegistry::get(cx, repo.host) else {
            return;
        };
        // Its own prompt rather than `scm_op_then`'s: that one names the file
        // when an op carries a single path, and "Discard changes to a.rs?"
        // would be the wrong question for a click that also sweeps the
        // untracked files.
        let answer = window.prompt(
            PromptLevel::Warning,
            &t(L10nKey::ScmDiscardAllConfirm).to_string(),
            None,
            &[t(L10nKey::Cancel), t(L10nKey::ScmDiscard)],
            cx,
        );
        cx.spawn_in(window, async move |app, cx| {
            let Ok(1) = answer.await else { return };
            let _ = app.update_in(cx, |app, window, cx| {
                app.run_git_op(
                    host,
                    repo.root,
                    first,
                    second.map(ScmFollowUp::Op),
                    window,
                    cx,
                )
            });
        })
        .detach();
    }

    /// Push the current branch to its upstream, or publish it if it has none.
    pub(crate) fn scm_push(
        &mut self,
        repo: RepoKey,
        force_with_lease: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(status) = crate::terminal::git_data::status_of(cx, repo.host, &repo.root) else {
            return;
        };
        let name = match crate::ui::scm::panel::pushable_branch(&status.head) {
            Ok(name) => name.to_string(),
            Err(reason) => {
                // "A swallowed click on Push looks exactly like a push that
                // finished instantly" — git_data.rs says it out loud about
                // the busy slot, and the same holds here. The tile and the
                // menu disable themselves, but the key binding, the palette
                // and the follow-up half of "Commit and Push" all land in
                // this guard, so the toast is the only place they can say
                // why nothing moved (#545).
                gpui_component::WindowExt::push_notification(window, t(reason).to_string(), cx);
                return;
            }
        };
        let (remote, branch) = match status.upstream.as_deref().and_then(split_upstream) {
            Some((remote, branch)) => (remote.to_string(), branch.to_string()),
            None => ("origin".to_string(), name),
        };
        let set_upstream = status.upstream.is_none();
        self.scm_op(
            repo,
            GitOp::Push {
                remote,
                branch,
                set_upstream,
                force_with_lease,
            },
            window,
            cx,
        );
    }

    /// Pull then push, which is what "sync" means everywhere else — and
    /// strictly in that order: the push rides in the pull's [`ScmFollowUp`],
    /// because a push racing the pull it was waiting for reads the pre-pull
    /// tip and earns a non-fast-forward rejection from the very sync that
    /// was fixing it. A failed pull stops the sequence.
    ///
    /// A branch with no upstream has nothing to pull, so sync is a publish.
    pub(crate) fn scm_sync(&mut self, repo: RepoKey, window: &mut Window, cx: &mut Context<Self>) {
        let has_upstream = crate::terminal::git_data::status_of(cx, repo.host, &repo.root)
            .is_some_and(|s| s.upstream.is_some());
        if has_upstream {
            self.scm_op_then(
                repo,
                GitOp::Pull {
                    mode: PullMode::FfOnly,
                },
                Some(ScmFollowUp::Push),
                window,
                cx,
            );
        } else {
            self.scm_push(repo, false, window, cx);
        }
    }

    /// Run the second half of a compound verb, from the first half's landing.
    pub(crate) fn scm_follow_up(
        &mut self,
        host: tty7_core::host::HostId,
        root: std::path::PathBuf,
        follow: ScmFollowUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let repo = RepoKey { host, root };
        match follow {
            ScmFollowUp::Push => self.scm_push(repo, false, window, cx),
            ScmFollowUp::Sync => self.scm_sync(repo, window, cx),
            ScmFollowUp::Op(op) => {
                let Some(shared) = HostRegistry::get(cx, repo.host) else {
                    return;
                };
                self.run_git_op(shared, repo.root, op, None, window, cx);
            }
        }
    }

    /// A dispatched commit came back with an error: disarm the latch that
    /// would otherwise clear the message box on the next unrelated HEAD move.
    /// The message itself stays where the user can see it.
    pub(crate) fn scm_commit_failed(
        &mut self,
        host: tty7_core::host::HostId,
        root: &std::path::Path,
    ) {
        if self
            .scm
            .committing
            .as_ref()
            .is_some_and(|(r, _, _)| r.host == host && r.root == root)
        {
            self.scm.committing = None;
        }
    }
}

/// What "discard all" actually runs, in order. Pure so a test can hold it up
/// against a status without a window.
fn discard_all_ops(status: &tty7_core::core::git::status::WorkingTreeStatus) -> Vec<GitOp> {
    let unstaged: Vec<_> = status
        .unstaged()
        .filter(|e| e.path.pathspec().is_some())
        .map(|e| e.path.clone())
        .collect();
    let untracked: Vec<_> = status
        .untracked()
        .filter(|e| e.path.pathspec().is_some())
        .map(|e| e.path.clone())
        .collect();
    let mut ops = Vec::new();
    if !unstaged.is_empty() {
        ops.push(GitOp::DiscardWorktree { paths: unstaged });
    }
    if !untracked.is_empty() {
        let directories = untracked.iter().any(|p| p.as_str().ends_with('/'));
        ops.push(GitOp::DiscardUntracked {
            paths: untracked,
            directories,
        });
    }
    ops
}

/// `origin/main` → `("origin", "main")`.
///
/// The first component is the remote: a branch name may contain slashes, a
/// remote name may not.
pub(crate) fn split_upstream(upstream: &str) -> Option<(&str, &str)> {
    let (remote, branch) = upstream.split_once('/')?;
    (!remote.is_empty() && !branch.is_empty()).then_some((remote, branch))
}

/// The question a destructive operation has to answer before it runs.
fn confirm_question(op: &GitOp, loss: Destructive) -> String {
    // Its own question, not the discard one: a hard reset to an older commit
    // drops commits off the branch, and a dialog that says "Discard every
    // change in this repository?" never mentions the part that hurts.
    if matches!(op, GitOp::Reset { .. }) {
        return t(L10nKey::ScmResetHardConfirm).to_string();
    }
    match loss {
        Destructive::RewritesHistory => t(L10nKey::ScmAmendConfirm).to_string(),
        // One file gets named; a whole group does not, because a list of two
        // hundred paths in a system dialog says less than the count does.
        _ => match op.paths() {
            [only] => t_fmt(L10nKey::ScmDiscardConfirm, &[("path", only.as_str())]),
            _ => t(L10nKey::ScmDiscardAllConfirm).to_string(),
        },
    }
}

fn confirm_verb(op: &GitOp, loss: Destructive) -> &'static str {
    if matches!(op, GitOp::Reset { .. }) {
        return t(L10nKey::ScmReset);
    }
    match loss {
        Destructive::RewritesHistory => t(L10nKey::ScmAmendLastCommit),
        _ => t(L10nKey::ScmDiscard),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tty7_core::core::git::status::{
        ChangeCode, EntryKind, HeadState, RepoPath, StatusEntry, WorkingTreeStatus,
    };

    fn entry(path: &str, index: ChangeCode, worktree: ChangeCode, kind: EntryKind) -> StatusEntry {
        StatusEntry {
            path: RepoPath::from_bytes(path.as_bytes()),
            orig_path: None,
            index,
            worktree,
            kind,
            submodule: None,
            rename_score: None,
            conflict: None,
        }
    }

    fn status_with(entries: Vec<StatusEntry>) -> WorkingTreeStatus {
        WorkingTreeStatus {
            root: std::path::PathBuf::from("/repo"),
            home: std::path::PathBuf::from("/repo"),
            head: HeadState::Branch {
                name: "main".into(),
                oid: "0".repeat(40),
            },
            upstream: None,
            ahead_behind: None,
            total_entries: entries.len(),
            entries,
            truncated: false,
            stash_count: 0,
            operation: None,
            prefilled_message: None,
        }
    }

    /// `checkout --` restores from the *index*: a staged-only path either
    /// survives it (staged edit) or — a staged deletion, in neither index nor
    /// worktree — makes git reject the whole batch as an unmatched pathspec,
    /// taking every real discard down with it. Only unstaged paths go in.
    #[test]
    fn discard_all_sends_only_unstaged_paths_to_checkout() {
        let status = status_with(vec![
            // Staged edit, clean worktree: not `checkout --`'s business.
            entry(
                "staged.rs",
                ChangeCode::Modified,
                ChangeCode::None,
                EntryKind::Tracked,
            ),
            // Staged deletion: the pathspec that used to sink the batch.
            entry(
                "deleted.rs",
                ChangeCode::Deleted,
                ChangeCode::None,
                EntryKind::Tracked,
            ),
            // Staged and edited again: the worktree half is discardable.
            entry(
                "both.rs",
                ChangeCode::Modified,
                ChangeCode::Modified,
                EntryKind::Tracked,
            ),
            entry(
                "edited.rs",
                ChangeCode::None,
                ChangeCode::Modified,
                EntryKind::Tracked,
            ),
            entry(
                "new.rs",
                ChangeCode::None,
                ChangeCode::None,
                EntryKind::Untracked,
            ),
        ]);
        let ops = discard_all_ops(&status);
        assert_eq!(ops.len(), 2, "one checkout batch, one clean batch");
        match &ops[0] {
            GitOp::DiscardWorktree { paths } => {
                let names: Vec<_> = paths.iter().map(|p| p.as_str()).collect();
                assert_eq!(names, vec!["both.rs", "edited.rs"]);
            }
            other => panic!("expected DiscardWorktree first, got {:?}", other.label()),
        }
        match &ops[1] {
            GitOp::DiscardUntracked { paths, directories } => {
                let names: Vec<_> = paths.iter().map(|p| p.as_str()).collect();
                assert_eq!(names, vec!["new.rs"]);
                assert!(!directories);
            }
            other => panic!("expected DiscardUntracked second, got {:?}", other.label()),
        }

        // Nothing to discard means nothing to run — and no prompt to answer.
        assert!(discard_all_ops(&status_with(Vec::new())).is_empty());
    }

    #[test]
    fn an_upstream_splits_on_its_first_slash_only() {
        assert_eq!(split_upstream("origin/main"), Some(("origin", "main")));
        // Branch names carry slashes; remote names cannot.
        assert_eq!(
            split_upstream("origin/feature/auth-retry"),
            Some(("origin", "feature/auth-retry"))
        );
        assert_eq!(split_upstream("main"), None);
        assert_eq!(split_upstream("/main"), None);
        assert_eq!(split_upstream("origin/"), None);
    }
}

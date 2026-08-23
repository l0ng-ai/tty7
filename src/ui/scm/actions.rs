//! Where the source control actions and palette commands land.
//!
//! One `ScmIntent` match, so the four ways of asking for a verb — the action,
//! the key binding, the palette entry and the button on the row — cannot drift
//! into meaning different things.

use gpui::{Context, PromptLevel, Window};

use tty7_core::core::git::ops::{Destructive, GitOp, PullMode, ResetMode};

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

    /// [`Self::scm_op`], with something to run once this operation has
    /// succeeded.
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
            &crate::ui::confirm_answers(confirm_verb(&op, loss), t(L10nKey::Cancel)),
            cx,
        );
        cx.spawn_in(window, async move |app, cx| {
            let Ok(0) = answer.await else { return };
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
            // The same reason `scm_push` gives for its own guard, one level
            // up. The panel's tiles and the menu disable themselves when
            // there is no repository, but the key bindings and the palette do
            // not — every Git command is listed there whatever the pane's
            // directory is — so all of them land here, and without this the
            // answer to pressing them is nothing at all (#545).
            gpui_component::WindowExt::push_notification(
                window,
                t(L10nKey::DiffNotARepo).to_string(),
                cx,
            );
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
            ScmIntent::CheckoutBranch => self.scm_begin_checkout_branch(window, cx),
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
        // The amend toggle is *not* cleared here: `run_git_op` clears it
        // where it arms `scm.committing`, at dispatch — after the amend
        // confirmation, not before it — so a cancelled prompt leaves the
        // toggle and the armed flag exactly as the user set them. Clearing
        // here made Cancel quietly switch amend off, and the next Commit
        // became the new-commit the user had just declined to risk. See
        // `scm_commit_landed`.
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
            t(L10nKey::ScmDiscardAllConfirm),
            None,
            &crate::ui::confirm_answers(t(L10nKey::ScmDiscard), t(L10nKey::Cancel)),
            cx,
        );
        cx.spawn_in(window, async move |app, cx| {
            let Ok(0) = answer.await else { return };
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
    //
    // The mode is matched, not just the op. Only a hard reset can arrive here
    // — `GitOp::destructive` calls soft and mixed harmless, because they keep
    // the worktree — and this question is written for that one mode:
    // "uncommitted changes are discarded" is simply untrue of the other two.
    // Naming the mode keeps the text and the operation from drifting apart if
    // that classification ever widens, instead of resting on an invariant
    // decided in another crate.
    if matches!(
        op,
        GitOp::Reset {
            mode: ResetMode::Hard,
            ..
        }
    ) {
        return t(L10nKey::ScmResetHardConfirm).to_string();
    }
    // The arm below is written for the two losses that are about *files*, and
    // its wording says so: "unstaged and untracked changes … staged changes are
    // kept" describes nothing that happens when commits go. `LosesCommits` can
    // only arrive as a hard reset, answered above, or as `GitOp::DeleteBranch`,
    // which nothing in the UI builds — so today it never gets here. Wiring one
    // up without writing its own question would hand someone deleting a branch
    // a dialog about their working tree, and a Discard button to confirm it
    // with.
    debug_assert_ne!(
        loss,
        Destructive::LosesCommits,
        "{} loses commits and needs its own question, not the discard one",
        op.label()
    );
    match loss {
        Destructive::RewritesHistory => t(L10nKey::ScmAmendConfirm).to_string(),
        // One file gets named; a whole group does not, because a list of two
        // hundred paths in a system dialog says less than the count does.
        _ => match op.paths() {
            [only] => t_fmt(
                L10nKey::ScmDiscardConfirm,
                &[("path", &crate::terminal::view::one_line(only.as_str()))],
            ),
            _ => t(L10nKey::ScmDiscardAllConfirm).to_string(),
        },
    }
}

fn confirm_verb(op: &GitOp, loss: Destructive) -> &'static str {
    // Every mode, unlike the question above: "Reset" is the honest button for
    // any of them, so there is nothing here to drift.
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

    /// Whatever the working tree looks like, discard-all only ever produces
    /// the two operations its prompt describes.
    ///
    /// This is the one place in the SCM panel where a destructive operation
    /// runs with no confirmation of its own: `scm_discard_all` asks once, then
    /// hands the second operation on as `ScmFollowUp::Op`, which
    /// `scm_follow_up` runs straight through `run_git_op`. That is sound only
    /// while the follow-up is something the single prompt actually described.
    ///
    /// The test above pins one working tree in detail. This one pins the
    /// property across the shapes a repository is actually found in, because
    /// the bypass is not "this fixture is fine" — it is "nothing else can ever
    /// come out of here". An op added for some other status would ride the
    /// approval given for discarding, and the user would never be asked.
    /// An untracked *directory* is only removed if `clean` is told to recurse.
    ///
    /// Porcelain v2 names an untracked directory with a trailing slash —
    /// `? newdir/` — and that slash is the whole of how this decides. Measured
    /// against real git: `clean -f` removes `loose.txt` and leaves `newdir/`
    /// standing, `clean -fd` takes both. Without the flag, "Discard all"
    /// quietly leaves every untracked directory where it was, which is not
    /// what the button says and not what its own confirmation described.
    #[test]
    fn discard_all_recurses_only_when_an_untracked_directory_is_there() {
        let files_only = status_with(vec![entry(
            "loose.txt",
            ChangeCode::None,
            ChangeCode::None,
            EntryKind::Untracked,
        )]);
        match &discard_all_ops(&files_only)[..] {
            [GitOp::DiscardUntracked { directories, .. }] => assert!(
                !*directories,
                "nothing here is a directory, so `clean` has no reason to recurse"
            ),
            other => panic!("expected one untracked discard, got {other:?}"),
        }

        let with_a_dir = status_with(vec![
            entry(
                "loose.txt",
                ChangeCode::None,
                ChangeCode::None,
                EntryKind::Untracked,
            ),
            entry(
                "newdir/",
                ChangeCode::None,
                ChangeCode::None,
                EntryKind::Untracked,
            ),
        ]);
        match &discard_all_ops(&with_a_dir)[..] {
            [GitOp::DiscardUntracked { directories, paths }] => {
                assert!(
                    *directories,
                    "an untracked directory survives `clean -f`, so this has to be -fd"
                );
                assert_eq!(paths.len(), 2, "both entries are still discarded");
            }
            other => panic!("expected one untracked discard, got {other:?}"),
        }
    }

    /// One unusable path does not cost the user the whole discard.
    ///
    /// A name that is not UTF-8 cannot be sent to git as a pathspec at all.
    /// It is dropped from the list here rather than carried into the op,
    /// because `GitOp::validate` rejects an *operation* that carries one — so
    /// leaving it in would turn "discard everything" into an error that
    /// discards nothing, on a repository that happens to contain one awkward
    /// filename.
    #[test]
    fn an_unusable_path_is_left_out_rather_than_failing_the_whole_discard() {
        let mut bad = entry(
            "ok.txt",
            ChangeCode::None,
            ChangeCode::Modified,
            EntryKind::Tracked,
        );
        bad.path = RepoPath::from_bytes(&[0xff, 0xfe, b'.', b'r', b's']);
        assert!(
            bad.path.pathspec().is_none(),
            "this fixture is only meaningful if the path really is unusable"
        );
        let status = status_with(vec![
            entry(
                "ok.txt",
                ChangeCode::None,
                ChangeCode::Modified,
                EntryKind::Tracked,
            ),
            bad,
        ]);

        let ops = discard_all_ops(&status);
        match &ops[..] {
            [GitOp::DiscardWorktree { paths }] => {
                assert_eq!(
                    paths.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
                    ["ok.txt"],
                    "the usable path is still discarded"
                );
            }
            other => panic!("expected one worktree discard, got {other:?}"),
        }
        for op in &ops {
            assert!(
                op.validate().is_ok(),
                "an op built here must not be one validate rejects: {op:?}"
            );
        }
    }

    #[test]
    fn discard_all_never_produces_an_operation_its_prompt_did_not_describe() {
        let shapes: Vec<(&str, Vec<_>)> = vec![
            ("nothing at all", Vec::new()),
            (
                "staged only",
                vec![entry(
                    "s.rs",
                    ChangeCode::Modified,
                    ChangeCode::None,
                    EntryKind::Tracked,
                )],
            ),
            (
                "unstaged only",
                vec![entry(
                    "u.rs",
                    ChangeCode::None,
                    ChangeCode::Modified,
                    EntryKind::Tracked,
                )],
            ),
            (
                "untracked only",
                vec![entry(
                    "n.rs",
                    ChangeCode::None,
                    ChangeCode::None,
                    EntryKind::Untracked,
                )],
            ),
            (
                "an untracked directory",
                vec![entry(
                    "build/",
                    ChangeCode::None,
                    ChangeCode::None,
                    EntryKind::Untracked,
                )],
            ),
            (
                "a deletion on both sides",
                vec![entry(
                    "gone.rs",
                    ChangeCode::Deleted,
                    ChangeCode::Deleted,
                    EntryKind::Tracked,
                )],
            ),
            (
                "everything at once",
                vec![
                    entry(
                        "s.rs",
                        ChangeCode::Modified,
                        ChangeCode::None,
                        EntryKind::Tracked,
                    ),
                    entry(
                        "u.rs",
                        ChangeCode::None,
                        ChangeCode::Modified,
                        EntryKind::Tracked,
                    ),
                    entry(
                        "n.rs",
                        ChangeCode::None,
                        ChangeCode::None,
                        EntryKind::Untracked,
                    ),
                    entry(
                        "d/",
                        ChangeCode::None,
                        ChangeCode::None,
                        EntryKind::Untracked,
                    ),
                ],
            ),
        ];

        for (what, entries) in shapes {
            for op in discard_all_ops(&status_with(entries)) {
                assert!(
                    matches!(
                        op,
                        GitOp::DiscardWorktree { .. } | GitOp::DiscardUntracked { .. }
                    ),
                    "with {what}, discard-all produced {:?}, which the one prompt \
                     the user answered did not describe — and the follow-up runs \
                     without asking again",
                    op.label()
                );
            }
        }
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

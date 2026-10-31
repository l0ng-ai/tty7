//! What the panel shows, whichever forge it came from.
//!
//! Each forge's JSON is decoded into these types next to its requests
//! (`super::github`). They are what the UI draws from,
//! already reduced to the decisions it needs — one [`ItemState`] rather than
//! the several fields a forge spreads it over.

use crate::core::git::diff::{DiffBudget, DiffParser, FileDiff, FileStatus};

/// Issues or pull requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Kind {
    #[default]
    Issues,
    Pulls,
}

/// The list's open/closed switch. "Closed" includes merged pull requests, as
/// it does on GitHub.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum StateFilter {
    #[default]
    Open,
    Closed,
}

impl StateFilter {
    pub fn as_query(self) -> &'static str {
        match self {
            StateFilter::Open => "open",
            StateFilter::Closed => "closed",
        }
    }
}

/// Where an issue or a pull request stands. Each one gets its own glyph
/// *shape*, not only its own colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ItemState {
    Open,
    /// A pull request marked as a draft (and still open).
    Draft,
    /// An issue closed as completed, or a pull request closed unmerged.
    Closed,
    /// An issue closed as "not planned" / duplicate.
    NotPlanned,
    Merged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    /// `0xRRGGBB`, when GitHub sent a usable colour.
    pub color: Option<u32>,
}

/// One row of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub number: u64,
    pub title: String,
    pub state: ItemState,
    pub is_pr: bool,
    pub author: String,
    pub labels: Vec<Label>,
    pub comments: u32,
    /// Unix seconds; zero when GitHub sent something unparseable.
    pub created_at: i64,
    pub updated_at: i64,
    pub html_url: String,
    /// The branches, when the row came from `/pulls`; `/issues` has none.
    pub pull: Option<PullRefs>,
}

/// A pull request's branches, as `/pulls` reports them.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PullRefs {
    pub head_ref: String,
    /// `owner:branch` — names the repository the head lives in, so a fork's
    /// branch is told apart from this repository's.
    pub head_label: String,
    pub base_ref: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    pub id: u64,
    pub author: String,
    pub body: String,
    pub created_at: i64,
    pub html_url: String,
}

/// The pull-request-only half of a detail view.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PullInfo {
    pub refs: PullRefs,
    /// The head commit — what the checks ran on. Empty if GitHub left it out.
    pub head_sha: String,
    pub additions: u32,
    pub deletions: u32,
    pub changed_files: u32,
    pub commits: u32,
    pub merge_state: MergeState,
}

/// GitHub's `mergeable_state`: whether the pull request could be merged now,
/// and if not, the broad reason. GitHub computes it lazily, so the first read
/// of a freshly pushed pull request is often `Unknown`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MergeState {
    /// Mergeable, everything required has passed.
    Clean,
    /// Mergeable, but a check that is not required is failing.
    Unstable,
    /// Branch protection says no: a required check or review is missing.
    Blocked,
    /// The base branch has moved on and protection requires being up to date.
    Behind,
    /// Merge conflicts.
    Dirty,
    Draft,
    #[default]
    Unknown,
}

/// Where one CI check stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CheckState {
    // Declared in the order the panel lists them: what needs attention first.
    Failed,
    Pending,
    Passed,
    /// Skipped, neutral, or stale — ran (or was never going to) without a
    /// verdict either way. Not counted toward "N of M passed".
    Skipped,
}

/// One CI check on a pull request's head commit: a check run (GitHub Actions
/// and any other GitHub App) or a commit status (the older API some external
/// CI still reports through). GitHub's own page lists both side by side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub state: CheckState,
    /// Unix seconds, zero when unknown. A commit status has no start time.
    pub started_at: i64,
    pub completed_at: i64,
    /// Its log or details page, when there is one.
    pub url: Option<String>,
}

/// Every check on a head commit, the ones that need attention first.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Checks {
    pub items: Vec<Check>,
    /// More checks exist than were fetched.
    pub truncated: bool,
}

impl Checks {
    pub fn count(&self, state: CheckState) -> usize {
        self.items.iter().filter(|c| c.state == state).count()
    }

    /// The checks with a verdict to give: everything but the skipped ones.
    pub fn counted(&self) -> usize {
        self.items.len() - self.count(CheckState::Skipped)
    }

    /// The whole set in one state, the way a list row or a badge shows it:
    /// any failure fails it, else anything running keeps it pending.
    pub fn rollup(&self) -> Option<CheckState> {
        if self.items.is_empty() {
            return None;
        }
        [CheckState::Failed, CheckState::Pending, CheckState::Passed]
            .into_iter()
            .find(|s| self.count(*s) > 0)
            .or(Some(CheckState::Skipped))
    }

    /// The ones that need attention first. Stable: within a state, the
    /// forge's own order — GitHub's by run, then context.
    pub(crate) fn sort(&mut self) {
        self.items.sort_by_key(|c| c.state);
    }
}

/// Where one reviewer stands on a pull request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    /// Left review comments without a verdict.
    Commented,
    /// Asked for a review that has not come in (or was asked again after one).
    Requested,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reviewer {
    /// A user's login, or `org/team` for a requested team.
    pub login: String,
    pub state: ReviewState,
}

/// Whether an open pull request can be merged, and if not, the one thing
/// most worth saying about why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    Conflicts,
    ChecksFailing(usize),
    WaitingOnChecks(usize),
    ChangesRequested,
    ReviewRequired,
    Behind,
    Blocked,
}

/// Folds the merge state, the checks and the reviews into one line's worth,
/// the way GitHub's merge box leads with its most pressing reason. `None` for
/// anything but an open, non-draft pull request, and while GitHub has not
/// worked the merge state out yet.
pub fn readiness(
    state: ItemState,
    merge: MergeState,
    checks: Option<&Checks>,
    reviewers: Option<&[Reviewer]>,
) -> Option<Readiness> {
    if state != ItemState::Open {
        return None;
    }
    let count = |s| checks.map_or(0, |c| c.count(s));
    let has = |s| reviewers.is_some_and(|r| r.iter().any(|r| r.state == s));
    Some(match merge {
        MergeState::Unknown | MergeState::Draft => return None,
        MergeState::Dirty => Readiness::Conflicts,
        _ if count(CheckState::Failed) > 0 && merge != MergeState::Clean => {
            Readiness::ChecksFailing(count(CheckState::Failed))
        }
        _ if count(CheckState::Pending) > 0 && merge != MergeState::Clean => {
            Readiness::WaitingOnChecks(count(CheckState::Pending))
        }
        _ if has(ReviewState::ChangesRequested) && merge != MergeState::Clean => {
            Readiness::ChangesRequested
        }
        MergeState::Behind => Readiness::Behind,
        MergeState::Clean | MergeState::Unstable => Readiness::Ready,
        MergeState::Blocked if !has(ReviewState::Approved) => Readiness::ReviewRequired,
        MergeState::Blocked => Readiness::Blocked,
    })
}

/// One file a pull request touches, from `/pulls/{n}/files`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrFile {
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub additions: u32,
    pub deletions: u32,
    /// The unified hunks, without a `diff --git` header. GitHub leaves it out
    /// for binary files and for patches too large to inline.
    pub patch: Option<String>,
}

impl PrFile {
    /// This file as the diff overlay's own model, so the overlay can show a
    /// pull request's patch exactly the way it shows a local one.
    ///
    /// The counts come from GitHub rather than from the parsed hunks: a file
    /// GitHub did not inline a patch for still has real `+N −M`.
    pub fn to_file_diff(&self, budget: DiffBudget) -> FileDiff {
        let mut parser = DiffParser::with_budget(budget);
        // A placeholder header opens the file; the real paths are set below,
        // so a path with spaces never has to survive header splitting.
        parser.push_line("diff --git a/f b/f");
        if let Some(patch) = &self.patch {
            for line in patch.lines() {
                parser.push_line(line);
            }
        }
        let mut file = parser.finish().into_iter().next().unwrap_or(FileDiff {
            path: String::new(),
            old_path: None,
            status: FileStatus::Modified,
            added: 0,
            removed: 0,
            binary: false,
            truncated: None,
            hunks: Vec::new(),
        });
        file.path = self.path.clone();
        file.old_path = self.old_path.clone();
        file.status = self.status;
        file.added = self.additions;
        file.removed = self.deletions;
        // No patch but real changes is GitHub declining to inline one — a
        // binary file or an oversized text one. The overlay's binary card
        // ("no preview") is the honest thing to show for both.
        file.binary = self.patch.is_none() && (self.additions > 0 || self.deletions > 0);
        file
    }
}

/// Everything the detail view needs about one issue or pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detail {
    pub item: Item,
    pub body: String,
    pub comments: Vec<Comment>,
    /// More comments exist than were fetched.
    pub comments_truncated: bool,
    pub pull: Option<PullInfo>,
    /// `Some` for a pull request.
    pub files: Option<Vec<PrFile>>,
    pub files_truncated: bool,
    /// The head commit's CI. `None` for an issue, and for a pull request
    /// whose checks could not be read — a detail does not fail over them.
    pub checks: Option<Checks>,
    /// Reviews in and reviews asked for, one entry per reviewer. `None` as
    /// for `checks`.
    pub reviewers: Option<Vec<Reviewer>>,
}

/// GitHub's `2024-05-01T12:34:56Z`, as unix seconds (zero if unparseable).
pub fn timestamp(text: &str) -> i64 {
    crate::core::git::log::parse_iso8601(text).map_or(0, |ts| ts.unix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_timestamps_are_unix_seconds() {
        assert_eq!(timestamp("1970-01-01T00:00:00Z"), 0);
        assert_eq!(timestamp("2024-02-29T12:00:00Z"), 1_709_208_000);
        assert_eq!(timestamp("garbage"), 0);
    }

    fn checks_of(states: &[CheckState]) -> Checks {
        Checks {
            items: states
                .iter()
                .map(|s| Check {
                    name: String::new(),
                    state: *s,
                    started_at: 0,
                    completed_at: 0,
                    url: None,
                })
                .collect(),
            truncated: false,
        }
    }

    #[test]
    fn a_rollup_is_failed_before_pending_before_passed() {
        use CheckState::*;
        assert_eq!(checks_of(&[]).rollup(), None);
        assert_eq!(checks_of(&[Passed, Pending, Failed]).rollup(), Some(Failed));
        assert_eq!(
            checks_of(&[Passed, Pending, Skipped]).rollup(),
            Some(Pending)
        );
        assert_eq!(checks_of(&[Passed, Skipped]).rollup(), Some(Passed));
        assert_eq!(checks_of(&[Skipped]).rollup(), Some(Skipped));
    }

    #[test]
    fn a_merge_box_leads_with_its_most_pressing_reason() {
        use CheckState::*;
        let approved = [Reviewer {
            login: "mara".into(),
            state: ReviewState::Approved,
        }];
        let changes = [Reviewer {
            login: "mara".into(),
            state: ReviewState::ChangesRequested,
        }];
        let r = |merge, checks: &[CheckState], reviews: &[Reviewer]| {
            readiness(
                ItemState::Open,
                merge,
                Some(&checks_of(checks)),
                Some(reviews),
            )
        };
        assert_eq!(
            r(MergeState::Blocked, &[Passed, Pending], &approved),
            Some(Readiness::WaitingOnChecks(1))
        );
        assert_eq!(
            r(MergeState::Blocked, &[Failed, Failed, Pending], &approved),
            Some(Readiness::ChecksFailing(2))
        );
        assert_eq!(
            r(MergeState::Dirty, &[Failed], &approved),
            Some(Readiness::Conflicts)
        );
        assert_eq!(
            r(MergeState::Blocked, &[Passed], &changes),
            Some(Readiness::ChangesRequested)
        );
        assert_eq!(
            r(MergeState::Blocked, &[Passed], &[]),
            Some(Readiness::ReviewRequired)
        );
        assert_eq!(
            r(MergeState::Blocked, &[Passed], &approved),
            Some(Readiness::Blocked)
        );
        assert_eq!(
            r(MergeState::Behind, &[Passed], &approved),
            Some(Readiness::Behind)
        );
        // Clean means everything required is in, whatever optional check is
        // still running.
        assert_eq!(
            r(MergeState::Clean, &[Pending], &[]),
            Some(Readiness::Ready)
        );
        assert_eq!(
            r(MergeState::Unstable, &[Failed], &[]),
            Some(Readiness::ChecksFailing(1))
        );
        assert_eq!(r(MergeState::Unknown, &[Passed], &approved), None);
        assert_eq!(
            readiness(ItemState::Draft, MergeState::Clean, None, None),
            None,
            "a draft is not up for merging"
        );
        assert_eq!(
            readiness(ItemState::Merged, MergeState::Clean, None, None),
            None
        );
    }

    fn file(patch: Option<&str>, additions: u32, deletions: u32) -> PrFile {
        PrFile {
            path: "src/a file.rs".into(),
            old_path: None,
            status: FileStatus::Modified,
            additions,
            deletions,
            patch: patch.map(str::to_string),
        }
    }

    #[test]
    fn a_github_patch_becomes_the_overlays_file_model() {
        let f = file(
            Some(
                "@@ -1,3 +1,3 @@\n fn a() {}\n-fn b() {}\n+fn c() {}\n fn d() {}\n\\ No newline at end of file",
            ),
            1,
            1,
        );
        let diff = f.to_file_diff(DiffBudget::default());
        assert_eq!(diff.path, "src/a file.rs", "a path with a space survives");
        assert_eq!((diff.added, diff.removed), (1, 1));
        assert!(!diff.binary);
        assert_eq!(diff.hunks.len(), 1);
        let lines = &diff.hunks[0].lines;
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1].old_no, Some(2));
        assert_eq!(lines[2].new_no, Some(2));
    }

    #[test]
    fn a_file_github_would_not_inline_reads_as_binary() {
        let diff = file(None, 0, 0).to_file_diff(DiffBudget::default());
        assert!(!diff.binary, "an empty change is not a binary one");
        let diff = file(None, 120, 4).to_file_diff(DiffBudget::default());
        assert!(diff.binary);
        assert_eq!((diff.added, diff.removed), (120, 4));
        assert!(diff.hunks.is_empty());
    }
}

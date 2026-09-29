//! What the panel shows, decoded out of GitHub's REST JSON.
//!
//! The wire structs (`Raw*`) mirror only the fields read; everything else in
//! GitHub's (large) payloads is ignored, so a field GitHub adds or drops later
//! cannot break decoding. The public types are what the UI draws from, already
//! reduced to the decisions it needs — one [`ItemState`] rather than the three
//! fields (`state`, `state_reason`, `merged_at`/`draft`) GitHub spreads it over.

use serde::Deserialize;

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
    pub head_ref: String,
    /// The head commit — what the checks ran on. Empty if GitHub left it out.
    pub head_sha: String,
    pub base_ref: String,
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

impl MergeState {
    fn parse(s: Option<&str>) -> MergeState {
        match s {
            // "has_hooks" is clean with a pre-receive hook in the way — for
            // the panel's purposes, clean.
            Some("clean") | Some("has_hooks") => MergeState::Clean,
            Some("unstable") => MergeState::Unstable,
            Some("blocked") => MergeState::Blocked,
            Some("behind") => MergeState::Behind,
            Some("dirty") => MergeState::Dirty,
            Some("draft") => MergeState::Draft,
            _ => MergeState::Unknown,
        }
    }
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

    fn sort(&mut self) {
        // Stable: within a state, GitHub's own order (by run, then context).
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

// ---- wire ------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct RawUser {
    #[serde(default)]
    login: String,
}

#[derive(Deserialize)]
pub(crate) struct RawLabel {
    #[serde(default)]
    name: String,
    #[serde(default)]
    color: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct RawPrRef {
    #[serde(default)]
    merged_at: Option<String>,
}

/// An entry of `/issues` — which also lists pull requests, marked by the
/// presence of `pull_request`.
#[derive(Deserialize)]
pub(crate) struct RawIssue {
    pub(crate) number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    state_reason: Option<String>,
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    labels: Vec<RawLabel>,
    #[serde(default)]
    comments: u32,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    pub(crate) body: Option<String>,
    /// Only with the `full` media type.
    #[serde(default)]
    pub(crate) body_html: Option<String>,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    pub(crate) pull_request: Option<RawPrRef>,
    #[serde(default)]
    draft: Option<bool>,
}

#[derive(Deserialize)]
pub(crate) struct RawBranchRef {
    #[serde(default, rename = "ref")]
    name: String,
    #[serde(default)]
    sha: String,
}

#[derive(Deserialize)]
pub(crate) struct RawTeam {
    #[serde(default)]
    slug: String,
}

/// An entry of `/pulls`, or `/pulls/{n}`.
#[derive(Deserialize)]
pub(crate) struct RawPull {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    draft: Option<bool>,
    #[serde(default)]
    merged_at: Option<String>,
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    labels: Vec<RawLabel>,
    #[serde(default)]
    comments: Option<u32>,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    head: Option<RawBranchRef>,
    #[serde(default)]
    base: Option<RawBranchRef>,
    #[serde(default)]
    additions: Option<u32>,
    #[serde(default)]
    deletions: Option<u32>,
    #[serde(default)]
    changed_files: Option<u32>,
    #[serde(default)]
    commits: Option<u32>,
    /// Only on `/pulls/{n}`.
    #[serde(default)]
    mergeable_state: Option<String>,
    #[serde(default)]
    pub(crate) requested_reviewers: Vec<RawUser>,
    #[serde(default)]
    pub(crate) requested_teams: Vec<RawTeam>,
}

#[derive(Deserialize)]
pub(crate) struct RawComment {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    body: Option<String>,
    /// Only with the `full` media type.
    #[serde(default)]
    body_html: Option<String>,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    html_url: String,
}

#[derive(Deserialize)]
pub(crate) struct RawFile {
    #[serde(default)]
    filename: String,
    #[serde(default)]
    previous_filename: Option<String>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    additions: u32,
    #[serde(default)]
    deletions: u32,
    #[serde(default)]
    patch: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct RawReview {
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    state: String,
}

/// `/commits/{sha}/check-runs`.
#[derive(Deserialize)]
pub(crate) struct RawCheckRuns {
    #[serde(default)]
    pub(crate) total_count: usize,
    #[serde(default)]
    pub(crate) check_runs: Vec<RawCheckRun>,
}

#[derive(Deserialize)]
pub(crate) struct RawCheckRun {
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    completed_at: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
}

/// `/commits/{sha}/status`: the latest status per context.
#[derive(Deserialize)]
pub(crate) struct RawCombinedStatus {
    #[serde(default)]
    pub(crate) total_count: usize,
    #[serde(default)]
    pub(crate) statuses: Vec<RawStatus>,
}

#[derive(Deserialize)]
pub(crate) struct RawStatus {
    #[serde(default)]
    context: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    target_url: Option<String>,
    #[serde(default)]
    updated_at: String,
}

fn login(user: Option<RawUser>) -> String {
    user.map(|u| u.login).unwrap_or_default()
}

fn labels(raw: Vec<RawLabel>) -> Vec<Label> {
    raw.into_iter()
        .filter(|l| !l.name.is_empty())
        .map(|l| Label {
            color: l
                .color
                .as_deref()
                .filter(|c| c.len() == 6)
                .and_then(|c| u32::from_str_radix(c, 16).ok()),
            name: l.name,
        })
        .collect()
}

/// GitHub's `2024-05-01T12:34:56Z`, as unix seconds (zero if unparseable).
pub fn timestamp(text: &str) -> i64 {
    crate::core::git::log::parse_iso8601(text).map_or(0, |ts| ts.unix)
}

impl RawIssue {
    pub(crate) fn is_pr(&self) -> bool {
        self.pull_request.is_some()
    }

    pub(crate) fn into_item(self) -> Item {
        let is_pr = self.is_pr();
        let merged = self
            .pull_request
            .as_ref()
            .is_some_and(|p| p.merged_at.is_some());
        let state = item_state(
            &self.state,
            self.state_reason.as_deref(),
            merged,
            self.draft.unwrap_or(false),
        );
        Item {
            number: self.number,
            title: self.title,
            state,
            is_pr,
            author: login(self.user),
            labels: labels(self.labels),
            comments: self.comments,
            created_at: timestamp(&self.created_at),
            updated_at: timestamp(&self.updated_at),
            html_url: self.html_url,
        }
    }
}

impl RawPull {
    pub(crate) fn into_item(self) -> (Item, PullInfo) {
        let state = item_state(
            &self.state,
            None,
            self.merged_at.is_some(),
            self.draft.unwrap_or(false),
        );
        let (head_ref, head_sha) = self.head.map(|h| (h.name, h.sha)).unwrap_or_default();
        let info = PullInfo {
            head_ref,
            head_sha,
            base_ref: self.base.map(|b| b.name).unwrap_or_default(),
            additions: self.additions.unwrap_or(0),
            deletions: self.deletions.unwrap_or(0),
            changed_files: self.changed_files.unwrap_or(0),
            commits: self.commits.unwrap_or(0),
            merge_state: MergeState::parse(self.mergeable_state.as_deref()),
        };
        let item = Item {
            number: self.number,
            title: self.title,
            state,
            is_pr: true,
            author: login(self.user),
            labels: labels(self.labels),
            comments: self.comments.unwrap_or(0),
            created_at: timestamp(&self.created_at),
            updated_at: timestamp(&self.updated_at),
            html_url: self.html_url,
        };
        (item, info)
    }
}

impl RawCheckRun {
    pub(crate) fn into_check(self) -> Check {
        let state = match (self.status.as_str(), self.conclusion.as_deref()) {
            ("completed", Some("success")) => CheckState::Passed,
            ("completed", Some("skipped" | "neutral" | "stale")) => CheckState::Skipped,
            // failure, cancelled, timed_out, action_required, startup_failure
            ("completed", _) => CheckState::Failed,
            // queued, in_progress, waiting, requested, pending
            _ => CheckState::Pending,
        };
        Check {
            name: self.name,
            state,
            started_at: self.started_at.as_deref().map_or(0, timestamp),
            completed_at: self.completed_at.as_deref().map_or(0, timestamp),
            url: self.html_url.filter(|u| !u.is_empty()),
        }
    }
}

impl RawStatus {
    pub(crate) fn into_check(self) -> Check {
        let state = match self.state.as_str() {
            "success" => CheckState::Passed,
            "pending" => CheckState::Pending,
            // failure, error
            _ => CheckState::Failed,
        };
        Check {
            name: self.context,
            state,
            started_at: 0,
            completed_at: if state == CheckState::Pending {
                0
            } else {
                timestamp(&self.updated_at)
            },
            url: self.target_url.filter(|u| !u.is_empty()),
        }
    }
}

/// Check runs and commit statuses as one list, the ones needing attention
/// first. `/check-runs` already keeps only each check's latest run, and
/// `/status` each context's latest status.
pub(crate) fn checks(runs: RawCheckRuns, statuses: RawCombinedStatus) -> Checks {
    let truncated =
        runs.total_count > runs.check_runs.len() || statuses.total_count > statuses.statuses.len();
    let mut checks = Checks {
        items: runs
            .check_runs
            .into_iter()
            .map(RawCheckRun::into_check)
            .chain(statuses.statuses.into_iter().map(RawStatus::into_check))
            .collect(),
        truncated,
    };
    checks.sort();
    checks
}

/// Each reviewer's standing, from the reviews in (oldest first, as GitHub
/// lists them) and the reviews still asked for.
///
/// A verdict sticks until the same reviewer gives another or it is dismissed;
/// a later plain comment does not undo an approval, the way GitHub's own
/// sidebar reads. Asking again puts a reviewer back to "requested" whatever
/// they said before. The author's replies in their own review threads are
/// reviews by GitHub's count, but not a reviewer.
pub(crate) fn reviewers(
    author: &str,
    reviews: Vec<RawReview>,
    requested_users: Vec<RawUser>,
    requested_teams: Vec<String>,
) -> Vec<Reviewer> {
    let mut out: Vec<Reviewer> = Vec::new();
    for review in reviews {
        let login = login(review.user);
        if login.is_empty() || login == author {
            continue;
        }
        let verdict = match review.state.as_str() {
            "APPROVED" => Some(ReviewState::Approved),
            "CHANGES_REQUESTED" => Some(ReviewState::ChangesRequested),
            "COMMENTED" | "DISMISSED" => None,
            // PENDING: the viewer's own unsubmitted review.
            _ => continue,
        };
        match out.iter_mut().find(|r| r.login == login) {
            Some(r) => match verdict {
                Some(v) => r.state = v,
                None if review.state == "DISMISSED" => r.state = ReviewState::Commented,
                None => {}
            },
            None => out.push(Reviewer {
                login,
                state: verdict.unwrap_or(ReviewState::Commented),
            }),
        }
    }
    let requested = requested_users
        .into_iter()
        .map(|u| u.login)
        .chain(requested_teams)
        .filter(|l| !l.is_empty());
    for login in requested {
        match out.iter_mut().find(|r| r.login == login) {
            Some(r) => r.state = ReviewState::Requested,
            None => out.push(Reviewer {
                login,
                state: ReviewState::Requested,
            }),
        }
    }
    out
}

impl RawPull {
    /// The teams asked to review, as `org/team` when the org is known.
    pub(crate) fn requested_team_names(&self, org: &str) -> Vec<String> {
        self.requested_teams
            .iter()
            .filter(|t| !t.slug.is_empty())
            .map(|t| format!("{org}/{}", t.slug))
            .collect()
    }
}

impl RawComment {
    pub(crate) fn into_comment(self) -> Comment {
        Comment {
            id: self.id,
            author: login(self.user),
            body: super::markdown::sign_attachments(
                self.body.as_deref().unwrap_or_default(),
                self.body_html.as_deref().unwrap_or_default(),
            ),
            created_at: timestamp(&self.created_at),
            html_url: self.html_url,
        }
    }
}

impl RawFile {
    pub(crate) fn into_file(self) -> PrFile {
        let status = match self.status.as_str() {
            "added" => FileStatus::Added,
            "removed" => FileStatus::Deleted,
            "renamed" => FileStatus::Renamed,
            "copied" => FileStatus::Copied,
            // "modified", "changed" (mode only) and "unchanged" all draw as a
            // modification; none of them is a different kind of row.
            _ => FileStatus::Modified,
        };
        PrFile {
            path: self.filename,
            old_path: self.previous_filename,
            status,
            additions: self.additions,
            deletions: self.deletions,
            patch: self.patch,
        }
    }
}

/// The one place GitHub's three state fields become one state.
fn item_state(state: &str, reason: Option<&str>, merged: bool, draft: bool) -> ItemState {
    if merged {
        return ItemState::Merged;
    }
    match state {
        "closed" => match reason {
            Some("not_planned") | Some("duplicate") => ItemState::NotPlanned,
            _ => ItemState::Closed,
        },
        _ if draft => ItemState::Draft,
        _ => ItemState::Open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_folds_three_fields_into_one() {
        assert_eq!(item_state("open", None, false, false), ItemState::Open);
        assert_eq!(item_state("open", None, false, true), ItemState::Draft);
        assert_eq!(item_state("closed", None, false, false), ItemState::Closed);
        assert_eq!(
            item_state("closed", Some("completed"), false, false),
            ItemState::Closed
        );
        assert_eq!(
            item_state("closed", Some("not_planned"), false, false),
            ItemState::NotPlanned
        );
        // Merged wins over "closed", and a closed draft is closed.
        assert_eq!(item_state("closed", None, true, false), ItemState::Merged);
        assert_eq!(item_state("closed", None, false, true), ItemState::Closed);
    }

    #[test]
    fn label_colours_parse_and_bad_ones_are_dropped() {
        let got = labels(vec![
            RawLabel {
                name: "bug".into(),
                color: Some("d73a4a".into()),
            },
            RawLabel {
                name: "weird".into(),
                color: Some("zzz".into()),
            },
            RawLabel {
                name: String::new(),
                color: None,
            },
        ]);
        assert_eq!(
            got,
            vec![
                Label {
                    name: "bug".into(),
                    color: Some(0xd73a4a)
                },
                Label {
                    name: "weird".into(),
                    color: None
                },
            ]
        );
    }

    #[test]
    fn github_timestamps_are_unix_seconds() {
        assert_eq!(timestamp("1970-01-01T00:00:00Z"), 0);
        assert_eq!(timestamp("2024-02-29T12:00:00Z"), 1_709_208_000);
        assert_eq!(timestamp("garbage"), 0);
    }

    fn review(login: &str, state: &str) -> RawReview {
        RawReview {
            user: Some(RawUser {
                login: login.into(),
            }),
            state: state.into(),
        }
    }

    fn states(r: &[Reviewer]) -> Vec<(&str, ReviewState)> {
        r.iter().map(|r| (r.login.as_str(), r.state)).collect()
    }

    #[test]
    fn a_verdict_sticks_until_the_reviewer_gives_another() {
        let got = reviewers(
            "author",
            vec![
                review("mara", "CHANGES_REQUESTED"),
                review("mara", "APPROVED"),
                review("mara", "COMMENTED"),
                review("jonas", "COMMENTED"),
                review("kai", "APPROVED"),
                review("kai", "DISMISSED"),
                review("author", "COMMENTED"),
                review("lee", "PENDING"),
            ],
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(
            states(&got),
            vec![
                ("mara", ReviewState::Approved),
                ("jonas", ReviewState::Commented),
                ("kai", ReviewState::Commented),
            ]
        );
    }

    #[test]
    fn asking_again_puts_a_reviewer_back_to_requested() {
        let got = reviewers(
            "author",
            vec![review("mara", "CHANGES_REQUESTED")],
            vec![RawUser {
                login: "mara".into(),
            }],
            vec!["acme/core".into()],
        );
        assert_eq!(
            states(&got),
            vec![
                ("mara", ReviewState::Requested),
                ("acme/core", ReviewState::Requested),
            ]
        );
    }

    fn run(status: &str, conclusion: Option<&str>) -> RawCheckRun {
        RawCheckRun {
            name: format!("{status}/{conclusion:?}"),
            status: status.into(),
            conclusion: conclusion.map(str::to_string),
            started_at: None,
            completed_at: None,
            html_url: None,
        }
    }

    #[test]
    fn check_runs_fold_into_four_states() {
        for (status, conclusion, want) in [
            ("completed", Some("success"), CheckState::Passed),
            ("completed", Some("failure"), CheckState::Failed),
            ("completed", Some("cancelled"), CheckState::Failed),
            ("completed", Some("timed_out"), CheckState::Failed),
            ("completed", Some("action_required"), CheckState::Failed),
            ("completed", Some("skipped"), CheckState::Skipped),
            ("completed", Some("neutral"), CheckState::Skipped),
            ("queued", None, CheckState::Pending),
            ("in_progress", None, CheckState::Pending),
            ("waiting", None, CheckState::Pending),
        ] {
            assert_eq!(
                run(status, conclusion).into_check().state,
                want,
                "{status} {conclusion:?}"
            );
        }
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

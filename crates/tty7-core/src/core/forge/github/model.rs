//! GitHub's REST JSON, and how it maps onto the forge model.
//!
//! The wire structs (`Raw*`) mirror only the fields read; everything else in
//! GitHub's (large) payloads is ignored, so a field GitHub adds or drops later
//! cannot break decoding. The mapping reduces them to the decisions the panel
//! draws from — one [`ItemState`] rather than the three fields (`state`,
//! `state_reason`, `merged_at`/`draft`) GitHub spreads it over.

use serde::Deserialize;

use crate::core::forge::model::{
    Check, CheckState, Checks, Comment, Item, ItemState, Label, MergeState, PrFile, PullInfo,
    PullRefs, ReviewState, Reviewer, timestamp,
};
use crate::core::git::diff::FileStatus;

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

#[derive(Deserialize)]
pub(super) struct RawUser {
    #[serde(default)]
    login: String,
}

#[derive(Deserialize)]
pub(super) struct RawLabel {
    #[serde(default)]
    name: String,
    #[serde(default)]
    color: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct RawPrRef {
    #[serde(default)]
    merged_at: Option<String>,
}

/// An entry of `/issues` — which also lists pull requests, marked by the
/// presence of `pull_request`.
#[derive(Deserialize)]
pub(super) struct RawIssue {
    pub(super) number: u64,
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
    pub(super) body: Option<String>,
    /// Only with the `full` media type.
    #[serde(default)]
    pub(super) body_html: Option<String>,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    pub(super) pull_request: Option<RawPrRef>,
    #[serde(default)]
    draft: Option<bool>,
}

#[derive(Deserialize, Default)]
pub(super) struct RawBranchRef {
    #[serde(default, rename = "ref")]
    name: String,
    #[serde(default)]
    sha: String,
    #[serde(default)]
    label: String,
}

#[derive(Deserialize)]
pub(super) struct RawTeam {
    #[serde(default)]
    slug: String,
}

/// An entry of `/pulls`, or `/pulls/{n}`.
#[derive(Deserialize)]
pub(super) struct RawPull {
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
    pub(super) requested_reviewers: Vec<RawUser>,
    #[serde(default)]
    pub(super) requested_teams: Vec<RawTeam>,
}

#[derive(Deserialize)]
pub(super) struct RawComment {
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
pub(super) struct RawFile {
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
pub(super) struct RawReview {
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    state: String,
}

/// `/commits/{sha}/check-runs`.
#[derive(Deserialize)]
pub(super) struct RawCheckRuns {
    #[serde(default)]
    pub(super) total_count: usize,
    #[serde(default)]
    pub(super) check_runs: Vec<RawCheckRun>,
}

#[derive(Deserialize)]
pub(super) struct RawCheckRun {
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
pub(super) struct RawCombinedStatus {
    #[serde(default)]
    pub(super) total_count: usize,
    #[serde(default)]
    pub(super) statuses: Vec<RawStatus>,
}

#[derive(Deserialize)]
pub(super) struct RawStatus {
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

impl RawIssue {
    pub(super) fn is_pr(&self) -> bool {
        self.pull_request.is_some()
    }

    pub(super) fn into_item(self) -> Item {
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
            pull: None,
        }
    }
}

impl RawPull {
    pub(super) fn into_item(self) -> (Item, PullInfo) {
        let state = item_state(
            &self.state,
            None,
            self.merged_at.is_some(),
            self.draft.unwrap_or(false),
        );
        let head = self.head.unwrap_or_default();
        let info = PullInfo {
            refs: PullRefs {
                head_ref: head.name,
                head_label: head.label,
                base_ref: self.base.map(|b| b.name).unwrap_or_default(),
            },
            head_sha: head.sha,
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
            pull: Some(info.refs.clone()),
        };
        (item, info)
    }
}

impl RawCheckRun {
    pub(super) fn into_check(self) -> Check {
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
    pub(super) fn into_check(self) -> Check {
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
pub(super) fn checks(runs: RawCheckRuns, statuses: RawCombinedStatus) -> Checks {
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
pub(super) fn reviewers(
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
    pub(super) fn head_sha(&self) -> &str {
        self.head.as_ref().map_or("", |h| h.sha.as_str())
    }

    /// The teams asked to review, as `org/team` when the org is known.
    pub(super) fn requested_team_names(&self, org: &str) -> Vec<String> {
        self.requested_teams
            .iter()
            .filter(|t| !t.slug.is_empty())
            .map(|t| format!("{org}/{}", t.slug))
            .collect()
    }
}

impl RawComment {
    pub(super) fn into_comment(self) -> Comment {
        Comment {
            id: self.id,
            author: login(self.user),
            body: crate::core::forge::markdown::sign_attachments(
                self.body.as_deref().unwrap_or_default(),
                self.body_html.as_deref().unwrap_or_default(),
            ),
            created_at: timestamp(&self.created_at),
            html_url: self.html_url,
        }
    }
}

impl RawFile {
    pub(super) fn into_file(self) -> PrFile {
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
}

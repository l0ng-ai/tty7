//! GitLab's REST JSON, and how it maps onto the forge model.
//!
//! As in the GitHub model, the wire structs (`Raw*`) mirror only the fields
//! read, so a field GitLab adds or drops later cannot break decoding; the
//! mapping functions make the decisions the panel draws from — one
//! [`ItemState`], one [`CheckState`] per job, one [`MergeState`].

use std::collections::HashSet;

use serde::Deserialize;

use crate::core::forge::model::{
    Check, CheckState, Checks, Comment, Item, ItemState, Label, MergeState, PrFile, PullInfo,
    PullRefs, ReviewState, Reviewer,
};
use crate::core::forge::remote::RepoSlug;
use crate::core::git::diff::FileStatus;

/// Open in the panel's sense: what the open list shows, drafts included.
pub(super) fn is_open(state: ItemState) -> bool {
    matches!(state, ItemState::Open | ItemState::Draft)
}

/// GitLab's `2026-09-01T10:00:00.123Z`, as unix seconds (zero if
/// unparseable). The fraction is dropped: the shared parser reads git's
/// whole-second timestamps.
pub(super) fn timestamp(text: &str) -> i64 {
    let whole = match text.get(19..) {
        Some(rest) if rest.starts_with('.') => {
            let digits = rest[1..].bytes().take_while(u8::is_ascii_digit).count();
            format!("{}{}", &text[..19], &rest[1 + digits..])
        }
        _ => text.to_string(),
    };
    crate::core::forge::model::timestamp(&whole)
}

/// GitLab Markdown's relative links made absolute, so they open in a
/// browser: `/uploads/…` is the project's own (an attachment), any other
/// `/…` is the host's (`/group/p/-/issues/3`, `/-/project/9/uploads/…`). In
/// Markdown link targets and raw `src`/`href` attributes alike; a
/// protocol-relative `//…` is left as it is.
pub(super) fn absolute_links(slug: &RepoSlug, text: &str) -> String {
    let host = format!("https://{}", slug.host);
    let uploads = format!("{}/uploads/", slug.web_url());
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((at, open)) = ["](/", "src=\"/", "href=\"/"]
        .iter()
        .filter_map(|p| rest.find(p).map(|i| (i, *p)))
        .min()
    {
        let lead = at + open.len() - 1;
        out.push_str(&rest[..lead]);
        let target = &rest[lead..];
        if let Some(path) = target.strip_prefix("/uploads/") {
            out.push_str(&uploads);
            rest = path;
        } else if target.starts_with("//") {
            out.push('/');
            rest = &target[1..];
        } else {
            out.push_str(&host);
            rest = target;
        }
    }
    out.push_str(rest);
    out
}

/// One reviewer per person: what the reviewers list says of them, overruled
/// by an approval, which is GitLab's one recorded verdict.
pub(super) fn merge_reviewers(
    author: &str,
    reviewers: Option<Vec<RawReviewer>>,
    approvals: Option<RawApprovals>,
) -> Option<Vec<Reviewer>> {
    if reviewers.is_none() && approvals.is_none() {
        return None;
    }
    let mut out: Vec<Reviewer> = Vec::new();
    for r in reviewers.into_iter().flatten() {
        let login = r.user.map(|u| u.username).unwrap_or_default();
        if login.is_empty() || out.iter().any(|o| o.login == login) {
            continue;
        }
        let state = match r.state.as_str() {
            "approved" => ReviewState::Approved,
            "requested_changes" => ReviewState::ChangesRequested,
            // "unapproved": an approval taken back, which leaves a review.
            "reviewed" | "unapproved" => ReviewState::Commented,
            // "unreviewed", "review_started"
            _ => ReviewState::Requested,
        };
        out.push(Reviewer { login, state });
    }
    for a in approvals.map(|a| a.approved_by).into_iter().flatten() {
        let login = a.user.map(|u| u.username).unwrap_or_default();
        if login.is_empty() || login == author {
            continue;
        }
        match out.iter_mut().find(|r| r.login == login) {
            Some(r) => r.state = ReviewState::Approved,
            None => out.push(Reviewer {
                login,
                state: ReviewState::Approved,
            }),
        }
    }
    Some(out)
}

/// Jobs, then each external status no job already stands for. GitLab lists a
/// pipeline's own jobs among the commit's statuses too, and a second
/// pipeline on the same commit repeats the job names; both are the same
/// check, and the head pipeline's word on it is the one that counts.
pub(super) fn pipeline_checks(jobs: Vec<RawJob>, statuses: Vec<RawJob>, truncated: bool) -> Checks {
    let ids: HashSet<u64> = jobs.iter().map(|j| j.id).collect();
    let mut names: HashSet<String> = jobs.iter().map(|j| j.name.clone()).collect();
    let mut items: Vec<Check> = jobs.into_iter().map(RawJob::into_check).collect();
    for status in statuses {
        if !ids.contains(&status.id) && names.insert(status.name.clone()) {
            items.push(status.into_check());
        }
    }
    let mut checks = Checks { items, truncated };
    checks.sort();
    checks
}

// ---- wire ------------------------------------------------------------------

#[derive(Deserialize)]
pub(super) struct RawUser {
    #[serde(default)]
    pub(super) username: String,
}

/// A label: a bare name, or with `with_labels_details` an object with its
/// colour.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawLabel {
    Name(String),
    Detailed {
        #[serde(default)]
        name: String,
        #[serde(default)]
        color: Option<String>,
    },
}

fn labels(raw: Vec<RawLabel>) -> Vec<Label> {
    raw.into_iter()
        .map(|l| match l {
            RawLabel::Name(name) => Label { name, color: None },
            RawLabel::Detailed { name, color } => Label {
                name,
                color: color
                    .as_deref()
                    .and_then(|c| c.strip_prefix('#'))
                    .filter(|c| c.len() == 6)
                    .and_then(|c| u32::from_str_radix(c, 16).ok()),
            },
        })
        .filter(|l| !l.name.is_empty())
        .collect()
}

#[derive(Deserialize)]
pub(super) struct RawIssue {
    iid: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    author: Option<RawUser>,
    #[serde(default)]
    labels: Vec<RawLabel>,
    #[serde(default)]
    user_notes_count: u32,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    web_url: String,
    #[serde(default)]
    pub(super) description: Option<String>,
}

impl RawIssue {
    pub(super) fn into_item(self) -> Item {
        Item {
            number: self.iid,
            title: self.title,
            state: match self.state.as_str() {
                "closed" => ItemState::Closed,
                _ => ItemState::Open,
            },
            is_pr: false,
            author: self.author.map(|u| u.username).unwrap_or_default(),
            labels: labels(self.labels),
            comments: self.user_notes_count,
            created_at: timestamp(&self.created_at),
            updated_at: timestamp(&self.updated_at),
            html_url: self.web_url,
            pull: None,
        }
    }
}

#[derive(Deserialize)]
pub(super) struct RawPipeline {
    #[serde(default)]
    pub(super) id: u64,
}

#[derive(Deserialize)]
pub(super) struct RawMr {
    iid: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    draft: Option<bool>,
    /// The pre-15.0 name of `draft`.
    #[serde(default)]
    work_in_progress: Option<bool>,
    #[serde(default)]
    merged_at: Option<String>,
    #[serde(default)]
    pub(super) author: Option<RawUser>,
    #[serde(default)]
    labels: Vec<RawLabel>,
    #[serde(default)]
    user_notes_count: u32,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    web_url: String,
    #[serde(default)]
    pub(super) description: Option<String>,
    #[serde(default)]
    source_branch: String,
    #[serde(default)]
    target_branch: String,
    #[serde(default)]
    pub(super) source_project_id: Option<u64>,
    #[serde(default)]
    pub(super) target_project_id: Option<u64>,
    #[serde(default)]
    pub(super) sha: Option<String>,
    /// Only on `/merge_requests/:iid`.
    #[serde(default)]
    pub(super) head_pipeline: Option<RawPipeline>,
    #[serde(default)]
    detailed_merge_status: Option<String>,
    /// Deprecated in 15.6 for `detailed_merge_status`; older servers have
    /// only this.
    #[serde(default)]
    merge_status: Option<String>,
    #[serde(default)]
    has_conflicts: Option<bool>,
    /// A string, `"1000+"` once GitLab stops counting.
    #[serde(default)]
    changes_count: Option<String>,
}

impl RawMr {
    pub(super) fn into_item(self, slug: &RepoSlug) -> (Item, PullInfo) {
        let draft = self.draft.or(self.work_in_progress).unwrap_or(false);
        let state = match self.state.as_str() {
            _ if self.merged_at.as_deref().is_some_and(|m| !m.is_empty()) => ItemState::Merged,
            "merged" => ItemState::Merged,
            "closed" => ItemState::Closed,
            // "opened", and "locked": an open MR whose discussion is locked.
            _ if draft => ItemState::Draft,
            _ => ItemState::Open,
        };
        // The stack builder ties a base branch to a head by `owner:branch`;
        // a fork's head gets a prefix no namespace can have, so it never
        // reads as this project's branch.
        let head_label = if self.source_project_id == self.target_project_id {
            format!("{}:{}", slug.owner, self.source_branch)
        } else {
            format!(
                "!{}:{}",
                self.source_project_id.unwrap_or_default(),
                self.source_branch
            )
        };
        let merge_state = merge_state(
            self.detailed_merge_status.as_deref(),
            self.merge_status.as_deref(),
            self.has_conflicts.unwrap_or(false),
            draft,
        );
        let info = PullInfo {
            refs: PullRefs {
                head_ref: self.source_branch,
                head_label,
                base_ref: self.target_branch,
            },
            head_sha: self.sha.unwrap_or_default(),
            additions: 0,
            deletions: 0,
            changed_files: self
                .changes_count
                .as_deref()
                .and_then(|c| c.trim_end_matches('+').parse().ok())
                .unwrap_or(0),
            commits: 0,
            merge_state,
        };
        let item = Item {
            number: self.iid,
            title: self.title,
            state,
            is_pr: true,
            author: self.author.map(|u| u.username).unwrap_or_default(),
            labels: labels(self.labels),
            comments: self.user_notes_count,
            created_at: timestamp(&self.created_at),
            updated_at: timestamp(&self.updated_at),
            html_url: self.web_url,
            pull: Some(info.refs.clone()),
        };
        (item, info)
    }
}

/// GitLab's merge check, in GitHub's terms: what the merge box would lead
/// with. Checks failing or running are told apart by the checks themselves,
/// so every "a rule says no" status is `Blocked`.
fn merge_state(
    detailed: Option<&str>,
    legacy: Option<&str>,
    conflicts: bool,
    draft: bool,
) -> MergeState {
    if draft {
        return MergeState::Draft;
    }
    if conflicts {
        return MergeState::Dirty;
    }
    match detailed {
        Some("mergeable") => MergeState::Clean,
        Some("conflict" | "broken_status") => MergeState::Dirty,
        Some("need_rebase") => MergeState::Behind,
        Some("draft_status") => MergeState::Draft,
        // Still being worked out, or not open.
        Some("unchecked" | "checking" | "preparing" | "approvals_syncing" | "not_open") => {
            MergeState::Unknown
        }
        Some(_) => MergeState::Blocked,
        None => match legacy {
            Some("can_be_merged") => MergeState::Clean,
            Some("cannot_be_merged") => MergeState::Dirty,
            _ => MergeState::Unknown,
        },
    }
}

#[derive(Deserialize)]
pub(super) struct RawNote {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    author: Option<RawUser>,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    system: bool,
}

#[derive(Deserialize)]
pub(super) struct RawDiff {
    #[serde(default)]
    old_path: String,
    #[serde(default)]
    new_path: String,
    #[serde(default)]
    new_file: bool,
    #[serde(default)]
    renamed_file: bool,
    #[serde(default)]
    deleted_file: bool,
    #[serde(default)]
    diff: String,
}

impl RawDiff {
    pub(super) fn into_file(self) -> PrFile {
        let status = if self.new_file {
            FileStatus::Added
        } else if self.deleted_file {
            FileStatus::Deleted
        } else if self.renamed_file {
            FileStatus::Renamed
        } else {
            FileStatus::Modified
        };
        // GitLab sends no counts; the hunks are counted instead. A diff that
        // is not hunks (a binary file's one-line notice) or was left out as
        // too large has no patch to show.
        let (mut additions, mut deletions) = (0, 0);
        for line in self.diff.lines() {
            match line.as_bytes().first() {
                Some(b'+') => additions += 1,
                Some(b'-') => deletions += 1,
                _ => {}
            }
        }
        let patch = self.diff.starts_with("@@").then_some(self.diff);
        PrFile {
            old_path: (self.renamed_file && self.old_path != self.new_path)
                .then_some(self.old_path),
            path: self.new_path,
            status,
            additions,
            deletions,
            patch,
        }
    }
}

#[derive(Deserialize)]
pub(super) struct RawChanges {
    #[serde(default)]
    pub(super) changes: Vec<RawDiff>,
    #[serde(default)]
    pub(super) overflow: bool,
}

/// A pipeline job, or a commit status — both are GitLab commit statuses, and
/// carry the same fields.
#[derive(Deserialize)]
pub(super) struct RawJob {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    allow_failure: bool,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    finished_at: Option<String>,
    /// A job's page.
    #[serde(default)]
    web_url: Option<String>,
    /// An external status's page.
    #[serde(default)]
    target_url: Option<String>,
}

impl RawJob {
    fn into_check(self) -> Check {
        let state = match self.status.as_str() {
            "success" => CheckState::Passed,
            "failed" if self.allow_failure => CheckState::Skipped,
            "failed" => CheckState::Failed,
            // Waiting on a person, or never going to run: no verdict.
            "canceled" | "canceling" | "skipped" | "manual" => CheckState::Skipped,
            // created, pending, running, preparing, scheduled,
            // waiting_for_resource, waiting_for_callback
            _ => CheckState::Pending,
        };
        Check {
            name: self.name,
            state,
            started_at: self.started_at.as_deref().map_or(0, timestamp),
            completed_at: match state {
                CheckState::Pending => 0,
                _ => self.finished_at.as_deref().map_or(0, timestamp),
            },
            url: self.web_url.or(self.target_url).filter(|u| !u.is_empty()),
        }
    }
}

#[derive(Deserialize)]
pub(super) struct RawReviewer {
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    state: String,
}

#[derive(Deserialize)]
struct RawApprover {
    #[serde(default)]
    user: Option<RawUser>,
}

#[derive(Deserialize)]
pub(super) struct RawApprovals {
    #[serde(default)]
    approved_by: Vec<RawApprover>,
}

#[derive(Deserialize)]
pub(super) struct RawProject {
    pub(super) id: u64,
}

impl RawNote {
    /// A user note as a comment, linked by its anchor on the item's page.
    /// `None` for one of GitLab's own system notes.
    pub(super) fn into_comment(self, slug: &RepoSlug, item_url: &str) -> Option<Comment> {
        (!self.system).then(|| Comment {
            id: self.id,
            author: self.author.map(|u| u.username).unwrap_or_default(),
            body: absolute_links(slug, self.body.as_deref().unwrap_or_default()),
            created_at: timestamp(&self.created_at),
            html_url: format!("{item_url}#note_{}", self.id),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_status_maps_to_the_merge_box() {
        assert_eq!(
            merge_state(Some("mergeable"), None, false, false),
            MergeState::Clean
        );
        assert_eq!(
            merge_state(Some("conflict"), None, false, false),
            MergeState::Dirty
        );
        assert_eq!(
            merge_state(Some("need_rebase"), None, false, false),
            MergeState::Behind
        );
        assert_eq!(
            merge_state(Some("ci_must_pass"), None, false, false),
            MergeState::Blocked
        );
        assert_eq!(
            merge_state(Some("checking"), None, false, false),
            MergeState::Unknown
        );
        assert_eq!(
            merge_state(Some("mergeable"), None, false, true),
            MergeState::Draft
        );
        assert_eq!(
            merge_state(Some("mergeable"), None, true, false),
            MergeState::Dirty
        );
        assert_eq!(
            merge_state(None, Some("can_be_merged"), false, false),
            MergeState::Clean
        );
        assert_eq!(
            merge_state(None, Some("unchecked"), false, false),
            MergeState::Unknown
        );
    }

    #[test]
    fn relative_links_become_absolute() {
        let slug = RepoSlug::gitlab("gitlab.example.com", "team", "tools");
        assert_eq!(
            absolute_links(
                &slug,
                "![a](/uploads/x/a.png) [b](/team/tools/-/issues/3) \
                 <img src=\"/-/project/9/uploads/y/c.png\"> [d](//cdn.io/d) [e](https://e.io/f)"
            ),
            "![a](https://gitlab.example.com/team/tools/uploads/x/a.png) \
             [b](https://gitlab.example.com/team/tools/-/issues/3) \
             <img src=\"https://gitlab.example.com/-/project/9/uploads/y/c.png\"> \
             [d](//cdn.io/d) [e](https://e.io/f)"
        );
    }

    #[test]
    fn timestamps_drop_their_fraction_and_keep_their_offset() {
        assert_eq!(timestamp("2026-09-01T10:00:00.000Z"), 1_788_256_800);
        assert_eq!(timestamp("2026-09-20T08:30:00.512+02:00"), 1_789_885_800);
        assert_eq!(timestamp("2026-09-01T10:00:00Z"), 1_788_256_800);
        assert_eq!(timestamp("garbage"), 0);
    }
}

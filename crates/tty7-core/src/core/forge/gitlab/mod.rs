//! The same calls as [`super::api`], answered by GitLab's REST API (v4):
//! the requests here, the JSON and its mapping in `model`, the sign-in in
//! [`glab`].
//!
//! Everything comes out as the GitHub-shaped model the panel already draws —
//! a merge request is an [`Item`] with `is_pr`, its pipeline's jobs are
//! [`Check`](super::model::Check)s — so the UI needs to know which forge it
//! is looking at only for wording. What does not map one to one:
//!
//! - **Numbers.** Issues and merge requests are numbered apart (`#5` and `!5`
//!   are two things), so a detail is asked for by kind as well as number.
//! - **Closed.** GitHub's "closed" includes merged pull requests; GitLab's
//!   `state=closed` does not, so the closed list reads `state=all` and keeps
//!   everything that is not open. Pages can filter down short, as `/issues`
//!   does on GitHub, and the panel already reads on through those.
//! - **Notes.** A conversation's notes include GitLab's own system notes
//!   ("added 1 commit", "changed the description"); those are not comments.
//! - **Links.** An attachment is a project-relative `/uploads/…` link, and
//!   other references are host-relative. Both are made absolute; an image not
//!   on GitHub is drawn as a link — a private project's upload needs a
//!   signed-in browser to load anyway.
//! - **Checks.** The head pipeline's jobs, plus whatever external commit
//!   statuses the commit carries. A failed job that is allowed to fail, and a
//!   manual, skipped or cancelled one, have no verdict either way.

pub mod glab;
mod model;

use super::api::{
    ApiError, DETAIL_PAGE, ListPage, ListQuery, PAGE_SIZE, Reply, Transport, decode, escape_query,
    join,
};
use super::model::{Checks, Comment, Detail, Item, Kind, PrFile, StateFilter};
use super::remote::RepoSlug;
use model::{
    RawApprovals, RawChanges, RawDiff, RawIssue, RawJob, RawMr, RawNote, RawProject, RawReviewer,
    absolute_links, is_open, merge_reviewers, pipeline_checks,
};

/// `/projects/group%2Fsub%2Fname` — GitLab takes a project by its URL-encoded
/// full path wherever it takes one by id.
fn project_path(slug: &RepoSlug) -> String {
    format!("/projects/{}", escape_query(&slug.full()))
}

pub(crate) fn list(t: &dyn Transport, q: &ListQuery, page: u32) -> Result<ListPage, ApiError> {
    let page = page.max(1);
    let (endpoint, state) = match (q.kind, q.state) {
        (Kind::Issues, StateFilter::Open) => ("issues", "opened"),
        (Kind::Issues, StateFilter::Closed) => ("issues", "closed"),
        (Kind::Pulls, StateFilter::Open) => ("merge_requests", "opened"),
        (Kind::Pulls, StateFilter::Closed) => ("merge_requests", "all"),
    };
    let mut path = format!(
        "{}/{endpoint}?state={state}&order_by=updated_at&sort=desc\
         &with_labels_details=true&per_page={PAGE_SIZE}&page={page}",
        project_path(&q.slug)
    );
    if let Some(label) = q.label.as_deref().filter(|l| !l.is_empty()) {
        path.push_str("&labels=");
        path.push_str(&escape_query(label));
    }
    let reply = t.get(&path)?;
    let items = match q.kind {
        Kind::Issues => decode::<Vec<RawIssue>>(&reply)?
            .into_iter()
            .map(RawIssue::into_item)
            .collect(),
        Kind::Pulls => decode::<Vec<RawMr>>(&reply)?
            .into_iter()
            .map(|m| m.into_item(&q.slug).0)
            .filter(|i| q.state == StateFilter::Open || !is_open(i.state))
            .collect(),
    };
    Ok(ListPage {
        items,
        next_page: reply.has_next.then_some(page + 1),
    })
}

pub(crate) fn detail(
    t: &dyn Transport,
    slug: &RepoSlug,
    number: u64,
    is_pr: bool,
) -> Result<Detail, ApiError> {
    let what = if is_pr { "merge_requests" } else { "issues" };
    let item_path = format!("{}/{what}/{number}", project_path(slug));
    let notes_path =
        format!("{item_path}/notes?sort=asc&order_by=created_at&per_page={DETAIL_PAGE}");
    if !is_pr {
        let (issue, notes) = std::thread::scope(|s| {
            let notes = s.spawn(|| t.get(&notes_path));
            (t.get(&item_path), join(notes))
        });
        let issue: RawIssue = decode(&issue?)?;
        let body = absolute_links(slug, issue.description.as_deref().unwrap_or_default());
        let item = issue.into_item();
        let (comments, comments_truncated) = comments(slug, &item.html_url, &notes?)?;
        return Ok(Detail {
            item,
            body,
            comments,
            comments_truncated,
            pull: None,
            files: None,
            files_truncated: false,
            checks: None,
            reviewers: None,
        });
    }

    // Every request but the checks needs only the number, so they all go out
    // together; the checks need the head pipeline, and follow the MR.
    let (mr, notes, diffs, commits, reviewers, approvals, checks) = std::thread::scope(|s| {
        let notes = s.spawn(|| t.get(&notes_path));
        let diffs = s.spawn(|| diffs(t, &item_path));
        // One commit is enough: the count is the page's `X-Total`.
        let commits = s.spawn(|| t.get(&format!("{item_path}/commits?per_page=1")));
        let reviewers = s.spawn(|| t.get(&format!("{item_path}/reviewers")));
        let approvals = s.spawn(|| t.get(&format!("{item_path}/approvals")));
        let mr = t.get(&item_path).and_then(|r| decode::<RawMr>(&r));
        let checks = mr.as_ref().ok().and_then(|mr| {
            mr_checks(t, slug, mr)
                .inspect_err(|e| log::warn!("gitlab: checks of {}!{number}: {e}", slug.full()))
                .ok()
                .flatten()
        });
        (
            mr,
            join(notes),
            join(diffs),
            join(commits),
            join(reviewers),
            join(approvals),
            checks,
        )
    });
    let mr = mr?;
    let body = absolute_links(slug, mr.description.as_deref().unwrap_or_default());
    let author = mr.author.as_ref().map(|u| u.username.clone());
    let (item, mut info) = mr.into_item(slug);
    let (comments, comments_truncated) = comments(slug, &item.html_url, &notes?)?;
    let (files, files_truncated) = diffs?;
    info.additions = files.iter().map(|f| f.additions).sum();
    info.deletions = files.iter().map(|f| f.deletions).sum();
    if info.changed_files == 0 {
        info.changed_files = files.len() as u32;
    }
    // Like GitHub's checks and reviews: the panel's summary, not the MR. One
    // that cannot be read is left out rather than failing the whole view.
    if let Some(total) = commits.ok().and_then(|r| r.total) {
        info.commits = total;
    }
    let reviewers = merge_reviewers(
        author.as_deref().unwrap_or_default(),
        reviewers.and_then(|r| decode::<Vec<RawReviewer>>(&r)).ok(),
        approvals.and_then(|r| decode::<RawApprovals>(&r)).ok(),
    );
    Ok(Detail {
        item,
        body,
        comments,
        comments_truncated,
        pull: Some(info),
        files: Some(files),
        files_truncated,
        checks,
        reviewers,
    })
}

/// The MR's changed files: `/diffs` (GitLab 15.7 and later, paged), else the
/// older `/changes`, which GitLab caps by itself and says when it did.
fn diffs(t: &dyn Transport, item_path: &str) -> Result<(Vec<PrFile>, bool), ApiError> {
    let (raw, truncated) = match t.get(&format!("{item_path}/diffs?per_page={DETAIL_PAGE}")) {
        Ok(reply) => (decode::<Vec<RawDiff>>(&reply)?, reply.has_next),
        Err(ApiError::NotFound) => {
            let raw: RawChanges = decode(&t.get(&format!("{item_path}/changes"))?)?;
            (raw.changes, raw.overflow)
        }
        Err(e) => return Err(e),
    };
    Ok((raw.into_iter().map(RawDiff::into_file).collect(), truncated))
}

/// The user notes of a conversation, oldest first.
fn comments(
    slug: &RepoSlug,
    item_url: &str,
    reply: &Reply,
) -> Result<(Vec<Comment>, bool), ApiError> {
    let comments = decode::<Vec<RawNote>>(reply)?
        .into_iter()
        .filter_map(|n| n.into_comment(slug, item_url))
        .collect();
    Ok((comments, reply.has_next))
}

/// The checks of merge request `number`, re-read for a poll: the MR first,
/// for its current head pipeline, then that pipeline's jobs.
pub(crate) fn checks(t: &dyn Transport, slug: &RepoSlug, number: u64) -> Result<Checks, ApiError> {
    let mr: RawMr = decode(&t.get(&format!("{}/merge_requests/{number}", project_path(slug)))?)?;
    Ok(mr_checks(t, slug, &mr)?.unwrap_or_default())
}

/// The head pipeline's jobs and the head commit's external statuses. `None`
/// when the MR has no head commit to ask about.
fn mr_checks(t: &dyn Transport, slug: &RepoSlug, mr: &RawMr) -> Result<Option<Checks>, ApiError> {
    let base = project_path(slug);
    let sha = mr.sha.as_deref().unwrap_or_default();
    if sha.is_empty() {
        return Ok(None);
    }
    let pipeline = mr.head_pipeline.as_ref().map(|p| p.id).filter(|&id| id > 0);
    let (jobs, statuses) = std::thread::scope(|s| {
        let statuses = s.spawn(|| {
            t.get(&format!(
                "{base}/repository/commits/{}/statuses?per_page={DETAIL_PAGE}",
                super::remote::escape_path(sha)
            ))
        });
        let jobs = pipeline.map(|id| {
            t.get(&format!(
                "{base}/pipelines/{id}/jobs?per_page={DETAIL_PAGE}"
            ))
        });
        (jobs, join(statuses))
    });
    let (jobs, jobs_more): (Vec<RawJob>, bool) = match jobs {
        Some(reply) => {
            let reply = reply?;
            (decode(&reply)?, reply.has_next)
        }
        None => (Vec::new(), false),
    };
    // Statuses are the extra: a token scoped away from them still gets the
    // jobs.
    let (statuses, statuses_more): (Vec<RawJob>, bool) = match statuses {
        Ok(reply) => (decode(&reply).unwrap_or_default(), reply.has_next),
        Err(e) => {
            log::warn!("gitlab: statuses of {}@{sha}: {e}", slug.full());
            (Vec::new(), false)
        }
    };
    Ok(Some(pipeline_checks(
        jobs,
        statuses,
        jobs_more || statuses_more,
    )))
}

/// The merge request branch `branch` of `head` (the repository itself, or a
/// fork of it) has opened against `slug`: the open one if there is one, else
/// the most recently updated.
pub(crate) fn pull_for_branch(
    t: &dyn Transport,
    slug: &RepoSlug,
    head: &RepoSlug,
    branch: &str,
) -> Result<Option<Item>, ApiError> {
    let path = format!(
        "{}/merge_requests?source_branch={}&state=all&order_by=updated_at&sort=desc&per_page=10",
        project_path(slug),
        escape_query(branch)
    );
    let same = head.full().eq_ignore_ascii_case(&slug.full());
    // A fork's MR names the fork by id; that lookup goes out alongside.
    let (raw, fork_id) = std::thread::scope(|s| {
        let fork = (!same).then(|| s.spawn(|| t.get(&project_path(head))));
        let raw = t.get(&path);
        let fork_id = fork
            .map(join)
            .and_then(|r| r.and_then(|r| decode::<RawProject>(&r)).ok())
            .map(|p| p.id);
        (raw, fork_id)
    });
    let items: Vec<Item> = decode::<Vec<RawMr>>(&raw?)?
        .into_iter()
        .filter(|m| match (same, fork_id) {
            (true, _) => m.source_project_id == m.target_project_id,
            (false, Some(id)) => m.source_project_id == Some(id),
            (false, None) => m.source_project_id != m.target_project_id,
        })
        .map(|m| m.into_item(slug).0)
        .collect();
    let open = items.iter().position(|i| is_open(i.state));
    Ok(match open {
        Some(i) => items.into_iter().nth(i),
        None => items.into_iter().next(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::forge::api::tests::Fixture;
    use crate::core::forge::api::{self as forge_api};
    use crate::core::forge::model::{CheckState, ItemState, MergeState, ReviewState};
    use crate::core::git::diff::FileStatus;

    fn slug() -> RepoSlug {
        RepoSlug::gitlab("gitlab.example.com", "team/infra", "tools")
    }

    const P: &str = "/projects/team%2Finfra%2Ftools";

    fn query(kind: Kind, state: StateFilter, label: Option<&str>) -> ListQuery {
        ListQuery {
            slug: slug(),
            kind,
            state,
            label: label.map(str::to_string),
        }
    }

    const MR_LIST: &str = r##"[
      {"iid": 7, "title": "Add cache", "state": "opened", "draft": true,
       "author": {"username": "ada"},
       "labels": [{"name": "perf", "color": "#d9534f"}, {"name": "odd", "color": "red"}],
       "user_notes_count": 4, "created_at": "2026-09-01T10:00:00.000Z",
       "updated_at": "2026-09-20T08:30:00.512+02:00",
       "web_url": "https://gitlab.example.com/team/infra/tools/-/merge_requests/7",
       "source_branch": "feat/cache", "target_branch": "main",
       "source_project_id": 3, "target_project_id": 3},
      {"iid": 6, "title": "From a fork", "state": "merged", "merged_at": "2026-09-03T10:00:00Z",
       "author": {"username": "bob"}, "labels": ["bug"],
       "source_branch": "fix", "target_branch": "main",
       "source_project_id": 9, "target_project_id": 3},
      {"iid": 5, "title": "Dropped", "state": "closed", "work_in_progress": false,
       "source_branch": "x", "target_branch": "main"}
    ]"##;

    #[test]
    fn open_merge_requests_list_with_labels_and_branches() {
        let mut t = Fixture::new();
        t.on(
            &format!(
                "{P}/merge_requests?state=opened&order_by=updated_at&sort=desc\
                 &with_labels_details=true&per_page=50&page=1"
            ),
            MR_LIST,
            true,
        );
        let page = forge_api::list(&t, &query(Kind::Pulls, StateFilter::Open, None), 1).unwrap();
        assert_eq!(page.next_page, Some(2));
        let first = &page.items[0];
        assert_eq!((first.number, first.state), (7, ItemState::Draft));
        assert!(first.is_pr);
        assert_eq!(first.author, "ada");
        assert_eq!(first.labels[0].color, Some(0xd9534f));
        assert_eq!(first.labels[1].color, None, "a named colour is not hex");
        assert_eq!(first.comments, 4);
        assert_eq!(first.created_at, 1_788_256_800, "milliseconds are dropped");
        assert_eq!(first.updated_at, 1_789_885_800, "the offset is honoured");
        let refs = first.pull.as_ref().unwrap();
        assert_eq!(refs.head_label, "team/infra:feat/cache");
        assert_eq!(page.items[1].state, ItemState::Merged);
        assert_eq!(page.items[1].labels[0].name, "bug");
        assert!(
            page.items[1]
                .pull
                .as_ref()
                .unwrap()
                .head_label
                .starts_with('!'),
            "a fork's branch is not this project's"
        );
        assert_eq!(page.items[2].state, ItemState::Closed);
    }

    #[test]
    fn closed_merge_requests_include_merged_and_drop_open() {
        let mut t = Fixture::new();
        t.on(
            &format!(
                "{P}/merge_requests?state=all&order_by=updated_at&sort=desc\
                 &with_labels_details=true&per_page=50&page=1&labels=good%20first"
            ),
            MR_LIST,
            false,
        );
        let page = forge_api::list(
            &t,
            &query(Kind::Pulls, StateFilter::Closed, Some("good first")),
            1,
        )
        .unwrap();
        let numbers: Vec<u64> = page.items.iter().map(|i| i.number).collect();
        assert_eq!(numbers, vec![6, 5]);
        assert_eq!(page.next_page, None);
    }

    #[test]
    fn issues_list_from_issues() {
        let mut t = Fixture::new();
        t.on(
            &format!(
                "{P}/issues?state=closed&order_by=updated_at&sort=desc\
                 &with_labels_details=true&per_page=50&page=1"
            ),
            r#"[{"iid": 3, "title": "Broken", "state": "closed", "author": null,
                 "labels": [], "user_notes_count": 1,
                 "web_url": "https://gitlab.example.com/team/infra/tools/-/issues/3"}]"#,
            false,
        );
        let page = forge_api::list(&t, &query(Kind::Issues, StateFilter::Closed, None), 1).unwrap();
        assert_eq!(page.items.len(), 1);
        let i = &page.items[0];
        assert_eq!((i.number, i.state, i.is_pr), (3, ItemState::Closed, false));
        assert_eq!(i.author, "");
    }

    #[test]
    fn an_issue_detail_drops_system_notes_and_links_uploads() {
        let mut t = Fixture::new();
        t.on(
            &format!("{P}/issues/3"),
            r#"{"iid": 3, "title": "Broken", "state": "opened",
                "web_url": "https://gitlab.example.com/team/infra/tools/-/issues/3",
                "description": "See ![shot](/uploads/abc/shot.png)"}"#,
            false,
        );
        t.on(
            &format!("{P}/issues/3/notes?sort=asc&order_by=created_at&per_page=100"),
            r#"[{"id": 11, "body": "changed the description", "system": true,
                 "author": {"username": "ada"}, "created_at": "2026-09-02T10:00:00Z"},
                {"id": 12, "body": "Same here", "system": false,
                 "author": {"username": "bob"}, "created_at": "2026-09-02T11:00:00Z"}]"#,
            true,
        );
        let d = forge_api::detail(&t, &slug(), 3, false).unwrap();
        assert_eq!(
            d.body,
            "See ![shot](https://gitlab.example.com/team/infra/tools/uploads/abc/shot.png)"
        );
        assert_eq!(d.comments.len(), 1);
        assert_eq!(d.comments[0].author, "bob");
        assert_eq!(
            d.comments[0].html_url,
            "https://gitlab.example.com/team/infra/tools/-/issues/3#note_12"
        );
        assert!(d.comments_truncated);
        assert!(d.pull.is_none() && d.checks.is_none());
    }

    fn mr_fixture(t: &mut Fixture, mr: &str) {
        t.on(&format!("{P}/merge_requests/7"), mr, false);
        t.on(
            &format!("{P}/merge_requests/7/notes?sort=asc&order_by=created_at&per_page=100"),
            "[]",
            false,
        );
        t.on(
            &format!("{P}/merge_requests/7/commits?per_page=1"),
            r#"[{"id": "a"}]"#,
            false,
        );
        t.total(&format!("{P}/merge_requests/7/commits?per_page=1"), 2);
    }

    const MR_7: &str = r#"{"iid": 7, "title": "Add cache", "state": "opened",
        "author": {"username": "ada"}, "source_branch": "feat/cache", "target_branch": "main",
        "source_project_id": 3, "target_project_id": 3, "sha": "abc123",
        "head_pipeline": {"id": 99, "status": "running"},
        "detailed_merge_status": "not_approved", "changes_count": "2"}"#;

    #[test]
    fn a_merge_request_detail_reads_files_jobs_and_reviewers() {
        let mut t = Fixture::new();
        mr_fixture(&mut t, MR_7);
        t.on(
            &format!("{P}/merge_requests/7/diffs?per_page=100"),
            r#"[{"old_path": "a.rs", "new_path": "a.rs", "diff": "@@ -1,2 +1,2 @@\n-x\n+y\n+z\n ctx"},
                {"old_path": "old.png", "new_path": "new.png", "renamed_file": true, "diff": ""},
                {"old_path": "gone", "new_path": "gone", "deleted_file": true,
                 "diff": "Binary files a/gone and /dev/null differ"}]"#,
            false,
        );
        t.on(
            &format!("{P}/pipelines/99/jobs?per_page=100"),
            r#"[{"id": 1, "name": "lint", "status": "success",
                 "started_at": "2026-09-01T10:00:00.1Z", "finished_at": "2026-09-01T10:00:18.9Z",
                 "web_url": "https://gitlab.example.com/j/1"},
                {"id": 2, "name": "test", "status": "running"},
                {"id": 3, "name": "flaky", "status": "failed", "allow_failure": true},
                {"id": 4, "name": "deploy", "status": "manual"},
                {"id": 5, "name": "build", "status": "failed"}]"#,
            false,
        );
        t.on(
            &format!("{P}/repository/commits/abc123/statuses?per_page=100"),
            r#"[{"id": 2, "name": "test", "status": "running"},
                {"id": 50, "name": "lint", "status": "failed"},
                {"id": 60, "name": "ext/sonar", "status": "success",
                 "target_url": "https://sonar.example.com/1"}]"#,
            false,
        );
        t.on(
            &format!("{P}/merge_requests/7/reviewers"),
            r#"[{"user": {"username": "jonas"}, "state": "unreviewed"},
                {"user": {"username": "mara"}, "state": "requested_changes"},
                {"user": {"username": "lee"}, "state": "reviewed"}]"#,
            false,
        );
        t.on(
            &format!("{P}/merge_requests/7/approvals"),
            r#"{"approved_by": [{"user": {"username": "lee"}}, {"user": {"username": "ada"}},
                                {"user": {"username": "kim"}}]}"#,
            false,
        );
        let d = forge_api::detail(&t, &slug(), 7, true).unwrap();
        let pull = d.pull.as_ref().unwrap();
        assert_eq!(pull.head_sha, "abc123");
        assert_eq!(pull.merge_state, MergeState::Blocked);
        assert_eq!((pull.additions, pull.deletions), (2, 1));
        assert_eq!((pull.changed_files, pull.commits), (2, 2));

        let files = d.files.as_ref().unwrap();
        assert!(files[0].patch.is_some());
        assert_eq!(files[1].status, FileStatus::Renamed);
        assert_eq!(files[1].old_path.as_deref(), Some("old.png"));
        assert_eq!(files[2].status, FileStatus::Deleted);
        assert!(files[2].patch.is_none(), "a binary notice is not a patch");

        let checks = d.checks.as_ref().unwrap();
        let got: Vec<(&str, CheckState)> = checks
            .items
            .iter()
            .map(|c| (c.name.as_str(), c.state))
            .collect();
        assert_eq!(
            got,
            vec![
                ("build", CheckState::Failed),
                ("test", CheckState::Pending),
                ("lint", CheckState::Passed),
                ("ext/sonar", CheckState::Passed),
                ("flaky", CheckState::Skipped),
                ("deploy", CheckState::Skipped),
            ],
            "the pipeline's jobs, then statuses no job stands for"
        );
        assert_eq!(
            checks.items[2].completed_at - checks.items[2].started_at,
            18
        );
        assert_eq!(
            checks.items[3].url.as_deref(),
            Some("https://sonar.example.com/1")
        );

        let reviewers: Vec<(&str, ReviewState)> = d
            .reviewers
            .as_ref()
            .unwrap()
            .iter()
            .map(|r| (r.login.as_str(), r.state))
            .collect();
        assert_eq!(
            reviewers,
            vec![
                ("jonas", ReviewState::Requested),
                ("mara", ReviewState::ChangesRequested),
                ("lee", ReviewState::Approved),
                ("kim", ReviewState::Approved),
            ],
            "an approval overrules the review; the author's own is not one"
        );
    }

    #[test]
    fn an_old_server_without_diffs_answers_from_changes() {
        let mut t = Fixture::new();
        mr_fixture(&mut t, MR_7);
        t.on(
            &format!("{P}/merge_requests/7/changes"),
            r#"{"changes": [{"old_path": "a", "new_path": "a", "new_file": true,
                             "diff": "@@ -0,0 +1 @@\n+a"}], "overflow": true}"#,
            false,
        );
        let d = forge_api::detail(&t, &slug(), 7, true).unwrap();
        let files = d.files.unwrap();
        assert_eq!(files[0].status, FileStatus::Added);
        assert!(d.files_truncated);
        // Nothing answers for jobs, statuses, reviewers or approvals.
        assert!(d.reviewers.is_none());
    }

    #[test]
    fn a_poll_follows_the_current_head_pipeline() {
        let mut t = Fixture::new();
        t.on(
            &format!("{P}/merge_requests/7"),
            r#"{"iid": 7, "state": "opened", "sha": "def456", "head_pipeline": {"id": 100}}"#,
            false,
        );
        t.on(
            &format!("{P}/pipelines/100/jobs?per_page=100"),
            r#"[{"id": 1, "name": "test", "status": "success"}]"#,
            false,
        );
        let checks = forge_api::checks(&t, &slug(), 7, "abc123").unwrap();
        assert_eq!(checks.rollup(), Some(CheckState::Passed));
        assert!(
            t.asked
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.contains("/commits/def456/")),
            "the MR's current head, not the one the detail had"
        );
    }

    #[test]
    fn a_branchs_merge_request_is_this_projects_or_the_forks() {
        let list = format!(
            "{P}/merge_requests?source_branch=feat%2Fx&state=all&order_by=updated_at\
             &sort=desc&per_page=10"
        );
        let body = r#"[{"iid": 9, "state": "merged", "source_branch": "feat/x",
                        "source_project_id": 3, "target_project_id": 3},
                       {"iid": 8, "state": "opened", "source_branch": "feat/x",
                        "source_project_id": 40, "target_project_id": 3},
                       {"iid": 4, "state": "opened", "source_branch": "feat/x",
                        "source_project_id": 41, "target_project_id": 3}]"#;
        let mut t = Fixture::new();
        t.on(&list, body, false);
        let own = forge_api::pull_for_branch(&t, &slug(), &slug(), "feat/x")
            .unwrap()
            .unwrap();
        assert_eq!(own.number, 9, "a fork's open MR is not this branch's");

        let fork = RepoSlug::gitlab("gitlab.example.com", "me", "tools");
        t.on("/projects/me%2Ftools", r#"{"id": 41}"#, false);
        let theirs = forge_api::pull_for_branch(&t, &slug(), &fork, "feat/x")
            .unwrap()
            .unwrap();
        assert_eq!(theirs.number, 4);
    }
}

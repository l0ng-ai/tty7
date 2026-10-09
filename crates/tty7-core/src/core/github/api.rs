//! The REST calls the panel makes, over an injected [`Transport`].
//!
//! Everything here is request shaping and response decoding; the bytes move
//! through a `Transport`, which the GUI backs with HTTPS
//! ([`super::http::HttpTransport`]) and tests back with fixtures. No test in
//! this crate touches the network.

use serde::de::DeserializeOwned;

use super::model::{
    Checks, Comment, Detail, Item, ItemState, Kind, PrFile, RawCheckRuns, RawCombinedStatus,
    RawComment, RawFile, RawIssue, RawPull, RawReview, StateFilter,
};
use super::remote::RepoSlug;

/// Rows per page. GitHub's maximum is 100; 50 keeps the first answer quick on
/// a slow link while still filling a tall panel.
pub const PAGE_SIZE: u32 = 50;

/// Comments and files fetched with a detail view. One page each: past that the
/// view says there is more and links to GitHub.
pub const DETAIL_PAGE: u32 = 100;

/// Why a call failed, in the terms the panel explains to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiError {
    /// 401: a token was sent and GitHub refused it (revoked, expired).
    Unauthorized,
    /// 404: no such repository — or a private one this request cannot see,
    /// which GitHub deliberately does not distinguish.
    NotFound,
    /// The rate limit is spent. `reset` is when it refills, unix seconds.
    RateLimited { reset: Option<i64> },
    /// 403 for another reason (SSO enforcement, a blocked token), with
    /// GitHub's own message.
    Forbidden(String),
    /// The request never got an HTTP answer.
    Network(String),
    /// Any other non-success status.
    Http(u16),
    /// A 2xx whose body did not decode.
    Decode(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => f.write_str("GitHub rejected the token (401)"),
            ApiError::NotFound => f.write_str("not found (404)"),
            ApiError::RateLimited { reset } => match reset {
                Some(at) => write!(f, "rate limited until {at}"),
                None => f.write_str("rate limited"),
            },
            ApiError::Forbidden(msg) => write!(f, "forbidden (403): {msg}"),
            ApiError::Network(msg) => write!(f, "network error: {msg}"),
            ApiError::Http(code) => write!(f, "HTTP {code}"),
            ApiError::Decode(msg) => write!(f, "unexpected response: {msg}"),
        }
    }
}

/// A successful answer: the body, and whether the `Link` header offered a
/// next page.
#[derive(Clone, Debug, Default)]
pub struct Reply {
    pub body: Vec<u8>,
    pub has_next: bool,
}

/// One authenticated-or-not GET against `https://api.github.com`.
pub trait Transport: Send + Sync {
    /// `path` starts with `/` and includes the query string.
    fn get(&self, path: &str) -> Result<Reply, ApiError>;
    /// The same GET, asking for the `full` media type: text fields come with
    /// their rendered `*_html` too, which is where GitHub puts the signed
    /// URLs a pasted attachment can actually be downloaded from (see
    /// [`markdown::sign_attachments`](super::markdown::sign_attachments)).
    /// Only the detail asks for it; a list does not need 50 bodies twice.
    fn get_full(&self, path: &str) -> Result<Reply, ApiError> {
        self.get(path)
    }
    /// Whether requests carry a token. Decides how a 404 or a rate limit is
    /// explained: to a signed-out user, both usually mean "sign in".
    fn authenticated(&self) -> bool;
}

/// Map a response's status and headers to success or an [`ApiError`].
///
/// `header` looks a response header up by lower-case name. `body` is only read
/// for GitHub's `message`, which tells a secondary rate limit apart from any
/// other 403.
pub fn classify(
    status: u16,
    header: &dyn Fn(&str) -> Option<String>,
    body: &[u8],
) -> Result<(), ApiError> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    let message = || {
        serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v.get("message")?.as_str().map(str::to_string))
            .unwrap_or_default()
    };
    let reset = || {
        header("x-ratelimit-reset")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .or_else(|| {
                let after = header("retry-after")?.trim().parse::<i64>().ok()?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_secs() as i64;
                Some(now.saturating_add(after))
            })
    };
    match status {
        401 => Err(ApiError::Unauthorized),
        404 => Err(ApiError::NotFound),
        429 => Err(ApiError::RateLimited { reset: reset() }),
        403 => {
            let spent = header("x-ratelimit-remaining").is_some_and(|v| v.trim() == "0");
            let msg = message();
            if spent
                || header("retry-after").is_some()
                || msg.to_ascii_lowercase().contains("rate limit")
            {
                Err(ApiError::RateLimited { reset: reset() })
            } else {
                Err(ApiError::Forbidden(msg))
            }
        }
        code => Err(ApiError::Http(code)),
    }
}

/// Whether a `Link` header names a `rel="next"` page.
pub fn link_has_next(link: &str) -> bool {
    link.split(',').any(|part| {
        part.split(';')
            .skip(1)
            .any(|p| p.trim().eq_ignore_ascii_case("rel=\"next\""))
    })
}

/// What a list shows: which repository, which kind, which state, which label.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ListQuery {
    pub slug: RepoSlug,
    pub kind: Kind,
    pub state: StateFilter,
    pub label: Option<String>,
}

/// One page of a list, and the page after it when there is one.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ListPage {
    pub items: Vec<Item>,
    pub next_page: Option<u32>,
}

/// Which endpoint answers a list query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Endpoint {
    /// `/issues`, keeping only the entries whose kind matches.
    Issues { want_prs: bool },
    /// `/pulls`, which can say draft and merged but cannot filter by label.
    Pulls,
}

fn repo_path(slug: &RepoSlug) -> String {
    format!(
        "/repos/{}/{}",
        super::remote::escape_path(&slug.owner),
        super::remote::escape_path(&slug.name)
    )
}

fn list_request(q: &ListQuery, page: u32) -> (String, Endpoint) {
    let base = repo_path(&q.slug);
    let common = format!(
        "state={}&sort=updated&direction=desc&per_page={PAGE_SIZE}&page={page}",
        q.state.as_query()
    );
    let label = q.label.as_deref().filter(|l| !l.is_empty());
    match (q.kind, label) {
        (Kind::Pulls, None) => (format!("{base}/pulls?{common}"), Endpoint::Pulls),
        (kind, label) => {
            let mut path = format!("{base}/issues?{common}");
            if let Some(label) = label {
                path.push_str("&labels=");
                path.push_str(&escape_query(label));
            }
            (
                path,
                Endpoint::Issues {
                    want_prs: kind == Kind::Pulls,
                },
            )
        }
    }
}

fn escape_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn decode<T: DeserializeOwned>(reply: &Reply) -> Result<T, ApiError> {
    serde_json::from_slice(&reply.body).map_err(|e| ApiError::Decode(e.to_string()))
}

/// Page `page` (1-based) of a list.
///
/// `/issues` interleaves pull requests with issues, so a page of it filtered
/// down to one kind can be short — even empty — while later pages still hold
/// matches. `next_page` follows GitHub's `Link` header, not the row count, so
/// "load more" stays offered for exactly as long as there is more to load.
pub fn list(t: &dyn Transport, q: &ListQuery, page: u32) -> Result<ListPage, ApiError> {
    let page = page.max(1);
    let (path, endpoint) = list_request(q, page);
    let reply = t.get(&path)?;
    let items = match endpoint {
        Endpoint::Pulls => decode::<Vec<RawPull>>(&reply)?
            .into_iter()
            .map(|p| p.into_item().0)
            .collect(),
        Endpoint::Issues { want_prs } => decode::<Vec<RawIssue>>(&reply)?
            .into_iter()
            .filter(|i| i.is_pr() == want_prs)
            .map(RawIssue::into_item)
            .collect(),
    };
    Ok(ListPage {
        items,
        next_page: reply.has_next.then_some(page + 1),
    })
}

/// One issue or pull request with its body and conversation — and, for a pull
/// request, its branches, size and changed files.
pub fn detail(t: &dyn Transport, slug: &RepoSlug, number: u64) -> Result<Detail, ApiError> {
    let base = repo_path(slug);
    // Requests that do not depend on each other go out together: a detail is
    // two round trips (three for a pull request) instead of seven.
    let (issue, comments) = std::thread::scope(|s| {
        let comments = s.spawn(|| {
            t.get_full(&format!(
                "{base}/issues/{number}/comments?per_page={DETAIL_PAGE}"
            ))
        });
        let issue = t.get_full(&format!("{base}/issues/{number}"));
        (issue, join(comments))
    });
    let issue: RawIssue = decode(&issue?)?;
    let is_pr = issue.is_pr();
    let body = super::markdown::sign_attachments(
        issue.body.as_deref().unwrap_or_default(),
        issue.body_html.as_deref().unwrap_or_default(),
    );
    let mut item = issue.into_item();

    let reply = comments?;
    let comments: Vec<Comment> = decode::<Vec<RawComment>>(&reply)?
        .into_iter()
        .map(RawComment::into_comment)
        .collect();
    let comments_truncated = reply.has_next;

    let (mut pull, mut files, mut files_truncated) = (None, None, false);
    let (mut checks_out, mut reviewers_out) = (None, None);
    if is_pr {
        let (raw, reviews, files_reply) = std::thread::scope(|s| {
            let reviews = s.spawn(|| {
                t.get(&format!(
                    "{base}/pulls/{number}/reviews?per_page={DETAIL_PAGE}"
                ))
            });
            let files = s.spawn(|| {
                t.get(&format!(
                    "{base}/pulls/{number}/files?per_page={DETAIL_PAGE}"
                ))
            });
            let raw = t
                .get(&format!("{base}/pulls/{number}"))
                .and_then(|r| decode::<RawPull>(&r));
            // Checks need the head commit, so they wait on the pull request,
            // still alongside the other two.
            if let Ok(raw) = &raw
                && !raw.head_sha().is_empty()
            {
                checks_out = checks(t, slug, raw.head_sha())
                    .inspect_err(|e| log::warn!("github: checks of {}#{number}: {e}", slug.full()))
                    .ok();
            }
            (raw, join(reviews), join(files))
        });
        let mut raw = raw?;
        let teams = raw.requested_team_names(&slug.owner);
        let requested = std::mem::take(&mut raw.requested_reviewers);
        let (pr_item, info) = raw.into_item();
        // `/pulls/{n}` is the one that knows about drafts; the issue's
        // labels and comment count are kept, which `/pulls` omits.
        item.state = pr_item.state;
        item.pull = pr_item.pull;
        // Checks and reviews are the panel's summary, not the pull request
        // itself: one that cannot be read is left out rather than failing
        // the whole view (an old commit's checks can be gone, a token can be
        // scoped away from them).
        reviewers_out = reviews
            .and_then(|r| decode::<Vec<RawReview>>(&r))
            .inspect_err(|e| log::warn!("github: reviews of {}#{number}: {e}", slug.full()))
            .ok()
            .map(|reviews| super::model::reviewers(&item.author, reviews, requested, teams));
        pull = Some(info);
        let reply = files_reply?;
        files = Some(
            decode::<Vec<RawFile>>(&reply)?
                .into_iter()
                .map(RawFile::into_file)
                .collect::<Vec<PrFile>>(),
        );
        files_truncated = reply.has_next;
    }

    Ok(Detail {
        item,
        body,
        comments,
        comments_truncated,
        pull,
        files,
        files_truncated,
        checks: checks_out,
        reviewers: reviewers_out,
    })
}

/// A scoped request's answer; a request that panicked is a failed one.
fn join(
    handle: std::thread::ScopedJoinHandle<'_, Result<Reply, ApiError>>,
) -> Result<Reply, ApiError> {
    handle
        .join()
        .unwrap_or_else(|_| Err(ApiError::Network("request thread panicked".into())))
}

/// Every check on commit `sha`: its check runs and its commit statuses.
/// This is also what a pending pull request is polled with, so it stays two
/// requests however the detail around it grows.
pub fn checks(t: &dyn Transport, slug: &RepoSlug, sha: &str) -> Result<Checks, ApiError> {
    let base = repo_path(slug);
    let sha = super::remote::escape_path(sha);
    let (runs, statuses) = std::thread::scope(|s| {
        let statuses = s.spawn(|| {
            t.get(&format!(
                "{base}/commits/{sha}/status?per_page={DETAIL_PAGE}"
            ))
        });
        let runs = t.get(&format!(
            "{base}/commits/{sha}/check-runs?per_page={DETAIL_PAGE}"
        ));
        (runs, join(statuses))
    });
    let runs: RawCheckRuns = decode(&runs?)?;
    let statuses: RawCombinedStatus = decode(&statuses?)?;
    Ok(super::model::checks(runs, statuses))
}

/// The pull request branch `branch` of `head_owner`'s fork (or of the
/// repository itself) opened against `slug`: the open one if there is one,
/// else the most recently updated. `None` when the branch has none.
pub fn pull_for_branch(
    t: &dyn Transport,
    slug: &RepoSlug,
    head_owner: &str,
    branch: &str,
) -> Result<Option<Item>, ApiError> {
    let head = escape_query(&format!("{head_owner}:{branch}"));
    let path = format!(
        "{}/pulls?state=all&head={head}&sort=updated&direction=desc&per_page=10",
        repo_path(slug)
    );
    let items: Vec<Item> = decode::<Vec<RawPull>>(&t.get(&path)?)?
        .into_iter()
        .map(|p| p.into_item().0)
        .collect();
    let open = items
        .iter()
        .position(|i| matches!(i.state, ItemState::Open | ItemState::Draft));
    Ok(match open {
        Some(i) => items.into_iter().nth(i),
        None => items.into_iter().next(),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::core::git::diff::FileStatus;
    use crate::core::github::model::{CheckState, ItemState, MergeState, ReviewState};
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A transport that answers from a path → reply table and records every
    /// path asked for. Paths it has no answer for are a 404.
    pub(crate) struct Fixture {
        pub(crate) replies: HashMap<String, Result<Reply, ApiError>>,
        pub(crate) asked: Mutex<Vec<String>>,
        pub(crate) authenticated: bool,
    }

    impl Fixture {
        pub(crate) fn new() -> Fixture {
            Fixture {
                replies: HashMap::new(),
                asked: Mutex::new(Vec::new()),
                authenticated: true,
            }
        }

        pub(crate) fn on(&mut self, path: &str, body: &str, has_next: bool) {
            self.replies.insert(
                path.to_string(),
                Ok(Reply {
                    body: body.as_bytes().to_vec(),
                    has_next,
                }),
            );
        }
    }

    impl Transport for Fixture {
        fn get(&self, path: &str) -> Result<Reply, ApiError> {
            self.asked.lock().unwrap().push(path.to_string());
            self.replies
                .get(path)
                .cloned()
                .unwrap_or(Err(ApiError::NotFound))
        }

        fn authenticated(&self) -> bool {
            self.authenticated
        }
    }

    pub(crate) fn slug() -> RepoSlug {
        RepoSlug {
            owner: "l0ng-ai".into(),
            name: "tty7".into(),
        }
    }

    const ISSUE_LIST: &str = r#"[
      {"number": 12, "title": "Crash on resize", "state": "open", "state_reason": null,
       "user": {"login": "ada"}, "labels": [{"name": "bug", "color": "d73a4a"}],
       "comments": 3, "created_at": "2026-09-01T10:00:00Z", "updated_at": "2026-09-20T08:30:00Z",
       "html_url": "https://github.com/l0ng-ai/tty7/issues/12", "body": "It crashes."},
      {"number": 13, "title": "Add panel", "state": "open", "user": {"login": "bob"},
       "labels": [], "comments": 0, "created_at": "2026-09-02T10:00:00Z",
       "updated_at": "2026-09-21T08:30:00Z", "html_url": "https://github.com/l0ng-ai/tty7/pull/13",
       "draft": true, "pull_request": {"url": "x", "merged_at": null}},
      {"number": 9, "title": "Old idea", "state": "closed", "state_reason": "not_planned",
       "user": null, "labels": [], "comments": 1, "created_at": "2026-08-01T10:00:00Z",
       "updated_at": "2026-08-02T10:00:00Z", "html_url": "https://github.com/l0ng-ai/tty7/issues/9"}
    ]"#;

    const PULL_LIST: &str = r#"[
      {"number": 13, "title": "Add panel", "state": "open", "draft": true, "merged_at": null,
       "user": {"login": "bob"}, "labels": [{"name": "ui", "color": "0e8a16"}],
       "created_at": "2026-09-02T10:00:00Z", "updated_at": "2026-09-21T08:30:00Z",
       "html_url": "https://github.com/l0ng-ai/tty7/pull/13",
       "head": {"ref": "feat/panel", "label": "bob:feat/panel"}, "base": {"ref": "main"}},
      {"number": 8, "title": "Fix typo", "state": "closed", "draft": false,
       "merged_at": "2026-09-03T10:00:00Z", "user": {"login": "cy"}, "labels": [],
       "created_at": "2026-09-02T10:00:00Z", "updated_at": "2026-09-03T10:00:00Z",
       "html_url": "https://github.com/l0ng-ai/tty7/pull/8",
       "head": {"ref": "typo"}, "base": {"ref": "main"}}
    ]"#;

    fn query(kind: Kind, label: Option<&str>) -> ListQuery {
        ListQuery {
            slug: slug(),
            kind,
            state: StateFilter::Open,
            label: label.map(str::to_string),
        }
    }

    #[test]
    fn issues_come_from_issues_with_the_pull_requests_filtered_out() {
        let mut t = Fixture::new();
        t.on(
            "/repos/l0ng-ai/tty7/issues?state=open&sort=updated&direction=desc&per_page=50&page=1",
            ISSUE_LIST,
            true,
        );
        let page = list(&t, &query(Kind::Issues, None), 1).unwrap();
        let numbers: Vec<u64> = page.items.iter().map(|i| i.number).collect();
        assert_eq!(numbers, vec![12, 9]);
        assert_eq!(page.next_page, Some(2));
        let first = &page.items[0];
        assert_eq!(first.author, "ada");
        assert_eq!(first.state, ItemState::Open);
        assert_eq!(first.labels[0].name, "bug");
        assert_eq!(first.comments, 3);
        assert!(first.updated_at > first.created_at);
        assert_eq!(page.items[1].state, ItemState::NotPlanned);
        assert_eq!(
            page.items[1].author, "",
            "a deleted user is blank, not a failure"
        );
    }

    #[test]
    fn pull_requests_come_from_pulls_with_draft_and_merged() {
        let mut t = Fixture::new();
        t.on(
            "/repos/l0ng-ai/tty7/pulls?state=open&sort=updated&direction=desc&per_page=50&page=2",
            PULL_LIST,
            false,
        );
        let page = list(&t, &query(Kind::Pulls, None), 2).unwrap();
        assert_eq!(page.next_page, None);
        let states: Vec<ItemState> = page.items.iter().map(|i| i.state).collect();
        assert_eq!(states, vec![ItemState::Draft, ItemState::Merged]);
        assert!(page.items.iter().all(|i| i.is_pr));
        let refs = page.items[0].pull.as_ref().unwrap();
        assert_eq!(
            (
                refs.head_label.as_str(),
                refs.head_ref.as_str(),
                refs.base_ref.as_str()
            ),
            ("bob:feat/panel", "feat/panel", "main")
        );
    }

    #[test]
    fn a_label_filter_on_pull_requests_goes_through_issues() {
        let mut t = Fixture::new();
        t.on(
            "/repos/l0ng-ai/tty7/issues?state=open&sort=updated&direction=desc&per_page=50&page=1&labels=good%20first%20issue",
            ISSUE_LIST,
            false,
        );
        let page = list(&t, &query(Kind::Pulls, Some("good first issue")), 1).unwrap();
        let numbers: Vec<u64> = page.items.iter().map(|i| i.number).collect();
        assert_eq!(numbers, vec![13]);
        assert_eq!(page.items[0].state, ItemState::Draft);
    }

    #[test]
    fn page_zero_is_page_one() {
        let t = Fixture::new();
        let _ = list(&t, &query(Kind::Issues, None), 0);
        assert!(t.asked.lock().unwrap()[0].ends_with("&page=1"));
    }

    #[test]
    fn an_issue_detail_reads_the_issue_and_its_comments_only() {
        let mut t = Fixture::new();
        t.on(
            "/repos/l0ng-ai/tty7/issues/12",
            r#"{"number": 12, "title": "Crash on resize", "state": "open",
                "user": {"login": "ada"}, "labels": [], "comments": 1,
                "created_at": "2026-09-01T10:00:00Z", "updated_at": "2026-09-20T08:30:00Z",
                "html_url": "https://github.com/l0ng-ai/tty7/issues/12", "body": "It **crashes**."}"#,
            false,
        );
        t.on(
            "/repos/l0ng-ai/tty7/issues/12/comments?per_page=100",
            r#"[{"id": 1, "user": {"login": "bob"}, "body": "Same here",
                 "created_at": "2026-09-02T10:00:00Z", "html_url": "https://github.com/c/1"}]"#,
            true,
        );
        let d = detail(&t, &slug(), 12).unwrap();
        assert_eq!(d.body, "It **crashes**.");
        assert_eq!(d.comments.len(), 1);
        assert_eq!(d.comments[0].author, "bob");
        assert!(d.comments_truncated);
        assert!(d.pull.is_none() && d.files.is_none());
        assert_eq!(t.asked.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_pull_request_detail_adds_branches_and_files() {
        let mut t = Fixture::new();
        t.on(
            "/repos/l0ng-ai/tty7/issues/13",
            r#"{"number": 13, "title": "Add panel", "state": "open", "user": {"login": "bob"},
                "labels": [{"name": "ui"}], "comments": 0, "created_at": "2026-09-02T10:00:00Z",
                "updated_at": "2026-09-21T08:30:00Z", "html_url": "https://github.com/l0ng-ai/tty7/pull/13",
                "body": null, "pull_request": {"merged_at": null}}"#,
            false,
        );
        t.on(
            "/repos/l0ng-ai/tty7/issues/13/comments?per_page=100",
            "[]",
            false,
        );
        t.on(
            "/repos/l0ng-ai/tty7/pulls/13",
            r#"{"number": 13, "title": "Add panel", "state": "open", "draft": true,
                "merged_at": null, "user": {"login": "bob"}, "labels": [],
                "created_at": "2026-09-02T10:00:00Z", "updated_at": "2026-09-21T08:30:00Z",
                "html_url": "https://github.com/l0ng-ai/tty7/pull/13",
                "head": {"ref": "feat/panel"}, "base": {"ref": "main"},
                "additions": 120, "deletions": 4, "changed_files": 2, "commits": 3}"#,
            false,
        );
        t.on(
            "/repos/l0ng-ai/tty7/pulls/13/files?per_page=100",
            r#"[{"filename": "src/new.rs", "status": "added", "additions": 118, "deletions": 0,
                 "patch": "@@ -0,0 +1,2 @@\n+fn a() {}\n+fn b() {}"},
                {"filename": "assets/logo.png", "status": "renamed",
                 "previous_filename": "logo.png", "additions": 2, "deletions": 4}]"#,
            true,
        );
        let d = detail(&t, &slug(), 13).unwrap();
        assert_eq!(d.body, "", "a null body is an empty one");
        assert_eq!(d.item.state, ItemState::Draft);
        assert_eq!(d.item.labels[0].name, "ui", "labels come from the issue");
        let pull = d.pull.unwrap();
        assert_eq!(
            (pull.refs.head_ref.as_str(), pull.refs.base_ref.as_str()),
            ("feat/panel", "main")
        );
        assert_eq!((pull.additions, pull.deletions, pull.commits), (120, 4, 3));
        let files = d.files.unwrap();
        assert_eq!(files[0].status, FileStatus::Added);
        assert!(files[0].patch.is_some());
        assert_eq!(files[1].status, FileStatus::Renamed);
        assert_eq!(files[1].old_path.as_deref(), Some("logo.png"));
        assert!(files[1].patch.is_none());
        assert!(d.files_truncated);
    }

    /// A pull request's issue, comments and `/pulls/{n}` — the three answers
    /// every pull request detail starts from.
    fn pull_fixture(t: &mut Fixture, pull: &str) {
        t.on(
            "/repos/l0ng-ai/tty7/issues/31",
            r#"{"number": 31, "title": "Handle empty summaries", "state": "open",
                "user": {"login": "ada"}, "pull_request": {"merged_at": null}}"#,
            false,
        );
        t.on(
            "/repos/l0ng-ai/tty7/issues/31/comments?per_page=100",
            "[]",
            false,
        );
        t.on("/repos/l0ng-ai/tty7/pulls/31", pull, false);
        t.on(
            "/repos/l0ng-ai/tty7/pulls/31/files?per_page=100",
            "[]",
            false,
        );
    }

    const PULL_31: &str = r#"{"number": 31, "title": "Handle empty summaries", "state": "open",
        "draft": false, "merged_at": null, "user": {"login": "ada"},
        "head": {"ref": "fix/summary", "sha": "abc123"}, "base": {"ref": "main"},
        "mergeable_state": "blocked",
        "requested_reviewers": [{"login": "jonas"}], "requested_teams": [{"slug": "core"}]}"#;

    #[test]
    fn a_pull_request_detail_reads_its_checks_and_reviews() {
        let mut t = Fixture::new();
        pull_fixture(&mut t, PULL_31);
        t.on(
            "/repos/l0ng-ai/tty7/commits/abc123/check-runs?per_page=100",
            r#"{"total_count": 3, "check_runs": [
                {"name": "lint", "status": "completed", "conclusion": "success",
                 "started_at": "2026-09-01T10:00:00Z", "completed_at": "2026-09-01T10:00:18Z",
                 "html_url": "https://github.com/l0ng-ai/tty7/actions/runs/1"},
                {"name": "test", "status": "in_progress", "conclusion": null,
                 "started_at": "2026-09-01T10:00:00Z"},
                {"name": "docs", "status": "completed", "conclusion": "skipped"}]}"#,
            false,
        );
        t.on(
            "/repos/l0ng-ai/tty7/commits/abc123/status?per_page=100",
            r#"{"total_count": 1, "statuses": [
                {"context": "ci/deploy", "state": "failure",
                 "target_url": "https://ci.example.com/9", "updated_at": "2026-09-01T10:05:00Z"}]}"#,
            false,
        );
        t.on(
            "/repos/l0ng-ai/tty7/pulls/31/reviews?per_page=100",
            r#"[{"user": {"login": "mara"}, "state": "APPROVED"},
                {"user": {"login": "ada"}, "state": "COMMENTED"}]"#,
            false,
        );
        let d = detail(&t, &slug(), 31).unwrap();
        let pull = d.pull.as_ref().unwrap();
        assert_eq!(pull.head_sha, "abc123");
        assert_eq!(pull.merge_state, MergeState::Blocked);

        let checks = d.checks.as_ref().unwrap();
        let names: Vec<&str> = checks.items.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["ci/deploy", "test", "lint", "docs"],
            "failures first"
        );
        assert_eq!(
            checks.items[2].completed_at - checks.items[2].started_at,
            18
        );
        assert_eq!(checks.counted(), 3, "a skipped check has no verdict");
        assert_eq!(checks.rollup(), Some(CheckState::Failed));
        assert!(!checks.truncated);

        let reviewers = d.reviewers.as_ref().unwrap();
        assert_eq!(
            reviewers
                .iter()
                .map(|r| (r.login.as_str(), r.state))
                .collect::<Vec<_>>(),
            vec![
                ("mara", ReviewState::Approved),
                ("jonas", ReviewState::Requested),
                ("l0ng-ai/core", ReviewState::Requested),
            ],
            "the author's own reply is not a review"
        );
    }

    /// Counts requests in flight at once. Each one is held until three are
    /// in flight, or for a second at most, so the overlap shows however slow
    /// the machine is to start the threads.
    struct Slow {
        inner: Fixture,
        now: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
    }

    impl Transport for Slow {
        fn get(&self, path: &str) -> Result<Reply, ApiError> {
            use std::sync::atomic::Ordering::SeqCst;
            let n = self.now.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(n, SeqCst);
            let until = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while self.peak.load(SeqCst) < 3 && std::time::Instant::now() < until {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            self.now.fetch_sub(1, SeqCst);
            self.inner.get(path)
        }

        fn authenticated(&self) -> bool {
            true
        }
    }

    #[test]
    fn a_pull_request_detail_sends_its_independent_requests_together() {
        let mut inner = Fixture::new();
        pull_fixture(&mut inner, PULL_31);
        let t = Slow {
            inner,
            now: Default::default(),
            peak: Default::default(),
        };
        detail(&t, &slug(), 31).unwrap();
        // Seven requests, three round trips: issue ‖ comments, then
        // pull → (check-runs ‖ status) alongside reviews ‖ files.
        assert_eq!(t.inner.asked.lock().unwrap().len(), 7);
        assert!(t.peak.load(std::sync::atomic::Ordering::SeqCst) >= 3);
    }

    #[test]
    fn checks_and_reviews_that_cannot_be_read_leave_the_detail_standing() {
        let mut t = Fixture::new();
        pull_fixture(&mut t, PULL_31);
        // Nothing answers for check runs, statuses or reviews.
        let d = detail(&t, &slug(), 31).unwrap();
        assert!(d.pull.is_some());
        assert!(d.checks.is_none());
        assert!(d.reviewers.is_none());
    }

    #[test]
    fn a_pull_request_without_a_head_sha_asks_for_no_checks() {
        let mut t = Fixture::new();
        pull_fixture(
            &mut t,
            r#"{"number": 31, "title": "x", "state": "open", "head": {"ref": "gone"}}"#,
        );
        let d = detail(&t, &slug(), 31).unwrap();
        assert!(d.checks.is_none());
        assert!(
            !t.asked
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.contains("/commits/")),
            "no commit to ask about"
        );
    }

    #[test]
    fn a_branchs_pull_request_prefers_the_open_one() {
        let mut t = Fixture::new();
        t.on(
            "/repos/l0ng-ai/tty7/pulls?state=all&head=bob%3Afeat%2Fpanel&sort=updated&direction=desc&per_page=10",
            r#"[{"number": 20, "title": "Second try", "state": "closed", "merged_at": null},
                {"number": 13, "title": "Add panel", "state": "open", "draft": true}]"#,
            false,
        );
        let got = pull_for_branch(&t, &slug(), "bob", "feat/panel")
            .unwrap()
            .unwrap();
        assert_eq!(got.number, 13);
        assert_eq!(got.state, ItemState::Draft);

        let mut t = Fixture::new();
        t.on(
            "/repos/l0ng-ai/tty7/pulls?state=all&head=bob%3Aold&sort=updated&direction=desc&per_page=10",
            r#"[{"number": 20, "title": "Merged", "state": "closed", "merged_at": "2026-09-01T10:00:00Z"}]"#,
            false,
        );
        assert_eq!(
            pull_for_branch(&t, &slug(), "bob", "old")
                .unwrap()
                .unwrap()
                .number,
            20,
            "no open one: the latest"
        );

        let mut t = Fixture::new();
        t.on(
            "/repos/l0ng-ai/tty7/pulls?state=all&head=bob%3Anone&sort=updated&direction=desc&per_page=10",
            "[]",
            false,
        );
        assert_eq!(pull_for_branch(&t, &slug(), "bob", "none"), Ok(None));
    }

    #[test]
    fn a_failed_sub_request_fails_the_detail() {
        let mut t = Fixture::new();
        t.on(
            "/repos/l0ng-ai/tty7/issues/12",
            r#"{"number": 12, "title": "x", "state": "open"}"#,
            false,
        );
        t.replies.insert(
            "/repos/l0ng-ai/tty7/issues/12/comments?per_page=100".into(),
            Err(ApiError::RateLimited { reset: Some(5) }),
        );
        assert_eq!(
            detail(&t, &slug(), 12),
            Err(ApiError::RateLimited { reset: Some(5) })
        );
    }

    #[test]
    fn a_body_that_is_not_the_expected_shape_is_a_decode_error() {
        let mut t = Fixture::new();
        t.on(
            "/repos/l0ng-ai/tty7/issues?state=open&sort=updated&direction=desc&per_page=50&page=1",
            r#"{"message": "surprise"}"#,
            false,
        );
        assert!(matches!(
            list(&t, &query(Kind::Issues, None), 1),
            Err(ApiError::Decode(_))
        ));
    }

    fn headers(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn statuses_map_to_distinct_errors() {
        let none = headers(&[]);
        assert_eq!(classify(200, &none, b""), Ok(()));
        assert_eq!(classify(204, &none, b""), Ok(()));
        assert_eq!(classify(401, &none, b""), Err(ApiError::Unauthorized));
        assert_eq!(classify(404, &none, b""), Err(ApiError::NotFound));
        assert_eq!(classify(500, &none, b""), Err(ApiError::Http(500)));
        assert_eq!(
            classify(
                403,
                &none,
                br#"{"message": "Resource protected by organization SAML enforcement."}"#
            ),
            Err(ApiError::Forbidden(
                "Resource protected by organization SAML enforcement.".into()
            ))
        );
    }

    #[test]
    fn a_spent_rate_limit_is_told_apart_from_other_403s() {
        let spent = headers(&[
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset", "1790000000"),
        ]);
        assert_eq!(
            classify(403, &spent, b"{}"),
            Err(ApiError::RateLimited {
                reset: Some(1_790_000_000)
            })
        );
        assert_eq!(
            classify(429, &spent, b"{}"),
            Err(ApiError::RateLimited {
                reset: Some(1_790_000_000)
            })
        );
        // A secondary limit keeps quota but says so in its message.
        let left = headers(&[("x-ratelimit-remaining", "12")]);
        assert_eq!(
            classify(
                403,
                &left,
                br#"{"message": "You have exceeded a secondary rate limit."}"#
            ),
            Err(ApiError::RateLimited { reset: None })
        );
        let retry = headers(&[("retry-after", "60")]);
        assert!(matches!(
            classify(403, &retry, b"{}"),
            Err(ApiError::RateLimited { reset: Some(_) })
        ));
    }

    #[test]
    fn the_link_header_decides_whether_there_is_more() {
        assert!(link_has_next(
            r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=9>; rel="last""#
        ));
        assert!(!link_has_next(
            r#"<https://api.github.com/x?page=1>; rel="prev", <https://api.github.com/x?page=1>; rel="first""#
        ));
        assert!(!link_has_next(""));
    }
}

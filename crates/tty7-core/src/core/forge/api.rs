//! The REST calls the panel makes, over an injected [`Transport`].
//!
//! Each call is answered by the forge the repository lives on — GitHub, in
//! [`super::github`] — decoded into the shared model. What is here is what
//! forges share: the transport, its errors, and the query a list is asked
//! for by.
//!
//! Everything here is request shaping and response decoding; the bytes move
//! through a `Transport`, which the GUI backs with HTTPS
//! ([`super::http::HttpTransport`]) and tests back with fixtures. No test in
//! this crate touches the network.

use serde::de::DeserializeOwned;

use super::model::{Checks, Detail, Item, Kind, StateFilter};
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
    /// 401: a token was sent and the forge refused it (revoked, expired).
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
            ApiError::Unauthorized => f.write_str("the token was rejected (401)"),
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

/// One authenticated-or-not GET against a forge's API — `https://api.github.com`.
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

pub(crate) fn escape_query(s: &str) -> String {
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

pub(crate) fn decode<T: DeserializeOwned>(reply: &Reply) -> Result<T, ApiError> {
    serde_json::from_slice(&reply.body).map_err(|e| ApiError::Decode(e.to_string()))
}

/// A scoped request's answer; a request that panicked is a failed one.
pub(crate) fn join<T>(
    handle: std::thread::ScopedJoinHandle<'_, Result<T, ApiError>>,
) -> Result<T, ApiError> {
    handle
        .join()
        .unwrap_or_else(|_| Err(ApiError::Network("request thread panicked".into())))
}

/// Page `page` (1-based) of a list, from the forge the repository is on.
pub fn list(t: &dyn Transport, q: &ListQuery, page: u32) -> Result<ListPage, ApiError> {
    super::github::list(t, q, page)
}

/// One issue or pull request with its body and conversation — and, for a pull
/// request, its branches, size and changed files.
pub fn detail(t: &dyn Transport, slug: &RepoSlug, number: u64) -> Result<Detail, ApiError> {
    super::github::detail(t, slug, number)
}

/// Every check on commit `sha`, as a pending pull request is polled with.
pub fn checks(t: &dyn Transport, slug: &RepoSlug, sha: &str) -> Result<Checks, ApiError> {
    super::github::checks(t, slug, sha)
}

/// The pull request branch `branch` of `head` (a fork, or the repository
/// itself) opened against `slug`: the open one if there is one, else the most
/// recently updated. `None` when the branch has none.
pub fn pull_for_branch(
    t: &dyn Transport,
    slug: &RepoSlug,
    head: &RepoSlug,
    branch: &str,
) -> Result<Option<Item>, ApiError> {
    super::github::pull_for_branch(t, slug, head, branch)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
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

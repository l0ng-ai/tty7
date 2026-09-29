//! The right panel's GitHub tab: state, and the fetches behind it.
//!
//! Read-only issues and pull requests for the repository the active pane is
//! in. The rendering lives in `ui::panel_github` (the list) and
//! [`detail`] (one issue or pull request); this module owns what they draw
//! from and how it gets there:
//!
//! - **Which repository.** The pane's working tree, resolved to its root the
//!   way the Source Control tab does (`scm_repo_root`), then its remotes read
//!   through the `Host` — the tree can be on another machine.
//! - **Which token.** Resolved once, on a worker, from the environment or the
//!   GitHub CLI (`tty7_core::core::github::token`). Refresh resolves it again,
//!   so a `gh auth login` in a pane takes effect without a restart. A
//!   repository the active `gh` account cannot see is retried with the
//!   CLI's other accounts (`tty7_core::core::github::accounts`).
//! - **The requests.** Made from *this* machine, never the remote host, on a
//!   dedicated thread each — a slow link must never hold a UI frame.
//!
//! Lists and details are cached per repository (not per pane), so flipping
//! tabs or panes back and forth does not refetch; an entry older than
//! [`STALE_AFTER`] is revalidated in the background while the old rows stay
//! on screen.
//!
//! Two things are fresher than that. A pull request on screen with checks
//! still running has just its checks re-read every [`CHECKS_POLL`], signed in
//! only, so "waiting on 1 check" turns into a verdict without anyone pressing
//! refresh. And the pane's branch is read every [`BRANCH_TTL`], so the pull
//! request pinned over the list follows a `git switch`.

pub(crate) mod detail;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{Context, Window};
use tty7_core::core::github::api::{self, ListQuery};
use tty7_core::core::github::{
    ApiError, Detail, GitHubRemote, Item, Kind, RepoSlug, StateFilter, Transport,
};

use tty7_core::core::config::RightPanelTab;
use tty7_core::core::github::CheckState;

use crate::ui::app::Tty7App;
use crate::ui::host_ops::SharedHost;
use crate::ui::scm::panel::RepoLookup;
use crate::ui::scm::state::RepoKey;

/// How old a cached list or detail may get before a render revalidates it.
pub(crate) const STALE_AFTER: Duration = Duration::from_secs(120);

/// How often an open pull request's running checks are re-read. Two requests
/// a go — well inside a signed-in rate limit, which is the only one polled.
pub(crate) const CHECKS_POLL: Duration = Duration::from_secs(20);

/// How long the pane's branch is trusted before it is read again.
const BRANCH_TTL: Duration = Duration::from_secs(10);

/// How far a list reads on by itself through pages that filter down to
/// nothing (a pull-request-heavy repository's `/issues`), before it leaves
/// the rest to "load more".
const EMPTY_PAGES_FOLLOWED: u32 = 5;

/// A transport, signed in or not.
#[derive(Clone)]
pub(crate) struct Connection {
    pub(crate) transport: Arc<dyn Transport>,
}

/// Builds a [`Connection`]. Blocking — it may run `gh`. Tests install one that
/// hands back a fixture transport, so no test ever reaches the network.
pub(crate) type Connector = Arc<dyn Fn() -> Connection + Send + Sync>;

pub(crate) enum RemoteLookup {
    Loading,
    Ready(Arc<Vec<GitHubRemote>>),
}

#[derive(Default)]
pub(crate) struct ListCache {
    pub(crate) items: Arc<Vec<Item>>,
    pub(crate) next_page: Option<u32>,
    pub(crate) loading: bool,
    /// Whether any answer has landed yet — tells "loading" from "empty".
    pub(crate) loaded: bool,
    pub(crate) error: Option<ApiError>,
    pub(crate) fetched: Option<Instant>,
    /// Bumped by every request; a reply carrying an older number is dropped,
    /// so a refresh's page 1 cannot be followed by a stale "load more".
    seq: u64,
}

#[derive(Default)]
pub(crate) struct DetailCache {
    pub(crate) detail: Option<Arc<Detail>>,
    pub(crate) loading: bool,
    pub(crate) error: Option<ApiError>,
    pub(crate) fetched: Option<Instant>,
    seq: u64,
    /// A checks poll is scheduled or in flight.
    polling: bool,
}

/// The pane's branch, and the branch it pushes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BranchHead {
    pub(crate) branch: String,
    /// `origin/feat/x`, when it tracks one.
    pub(crate) upstream: Option<String>,
}

#[derive(Default)]
pub(crate) struct BranchLookup {
    /// `None` on a detached HEAD, or before the first answer.
    pub(crate) head: Option<BranchHead>,
    pub(crate) read_at: Option<Instant>,
    pub(crate) loading: bool,
}

/// Which pull request a branch would have: opened against `slug`, from
/// `owner`'s `branch`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct BranchPullKey {
    pub(crate) slug: RepoSlug,
    pub(crate) owner: String,
    pub(crate) branch: String,
}

#[derive(Default)]
pub(crate) struct BranchPullCache {
    pub(crate) item: Option<Item>,
    pub(crate) loading: bool,
    pub(crate) error: bool,
    pub(crate) fetched: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct GitHubPanelState {
    pub(crate) connection: Option<Connection>,
    pub(crate) connecting: bool,
    /// `None` builds the real HTTPS transport.
    pub(crate) connector: Option<Connector>,
    pub(crate) remotes: HashMap<RepoKey, RemoteLookup>,
    /// The remote the user picked per repository, by remote name.
    pub(crate) remote_pick: HashMap<RepoKey, String>,
    pub(crate) kind: Kind,
    pub(crate) state: StateFilter,
    pub(crate) label: Option<String>,
    pub(crate) lists: HashMap<ListQuery, ListCache>,
    pub(crate) details: HashMap<(RepoSlug, u64), DetailCache>,
    /// The issue or pull request whose detail replaces the list.
    pub(crate) open: Option<(RepoSlug, u64)>,
    pub(crate) list_scroll: gpui::ScrollHandle,
    pub(crate) detail_scroll: gpui::ScrollHandle,
    /// The list row under the pointer, by number: the one that shows its
    /// labels and age.
    pub(crate) hovered: Option<u64>,
    pub(crate) branches: HashMap<RepoKey, BranchLookup>,
    pub(crate) branch_pulls: HashMap<BranchPullKey, BranchPullCache>,
    /// The long sections of a detail the user unfolded.
    pub(crate) unfolded: std::collections::HashSet<(RepoSlug, u64, Fold)>,
}

/// A detail section that folds when it runs long.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Fold {
    Checks,
    Files,
    Reviews,
    /// The comments between the first and the last few.
    Comments,
    /// The description, clamped when long.
    Body,
    /// One long comment, by id.
    Comment(u64),
}

/// What the panel can say about the active pane's repository.
pub(crate) enum GhTarget {
    NoPane,
    Pending,
    NotARepo,
    NoRemote,
    Ready {
        host: SharedHost,
        repo: RepoKey,
        remotes: Arc<Vec<GitHubRemote>>,
        chosen: GitHubRemote,
    },
}

/// Run blocking work on a thread of its own and await it from the UI.
///
/// Not gpui's background executor: a request can sit on a dead link for its
/// whole timeout, and parking one of the executor's few workers for that long
/// starves everything else scheduled on it.
fn off_ui<T, F>(f: F) -> impl std::future::Future<Output = Option<T>>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = smol::channel::bounded(1);
    let spawned = std::thread::Builder::new()
        .name("tty7-github".into())
        .spawn(move || {
            let _ = tx.send_blocking(f());
        });
    async move {
        spawned.ok()?;
        rx.recv().await.ok()
    }
}

/// The real connector: resolve a token (env, then `gh`), build the client.
/// A token from `gh` comes with the CLI's other accounts behind it, listed
/// only if the active one comes up short.
fn system_connector(proxy: Option<String>) -> Connector {
    use tty7_core::core::github::accounts::{Account, FallbackTransport};
    use tty7_core::core::github::http::HttpTransport;
    use tty7_core::core::github::token::{self, TokenSource};
    Arc::new(move || {
        let resolved = token::resolve_token_from_system();
        if let Some((_, source)) = &resolved {
            log::info!("github: signed in via {source:?}");
        }
        let from_gh = matches!(resolved, Some((_, TokenSource::GhCli)));
        let first: Arc<dyn Transport> = Arc::new(HttpTransport::new(
            resolved.map(|(t, _)| t),
            proxy.as_deref(),
        ));
        if !from_gh {
            return Connection { transport: first };
        }
        let proxy = proxy.clone();
        let load = Box::new(move || {
            token::other_gh_accounts_from_system()
                .into_iter()
                .map(|(login, t)| Account {
                    login,
                    transport: Arc::new(HttpTransport::new(Some(t), proxy.as_deref())),
                })
                .collect()
        });
        Connection {
            transport: Arc::new(FallbackTransport::new(first, load)),
        }
    })
}

impl Tty7App {
    /// The active pane's GitHub repository, resolving whatever is not known
    /// yet. Safe to call every frame: each lookup is dispatched once and
    /// cached.
    pub(crate) fn github_target(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> GhTarget {
        let Some((host, cwd)) = self.scm_pane_target(window, cx) else {
            return GhTarget::NoPane;
        };
        let root = match self.scm_repo_root(&host, &cwd, cx) {
            RepoLookup::Pending => return GhTarget::Pending,
            RepoLookup::NotARepo => return GhTarget::NotARepo,
            RepoLookup::Root(root) => root,
        };
        let repo = RepoKey {
            host: host.id(),
            root,
        };
        self.github_target_for(host, repo, cx)
    }

    pub(crate) fn github_target_for(
        &mut self,
        host: SharedHost,
        repo: RepoKey,
        cx: &mut Context<Self>,
    ) -> GhTarget {
        let remotes = match self.github.remotes.get(&repo) {
            Some(RemoteLookup::Ready(list)) => list.clone(),
            Some(RemoteLookup::Loading) => return GhTarget::Pending,
            None => {
                self.github_load_remotes(host, repo, cx);
                return GhTarget::Pending;
            }
        };
        let pick = self.github.remote_pick.get(&repo).map(String::as_str);
        match tty7_core::core::github::remote::default_remote(&remotes, pick) {
            Some(chosen) => {
                let chosen = chosen.clone();
                GhTarget::Ready {
                    host,
                    repo,
                    remotes,
                    chosen,
                }
            }
            None => GhTarget::NoRemote,
        }
    }

    fn github_load_remotes(&mut self, host: SharedHost, repo: RepoKey, cx: &mut Context<Self>) {
        self.github
            .remotes
            .insert(repo.clone(), RemoteLookup::Loading);
        let root = repo.root.clone();
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            move |h| tty7_core::core::git::git(h, &root, &["remote", "-v"]),
            move |this, out, cx| {
                let remotes =
                    tty7_core::core::github::remote::github_remotes(&out.unwrap_or_default());
                this.github
                    .remotes
                    .insert(repo, RemoteLookup::Ready(Arc::new(remotes)));
                cx.notify();
            },
        );
    }

    /// The connection, or `None` while one is being made.
    pub(crate) fn github_connection(&mut self, cx: &mut Context<Self>) -> Option<Connection> {
        if let Some(c) = &self.github.connection {
            return Some(c.clone());
        }
        if self.github.connecting {
            return None;
        }
        self.github.connecting = true;
        let connector = self.github.connector.clone().unwrap_or_else(|| {
            system_connector(
                cx.global::<crate::core::config::Config>()
                    .http_proxy
                    .clone(),
            )
        });
        cx.spawn(async move |this, cx| {
            let Some(connection) = off_ui(move || connector()).await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.github.connecting = false;
                this.github.connection = Some(connection);
                cx.notify();
            });
        })
        .detach();
        None
    }

    /// Make sure `q`'s first page is loaded, or being revalidated when old.
    pub(crate) fn github_ensure_list(&mut self, q: &ListQuery, cx: &mut Context<Self>) {
        let due = match self.github.lists.get(q) {
            None => true,
            Some(c) => {
                !c.loading
                    && c.error.is_none()
                    && c.fetched.is_none_or(|t| t.elapsed() > STALE_AFTER)
            }
        };
        if due {
            self.github_fetch_list(q.clone(), 1, cx);
        }
    }

    /// Fetch one page of `q` — page 1 replaces the list, later pages extend it.
    pub(crate) fn github_fetch_list(&mut self, q: ListQuery, page: u32, cx: &mut Context<Self>) {
        // Without a connection yet, nothing is recorded: the render the
        // connection's landing causes finds the query due and asks again.
        let Some(connection) = self.github_connection(cx) else {
            return;
        };
        let entry = self.github.lists.entry(q.clone()).or_default();
        entry.loading = true;
        entry.seq += 1;
        let seq = entry.seq;
        let query = q.clone();
        cx.spawn(async move |this, cx| {
            let transport = connection.transport.clone();
            let Some(result) = off_ui(move || api::list(&*transport, &query, page)).await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let Some(entry) = this.github.lists.get_mut(&q).filter(|e| e.seq == seq) else {
                    return;
                };
                entry.loading = false;
                entry.loaded = true;
                let mut follow = None;
                match result {
                    Ok(got) => {
                        let items = if page <= 1 {
                            got.items
                        } else {
                            // A row that moved between pages while they were
                            // read (it was updated, and the list sorts by
                            // update time) is kept once, where it first was.
                            let mut items = entry.items.as_ref().clone();
                            for item in got.items {
                                if !items.iter().any(|i| i.number == item.number) {
                                    items.push(item);
                                }
                            }
                            items
                        };
                        // Nothing of this kind on the pages read so far, but
                        // more pages: read on (a few, not the whole history)
                        // rather than say the repository has none.
                        if items.is_empty() && page < EMPTY_PAGES_FOLLOWED {
                            follow = got.next_page;
                        }
                        entry.items = Arc::new(items);
                        entry.next_page = got.next_page;
                        entry.error = None;
                        entry.fetched = Some(Instant::now());
                    }
                    Err(e) => {
                        log::warn!("github: listing {}: {e}", q.slug.full());
                        entry.error = Some(e);
                    }
                }
                if let Some(next) = follow {
                    this.github_fetch_list(q.clone(), next, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Make sure one issue's or pull request's detail is loaded.
    pub(crate) fn github_ensure_detail(
        &mut self,
        slug: &RepoSlug,
        number: u64,
        cx: &mut Context<Self>,
    ) {
        let key = (slug.clone(), number);
        let due = match self.github.details.get(&key) {
            None => true,
            Some(c) => {
                !c.loading
                    && c.error.is_none()
                    && c.fetched.is_none_or(|t| t.elapsed() > STALE_AFTER)
            }
        };
        if !due {
            self.github_poll_checks(&key, cx);
            return;
        }
        let Some(connection) = self.github_connection(cx) else {
            return;
        };
        let entry = self.github.details.entry(key.clone()).or_default();
        entry.loading = true;
        entry.seq += 1;
        let seq = entry.seq;
        let slug = slug.clone();
        cx.spawn(async move |this, cx| {
            let transport = connection.transport.clone();
            let Some(result) = off_ui(move || api::detail(&*transport, &slug, number)).await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let Some(entry) = this.github.details.get_mut(&key).filter(|e| e.seq == seq) else {
                    return;
                };
                entry.loading = false;
                match result {
                    Ok(d) => {
                        entry.detail = Some(Arc::new(d));
                        entry.error = None;
                        entry.fetched = Some(Instant::now());
                    }
                    Err(e) => {
                        log::warn!("github: reading {}#{}: {e}", key.0.full(), key.1);
                        entry.error = Some(e);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Re-read the checks of the pull request on screen, after a pause, while
    /// any of them are still running. Safe to call every frame: one poll per
    /// detail is ever scheduled. Stops by itself once the detail is closed,
    /// the tab is hidden, or every check has a verdict — and then marks the
    /// detail due, so the merge state is read again with the verdicts in.
    fn github_poll_checks(&mut self, key: &(RepoSlug, u64), cx: &mut Context<Self>) {
        let Some(entry) = self.github.details.get(key) else {
            return;
        };
        let pending = entry.detail.as_ref().is_some_and(|d| {
            d.checks
                .as_ref()
                .is_some_and(|c| c.count(CheckState::Pending) > 0)
                && d.pull.as_ref().is_some_and(|p| !p.head_sha.is_empty())
        });
        // Signed out, the hourly allowance is 60 requests; a poll would spend
        // it in ten minutes. Refresh still works by hand.
        let signed_in = self
            .github
            .connection
            .as_ref()
            .is_some_and(|c| c.transport.authenticated());
        if entry.polling
            || entry.loading
            || !pending
            || !signed_in
            || !self.github_detail_shown(key)
        {
            return;
        }
        if let Some(entry) = self.github.details.get_mut(key) {
            entry.polling = true;
        }
        let key = key.clone();
        let timer = cx.background_executor().timer(CHECKS_POLL);
        cx.spawn(async move |this, cx| {
            timer.await;
            let request = this
                .update(cx, |this, _cx| {
                    let entry = this.github.details.get_mut(&key)?;
                    let sha = entry
                        .detail
                        .as_ref()?
                        .pull
                        .as_ref()
                        .map(|p| p.head_sha.clone())?;
                    let seq = entry.seq;
                    let transport = this.github.connection.as_ref()?.transport.clone();
                    if !this.github_detail_shown(&key) {
                        return None;
                    }
                    Some((sha, seq, transport))
                })
                .ok()
                .flatten();
            let Some((sha, seq, transport)) = request else {
                let _ = this.update(cx, |this, _cx| {
                    if let Some(entry) = this.github.details.get_mut(&key) {
                        entry.polling = false;
                    }
                });
                return;
            };
            let slug = key.0.clone();
            let result = off_ui(move || api::checks(&*transport, &slug, &sha)).await;
            let _ = this.update(cx, |this, cx| {
                let Some(entry) = this.github.details.get_mut(&key) else {
                    return;
                };
                entry.polling = false;
                // A full read landed (or started) meanwhile: it has the
                // newer word on the checks.
                if entry.seq != seq || entry.loading {
                    return;
                }
                let Some(Ok(checks)) = result else {
                    return;
                };
                let settled = checks.count(CheckState::Pending) == 0;
                if let Some(detail) = entry.detail.as_mut() {
                    Arc::make_mut(detail).checks = Some(checks);
                }
                if settled {
                    entry.fetched = None;
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Whether `key`'s detail is what the right panel is showing.
    fn github_detail_shown(&self, key: &(RepoSlug, u64)) -> bool {
        self.right_panel_visible
            && self.right_panel_tab == RightPanelTab::GitHub
            && self.github.open.as_ref() == Some(key)
    }

    /// The branch `repo` is on, re-read every [`BRANCH_TTL`].
    pub(crate) fn github_branch(
        &mut self,
        host: SharedHost,
        repo: &RepoKey,
        cx: &mut Context<Self>,
    ) -> Option<BranchHead> {
        let entry = self.github.branches.entry(repo.clone()).or_default();
        let due = !entry.loading && entry.read_at.is_none_or(|t| t.elapsed() > BRANCH_TTL);
        let head = entry.head.clone();
        if due {
            entry.loading = true;
            let root = repo.root.clone();
            let repo = repo.clone();
            crate::ui::host_ops::HostOps::run(
                host,
                cx,
                move |h| {
                    use tty7_core::core::git::git;
                    let branch = git(h, &root, &["symbolic-ref", "-q", "--short", "HEAD"])?
                        .trim()
                        .to_string();
                    if branch.is_empty() {
                        return None;
                    }
                    let upstream = git(
                        h,
                        &root,
                        &[
                            "rev-parse",
                            "--abbrev-ref",
                            "--symbolic-full-name",
                            &format!("{branch}@{{upstream}}"),
                        ],
                    )
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                    Some(BranchHead { branch, upstream })
                },
                move |this, head, cx| {
                    let entry = this.github.branches.entry(repo).or_default();
                    entry.loading = false;
                    entry.read_at = Some(Instant::now());
                    if entry.head != head {
                        entry.head = head;
                        cx.notify();
                    }
                },
            );
        }
        head
    }

    /// The pull request the pane's branch has on the repository shown, if
    /// any — fetched once and revalidated like a list.
    pub(crate) fn github_branch_pull(
        &mut self,
        host: SharedHost,
        repo: &RepoKey,
        remotes: &[GitHubRemote],
        chosen: &GitHubRemote,
        cx: &mut Context<Self>,
    ) -> Option<Item> {
        let head = self.github_branch(host, repo, cx)?;
        let key = branch_pull_key(&head, remotes, chosen)?;
        let entry = self.github.branch_pulls.entry(key.clone()).or_default();
        let item = entry.item.clone();
        let due = !entry.loading
            && !entry.error
            && entry.fetched.is_none_or(|t| t.elapsed() > STALE_AFTER);
        if !due {
            return item;
        }
        let Some(connection) = self.github_connection(cx) else {
            return item;
        };
        if let Some(entry) = self.github.branch_pulls.get_mut(&key) {
            entry.loading = true;
        }
        cx.spawn(async move |this, cx| {
            let transport = connection.transport.clone();
            let q = key.clone();
            let Some(result) =
                off_ui(move || api::pull_for_branch(&*transport, &q.slug, &q.owner, &q.branch))
                    .await
            else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let entry = this.github.branch_pulls.entry(key.clone()).or_default();
                entry.loading = false;
                match result {
                    Ok(item) => {
                        entry.item = item;
                        entry.error = false;
                        entry.fetched = Some(Instant::now());
                    }
                    Err(e) => {
                        log::warn!("github: pull request of {}:{}: {e}", key.owner, key.branch);
                        entry.error = true;
                    }
                }
                cx.notify();
            });
        })
        .detach();
        item
    }

    /// Start over for the repository on screen: resolve the token again (a
    /// `gh auth login` since the last try counts), re-read the remotes, and
    /// mark the visible list and detail due.
    pub(crate) fn github_refresh(&mut self, repo: Option<RepoKey>, cx: &mut Context<Self>) {
        if !self.github.connecting {
            self.github.connection = None;
        }
        if let Some(repo) = repo {
            self.github.remotes.remove(&repo);
        }
        for entry in self.github.lists.values_mut() {
            entry.error = None;
            entry.fetched = None;
            entry.loading = false;
            // A new sequence number orphans whatever is still in flight.
            entry.seq += 1;
        }
        // Lists with nothing to show are dropped so they read as loading
        // rather than as an empty result.
        self.github.lists.retain(|_, e| e.loaded);
        for entry in self.github.details.values_mut() {
            entry.error = None;
            entry.fetched = None;
            entry.loading = false;
            entry.seq += 1;
        }
        self.github.details.retain(|_, e| e.detail.is_some());
        for entry in self.github.branches.values_mut() {
            entry.read_at = None;
        }
        for entry in self.github.branch_pulls.values_mut() {
            entry.error = false;
            entry.fetched = None;
        }
        cx.notify();
    }

    /// The list query the panel shows for `slug`.
    pub(crate) fn github_query(&self, slug: &RepoSlug) -> ListQuery {
        ListQuery {
            slug: slug.clone(),
            kind: self.github.kind,
            state: self.github.state,
            label: self.github.label.clone(),
        }
    }

    pub(crate) fn github_open_detail(
        &mut self,
        slug: RepoSlug,
        number: u64,
        cx: &mut Context<Self>,
    ) {
        self.github.open = Some((slug, number));
        self.github.detail_scroll = gpui::ScrollHandle::new();
        cx.notify();
    }

    pub(crate) fn github_close_detail(&mut self, cx: &mut Context<Self>) {
        self.github.open = None;
        cx.notify();
    }
}

/// Where to look for `head`'s pull request: its upstream's remote, when that
/// is a GitHub remote (a fork's branch lives in the fork), else the repository
/// shown under the local branch's name. `None` when it tracks a branch
/// somewhere other than GitHub.
pub(crate) fn branch_pull_key(
    head: &BranchHead,
    remotes: &[GitHubRemote],
    chosen: &GitHubRemote,
) -> Option<BranchPullKey> {
    let (owner, branch) = match &head.upstream {
        None => (chosen.slug.owner.clone(), head.branch.clone()),
        Some(upstream) => {
            // The longest remote name that prefixes it: `origin` must not
            // claim `origin-old/x`, and a remote may itself contain a `/`.
            let remote = remotes
                .iter()
                .filter(|r| {
                    upstream
                        .strip_prefix(r.remote.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
                })
                .max_by_key(|r| r.remote.len())?;
            let branch = upstream[remote.remote.len() + 1..].to_string();
            (remote.slug.owner.clone(), branch)
        }
    };
    (!branch.is_empty()).then(|| BranchPullKey {
        slug: chosen.slug.clone(),
        owner,
        branch,
    })
}

/// Seconds since the epoch, for relative times.
pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(name: &str, owner: &str) -> GitHubRemote {
        GitHubRemote {
            remote: name.into(),
            slug: RepoSlug {
                owner: owner.into(),
                name: "widgets".into(),
            },
        }
    }

    fn head(branch: &str, upstream: Option<&str>) -> BranchHead {
        BranchHead {
            branch: branch.into(),
            upstream: upstream.map(str::to_string),
        }
    }

    #[test]
    fn a_forks_branch_is_looked_for_under_the_forks_owner() {
        let remotes = [remote("origin", "me"), remote("upstream", "acme")];
        let key = branch_pull_key(
            &head("local-name", Some("origin/feat/x")),
            &remotes,
            &remotes[1],
        )
        .unwrap();
        assert_eq!(key.slug.owner, "acme", "asked of the repository shown");
        assert_eq!((key.owner.as_str(), key.branch.as_str()), ("me", "feat/x"));
    }

    #[test]
    fn an_untracked_branch_is_looked_for_under_its_own_name() {
        let remotes = [remote("origin", "acme")];
        let key = branch_pull_key(&head("feat/y", None), &remotes, &remotes[0]).unwrap();
        assert_eq!(
            (key.owner.as_str(), key.branch.as_str()),
            ("acme", "feat/y")
        );
    }

    #[test]
    fn the_longest_remote_name_claims_the_upstream() {
        let remotes = [remote("origin", "a"), remote("origin-old", "b")];
        let key = branch_pull_key(&head("x", Some("origin-old/x")), &remotes, &remotes[0]).unwrap();
        assert_eq!(key.owner, "b");
        assert!(
            branch_pull_key(&head("x", Some("gitlab/x")), &remotes, &remotes[0]).is_none(),
            "a branch pushed somewhere else has no pull request here"
        );
    }
}

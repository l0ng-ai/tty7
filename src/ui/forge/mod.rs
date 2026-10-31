//! The right panel's forge tab: state, and the fetches behind it.
//!
//! Read-only issues and pull requests for the repository the active pane is
//! in. The rendering lives in `ui::panel_forge` (the list) and
//! [`detail`] (one issue or pull request); this module owns what they draw
//! from and how it gets there:
//!
//! - **Which repository.** The pane's working tree, resolved to its root the
//!   way the Source Control tab does (`scm_repo_root`), then its remotes read
//!   through the `Host` — the tree can be on another machine.
//! - **Which token.** Resolved once per forge host, on a worker, from the
//!   environment or the forge's CLI (`tty7_core::core::forge::github::gh`).
//!   Refresh resolves it again, so an `auth login` in a pane takes effect
//!   without a restart. A GitHub repository the active `gh` account cannot
//!   see is retried with the CLI's other accounts
//!   (`tty7_core::core::forge::github::accounts`).
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
pub(crate) mod rows;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{Context, Window};
use tty7_core::core::forge::api::{self, ListQuery};
use tty7_core::core::forge::{
    ApiError, Detail, Forge, ForgeRemote, Item, Kind, RepoSlug, StateFilter, Transport,
};

use tty7_core::core::config::RightPanelTab;
use tty7_core::core::forge::CheckState;

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

/// Builds the [`Connection`] to a repository's forge host. Blocking — it may
/// run the forge's CLI. Tests install one that hands back a fixture transport,
/// so no test ever reaches the network.
pub(crate) type Connector = Arc<dyn Fn(&RepoSlug) -> Connection + Send + Sync>;

/// Which connection answers for a repository: one per forge host, since each
/// has its own API and its own token.
pub(crate) type ConnKey = (Forge, String);

fn conn_key(slug: &RepoSlug) -> ConnKey {
    (slug.forge, slug.host.clone())
}

/// One issue or pull request. A forge may number the two apart, so the
/// number alone does not say which.
pub(crate) type DetailKey = (RepoSlug, u64, bool);

pub(crate) enum RemoteLookup {
    Loading,
    Ready(Arc<Vec<ForgeRemote>>),
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
/// `head`'s `branch` — `head` being the fork it was pushed to, or `slug`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct BranchPullKey {
    pub(crate) slug: RepoSlug,
    pub(crate) head: RepoSlug,
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
pub(crate) struct ForgePanelState {
    pub(crate) connections: HashMap<ConnKey, Connection>,
    pub(crate) connecting: std::collections::HashSet<ConnKey>,
    /// `None` builds the real HTTPS transport.
    pub(crate) connector: Option<Connector>,
    pub(crate) remotes: HashMap<RepoKey, RemoteLookup>,
    /// The remote the user picked per repository, by remote name.
    pub(crate) remote_pick: HashMap<RepoKey, String>,
    pub(crate) kind: Kind,
    pub(crate) state: StateFilter,
    pub(crate) label: Option<String>,
    pub(crate) lists: HashMap<ListQuery, ListCache>,
    pub(crate) details: HashMap<DetailKey, DetailCache>,
    /// The issue or pull request whose detail replaces the list.
    pub(crate) open: Option<DetailKey>,
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
pub(crate) enum ForgeTarget {
    NoPane,
    Pending,
    NotARepo,
    NoRemote,
    Ready {
        host: SharedHost,
        repo: RepoKey,
        remotes: Arc<Vec<ForgeRemote>>,
        chosen: ForgeRemote,
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
        .name("tty7-forge".into())
        .spawn(move || {
            let _ = tx.send_blocking(f());
        });
    async move {
        spawned.ok()?;
        rx.recv().await.ok()
    }
}

/// The real connector: resolve a token (env, then the forge's CLI), build
/// the client. A token from `gh` comes with the CLI's other accounts behind
/// it, listed only if the active one comes up short.
fn system_connector(proxy: Option<String>) -> Connector {
    use tty7_core::core::forge::TokenSource;
    use tty7_core::core::forge::github::accounts::{Account, FallbackTransport};
    use tty7_core::core::forge::github::gh;
    use tty7_core::core::forge::http::HttpTransport;
    Arc::new(move |_: &RepoSlug| {
        let resolved = gh::resolve_token_from_system();
        if let Some((_, source)) = &resolved {
            log::info!("forge: signed in via {source:?}");
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
            gh::other_gh_accounts_from_system()
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
    /// The active pane's forge repository, resolving whatever is not known
    /// yet. Safe to call every frame: each lookup is dispatched once and
    /// cached.
    pub(crate) fn forge_target(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ForgeTarget {
        let Some((host, cwd)) = self.scm_pane_target(window, cx) else {
            return ForgeTarget::NoPane;
        };
        let root = match self.scm_repo_root(&host, &cwd, cx) {
            RepoLookup::Pending => return ForgeTarget::Pending,
            RepoLookup::NotARepo => return ForgeTarget::NotARepo,
            RepoLookup::Root(root) => root,
        };
        let repo = RepoKey {
            host: host.id(),
            root,
        };
        self.forge_target_for(host, repo, cx)
    }

    pub(crate) fn forge_target_for(
        &mut self,
        host: SharedHost,
        repo: RepoKey,
        cx: &mut Context<Self>,
    ) -> ForgeTarget {
        let remotes = match self.forge.remotes.get(&repo) {
            Some(RemoteLookup::Ready(list)) => list.clone(),
            Some(RemoteLookup::Loading) => return ForgeTarget::Pending,
            None => {
                self.forge_load_remotes(host, repo, cx);
                return ForgeTarget::Pending;
            }
        };
        let pick = self.forge.remote_pick.get(&repo).map(String::as_str);
        match tty7_core::core::forge::remote::default_remote(&remotes, pick) {
            Some(chosen) => {
                let chosen = chosen.clone();
                ForgeTarget::Ready {
                    host,
                    repo,
                    remotes,
                    chosen,
                }
            }
            None => ForgeTarget::NoRemote,
        }
    }

    fn forge_load_remotes(&mut self, host: SharedHost, repo: RepoKey, cx: &mut Context<Self>) {
        self.forge
            .remotes
            .insert(repo.clone(), RemoteLookup::Loading);
        let root = repo.root.clone();
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            move |h| tty7_core::core::git::git(h, &root, &["remote", "-v"]),
            move |this, out, cx| {
                let remotes =
                    tty7_core::core::forge::remote::forge_remotes(&out.unwrap_or_default());
                this.forge
                    .remotes
                    .insert(repo, RemoteLookup::Ready(Arc::new(remotes)));
                cx.notify();
            },
        );
    }

    /// The connection to `slug`'s forge host, or `None` while one is being
    /// made.
    pub(crate) fn forge_connection(
        &mut self,
        slug: &RepoSlug,
        cx: &mut Context<Self>,
    ) -> Option<Connection> {
        let key = conn_key(slug);
        if let Some(c) = self.forge.connections.get(&key) {
            return Some(c.clone());
        }
        if !self.forge.connecting.insert(key.clone()) {
            return None;
        }
        let connector = self.forge.connector.clone().unwrap_or_else(|| {
            system_connector(
                cx.global::<crate::core::config::Config>()
                    .http_proxy
                    .clone(),
            )
        });
        let slug = slug.clone();
        cx.spawn(async move |this, cx| {
            let Some(connection) = off_ui(move || connector(&slug)).await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                this.forge.connecting.remove(&key);
                this.forge.connections.insert(key, connection);
                cx.notify();
            });
        })
        .detach();
        None
    }

    /// Whether `slug`'s forge host is read signed in.
    pub(crate) fn forge_signed_in(&self, slug: &RepoSlug) -> bool {
        self.forge
            .connections
            .get(&conn_key(slug))
            .is_some_and(|c| c.transport.authenticated())
    }

    /// Make sure `q`'s first page is loaded, or being revalidated when old.
    pub(crate) fn forge_ensure_list(&mut self, q: &ListQuery, cx: &mut Context<Self>) {
        let due = match self.forge.lists.get(q) {
            None => true,
            Some(c) => {
                !c.loading
                    && c.error.is_none()
                    && c.fetched.is_none_or(|t| t.elapsed() > STALE_AFTER)
            }
        };
        if due {
            self.forge_fetch_list(q.clone(), 1, cx);
        }
    }

    /// Fetch one page of `q` — page 1 replaces the list, later pages extend it.
    pub(crate) fn forge_fetch_list(&mut self, q: ListQuery, page: u32, cx: &mut Context<Self>) {
        // Without a connection yet, nothing is recorded: the render the
        // connection's landing causes finds the query due and asks again.
        let Some(connection) = self.forge_connection(&q.slug, cx) else {
            return;
        };
        let entry = self.forge.lists.entry(q.clone()).or_default();
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
                let Some(entry) = this.forge.lists.get_mut(&q).filter(|e| e.seq == seq) else {
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
                        log::warn!("forge: listing {}: {e}", q.slug.full());
                        entry.error = Some(e);
                    }
                }
                if let Some(next) = follow {
                    this.forge_fetch_list(q.clone(), next, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Make sure one issue's or pull request's detail is loaded.
    pub(crate) fn forge_ensure_detail(&mut self, key: &DetailKey, cx: &mut Context<Self>) {
        let key = key.clone();
        let due = match self.forge.details.get(&key) {
            None => true,
            Some(c) => {
                !c.loading
                    && c.error.is_none()
                    && c.fetched.is_none_or(|t| t.elapsed() > STALE_AFTER)
            }
        };
        if !due {
            self.forge_poll_checks(&key, cx);
            return;
        }
        let Some(connection) = self.forge_connection(&key.0, cx) else {
            return;
        };
        let entry = self.forge.details.entry(key.clone()).or_default();
        entry.loading = true;
        entry.seq += 1;
        let seq = entry.seq;
        let (slug, number, _) = key.clone();
        cx.spawn(async move |this, cx| {
            let transport = connection.transport.clone();
            let Some(result) = off_ui(move || api::detail(&*transport, &slug, number)).await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let Some(entry) = this.forge.details.get_mut(&key).filter(|e| e.seq == seq) else {
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
                        log::warn!("forge: reading {}#{}: {e}", key.0.full(), key.1);
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
    fn forge_poll_checks(&mut self, key: &DetailKey, cx: &mut Context<Self>) {
        let Some(entry) = self.forge.details.get(key) else {
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
        let signed_in = self.forge_signed_in(&key.0);
        if entry.polling || entry.loading || !pending || !signed_in || !self.forge_detail_shown(key)
        {
            return;
        }
        if let Some(entry) = self.forge.details.get_mut(key) {
            entry.polling = true;
        }
        let key = key.clone();
        let timer = cx.background_executor().timer(CHECKS_POLL);
        cx.spawn(async move |this, cx| {
            timer.await;
            let request = this
                .update(cx, |this, _cx| {
                    let entry = this.forge.details.get_mut(&key)?;
                    let sha = entry
                        .detail
                        .as_ref()?
                        .pull
                        .as_ref()
                        .map(|p| p.head_sha.clone())?;
                    let seq = entry.seq;
                    let transport = this
                        .forge
                        .connections
                        .get(&conn_key(&key.0))?
                        .transport
                        .clone();
                    if !this.forge_detail_shown(&key) {
                        return None;
                    }
                    Some((sha, seq, transport))
                })
                .ok()
                .flatten();
            let Some((sha, seq, transport)) = request else {
                let _ = this.update(cx, |this, _cx| {
                    if let Some(entry) = this.forge.details.get_mut(&key) {
                        entry.polling = false;
                    }
                });
                return;
            };
            let slug = key.0.clone();
            let result = off_ui(move || api::checks(&*transport, &slug, &sha)).await;
            let _ = this.update(cx, |this, cx| {
                let Some(entry) = this.forge.details.get_mut(&key) else {
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
    fn forge_detail_shown(&self, key: &DetailKey) -> bool {
        self.right_panel_visible
            && self.right_panel_tab == RightPanelTab::Forge
            && self.forge.open.as_ref() == Some(key)
    }

    /// The branch `repo` is on, re-read every [`BRANCH_TTL`].
    pub(crate) fn forge_branch(
        &mut self,
        host: SharedHost,
        repo: &RepoKey,
        cx: &mut Context<Self>,
    ) -> Option<BranchHead> {
        let entry = self.forge.branches.entry(repo.clone()).or_default();
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
                    let entry = this.forge.branches.entry(repo).or_default();
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
    pub(crate) fn forge_branch_pull(
        &mut self,
        host: SharedHost,
        repo: &RepoKey,
        remotes: &[ForgeRemote],
        chosen: &ForgeRemote,
        cx: &mut Context<Self>,
    ) -> Option<Item> {
        let head = self.forge_branch(host, repo, cx)?;
        let key = branch_pull_key(&head, remotes, chosen)?;
        let entry = self.forge.branch_pulls.entry(key.clone()).or_default();
        let item = entry.item.clone();
        let due = !entry.loading
            && !entry.error
            && entry.fetched.is_none_or(|t| t.elapsed() > STALE_AFTER);
        if !due {
            return item;
        }
        let Some(connection) = self.forge_connection(&key.slug, cx) else {
            return item;
        };
        if let Some(entry) = self.forge.branch_pulls.get_mut(&key) {
            entry.loading = true;
        }
        cx.spawn(async move |this, cx| {
            let transport = connection.transport.clone();
            let q = key.clone();
            let Some(result) =
                off_ui(move || api::pull_for_branch(&*transport, &q.slug, &q.head, &q.branch))
                    .await
            else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let entry = this.forge.branch_pulls.entry(key.clone()).or_default();
                entry.loading = false;
                match result {
                    Ok(item) => {
                        entry.item = item;
                        entry.error = false;
                        entry.fetched = Some(Instant::now());
                    }
                    Err(e) => {
                        log::warn!(
                            "forge: pull request of {}:{}: {e}",
                            key.head.full(),
                            key.branch
                        );
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
    pub(crate) fn forge_refresh(&mut self, repo: Option<RepoKey>, cx: &mut Context<Self>) {
        // A connection still being made is left to land; it is as fresh as
        // a new one would be.
        self.forge.connections.clear();
        if let Some(repo) = repo {
            self.forge.remotes.remove(&repo);
        }
        for entry in self.forge.lists.values_mut() {
            entry.error = None;
            entry.fetched = None;
            entry.loading = false;
            // A new sequence number orphans whatever is still in flight.
            entry.seq += 1;
        }
        // Lists with nothing to show are dropped so they read as loading
        // rather than as an empty result.
        self.forge.lists.retain(|_, e| e.loaded);
        for entry in self.forge.details.values_mut() {
            entry.error = None;
            entry.fetched = None;
            entry.loading = false;
            entry.seq += 1;
        }
        self.forge.details.retain(|_, e| e.detail.is_some());
        for entry in self.forge.branches.values_mut() {
            entry.read_at = None;
        }
        for entry in self.forge.branch_pulls.values_mut() {
            entry.error = false;
            entry.fetched = None;
        }
        cx.notify();
    }

    /// The list query the panel shows for `slug`.
    pub(crate) fn forge_query(&self, slug: &RepoSlug) -> ListQuery {
        ListQuery {
            slug: slug.clone(),
            kind: self.forge.kind,
            state: self.forge.state,
            label: self.forge.label.clone(),
        }
    }

    pub(crate) fn forge_open_detail(
        &mut self,
        slug: RepoSlug,
        number: u64,
        is_pr: bool,
        cx: &mut Context<Self>,
    ) {
        self.forge.open = Some((slug, number, is_pr));
        self.forge.detail_scroll = gpui::ScrollHandle::new();
        cx.notify();
    }

    pub(crate) fn forge_close_detail(&mut self, cx: &mut Context<Self>) {
        self.forge.open = None;
        cx.notify();
    }
}

/// Where to look for `head`'s pull request: its upstream's remote, when that
/// is a remote on the same forge host (a fork's branch lives in the fork),
/// else the repository shown under the local branch's name. `None` when it
/// tracks a branch somewhere else.
pub(crate) fn branch_pull_key(
    head: &BranchHead,
    remotes: &[ForgeRemote],
    chosen: &ForgeRemote,
) -> Option<BranchPullKey> {
    let (head_repo, branch) = match &head.upstream {
        None => (chosen.slug.clone(), head.branch.clone()),
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
            // A fork on another forge cannot have opened anything here.
            if (remote.slug.forge, &remote.slug.host) != (chosen.slug.forge, &chosen.slug.host) {
                return None;
            }
            let branch = upstream[remote.remote.len() + 1..].to_string();
            (remote.slug.clone(), branch)
        }
    };
    (!branch.is_empty()).then(|| BranchPullKey {
        slug: chosen.slug.clone(),
        head: head_repo,
        branch,
    })
}

/// A forge-named string: `{forge}` and `{cli}` filled in for `forge`.
pub(crate) fn forge_t(key: crate::ui::i18n::L10nKey, forge: Forge) -> String {
    crate::ui::i18n::t_fmt(key, &[("forge", forge.name()), ("cli", forge.cli())])
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

    fn remote(name: &str, owner: &str) -> ForgeRemote {
        ForgeRemote {
            remote: name.into(),
            slug: RepoSlug::github(owner, "widgets"),
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
        assert_eq!(
            (key.head.owner.as_str(), key.branch.as_str()),
            ("me", "feat/x")
        );
    }

    #[test]
    fn an_untracked_branch_is_looked_for_under_its_own_name() {
        let remotes = [remote("origin", "acme")];
        let key = branch_pull_key(&head("feat/y", None), &remotes, &remotes[0]).unwrap();
        assert_eq!(
            (key.head.owner.as_str(), key.branch.as_str()),
            ("acme", "feat/y")
        );
    }

    #[test]
    fn the_longest_remote_name_claims_the_upstream() {
        let remotes = [remote("origin", "a"), remote("origin-old", "b")];
        let key = branch_pull_key(&head("x", Some("origin-old/x")), &remotes, &remotes[0]).unwrap();
        assert_eq!(key.head.owner, "b");
        assert!(
            branch_pull_key(&head("x", Some("gitlab/x")), &remotes, &remotes[0]).is_none(),
            "a branch pushed somewhere else has no pull request here"
        );
    }
}

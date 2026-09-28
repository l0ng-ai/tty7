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

pub(crate) mod detail;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{Context, Window};
use tty7_core::core::github::api::{self, ListQuery};
use tty7_core::core::github::{
    ApiError, Detail, GitHubRemote, Item, Kind, RepoSlug, StateFilter, Transport,
};

use crate::ui::app::Tty7App;
use crate::ui::host_ops::SharedHost;
use crate::ui::scm::panel::RepoLookup;
use crate::ui::scm::state::RepoKey;

/// How old a cached list or detail may get before a render revalidates it.
pub(crate) const STALE_AFTER: Duration = Duration::from_secs(120);

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
    /// Each checked-out branch's upstream (`origin/feat/x`, or `None` for a
    /// branch never pushed), for the Info tab's link. Keyed by branch, so a
    /// checkout of another branch asks again.
    pub(crate) upstreams: HashMap<(RepoKey, String), Option<String>>,
    pub(crate) upstream_lookups: std::collections::HashSet<(RepoKey, String)>,
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
}

/// What the panel can say about the active pane's repository.
pub(crate) enum GhTarget {
    NoPane,
    Pending,
    NotARepo,
    NoRemote,
    Ready {
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
        self.github.upstreams.clear();
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

    /// The GitHub page for the active pane's checkout, for the Info tab: the
    /// branch on the remote it tracks, or the repository when it was never
    /// pushed. `None` until the lookups land, and for a tree with no GitHub
    /// remote.
    pub(crate) fn github_info_link(
        &mut self,
        branch: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(String, String)> {
        let GhTarget::Ready {
            repo,
            remotes,
            chosen,
        } = self.github_target(window, cx)
        else {
            return None;
        };
        let key = (repo.clone(), branch.to_string());
        let upstream = match self.github.upstreams.get(&key) {
            Some(upstream) => upstream.clone(),
            None => {
                self.github_load_upstream(key, cx);
                None
            }
        };
        let (slug, url) =
            tty7_core::core::github::remote::checkout_url(&remotes, &chosen, upstream.as_deref());
        Some((slug.full(), url))
    }

    fn github_load_upstream(&mut self, key: (RepoKey, String), cx: &mut Context<Self>) {
        if !self.github.upstream_lookups.insert(key.clone()) {
            return;
        }
        let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, key.0.host) else {
            return;
        };
        let root = key.0.root.clone();
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            move |h| {
                tty7_core::core::git::git(
                    h,
                    &root,
                    &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
                )
            },
            move |this, out, cx| {
                this.github.upstream_lookups.remove(&key);
                let upstream = out
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                this.github.upstreams.insert(key, upstream);
                cx.notify();
            },
        );
    }
}

/// Seconds since the epoch, for relative times.
pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

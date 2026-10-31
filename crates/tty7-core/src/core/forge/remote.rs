//! Which forge repository a working tree belongs to.
//!
//! Read off `git remote -v`, which the caller runs through the `Host` — the
//! repository can live on another machine, and only its git knows the remotes
//! (including any `url.<base>.insteadOf` rewrite, which `remote -v` applies and
//! a raw `config --get` would not).
//!
//! A forge is recognised by its host. Only `github.com` is, so far. GitHub
//! Enterprise hosts are out of scope: they need their own API base and their
//! own token, and guessing either from a hostname would send the user's
//! github.com token to a server it was never issued for.

/// Which kind of server a repository lives on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum Forge {
    #[default]
    GitHub,
}

impl Forge {
    /// The product's own spelling, for the UI.
    pub fn name(self) -> &'static str {
        match self {
            Forge::GitHub => "GitHub",
        }
    }

    /// The forge's CLI, whose sign-in tty7 borrows.
    pub fn cli(self) -> &'static str {
        match self {
            Forge::GitHub => "gh",
        }
    }
}

/// One repository on one forge: `owner/name` on `host`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoSlug {
    pub forge: Forge,
    /// Lower-cased.
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl RepoSlug {
    /// `owner/name` on github.com.
    pub fn github(owner: &str, name: &str) -> RepoSlug {
        RepoSlug {
            forge: Forge::GitHub,
            host: "github.com".into(),
            owner: owner.into(),
            name: name.into(),
        }
    }

    /// `owner/name`, the spelling the forge itself uses in its UI.
    pub fn full(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// The repository's page.
    pub fn web_url(&self) -> String {
        format!("https://{}/{}/{}", self.host, self.owner, self.name)
    }

    /// The page for one branch's tree, or the repository page when the branch
    /// is unknown. Slashes in a branch name stay slashes — GitHub resolves
    /// `tree/feat/x` to the branch `feat/x` — but everything that would end
    /// the path segment early (`#`, `?`, `%`, spaces) is escaped.
    pub fn tree_url(&self, branch: Option<&str>) -> String {
        match branch.map(str::trim).filter(|b| !b.is_empty()) {
            Some(branch) => format!("{}/tree/{}", self.web_url(), escape_path(branch)),
            None => self.web_url(),
        }
    }
}

/// One remote that points at a recognised forge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeRemote {
    /// The git remote's name — `origin`, `upstream`, …
    pub remote: String,
    pub slug: RepoSlug,
}

/// The forge `host` runs, when tty7 can tell without asking it.
fn forge_of(host: &str) -> Option<Forge> {
    let host = host.to_ascii_lowercase();
    (host == "github.com" || host == "www.github.com").then_some(Forge::GitHub)
}

/// The repository a remote URL names, when its host is a forge tty7 knows.
///
/// Accepts every spelling git itself accepts for a host:
///
/// - `https://host/owner/name(.git)(/)`, with or without userinfo
///   (`https://token@host/…`) and an explicit `:443`
/// - `git@host:owner/name(.git)` (scp-like)
/// - `ssh://git@host(:22)/owner/name(.git)`
/// - `git://host/owner/name.git`
///
/// The host comparison ignores case, the way DNS does. Everything else — an
/// unknown host, a local path — is `None`.
pub fn parse_remote_url(url: &str) -> Option<RepoSlug> {
    let url = url.trim();
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "https" | "http" | "ssh" | "git" | "git+ssh" | "ssh+git"
        ) {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        // `?`, `#` and `\` end the authority for a URL parser, so
        // `https://evil.io#@github.com/o/r` is a URL for evil.io.
        if authority.contains(['?', '#', '\\']) {
            return None;
        }
        // Userinfo can itself hold a `@` only percent-encoded, so the last
        // one is the separator.
        let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        (host_port.split(':').next().unwrap_or(""), path)
    } else {
        // scp-like: `[user@]host:path`. A colon before any slash is what tells
        // it from a local path (`./a:b` has its slash first).
        let (head, path) = url.split_once(':')?;
        if head.contains('/') {
            return None;
        }
        (head.rsplit_once('@').map_or(head, |(_, h)| h), path)
    };
    match forge_of(host)? {
        Forge::GitHub => slug_from_path(path),
    }
}

fn slug_from_path(path: &str) -> Option<RepoSlug> {
    let path = path.trim_matches('/');
    let mut parts = path.split('/');
    let owner = parts.next()?.trim();
    let name = parts.next()?.trim();
    // `github.com/owner/name/tree/main` is a page, not a remote.
    if parts.next().is_some() {
        return None;
    }
    let name = name.strip_suffix(".git").unwrap_or(name);
    if !valid_segment(owner) || !valid_segment(name) {
        return None;
    }
    Some(RepoSlug::github(owner, name))
}

/// What GitHub allows in an owner or repository name: ASCII letters, digits,
/// `-`, `_`, `.` — and never `.`/`..`, which would walk the API path.
fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The forge remotes in `git remote -v` output, one per remote name, in the
/// order git listed them.
///
/// Each remote appears twice (fetch and push); the fetch URL is the one that
/// says where the repository lives. A push-only override (`pushurl`) at a
/// different host is exactly the case where the two disagree, and the fetch
/// side is the one reads come from.
pub fn forge_remotes(remote_v: &str) -> Vec<ForgeRemote> {
    let mut out: Vec<ForgeRemote> = Vec::new();
    for line in remote_v.lines() {
        let mut fields = line.split_whitespace();
        let (Some(name), Some(url)) = (fields.next(), fields.next()) else {
            continue;
        };
        let kind = fields.next().unwrap_or("(fetch)");
        if kind != "(fetch)" || out.iter().any(|r| r.remote == name) {
            continue;
        }
        if let Some(slug) = parse_remote_url(url) {
            out.push(ForgeRemote {
                remote: name.to_string(),
                slug,
            });
        }
    }
    out
}

/// Which remote the panel shows by default.
///
/// A fork is cloned with `origin` at the fork and `upstream` at the project,
/// and the project is where the issues and pull requests are — so `upstream`
/// wins, then `origin`, then whatever git listed first. `preferred` is the
/// user's own pick from the panel, honoured while that remote still exists.
pub fn default_remote<'a>(
    remotes: &'a [ForgeRemote],
    preferred: Option<&str>,
) -> Option<&'a ForgeRemote> {
    let named = |name: &str| remotes.iter().find(|r| r.remote == name);
    preferred
        .and_then(named)
        .or_else(|| named("upstream"))
        .or_else(|| named("origin"))
        .or_else(|| remotes.first())
}

/// Where "view this on the forge" should land for a checkout: the branch's
/// page on the remote it tracks, when that remote is on a forge, and
/// otherwise the chosen repository's front page.
///
/// `upstream` is `git rev-parse --abbrev-ref @{u}` — `origin/feat/x`. A branch
/// with no upstream has never been pushed, and a link to its tree would be a
/// 404, so it gets the repository instead. Returns the repository the link is
/// in, with the URL.
pub fn checkout_url<'a>(
    remotes: &'a [ForgeRemote],
    chosen: &'a ForgeRemote,
    upstream: Option<&str>,
) -> (&'a RepoSlug, String) {
    let tracked = upstream.and_then(|up| {
        remotes.iter().find_map(|r| {
            let branch = up.strip_prefix(r.remote.as_str())?.strip_prefix('/')?;
            (!branch.is_empty()).then_some((r, branch))
        })
    });
    match tracked {
        Some((r, branch)) => (&r.slug, r.slug.tree_url(Some(branch))),
        None => (&chosen.slug, chosen.slug.web_url()),
    }
}

/// Percent-encode what cannot appear raw in one URL path segment, keeping `/`.
pub(crate) fn escape_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slug(owner: &str, name: &str) -> Option<RepoSlug> {
        Some(RepoSlug::github(owner, name))
    }

    fn parse(url: &str) -> Option<RepoSlug> {
        parse_remote_url(url)
    }

    #[test]
    fn every_spelling_git_accepts_for_github_parses() {
        for url in [
            "https://github.com/l0ng-ai/tty7",
            "https://github.com/l0ng-ai/tty7.git",
            "https://github.com/l0ng-ai/tty7/",
            "https://github.com/l0ng-ai/tty7.git/",
            "http://github.com/l0ng-ai/tty7",
            "https://GitHub.com/l0ng-ai/tty7",
            "https://www.github.com/l0ng-ai/tty7",
            "https://x-access-token:abc@github.com/l0ng-ai/tty7.git",
            "https://github.com:443/l0ng-ai/tty7.git",
            "git@github.com:l0ng-ai/tty7.git",
            "git@github.com:l0ng-ai/tty7",
            "github.com:l0ng-ai/tty7.git",
            "ssh://git@github.com/l0ng-ai/tty7.git",
            "ssh://git@github.com:22/l0ng-ai/tty7",
            "git+ssh://git@github.com/l0ng-ai/tty7.git",
            "git://github.com/l0ng-ai/tty7.git",
            "  git@github.com:l0ng-ai/tty7.git  ",
        ] {
            assert_eq!(parse(url), slug("l0ng-ai", "tty7"), "{url}");
        }
    }

    #[test]
    fn a_repository_name_may_contain_dots() {
        assert_eq!(
            parse("https://github.com/owner/my.site.git"),
            slug("owner", "my.site")
        );
        assert_eq!(
            parse("git@github.com:owner/name.github.io"),
            slug("owner", "name.github.io")
        );
    }

    #[test]
    fn other_hosts_and_non_remotes_are_ignored() {
        for url in [
            "https://git.example.com/owner/name.git",
            "https://github.example.com/owner/name.git",
            "git@github.example.com:owner/name.git",
            "https://notgithub.com/owner/name",
            "https://github.com.evil.io/owner/name",
            "https://evil.io#@github.com/owner/name",
            "https://evil.io?@github.com/owner/name",
            "https://evil.io\\@github.com/owner/name",
            "/srv/git/name.git",
            "./relative:path",
            "file:///srv/git/name.git",
            "https://github.com/owner",
            "https://github.com/owner/name/tree/main",
            "https://github.com/../name",
            "https://github.com/owner/na%20me",
            "",
        ] {
            assert_eq!(parse(url), None, "{url}");
        }
    }

    #[test]
    fn remote_v_keeps_one_fetch_url_per_github_remote() {
        let out = "\
origin\tgit@github.com:me/tty7.git (fetch)
origin\tgit@github.com:me/tty7.git (push)
upstream\thttps://github.com/l0ng-ai/tty7.git (fetch)
upstream\tno_push (push)
mirror\thttps://git.example.com/me/tty7.git (fetch)
mirror\thttps://git.example.com/me/tty7.git (push)
";
        let remotes = forge_remotes(out);
        assert_eq!(
            remotes,
            vec![
                ForgeRemote {
                    remote: "origin".into(),
                    slug: slug("me", "tty7").unwrap()
                },
                ForgeRemote {
                    remote: "upstream".into(),
                    slug: slug("l0ng-ai", "tty7").unwrap()
                },
            ]
        );
    }

    #[test]
    fn a_push_only_github_url_does_not_make_a_github_remote() {
        let out = "\
mirror\thttps://git.example.com/me/tty7.git (fetch)
mirror\tgit@github.com:me/tty7.git (push)
";
        assert!(forge_remotes(out).is_empty());
    }

    fn remote(name: &str) -> ForgeRemote {
        ForgeRemote {
            remote: name.into(),
            slug: slug("o", name).unwrap(),
        }
    }

    #[test]
    fn a_fork_defaults_to_upstream_then_origin_then_the_first() {
        let pick = |names: &[&str], preferred: Option<&str>| {
            let remotes: Vec<_> = names.iter().map(|n| remote(n)).collect();
            default_remote(&remotes, preferred).map(|r| r.remote.clone())
        };
        assert_eq!(
            pick(&["origin", "upstream"], None).as_deref(),
            Some("upstream")
        );
        assert_eq!(pick(&["fork", "origin"], None).as_deref(), Some("origin"));
        assert_eq!(pick(&["fork", "mine"], None).as_deref(), Some("fork"));
        assert_eq!(pick(&[], None), None);
        // The user's pick wins while it exists, and is forgotten once it does
        // not rather than leaving the panel on nothing.
        assert_eq!(
            pick(&["origin", "upstream"], Some("origin")).as_deref(),
            Some("origin")
        );
        assert_eq!(
            pick(&["origin", "upstream"], Some("gone")).as_deref(),
            Some("upstream")
        );
    }

    #[test]
    fn a_branch_url_keeps_slashes_and_escapes_the_rest() {
        let s = slug("l0ng-ai", "tty7").unwrap();
        assert_eq!(s.full(), "l0ng-ai/tty7");
        assert_eq!(s.tree_url(None), "https://github.com/l0ng-ai/tty7");
        assert_eq!(s.tree_url(Some("  ")), "https://github.com/l0ng-ai/tty7");
        assert_eq!(
            s.tree_url(Some("feat/panel-github")),
            "https://github.com/l0ng-ai/tty7/tree/feat/panel-github"
        );
        assert_eq!(
            s.tree_url(Some("fix#12 ?x")),
            "https://github.com/l0ng-ai/tty7/tree/fix%2312%20%3Fx"
        );
    }

    #[test]
    fn a_checkout_links_to_its_pushed_branch_or_else_the_repository() {
        let remotes = vec![
            ForgeRemote {
                remote: "origin".into(),
                slug: slug("me", "tty7").unwrap(),
            },
            ForgeRemote {
                remote: "upstream".into(),
                slug: slug("l0ng-ai", "tty7").unwrap(),
            },
        ];
        let chosen = &remotes[1];
        // Tracked on the fork: the fork's branch page, not upstream's.
        let (repo, url) = checkout_url(&remotes, chosen, Some("origin/feat/x"));
        assert_eq!(repo.full(), "me/tty7");
        assert_eq!(url, "https://github.com/me/tty7/tree/feat/x");
        // Never pushed: the repository, not a 404.
        let (repo, url) = checkout_url(&remotes, chosen, None);
        assert_eq!(repo.full(), "l0ng-ai/tty7");
        assert_eq!(url, "https://github.com/l0ng-ai/tty7");
        // Tracking a remote that is not on GitHub.
        let (_, url) = checkout_url(&remotes, chosen, Some("gitlab/main"));
        assert_eq!(url, "https://github.com/l0ng-ai/tty7");
        // `origin-old/x` is not `origin` + `-old/x`.
        let (_, url) = checkout_url(&remotes, chosen, Some("origin-old/x"));
        assert_eq!(url, "https://github.com/l0ng-ai/tty7");
    }
}

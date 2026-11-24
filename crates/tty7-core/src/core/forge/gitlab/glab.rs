//! Where a GitLab token comes from, and the hosts `glab` knows.
//!
//! As for GitHub, tty7 stores no credential of its own; it borrows the
//! GitLab CLI's. Per host, in order:
//!
//! 1. `GITLAB_TOKEN`, `GITLAB_ACCESS_TOKEN`, `OAUTH_TOKEN` — the variables
//!    `glab` honours. `glab` applies them to every host; tty7 only to the
//!    host they were plausibly issued for: `GITLAB_HOST` when that is set,
//!    else gitlab.com. A host with a `gitlab` label is recognised on sight,
//!    so applying them everywhere would hand a gitlab.com token to any server
//!    a repository happens to be cloned from.
//! 2. `glab config get token --host H` — the token `glab auth login` saved in
//!    its config file. Local, no network.
//! 3. `glab api` itself ([`GlabApiTransport`]), when `glab` is signed in to
//!    the host but keeps the token where (2) cannot read it — the OS keyring
//!    — or the token is an OAuth one that has expired: `glab api` refreshes it
//!    on its way out, which a request with the stale token cannot.
//!
//! The hosts `glab` is configured for, and `GITLAB_HOST`, count as GitLab
//! hosts ([`known_hosts`]), and `glab`'s `api_host`/`api_protocol` say
//! where a host's API lives when that is not `https://<host>/api/v4`. Both
//! are read straight from `glab`'s `config.yml` rather than `glab auth
//! status`, which checks every host over the network.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::core::forge::api::{ApiError, MAX_BODY, Reply, Transport, classify};
use crate::core::forge::token::{Token, TokenSource, cli_candidates, run_cli};

/// The environment variables consulted, in order.
pub(crate) const TOKEN_VARS: [&str; 3] = ["GITLAB_TOKEN", "GITLAB_ACCESS_TOKEN", "OAUTH_TOKEN"];

/// Fixed places `glab` lands when installed outside the login `PATH`.
pub(crate) const GLAB_FALLBACKS: [&str; 3] = [
    "/opt/homebrew/bin/glab",
    "/opt/homebrew/opt/glab/bin/glab",
    "/usr/local/bin/glab",
];

/// One host in `glab`'s config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlabHost {
    /// Lower-cased.
    pub host: String,
    /// `<api_protocol>://<api_host>/api/v4`, when the config names an API
    /// host.
    pub api: Option<String>,
}

/// The side effects resolution needs, injected so tests never read the real
/// environment or run the real `glab`.
pub struct GlabEnv<'a> {
    pub var: &'a dyn Fn(&str) -> Option<String>,
    pub is_file: &'a dyn Fn(&Path) -> bool,
    pub read: &'a dyn Fn(&Path) -> Option<String>,
    /// Run `<glab> <args>` and hand back its stdout on success.
    pub glab: &'a dyn Fn(&Path, &[&str]) -> Option<String>,
}

/// Whether the token variables are meant for `host`.
fn env_applies(env: &GlabEnv<'_>, host: &str) -> bool {
    let target = (env.var)("GITLAB_HOST")
        .map(|h| bare_host(&h))
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "gitlab.com".into());
    target.eq_ignore_ascii_case(host)
}

/// `https://gitlab.example.com/` → `gitlab.example.com`.
fn bare_host(s: &str) -> String {
    let s = s.trim();
    let s = s.split_once("://").map_or(s, |(_, rest)| rest);
    s.split('/').next().unwrap_or("").to_ascii_lowercase()
}

/// A token for `host`, or `None` to go without one (and through `glab api`,
/// when `glab` is there).
pub fn resolve_token(env: &GlabEnv<'_>, host: &str) -> Option<(Token, TokenSource)> {
    if env_applies(env, host) {
        for var in TOKEN_VARS {
            if let Some(token) = (env.var)(var).as_deref().and_then(Token::new) {
                return Some((token, TokenSource::Env(var)));
            }
        }
    }
    let glab = find_glab(env)?;
    let out = (env.glab)(&glab, &["config", "get", "token", "--host", host])?;
    Token::new(out.lines().next()?).map(|t| (t, TokenSource::GlabCli))
}

/// The `glab` to ask: the first one found, as for `gh`.
pub fn find_glab(env: &GlabEnv<'_>) -> Option<PathBuf> {
    cli_candidates("glab", (env.var)("PATH").as_deref(), &GLAB_FALLBACKS)
        .into_iter()
        .find(|p| (env.is_file)(p))
}

/// Where `glab` keeps its config: `$GLAB_CONFIG_DIR`, else
/// `$XDG_CONFIG_HOME/glab-cli`, else `~/.config/glab-cli` — on every
/// platform, macOS included.
pub fn config_path(env: &GlabEnv<'_>) -> Option<PathBuf> {
    let var = |name: &str| (env.var)(name).filter(|v| !v.trim().is_empty());
    let dir = var("GLAB_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| var("XDG_CONFIG_HOME").map(|x| Path::new(&x).join("glab-cli")))
        .or_else(|| {
            var("HOME")
                .or_else(|| var("USERPROFILE"))
                .map(|h| Path::new(&h).join(".config").join("glab-cli"))
        })?;
    Some(dir.join("config.yml"))
}

/// The hosts in `glab`'s config, or none when there is no config.
pub fn hosts(env: &GlabEnv<'_>) -> Vec<GlabHost> {
    config_path(env)
        .and_then(|p| (env.read)(&p))
        .map(|text| parse_hosts(&text))
        .unwrap_or_default()
}

/// The `hosts:` block of `glab`'s `config.yml`. A YAML parser for one
/// two-level map would be a new dependency; the block's shape is `glab`'s
/// own and fixed:
///
/// ```yaml
/// hosts:
///     gitlab.example.com:
///         api_protocol: https
///         api_host: gitlab.example.com
/// ```
pub fn parse_hosts(text: &str) -> Vec<GlabHost> {
    let mut out: Vec<GlabHost> = Vec::new();
    let mut in_hosts = false;
    let mut host_indent: Option<usize> = None;
    // The current host's `api_protocol` and `api_host`.
    let mut fields: (Option<String>, Option<String>) = (None, None);
    let finish = |out: &mut Vec<GlabHost>, fields: &mut (Option<String>, Option<String>)| {
        if let (Some(last), (protocol, Some(api_host))) = (out.last_mut(), std::mem::take(fields)) {
            let protocol = protocol.unwrap_or_else(|| "https".into());
            last.api = Some(format!("{protocol}://{api_host}/api/v4"));
        }
    };
    for line in text.lines() {
        let trimmed = line.trim_end();
        let body = trimmed.trim_start();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        let indent = trimmed.len() - body.len();
        if indent == 0 {
            if in_hosts {
                finish(&mut out, &mut fields);
            }
            in_hosts = body == "hosts:";
            host_indent = None;
            continue;
        }
        if !in_hosts {
            continue;
        }
        let indent_of_hosts = *host_indent.get_or_insert(indent);
        let (key, value) = match body.split_once(':') {
            Some((k, v)) => (unquote(k), unquote(v)),
            None => continue,
        };
        if indent == indent_of_hosts {
            finish(&mut out, &mut fields);
            if !key.is_empty() && value.is_empty() {
                out.push(GlabHost {
                    host: key.to_ascii_lowercase(),
                    api: None,
                });
            }
        } else if indent > indent_of_hosts && !value.is_empty() {
            match key.as_str() {
                "api_protocol" => fields.0 = Some(value),
                "api_host" => fields.1 = Some(value),
                _ => {}
            }
        }
    }
    if in_hosts {
        finish(&mut out, &mut fields);
    }
    out
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(s);
    s.to_string()
}

/// The hosts that run GitLab by tty7's knowledge, beyond the ones that say
/// so in their name: every host in `glab`'s config, and `GITLAB_HOST` — a
/// token in the environment with no `glab` sign-in is for that host.
pub fn known_hosts(env: &GlabEnv<'_>) -> Vec<String> {
    let mut out: Vec<String> = hosts(env).into_iter().map(|h| h.host).collect();
    if let Some(host) = (env.var)("GITLAB_HOST")
        .map(|h| bare_host(&h))
        .filter(|h| !h.is_empty() && !out.contains(h))
    {
        out.push(host);
    }
    out
}

/// Where `host`'s REST API is: what `glab`'s config says, else
/// `https://<host>/api/v4`.
pub fn api_base(host: &str, glab: &[GlabHost]) -> String {
    glab.iter()
        .find(|h| h.host.eq_ignore_ascii_case(host))
        .and_then(|h| h.api.clone())
        .unwrap_or_else(|| format!("https://{host}/api/v4"))
}

/// Reads through `glab api --include`, signed in however `glab` is. Slower
/// than HTTPS (a process per request) — the fallback, not the way in.
pub struct GlabApiTransport {
    glab: PathBuf,
    host: String,
}

impl GlabApiTransport {
    pub fn new(glab: PathBuf, host: &str) -> GlabApiTransport {
        GlabApiTransport {
            glab,
            host: host.to_string(),
        }
    }
}

/// What keeps `glab` from prompting, or from checking for its own update.
const GLAB_QUIET: [(&str, &str); 2] = [("NO_PROMPT", "1"), ("GLAB_CHECK_UPDATE", "false")];

/// How long one `glab api` may take: a request's worth, plus a refresh.
const GLAB_API_TIMEOUT: Duration = Duration::from_secs(40);

impl Transport for GlabApiTransport {
    fn get(&self, path: &str) -> Result<Reply, ApiError> {
        use crate::core::proc::{hide_console, output_within};
        let mut cmd = std::process::Command::new(&self.glab);
        cmd.args([
            "api",
            "--hostname",
            &self.host,
            "--include",
            path.trim_start_matches('/'),
        ])
        .envs(GLAB_QUIET);
        let out = output_within(hide_console(&mut cmd), GLAB_API_TIMEOUT)
            .map_err(|e| ApiError::Network(format!("glab: {e}")))?;
        // `glab` has read the whole answer by now. Past the cap it is refused
        // all the same, as an HTTPS one would be.
        if out.stdout.len() as u64 > MAX_BODY {
            return Err(ApiError::Network("glab: the answer is too large".into()));
        }
        // A 4xx exits non-zero but still prints the response; only output
        // with no response in it is a failure to ask.
        parse_included(&out.stdout).ok_or_else(|| {
            let err = String::from_utf8_lossy(&out.stderr);
            ApiError::Network(format!(
                "glab: {}",
                err.lines().next().unwrap_or("no answer")
            ))
        })?
    }

    fn authenticated(&self) -> bool {
        true
    }
}

/// `glab api --include`'s output: a status line, headers, a blank line, the
/// body. `None` when it is not a response at all.
fn parse_included(out: &[u8]) -> Option<Result<Reply, ApiError>> {
    let split = |sep: &[u8]| {
        out.windows(sep.len())
            .position(|w| w == sep)
            .map(|i| (i, sep.len()))
    };
    let (end, sep) = split(b"\r\n\r\n").or_else(|| split(b"\n\n"))?;
    let head = std::str::from_utf8(&out[..end]).ok()?;
    let body = out[end + sep..].to_vec();
    let mut lines = head.lines();
    let status: u16 = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
        })
        .collect();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    if let Err(e) = classify(status, &header, &body) {
        return Some(Err(e));
    }
    Some(Ok(Reply::new(body, &header)))
}

/// HTTPS with the token found, handing over to `glab api` for good once the
/// token is refused — an expired OAuth token, which only `glab` can refresh.
#[cfg(feature = "forge")]
pub struct GitLabTransport {
    http: crate::core::forge::http::HttpTransport,
    signed_in: bool,
    glab: Option<GlabApiTransport>,
    via_glab: std::sync::atomic::AtomicBool,
}

#[cfg(feature = "forge")]
impl Transport for GitLabTransport {
    fn get(&self, path: &str) -> Result<Reply, ApiError> {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(glab) = &self.glab else {
            return self.http.get(path);
        };
        if self.via_glab.load(Relaxed) {
            return glab.get(path);
        }
        match self.http.get(path) {
            Err(ApiError::Unauthorized) => {
                log::info!("gitlab: token refused, reading through glab api");
                self.via_glab.store(true, Relaxed);
                glab.get(path)
            }
            reply => reply,
        }
    }

    fn authenticated(&self) -> bool {
        self.signed_in
    }
}

/// The transport for GitLab `host`, against the real environment and `glab`.
/// Blocking — it may run `glab`; call it off the UI thread.
#[cfg(feature = "forge")]
pub fn transport_from_system(host: &str, manual_proxy: Option<&str>) -> GitLabTransport {
    let env = system_env();
    let glab_path = find_glab(&env);
    let glab_hosts = hosts(&env);
    let knows_host = glab_hosts.iter().any(|h| h.host.eq_ignore_ascii_case(host));
    let token = resolve_token(&env, host);
    if let Some((_, source)) = &token {
        log::info!("gitlab: {host} signed in via {source:?}");
    }
    let api = api_base(host, &glab_hosts);
    let signed_in = token.is_some();
    // Only a host `glab` is configured for can be read through it; for any
    // other, `glab api` would answer unauthenticated at best.
    let glab = glab_path
        .filter(|_| knows_host)
        .map(|p| GlabApiTransport::new(p, host));
    // `glab` holds a token tty7 could not read (the keyring): go through it
    // from the start.
    let via_glab = !signed_in && glab.is_some();
    if via_glab {
        log::info!("gitlab: {host} read through glab api");
    }
    GitLabTransport {
        http: crate::core::forge::http::HttpTransport::gitlab(
            &api,
            token.map(|(t, _)| t),
            manual_proxy,
        ),
        signed_in: signed_in || glab.is_some(),
        glab,
        via_glab: std::sync::atomic::AtomicBool::new(via_glab),
    }
}

/// [`known_hosts`], against the real environment and `glab` config. Quick —
/// one small local file — but still a file read.
pub fn known_hosts_from_system() -> Vec<String> {
    known_hosts(&system_env())
}

fn system_env<'a>() -> GlabEnv<'a> {
    GlabEnv {
        var: &|name| std::env::var(name).ok(),
        is_file: &|p| p.is_file(),
        read: &|p| std::fs::read_to_string(p).ok(),
        glab: &run_glab,
    }
}

/// `glab config get` reads a file; a locked keyring can still make it wait.
const GLAB_TIMEOUT: Duration = Duration::from_secs(5);

fn run_glab(glab: &Path, args: &[&str]) -> Option<String> {
    run_cli(glab, args, &GLAB_QUIET, GLAB_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::cell::RefCell;
    #[cfg(unix)]
    use std::collections::HashMap;

    #[cfg(unix)]
    struct Fake {
        vars: HashMap<&'static str, &'static str>,
        files: HashMap<PathBuf, &'static str>,
        token_out: Option<&'static str>,
        asked: RefCell<Vec<String>>,
    }

    #[cfg(unix)]
    impl Fake {
        fn new() -> Fake {
            let mut files = HashMap::new();
            files.insert(PathBuf::from("/usr/local/bin/glab"), "");
            Fake {
                vars: HashMap::new(),
                files,
                token_out: None,
                asked: RefCell::new(Vec::new()),
            }
        }

        fn with_env<T>(&self, f: impl FnOnce(&GlabEnv<'_>) -> T) -> T {
            f(&GlabEnv {
                var: &|name| self.vars.get(name).map(|v| v.to_string()),
                is_file: &|p| self.files.contains_key(p),
                read: &|p| self.files.get(p).map(|t| t.to_string()),
                glab: &|_, args| {
                    self.asked.borrow_mut().push(args.join(" "));
                    self.token_out.map(str::to_string)
                },
            })
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_env_token_only_goes_to_its_own_host() {
        let mut f = Fake::new();
        f.vars.insert("GITLAB_TOKEN", "glpat-env");
        f.token_out = Some("glpat-cli\n");
        let got = |f: &Fake, host| f.with_env(|e| resolve_token(e, host)).unwrap();
        assert_eq!(got(&f, "gitlab.com").1, TokenSource::Env("GITLAB_TOKEN"));
        let (token, source) = got(&f, "gitlab.corp.io");
        assert_eq!(
            (token.expose(), source),
            ("glpat-cli", TokenSource::GlabCli),
            "a gitlab.com token is not sent to another host"
        );
        assert!(
            f.asked
                .borrow()
                .contains(&"config get token --host gitlab.corp.io".to_string())
        );
        f.vars.insert("GITLAB_HOST", "https://gitlab.corp.io/");
        assert_eq!(
            got(&f, "gitlab.corp.io").1,
            TokenSource::Env("GITLAB_TOKEN")
        );
        assert_eq!(got(&f, "gitlab.com").1, TokenSource::GlabCli);
    }

    #[cfg(unix)]
    #[test]
    fn no_token_anywhere_is_none() {
        let f = Fake::new();
        assert!(f.with_env(|e| resolve_token(e, "gitlab.com")).is_none());
        let mut f = Fake::new();
        f.files.clear();
        assert!(f.with_env(|e| resolve_token(e, "gitlab.com")).is_none());
        assert!(f.asked.borrow().is_empty(), "no glab, nothing run");
    }

    const CONFIG: &str = "\
git_protocol: ssh
# comment
hosts:
    gitlab.com:
        api_protocol: https
        api_host: gitlab.com
        token: secret
    Git.Corp.IO:
        token: \"x\"
        api_host: git.corp.io:8443
        api_protocol: \"http\"
    bare.example.com:
        token: y
no_prompt: true
";

    #[test]
    fn hosts_come_out_of_glabs_config() {
        assert_eq!(
            parse_hosts(CONFIG),
            vec![
                GlabHost {
                    host: "gitlab.com".into(),
                    api: Some("https://gitlab.com/api/v4".into())
                },
                GlabHost {
                    host: "git.corp.io".into(),
                    api: Some("http://git.corp.io:8443/api/v4".into())
                },
                GlabHost {
                    host: "bare.example.com".into(),
                    api: None
                },
            ]
        );
        assert!(parse_hosts("hosts: {}\n").is_empty());
        assert!(parse_hosts("").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_config_is_found_where_glab_keeps_it() {
        let mut f = Fake::new();
        f.vars.insert("HOME", "/home/me");
        let p = |f: &Fake| f.with_env(config_path).unwrap();
        assert_eq!(p(&f), PathBuf::from("/home/me/.config/glab-cli/config.yml"));
        f.vars.insert("XDG_CONFIG_HOME", "/xdg");
        assert_eq!(p(&f), PathBuf::from("/xdg/glab-cli/config.yml"));
        f.vars.insert("GLAB_CONFIG_DIR", "/custom");
        assert_eq!(p(&f), PathBuf::from("/custom/config.yml"));
        f.files.insert(PathBuf::from("/custom/config.yml"), CONFIG);
        assert_eq!(f.with_env(hosts).len(), 3);
    }

    #[test]
    fn the_api_base_is_glabs_or_the_default() {
        let glab = parse_hosts(CONFIG);
        assert_eq!(
            api_base("git.corp.io", &glab),
            "http://git.corp.io:8443/api/v4"
        );
        assert_eq!(api_base("other.io", &glab), "https://other.io/api/v4");
    }

    #[cfg(unix)]
    #[test]
    fn known_hosts_are_glabs_and_gitlab_host() {
        let mut f = Fake::new();
        f.vars.insert("GLAB_CONFIG_DIR", "/c");
        f.files.insert(PathBuf::from("/c/config.yml"), CONFIG);
        f.vars.insert("GITLAB_HOST", "https://Code.Corp.io/");
        assert_eq!(
            f.with_env(known_hosts),
            vec![
                "gitlab.com",
                "git.corp.io",
                "bare.example.com",
                "code.corp.io"
            ]
        );
        // Already listed: not twice.
        f.vars.insert("GITLAB_HOST", "git.corp.io");
        assert_eq!(f.with_env(known_hosts).len(), 3);
    }

    #[test]
    fn glab_api_output_is_a_response() {
        let ok = b"HTTP/2.0 200 OK\r\nContent-Type: application/json\r\nX-Next-Page: 2\r\n\r\n[1]";
        let reply = parse_included(ok).unwrap().unwrap();
        assert_eq!((reply.body.as_slice(), reply.has_next), (&b"[1]"[..], true));
        let last = b"HTTP/2.0 200 OK\nX-Next-Page: \n\n[]";
        assert!(!parse_included(last).unwrap().unwrap().has_next);
        let missing = b"HTTP/2.0 404 Not Found\r\n\r\n{\"message\":\"404 Project Not Found\"}";
        assert_eq!(
            parse_included(missing).unwrap().unwrap_err(),
            ApiError::NotFound
        );
        let limited = b"HTTP/1.1 429 Too Many Requests\r\nRateLimit-Reset: 1790000000\r\n\r\n";
        assert_eq!(
            parse_included(limited).unwrap().unwrap_err(),
            ApiError::RateLimited {
                reset: Some(1_790_000_000)
            }
        );
        assert!(parse_included(b"glab: not logged in").is_none());
    }
}

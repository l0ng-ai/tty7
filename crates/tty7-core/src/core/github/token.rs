//! Where the GitHub token comes from.
//!
//! tty7 has no OAuth app and stores no credential of its own: it borrows the
//! one the user already gave the GitHub CLI. In order:
//!
//! 1. `GH_TOKEN`, then `GITHUB_TOKEN` — the variables `gh` itself honours, so
//!    a CI-style setup behaves the same here as in the shell;
//! 2. `gh auth token --hostname github.com`.
//!
//! When the answer came from `gh`, the CLI's *other* github.com accounts are
//! on offer too ([`other_gh_accounts`]): a repository the active account
//! cannot see is retried with them (`super::accounts`). A token from the
//! environment is taken as a deliberate choice and gets no fallback.
//!
//! No token is not an error. Public repositories read fine without one, at
//! GitHub's unauthenticated rate (60 requests an hour).
//!
//! `gh` is looked for on `PATH` *and* at the Homebrew and `/usr/local`
//! locations, because an app launched from Finder or the Dock inherits
//! launchd's minimal `PATH` rather than the login shell's, and `gh` is almost
//! never on it there.
//!
//! The token never reaches a log line or the screen: [`Token`]'s `Debug`
//! prints a placeholder, and nothing else in tty7 formats it.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// A GitHub token. Opaque on purpose — see the module docs.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// A token from wherever it was found, or `None` for an empty value
    /// (`GH_TOKEN=` set but blank is "no token", as it is for `gh`).
    pub fn new(raw: &str) -> Option<Token> {
        let raw = raw.trim();
        (!raw.is_empty() && !raw.contains(char::is_whitespace)).then(|| Token(raw.to_string()))
    }

    /// For the `Authorization` header, and for nothing else.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// Where a token was found, for the panel to say so ("signed in via gh").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenSource {
    Env(&'static str),
    GhCli,
}

/// The environment variables consulted, in order.
pub const TOKEN_VARS: [&str; 2] = ["GH_TOKEN", "GITHUB_TOKEN"];

/// Fixed places `gh` lands when installed outside the login `PATH`.
pub const GH_FALLBACKS: [&str; 3] = [
    "/opt/homebrew/bin/gh",
    "/opt/homebrew/opt/gh/bin/gh",
    "/usr/local/bin/gh",
];

/// The side effects token resolution needs, injected so tests never read the
/// real environment or run the real `gh`.
pub struct TokenEnv<'a> {
    pub var: &'a dyn Fn(&str) -> Option<String>,
    pub is_file: &'a dyn Fn(&Path) -> bool,
    /// Run `<gh> <args>` and hand back its stdout on success.
    pub gh: &'a dyn Fn(&Path, &[&str]) -> Option<String>,
}

/// Resolve a token, or `None` to go unauthenticated.
pub fn resolve_token(env: &TokenEnv<'_>) -> Option<(Token, TokenSource)> {
    for var in TOKEN_VARS {
        if let Some(token) = (env.var)(var).as_deref().and_then(Token::new) {
            return Some((token, TokenSource::Env(var)));
        }
    }
    let gh = find_gh(env)?;
    gh_token(env, &gh, None).map(|t| (t, TokenSource::GhCli))
}

/// The `gh` to ask. Only the first one found: a second install answering for
/// a different account than the one on PATH would be a surprise, not a
/// fallback.
pub fn find_gh(env: &TokenEnv<'_>) -> Option<PathBuf> {
    gh_candidates((env.var)("PATH").as_deref())
        .into_iter()
        .find(|p| (env.is_file)(p))
}

/// `gh auth token` for github.com — the active account's, or `user`'s.
fn gh_token(env: &TokenEnv<'_>, gh: &Path, user: Option<&str>) -> Option<Token> {
    let mut args = vec!["auth", "token", "--hostname", "github.com"];
    if let Some(user) = user {
        args.extend(["--user", user]);
    }
    let out = (env.gh)(gh, &args)?;
    // `gh auth token` prints the token and a newline; anything multi-line is
    // not a token.
    Token::new(out.lines().next()?)
}

/// Every github.com account `gh` is signed in to besides the active one, by
/// login, in the order `gh` lists them. Accounts whose sign-in `gh` itself
/// reports as broken are left out.
///
/// Slow — `gh auth status` checks each account against GitHub — so it is
/// only asked once the active account has come up short.
pub fn other_gh_accounts(env: &TokenEnv<'_>) -> Vec<(String, Token)> {
    let Some(gh) = find_gh(env) else {
        return Vec::new();
    };
    let Some(status) = (env.gh)(
        &gh,
        &[
            "auth",
            "status",
            "--hostname",
            "github.com",
            "--json",
            "hosts",
        ],
    ) else {
        return Vec::new();
    };
    inactive_logins(&status)
        .into_iter()
        .filter_map(|login| gh_token(env, &gh, Some(&login)).map(|t| (login, t)))
        .collect()
}

/// The signed-in, inactive github.com logins in `gh auth status --json hosts`.
fn inactive_logins(status: &str) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Status {
        #[serde(default)]
        hosts: std::collections::HashMap<String, Vec<Account>>,
    }
    #[derive(serde::Deserialize)]
    struct Account {
        #[serde(default)]
        state: String,
        #[serde(default)]
        active: bool,
        #[serde(default)]
        login: String,
    }
    let Ok(status) = serde_json::from_str::<Status>(status) else {
        return Vec::new();
    };
    status
        .hosts
        .get("github.com")
        .into_iter()
        .flatten()
        .filter(|a| !a.active && a.state == "success" && !a.login.is_empty())
        .map(|a| a.login.clone())
        .collect()
}

/// Every place `gh` might be, `PATH` first, without duplicates.
pub fn gh_candidates(path_var: Option<&str>) -> Vec<PathBuf> {
    let exe = if cfg!(windows) { "gh.exe" } else { "gh" };
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(path) = path_var {
        for dir in std::env::split_paths(path) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            out.push(dir.join(exe));
        }
    }
    if !cfg!(windows) {
        out.extend(GH_FALLBACKS.iter().map(PathBuf::from));
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// How long `gh auth token` may take. It reads a keyring, which is local and
/// fast — but a locked keyring can sit on a prompt, and the panel must not
/// wait on that forever.
const GH_TIMEOUT: Duration = Duration::from_secs(5);

/// `gh auth status` asks GitHub about every account, so it gets a network
/// round trip's worth more.
const GH_STATUS_TIMEOUT: Duration = Duration::from_secs(20);

fn system_env<'a>() -> TokenEnv<'a> {
    TokenEnv {
        var: &|name| std::env::var(name).ok(),
        is_file: &|p| p.is_file(),
        gh: &run_gh,
    }
}

/// [`resolve_token`] against the real process environment. Blocking: call it
/// off the UI thread.
pub fn resolve_token_from_system() -> Option<(Token, TokenSource)> {
    resolve_token(&system_env())
}

/// [`other_gh_accounts`] against the real `gh`. Blocking: call it off the UI
/// thread.
pub fn other_gh_accounts_from_system() -> Vec<(String, Token)> {
    other_gh_accounts(&system_env())
}

fn run_gh(gh: &Path, args: &[&str]) -> Option<String> {
    use crate::core::proc::{hide_console, output_within};
    let mut cmd = std::process::Command::new(gh);
    cmd.args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1");
    let timeout = if args.get(1) == Some(&"status") {
        GH_STATUS_TIMEOUT
    } else {
        GH_TIMEOUT
    };
    // A GUI app spawning a console program on Windows flashes a console
    // window unless told not to.
    let out = output_within(hide_console(&mut cmd), timeout).ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    struct Fake {
        vars: HashMap<&'static str, &'static str>,
        files: Vec<PathBuf>,
        gh_out: Option<&'static str>,
        status_out: Option<&'static str>,
        per_user: HashMap<&'static str, &'static str>,
        asked: RefCell<Vec<PathBuf>>,
        args: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                vars: HashMap::new(),
                files: Vec::new(),
                gh_out: None,
                status_out: None,
                per_user: HashMap::new(),
                asked: RefCell::new(Vec::new()),
                args: RefCell::new(Vec::new()),
            }
        }

        fn with_env<T>(&self, f: impl FnOnce(&TokenEnv<'_>) -> T) -> T {
            f(&TokenEnv {
                var: &|name| self.vars.get(name).map(|v| v.to_string()),
                is_file: &|p| self.files.iter().any(|f| f == p),
                gh: &|p, args| {
                    self.asked.borrow_mut().push(p.to_path_buf());
                    self.args.borrow_mut().push(args.join(" "));
                    match args {
                        ["auth", "status", ..] => self.status_out.map(str::to_string),
                        [.., "--user", user] => self.per_user.get(user).map(|t| t.to_string()),
                        _ => self.gh_out.map(str::to_string),
                    }
                },
            })
        }

        fn resolve(&self) -> Option<(Token, TokenSource)> {
            self.with_env(resolve_token)
        }
    }

    #[test]
    fn gh_token_outranks_github_token_and_both_outrank_the_cli() {
        let mut f = Fake::new();
        f.vars.insert("GH_TOKEN", "from-gh-token");
        f.vars.insert("GITHUB_TOKEN", "from-github-token");
        f.files.push(PathBuf::from("/usr/local/bin/gh"));
        f.gh_out = Some("from-cli\n");
        let (token, source) = f.resolve().unwrap();
        assert_eq!(token.expose(), "from-gh-token");
        assert_eq!(source, TokenSource::Env("GH_TOKEN"));
        assert!(
            f.asked.borrow().is_empty(),
            "gh is not run when env answers"
        );

        f.vars.remove("GH_TOKEN");
        let (token, source) = f.resolve().unwrap();
        assert_eq!(token.expose(), "from-github-token");
        assert_eq!(source, TokenSource::Env("GITHUB_TOKEN"));
    }

    #[test]
    fn a_blank_variable_is_no_token_and_falls_through() {
        let mut f = Fake::new();
        f.vars.insert("GH_TOKEN", "   ");
        // A `gh` on PATH, spelled the way this platform spells it: the
        // Homebrew fallbacks don't exist on Windows, so the test must not
        // lean on them.
        let path = if cfg!(windows) {
            r"C:\fake\bin"
        } else {
            "/fake/bin"
        };
        f.vars.insert("PATH", path);
        let gh = gh_candidates(Some(path)).remove(0);
        f.files.push(gh.clone());
        f.gh_out = Some("gho_cli\n");
        let (token, source) = f.resolve().unwrap();
        assert_eq!(token.expose(), "gho_cli");
        assert_eq!(source, TokenSource::GhCli);
        assert_eq!(*f.asked.borrow(), vec![gh]);
    }

    #[cfg(unix)]
    #[test]
    fn gh_on_path_is_asked_before_the_homebrew_fallbacks() {
        let mut f = Fake::new();
        f.vars.insert("PATH", "/usr/bin:/home/me/bin");
        f.files.push(PathBuf::from("/home/me/bin/gh"));
        f.files.push(PathBuf::from("/opt/homebrew/bin/gh"));
        f.gh_out = Some("gho_x\n");
        f.resolve().unwrap();
        assert_eq!(*f.asked.borrow(), vec![PathBuf::from("/home/me/bin/gh")]);
    }

    #[cfg(unix)]
    #[test]
    fn a_finder_launch_still_finds_homebrew_gh() {
        let mut f = Fake::new();
        // launchd's PATH: none of it holds gh.
        f.vars.insert("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        f.files.push(PathBuf::from("/opt/homebrew/opt/gh/bin/gh"));
        f.gh_out = Some("gho_brew\n");
        let (token, _) = f.resolve().unwrap();
        assert_eq!(token.expose(), "gho_brew");
        assert_eq!(
            *f.asked.borrow(),
            vec![PathBuf::from("/opt/homebrew/opt/gh/bin/gh")]
        );
    }

    #[test]
    fn no_env_and_no_gh_is_unauthenticated_not_an_error() {
        let f = Fake::new();
        assert!(f.resolve().is_none());
        assert!(f.asked.borrow().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_signed_out_gh_is_unauthenticated() {
        let mut f = Fake::new();
        f.files.push(PathBuf::from("/usr/local/bin/gh"));
        f.gh_out = None;
        assert!(f.resolve().is_none());
        f.gh_out = Some("\n");
        assert!(f.resolve().is_none());
    }

    #[test]
    fn candidates_skip_empty_path_entries_and_duplicates() {
        let c = gh_candidates(Some(""));
        let exe = if cfg!(windows) { "gh.exe" } else { "gh" };
        assert!(c.iter().all(|p| p.file_name().unwrap() == exe));
        let unique: std::collections::HashSet<_> = c.iter().collect();
        assert_eq!(unique.len(), c.len());
    }

    const STATUS: &str = r#"{"hosts":{"github.com":[
        {"state":"success","active":true,"host":"github.com","login":"me"},
        {"state":"success","active":false,"host":"github.com","login":"work"},
        {"state":"error","active":false,"host":"github.com","login":"expired"},
        {"state":"success","active":false,"host":"github.com","login":"side"}
      ],"ghe.example.com":[
        {"state":"success","active":false,"host":"ghe.example.com","login":"elsewhere"}
      ]}}"#;

    #[cfg(unix)]
    #[test]
    fn other_accounts_are_the_inactive_signed_in_github_com_ones() {
        let mut f = Fake::new();
        f.files.push(PathBuf::from("/usr/local/bin/gh"));
        f.status_out = Some(STATUS);
        f.per_user.insert("work", "gho_work\n");
        f.per_user.insert("side", "gho_side\n");
        let got: Vec<(String, String)> = f
            .with_env(other_gh_accounts)
            .into_iter()
            .map(|(login, t)| (login, t.expose().to_string()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("work".to_string(), "gho_work".to_string()),
                ("side".to_string(), "gho_side".to_string()),
            ]
        );
        assert!(
            f.args
                .borrow()
                .contains(&"auth token --hostname github.com --user work".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_account_gh_has_no_token_for_is_skipped() {
        let mut f = Fake::new();
        f.files.push(PathBuf::from("/usr/local/bin/gh"));
        f.status_out = Some(STATUS);
        f.per_user.insert("side", "gho_side\n");
        let got = f.with_env(other_gh_accounts);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "side");
    }

    #[test]
    fn no_gh_or_an_unreadable_status_is_no_other_accounts() {
        let f = Fake::new();
        assert!(f.with_env(other_gh_accounts).is_empty());
        assert!(inactive_logins("gh: unknown flag --json").is_empty());
        assert!(inactive_logins(r#"{"hosts":{}}"#).is_empty());
    }

    #[test]
    fn a_token_never_prints_itself() {
        let token = Token::new("ghp_secretsecret").unwrap();
        let shown = format!("{token:?} {:?}", Some((token.clone(), TokenSource::GhCli)));
        assert!(!shown.contains("secret"), "{shown}");
    }
}

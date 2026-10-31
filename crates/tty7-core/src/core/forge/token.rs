//! A forge token, wherever it was found.
//!
//! tty7 has no OAuth app and stores no credential of its own: it borrows the
//! sign-in the user already gave a forge's CLI — `gh` (`super::github::gh`)
//! — or set in the environment.
//!
//! The token never reaches a log line or the screen: [`Token`]'s `Debug`
//! prints a placeholder, and nothing else in tty7 formats it.
//!
//! Finding and running those CLIs is the same for each of them, and is here
//! too ([`cli_candidates`], [`run_cli`]).

use std::path::{Path, PathBuf};
use std::time::Duration;

/// A forge token. Opaque on purpose — see the module docs.
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

/// Every place the CLI `exe` might be, `PATH` first, then `fallbacks` (where
/// it lands when installed outside the login `PATH`), without duplicates.
/// On Windows `exe` gets its `.exe`, and there are no fallbacks.
pub(crate) fn cli_candidates(
    exe: &str,
    path_var: Option<&str>,
    fallbacks: &[&str],
) -> Vec<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{exe}.exe")
    } else {
        exe.to_string()
    };
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(path) = path_var {
        for dir in std::env::split_paths(path) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            out.push(dir.join(&exe));
        }
    }
    if !cfg!(windows) {
        out.extend(fallbacks.iter().map(PathBuf::from));
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// Run `<cli> <args>` with `env` set, and hand back its stdout if it
/// succeeds within `timeout`.
pub(crate) fn run_cli(
    cli: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    timeout: Duration,
) -> Option<String> {
    use crate::core::proc::{hide_console, output_within};
    let mut cmd = std::process::Command::new(cli);
    cmd.args(args).envs(env.iter().copied());
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

    #[test]
    fn candidates_skip_empty_path_entries_and_duplicates() {
        let c = cli_candidates("gh", Some(""), &["/usr/local/bin/gh", "/usr/local/bin/gh"]);
        let exe = if cfg!(windows) { "gh.exe" } else { "gh" };
        assert!(c.iter().all(|p| p.file_name().unwrap() == exe));
        let unique: std::collections::HashSet<_> = c.iter().collect();
        assert_eq!(unique.len(), c.len());
    }

    #[test]
    fn a_token_never_prints_itself() {
        let token = Token::new("ghp_secretsecret").unwrap();
        let shown = format!("{token:?} {:?}", Some((token.clone(), TokenSource::GhCli)));
        assert!(!shown.contains("secret"), "{shown}");
    }
}

//! Lightweight discovery of OpenSSH host aliases for UI pickers.
//!
//! tty7 does not try to resolve the final SSH configuration here. OpenSSH is the
//! source of truth for `HostName`, `User`, `Port`, `ProxyJump`, `Match`, and the
//! rest when we eventually run `ssh <alias>`. This module only finds concrete
//! `Host` aliases worth listing in the command palette.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

const MAX_INCLUDE_DEPTH: usize = 8;
const MAX_CONFIG_FILES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshProfile {
    pub alias: String,
    pub source: PathBuf,
}

pub fn discover_profiles() -> Vec<SshProfile> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    discover_profiles_from(home.join(".ssh/config"), &home)
}

fn discover_profiles_from(root: PathBuf, home: &Path) -> Vec<SshProfile> {
    let mut profiles = Vec::new();
    let mut aliases = HashSet::new();
    let mut seen_files = HashSet::new();
    let mut queue = VecDeque::from([(root, 0usize)]);

    while let Some((path, depth)) = queue.pop_front() {
        if depth > MAX_INCLUDE_DEPTH || seen_files.len() >= MAX_CONFIG_FILES {
            continue;
        }
        let path = expand_path(&path, home);
        if !seen_files.insert(path.clone()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let base = path.parent().unwrap_or(home);
        for line in text.lines() {
            let line = strip_comment(line).trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, rest)) = split_keyword(line) else {
                continue;
            };
            if key.eq_ignore_ascii_case("host") {
                for token in split_words(rest) {
                    if concrete_host_alias(&token) && aliases.insert(token.clone()) {
                        profiles.push(SshProfile {
                            alias: token,
                            source: path.clone(),
                        });
                    }
                }
            } else if key.eq_ignore_ascii_case("include") {
                for token in split_words(rest) {
                    for include in expand_include(&token, base, home) {
                        queue.push_back((include, depth + 1));
                    }
                }
            }
        }
    }

    profiles.sort_by(|a, b| a.alias.cmp(&b.alias).then_with(|| a.source.cmp(&b.source)));
    profiles
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
    }
}

fn strip_comment(line: &str) -> &str {
    line.split_once('#').map(|(head, _)| head).unwrap_or(line)
}

fn split_keyword(line: &str) -> Option<(&str, &str)> {
    let line = line.trim_start();
    let ix = line.find(char::is_whitespace)?;
    Some((&line[..ix], line[ix..].trim_start()))
}

fn split_words(input: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            (Some(_), c) => current.push(c),
            (None, '\'' | '"') => quote = Some(ch),
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            (None, c) => current.push(c),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn concrete_host_alias(alias: &str) -> bool {
    !alias.is_empty()
        && !alias.starts_with('!')
        && !alias.chars().any(|ch| matches!(ch, '*' | '?' | '[' | ']'))
}

fn expand_include(pattern: &str, base: &Path, home: &Path) -> Vec<PathBuf> {
    let pattern = expand_path(&PathBuf::from(pattern), home);
    let pattern = if pattern.is_absolute() {
        pattern
    } else {
        base.join(pattern)
    };
    let text = pattern.to_string_lossy();
    if !text.contains('*') && !text.contains('?') {
        return vec![pattern];
    }

    expand_one_glob(&pattern)
}

fn expand_path(path: &Path, home: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home.join(rest);
    }
    path.to_path_buf()
}

fn expand_one_glob(pattern: &Path) -> Vec<PathBuf> {
    let Some(parent) = pattern.parent() else {
        return Vec::new();
    };
    let Some(file_pattern) = pattern.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if glob_match(file_pattern, name) {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn glob_match(pattern: &str, text: &str) -> bool {
    fn inner(p: &[u8], t: &[u8]) -> bool {
        match (p.split_first(), t.split_first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some((&b'*', rest)), _) => inner(rest, t) || (!t.is_empty() && inner(p, &t[1..])),
            (Some((&b'?', rest)), Some(_)) => inner(rest, &t[1..]),
            (Some((&pc, rest)), Some((&tc, tail))) if pc == tc => inner(rest, tail),
            _ => false,
        }
    }
    inner(pattern.as_bytes(), text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_concrete_host_aliases_and_skips_patterns() {
        let root = temp_root("hosts");
        let ssh = root.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("config"),
            "Host dev *.corp !blocked prod\n  User me\nHost \"quoted host\"\n",
        )
        .unwrap();

        let aliases: Vec<_> = discover_profiles_from(ssh.join("config"), &root)
            .into_iter()
            .map(|p| p.alias)
            .collect();
        assert_eq!(aliases, vec!["dev", "prod", "quoted host"]);
    }

    #[test]
    fn follows_includes_relative_to_config_file() {
        let root = temp_root("includes");
        let ssh = root.join(".ssh");
        std::fs::create_dir_all(ssh.join("conf.d")).unwrap();
        std::fs::write(ssh.join("config"), "Include conf.d/*\nHost root\n").unwrap();
        std::fs::write(ssh.join("conf.d/dev"), "Host dev\n").unwrap();
        std::fs::write(ssh.join("conf.d/prod"), "Host prod\n").unwrap();

        let aliases: Vec<_> = discover_profiles_from(ssh.join("config"), &root)
            .into_iter()
            .map(|p| p.alias)
            .collect();
        assert_eq!(aliases, vec!["dev", "prod", "root"]);
    }

    #[test]
    fn glob_match_supports_star_and_question() {
        assert!(glob_match("*.conf", "dev.conf"));
        assert!(glob_match("host?", "host1"));
        assert!(!glob_match("host?", "host12"));
    }

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tty7-ssh-config-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

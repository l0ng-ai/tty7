//! Lightweight discovery of OpenSSH host aliases for UI pickers.
//!
//! tty7 does not try to resolve the final SSH configuration here. OpenSSH is the
//! source of truth for `HostName`, `User`, `Port`, `ProxyJump`, `Match`, and the
//! rest when we eventually run `ssh <alias>`. This module only finds concrete
//! `Host` aliases worth listing in the command palette.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::core::ssh_profile::SshProfile as ManagedProfile;

const MAX_INCLUDE_DEPTH: usize = 8;
const MAX_CONFIG_FILES: usize = 256;

/// The `group` label stamped on profiles imported from `~/.ssh/config` (also the
/// marker used to recognize them). Newly imported entries get this; an existing
/// profile's group is preserved on re-import.
pub const IMPORTED_GROUP: &str = "Imported from ssh_config";

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

// ─────────────────────────────────────────────────────────────────────────────
// ssh_config → profile import (PRD §3.3)
//
// `discover_profiles` above stays untouched (live alias discovery for the
// palette). The code below resolves the common fields of each concrete `Host`
// alias into a [`ManagedProfile`], so users who want credentials / SFTP / forward
// management on a config entry can import it. Scope, per PRD §3.3:
//
// - fields resolved: HostName, User, Port, IdentityFile (multiple), ProxyJump,
//   ProxyCommand, ForwardAgent;
// - first-match-wins per OpenSSH semantics, including wildcard `Host *` fallbacks;
// - `Match` blocks and `canonicalize` are intentionally NOT evaluated (a config
//   that needs them should use per-profile system-ssh compat mode instead);
// - import is explicit and repeatable: re-importing an unchanged config is a
//   no-op (existing profiles are matched by name and their ids/secrets/flags kept).
// ─────────────────────────────────────────────────────────────────────────────

/// One imported alias: the resolved profile plus the raw `ProxyJump` target (if
/// any). Jump targets are strings here; mapping them to a profile id happens in
/// [`merge_imported`], once all imported profiles have ids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedProfile {
    /// The resolved profile (its `jump_host` is always `None` at this stage).
    pub profile: ManagedProfile,
    /// The raw `ProxyJump` target as written (e.g. `bastion`, `me@jump:2222`), if
    /// the alias set one.
    pub proxy_jump: Option<String>,
}

/// Parse `~/.ssh/config` (following `Include`) and resolve every concrete `Host`
/// alias into an [`ImportedProfile`]. Returns an empty vec when no config exists.
// Consumed by the import UI (a later workstream); unused until that merges.
#[allow(dead_code)]
pub fn import_profiles() -> Vec<ImportedProfile> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    import_profiles_from(home.join(".ssh/config"), &home)
}

/// [`import_profiles`] against an explicit root/home (for tests).
pub fn import_profiles_from(root: PathBuf, home: &Path) -> Vec<ImportedProfile> {
    let blocks = parse_config_blocks(root, home);

    // Collect concrete aliases in first-seen order (dedup, skip wildcards/negations
    // and the synthetic pre-Host global block).
    let mut aliases: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for block in &blocks {
        for pat in &block.patterns {
            if concrete_host_alias(pat) && seen.insert(pat.clone()) {
                aliases.push(pat.clone());
            }
        }
    }
    aliases.sort();

    aliases
        .into_iter()
        .map(|alias| {
            let resolved = resolve_alias(&alias, &blocks);
            let mut profile = ManagedProfile::new(alias.clone());
            profile.group = Some(IMPORTED_GROUP.to_string());
            profile.host = resolved.hostname.unwrap_or(alias);
            profile.user = resolved.user.unwrap_or_default();
            profile.port = resolved.port.unwrap_or(22);
            profile.identity_files = resolved.identity_files;
            profile.proxy_command = resolved.proxy_command;
            profile.agent_forward = resolved.forward_agent.unwrap_or(false);
            ImportedProfile {
                profile,
                proxy_jump: resolved.proxy_jump,
            }
        })
        .collect()
}

/// Upsert `imported` into `existing`, matched by profile **name** (the alias).
///
/// - New alias → pushed with a fresh id and the [`IMPORTED_GROUP`] label.
/// - Existing name → connection fields are overwritten (host/port/user/identity
///   files/proxy command/agent-forward/jump host); the user-owned id, group,
///   `credential_ref`, auth, forwards, and other flags are preserved.
///
/// `ProxyJump` targets are resolved to `jump_host` ids in a second pass by
/// matching the jump alias against a profile name; unresolved targets leave
/// `jump_host` as `None`.
// Consumed by the import UI (a later workstream); unused until that merges.
#[allow(dead_code)]
pub fn merge_imported(existing: &mut Vec<ManagedProfile>, imported: Vec<ImportedProfile>) {
    // Remember each imported alias's raw jump target for the resolve pass.
    let mut jump_targets: Vec<(String, String)> = Vec::new();

    for entry in imported {
        let ImportedProfile { profile, proxy_jump } = entry;
        if let Some(raw) = proxy_jump {
            jump_targets.push((profile.name.clone(), raw));
        }
        match existing.iter_mut().find(|p| p.name == profile.name) {
            Some(current) => {
                // Overwrite connection fields; keep everything user-owned.
                current.host = profile.host;
                current.port = profile.port;
                current.user = profile.user;
                current.identity_files = profile.identity_files;
                current.proxy_command = profile.proxy_command;
                current.agent_forward = profile.agent_forward;
            }
            None => existing.push(profile),
        }
    }

    // Second pass: resolve jump aliases → profile ids now that all names exist.
    for (name, raw) in jump_targets {
        let Some(target_alias) = jump_alias(&raw) else {
            continue;
        };
        let target_id = existing
            .iter()
            .find(|p| p.name == target_alias)
            .map(|p| p.id);
        if let Some(profile) = existing.iter_mut().find(|p| p.name == name) {
            profile.jump_host = target_id;
        }
    }
}

/// Extract the alias/host from a `ProxyJump` target, taking the first hop of a
/// comma-separated chain and stripping any `user@`/`:port` (bracketed IPv6 aware).
#[allow(dead_code)] // only reached via merge_imported (a later workstream's entry point)
fn jump_alias(raw: &str) -> Option<String> {
    let first = raw.split(',').next().unwrap_or(raw).trim();
    if first.is_empty() {
        return None;
    }
    crate::core::ssh_profile::parse_quick_connect(first).map(|q| q.host)
}

/// A single `Host <patterns>` block with its option lines (keyword lowercased).
struct HostBlock {
    patterns: Vec<String>,
    options: Vec<(String, String)>,
}

/// The subset of resolved options an import cares about.
#[derive(Default)]
struct ResolvedHost {
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity_files: Vec<String>,
    proxy_jump: Option<String>,
    proxy_command: Option<String>,
    forward_agent: Option<bool>,
}

/// Walk the config (expanding `Include` inline so file order — and thus
/// first-match-wins — is preserved) into an ordered list of [`HostBlock`]s.
fn parse_config_blocks(root: PathBuf, home: &Path) -> Vec<HostBlock> {
    let mut blocks = Vec::new();
    let mut seen = HashSet::new();
    parse_config_file(&root, home, 0, &mut blocks, &mut seen);
    blocks
}

fn parse_config_file(
    path: &Path,
    home: &Path,
    depth: usize,
    blocks: &mut Vec<HostBlock>,
    seen: &mut HashSet<PathBuf>,
) {
    if depth > MAX_INCLUDE_DEPTH || seen.len() >= MAX_CONFIG_FILES {
        return;
    }
    let path = expand_path(path, home);
    if !seen.insert(path.clone()) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let base = path.parent().unwrap_or(home).to_path_buf();

    let mut current: Option<HostBlock> = None;
    // Options appearing before the first `Host` apply globally; model them as a
    // synthetic `Host *` block so first-match-wins picks them up as a fallback.
    let mut global: Option<HostBlock> = None;
    // Inside an (unsupported) `Match` block, ignore option lines until the next
    // `Host`.
    let mut in_match = false;

    for line in text.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, rest)) = split_keyword(line) else {
            continue;
        };

        if key.eq_ignore_ascii_case("host") {
            in_match = false;
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            current = Some(HostBlock {
                patterns: split_words(rest),
                options: Vec::new(),
            });
        } else if key.eq_ignore_ascii_case("match") {
            // Match is not evaluated; flush the current block and skip its options.
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            in_match = true;
        } else if key.eq_ignore_ascii_case("include") {
            if in_match {
                continue;
            }
            // Flush the current block so included content sorts after it (close
            // enough for first-match-wins; nested-within-a-Host includes are rare).
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            if let Some(block) = global.take() {
                blocks.push(block);
            }
            for token in split_words(rest) {
                for include in expand_include(&token, &base, home) {
                    parse_config_file(&include, home, depth + 1, blocks, seen);
                }
            }
        } else if !in_match {
            let opt = (key.to_ascii_lowercase(), rest.to_string());
            match current.as_mut() {
                Some(block) => block.options.push(opt),
                None => global
                    .get_or_insert_with(|| HostBlock {
                        patterns: vec!["*".to_string()],
                        options: Vec::new(),
                    })
                    .options
                    .push(opt),
            }
        }
    }
    if let Some(block) = current.take() {
        blocks.push(block);
    }
    if let Some(block) = global.take() {
        blocks.push(block);
    }
}

/// Resolve one alias against the ordered blocks with first-match-wins semantics
/// (wildcard blocks included). `IdentityFile` accumulates across matching blocks.
fn resolve_alias(alias: &str, blocks: &[HostBlock]) -> ResolvedHost {
    let mut r = ResolvedHost::default();
    for block in blocks {
        if !block_matches(block, alias) {
            continue;
        }
        for (key, val) in &block.options {
            match key.as_str() {
                "hostname" if r.hostname.is_none() => {
                    r.hostname = first_word(val);
                }
                "user" if r.user.is_none() => {
                    r.user = first_word(val);
                }
                "port" if r.port.is_none() => {
                    r.port = first_word(val).and_then(|p| p.parse::<u16>().ok());
                }
                "identityfile" => {
                    if let Some(file) = first_word(val) {
                        if !r.identity_files.contains(&file) {
                            r.identity_files.push(file);
                        }
                    }
                }
                "proxyjump" if r.proxy_jump.is_none() => {
                    let v = val.trim();
                    if !v.is_empty() && !v.eq_ignore_ascii_case("none") {
                        r.proxy_jump = Some(v.to_string());
                    }
                }
                "proxycommand" if r.proxy_command.is_none() => {
                    // A ProxyCommand is a whole command line — do not tokenize it.
                    let v = val.trim();
                    if !v.is_empty() && !v.eq_ignore_ascii_case("none") {
                        r.proxy_command = Some(v.to_string());
                    }
                }
                "forwardagent" if r.forward_agent.is_none() => {
                    r.forward_agent = first_word(val).map(|v| {
                        matches!(v.to_ascii_lowercase().as_str(), "yes" | "true")
                    });
                }
                _ => {}
            }
        }
    }
    r
}

/// Whether a block's pattern list matches `alias` (OpenSSH semantics: at least one
/// positive `*`/`?` glob matches and no negated `!pattern` matches).
fn block_matches(block: &HostBlock, alias: &str) -> bool {
    let mut positive = false;
    for pat in &block.patterns {
        if let Some(neg) = pat.strip_prefix('!') {
            if glob_match(neg, alias) {
                return false;
            }
        } else if glob_match(pat, alias) {
            positive = true;
        }
    }
    positive
}

/// The first whitespace-delimited word of a value, respecting quotes.
fn first_word(value: &str) -> Option<String> {
    split_words(value).into_iter().next()
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

    #[test]
    fn import_resolves_common_fields_with_first_match_wins() {
        let root = temp_root("import-fields");
        let ssh = root.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("config"),
            concat!(
                "Host prod\n",
                "  HostName 10.0.0.5\n",
                "  User deploy\n",
                "  Port 2222\n",
                "  IdentityFile ~/.ssh/id_prod\n",
                "  ProxyJump bastion\n",
                "  ForwardAgent yes\n",
                "Host bastion\n",
                "  HostName jump.example.com\n",
                "  ProxyCommand corkscrew proxy 8080 %h %p\n",
                "Host *\n",
                "  User fallback-user\n",
                "  IdentityFile ~/.ssh/id_common\n",
            ),
        )
        .unwrap();

        let imported = import_profiles_from(ssh.join("config"), &root);
        // Sorted by alias: bastion, prod.
        let names: Vec<_> = imported.iter().map(|i| i.profile.name.as_str()).collect();
        assert_eq!(names, vec!["bastion", "prod"]);

        let prod = &imported[1];
        assert_eq!(prod.profile.host, "10.0.0.5");
        assert_eq!(prod.profile.user, "deploy"); // specific block wins over Host *
        assert_eq!(prod.profile.port, 2222);
        // IdentityFile accumulates: the profile's own, then the Host * fallback.
        assert_eq!(
            prod.profile.identity_files,
            vec!["~/.ssh/id_prod".to_string(), "~/.ssh/id_common".to_string()]
        );
        assert!(prod.profile.agent_forward);
        assert_eq!(prod.proxy_jump.as_deref(), Some("bastion"));
        assert_eq!(prod.profile.group.as_deref(), Some(IMPORTED_GROUP));

        let bastion = &imported[0];
        assert_eq!(bastion.profile.host, "jump.example.com");
        // No User set → falls back to Host *.
        assert_eq!(bastion.profile.user, "fallback-user");
        assert_eq!(
            bastion.profile.proxy_command.as_deref(),
            Some("corkscrew proxy 8080 %h %p")
        );
    }

    #[test]
    fn import_skips_match_blocks_and_negations() {
        let root = temp_root("import-match");
        let ssh = root.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("config"),
            concat!(
                "Host secure\n",
                "  HostName real.example.com\n",
                "Match host secure\n",
                "  User should-be-ignored\n",
                "Host web !web-staging\n", // negation-bearing pattern list (not concrete)
                "  HostName web.example.com\n",
            ),
        )
        .unwrap();

        let imported = import_profiles_from(ssh.join("config"), &root);
        let names: Vec<_> = imported.iter().map(|i| i.profile.name.as_str()).collect();
        // `secure` and `web` are concrete; `!web-staging` is a negation, not an alias.
        assert_eq!(names, vec!["secure", "web"]);
        // The Match block's User must not leak onto `secure`.
        let secure = imported.iter().find(|i| i.profile.name == "secure").unwrap();
        assert_eq!(secure.profile.user, "");
    }

    #[test]
    fn merge_upserts_by_name_preserving_user_fields_and_is_idempotent() {
        use crate::core::keychain::CredentialRef;
        use crate::core::ssh_profile::AuthMode;

        let root = temp_root("import-merge");
        let ssh = root.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("config"),
            "Host prod\n  HostName 10.0.0.5\n  User deploy\nHost bastion\n  HostName jump\n",
        )
        .unwrap();

        // A user already has a `prod` profile with a credential + custom auth.
        let mut existing = vec![{
            let mut p = ManagedProfile::new("prod");
            p.host = "old-host".to_string();
            p.auth = AuthMode::Password;
            p.credential_ref = Some(CredentialRef::password("deploy", "10.0.0.5", 22));
            p.group = Some("My Servers".to_string());
            p
        }];
        let prod_id = existing[0].id;

        let imported = import_profiles_from(ssh.join("config"), &root);
        merge_imported(&mut existing, imported);

        assert_eq!(existing.len(), 2); // prod updated + bastion added
        let prod = existing.iter().find(|p| p.name == "prod").unwrap();
        // Connection field overwritten...
        assert_eq!(prod.host, "10.0.0.5");
        // ...but id, group, credential, and auth preserved.
        assert_eq!(prod.id, prod_id);
        assert_eq!(prod.group.as_deref(), Some("My Servers"));
        assert_eq!(prod.auth, AuthMode::Password);
        assert!(prod.credential_ref.is_some());

        let bastion = existing.iter().find(|p| p.name == "bastion").unwrap();
        assert_eq!(bastion.group.as_deref(), Some(IMPORTED_GROUP));

        // Re-import of the unchanged config is a no-op (idempotent): same ids, same
        // count, same fields.
        let snapshot = existing.clone();
        let imported_again = import_profiles_from(ssh.join("config"), &root);
        merge_imported(&mut existing, imported_again);
        assert_eq!(existing, snapshot);
    }

    #[test]
    fn merge_resolves_proxy_jump_to_profile_id() {
        let root = temp_root("import-jump");
        let ssh = root.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("config"),
            "Host prod\n  HostName 10.0.0.5\n  ProxyJump me@bastion:2222\nHost bastion\n  HostName jump\n",
        )
        .unwrap();

        let mut existing = Vec::new();
        merge_imported(&mut existing, import_profiles_from(ssh.join("config"), &root));

        let bastion_id = existing.iter().find(|p| p.name == "bastion").unwrap().id;
        let prod = existing.iter().find(|p| p.name == "prod").unwrap();
        // The `me@bastion:2222` jump target resolves to the `bastion` profile's id.
        assert_eq!(prod.jump_host, Some(bastion_id));
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

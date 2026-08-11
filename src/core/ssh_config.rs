use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::core::ssh_profile::{ForwardKind, ForwardRule, HostPort, SshProfile as ManagedProfile};

const MAX_INCLUDE_DEPTH: usize = 8;
const MAX_CONFIG_FILES: usize = 256;

pub const IMPORTED_GROUP: &str = "Imported from ssh_config";

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

fn expand_hostname_tokens(hostname: &str, alias: &str) -> String {
    let mut out = String::with_capacity(hostname.len());
    let mut chars = hostname.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('h') => out.push_str(alias),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

fn strip_comment(line: &str) -> &str {
    if line.trim_start().starts_with('#') {
        ""
    } else {
        line
    }
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedProfile {
    pub profile: ManagedProfile,
    pub proxy_jump: Option<String>,
}

/// A keyword the file sets for a host tty7 is importing, that no `SshProfile`
/// field can hold — `IdentityAgent`, `CertificateFile`, `AddKeysToAgent` and
/// the rest of what `resolve_alias` walks past.
///
/// Grouped by keyword rather than by host because that is the question someone
/// reads the report to answer — "what did it not keep?" — and because the same
/// keyword under a two-alias `Host` line is one omission, not two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IgnoredOption {
    /// Spelled as the file spells it. A report that says `identityagent` when
    /// the file says `IdentityAgent` sends the reader hunting for a typo that
    /// is tty7's, not theirs.
    pub option: String,
    pub hosts: Vec<String>,
}

/// Everything one pass over `~/.ssh/config` found, including the parts of it
/// that went nowhere.
///
/// `import_profiles_from` answers only the first field, because it is also on a
/// render path; the import button wants the rest so it can say what happened
/// instead of leaving the person to diff the host list by eye.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReport {
    pub profiles: Vec<ImportedProfile>,
    pub ignored: Vec<IgnoredOption>,
    pub source: PathBuf,
    /// Whether `source` itself could be opened. An `Include` that matches
    /// nothing is ordinary — most of these files carry one for a directory
    /// that may or may not exist — but a root that cannot be read is the whole
    /// import, and the two are indistinguishable in `profiles`, which comes
    /// back empty either way.
    pub source_read: bool,
    /// `source` plus every `Include` that resolved to a file that was read,
    /// for the log line. Someone who edited the wrong `conf.d` fragment finds
    /// out here that tty7 never opened it.
    pub files_read: usize,
}

#[allow(dead_code)]
pub fn import_profiles() -> Vec<ImportedProfile> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    import_profiles_from(home.join(".ssh/config"), &home)
}

pub fn import_profiles_from(root: PathBuf, home: &Path) -> Vec<ImportedProfile> {
    profiles_from_blocks(&parse_config(&root, home).blocks)
}

pub fn import_report() -> ImportReport {
    let Some(home) = home_dir() else {
        // Without a home directory there is no path to have failed at, and the
        // caller still has to name one. `~/.ssh/config` is the name the button
        // itself uses, so it is the name the failure uses too.
        return ImportReport {
            profiles: Vec::new(),
            ignored: Vec::new(),
            source: PathBuf::from("~/.ssh/config"),
            source_read: false,
            files_read: 0,
        };
    };
    import_report_from(home.join(".ssh/config"), &home)
}

pub fn import_report_from(root: PathBuf, home: &Path) -> ImportReport {
    let parsed = parse_config(&root, home);
    ImportReport {
        profiles: profiles_from_blocks(&parsed.blocks),
        ignored: ignored_options(&parsed.blocks),
        source: root,
        source_read: parsed.root_read,
        files_read: parsed.files_read,
    }
}

fn profiles_from_blocks(blocks: &[HostBlock]) -> Vec<ImportedProfile> {
    let mut aliases: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for block in blocks {
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
            let resolved = resolve_alias(&alias, blocks);
            let mut profile = ManagedProfile::new(alias.clone());
            profile.group = Some(IMPORTED_GROUP.to_string());
            let proxy_jump = apply_resolved(&mut profile, &alias, resolved);
            ImportedProfile {
                profile,
                proxy_jump,
            }
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAlias {
    pub profile: ManagedProfile,
    pub proxy_jump: Option<String>,
}

pub fn resolve_alias_to_profile(alias: &str) -> Option<ResolvedAlias> {
    let home = home_dir()?;
    resolve_alias_to_profile_from(home.join(".ssh/config"), &home, alias)
}

pub fn resolve_alias_to_profile_from(
    root: PathBuf,
    home: &Path,
    alias: &str,
) -> Option<ResolvedAlias> {
    let blocks = parse_config(&root, home).blocks;
    let matched = blocks.iter().any(|block| block_matches(block, alias));
    let resolved = resolve_alias(alias, &blocks);
    if !matched && resolved.hostname.is_none() {
        return None;
    }
    let mut profile = ManagedProfile::new(alias.to_string());
    let proxy_jump = apply_resolved(&mut profile, alias, resolved);
    Some(ResolvedAlias {
        profile,
        proxy_jump,
    })
}

fn apply_resolved(profile: &mut ManagedProfile, alias: &str, r: ResolvedHost) -> Option<String> {
    profile.host = r
        .hostname
        .map(|h| expand_hostname_tokens(&h, alias))
        .unwrap_or_else(|| alias.to_string());
    profile.user = r.user.unwrap_or_default();
    profile.port = r.port.unwrap_or(22);
    profile.identity_files = r.identity_files;
    profile.proxy_command = r.proxy_command;
    profile.agent_forward = r.forward_agent.unwrap_or(false);
    profile.connect_timeout_s = r.connect_timeout;
    profile.keepalive_interval_s = r.keepalive_interval;
    profile.keepalive_count_max = r.keepalive_count_max;
    profile.x11 = r.forward_x11.unwrap_or(false);
    profile.verify_host_keys = r.verify_host_keys;
    profile.algorithms.cipher = r.ciphers.unwrap_or_default();
    profile.algorithms.mac = r.macs.unwrap_or_default();
    profile.algorithms.kex = r.kex.unwrap_or_default();
    profile.algorithms.hostkey = r.hostkey_algorithms.unwrap_or_default();
    profile.algorithms.compression = if r.compression == Some(true) {
        vec![
            "zlib@openssh.com".to_string(),
            "zlib".to_string(),
            "none".to_string(),
        ]
    } else {
        Vec::new()
    };
    profile.forwards = r.forwards;
    r.proxy_jump
}

/// What a merge did, in the three counts the person who pressed the button is
/// owed: a host that arrived, one whose details moved, and one that already
/// said the right thing.
///
/// The third is the one worth having. Importing the same unedited file twice
/// changes nothing — the suite pins that — and a report that called those
/// hosts "updated" would be the old silence with a number on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeStats {
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
}

#[allow(dead_code)]
pub fn merge_imported(
    existing: &mut Vec<ManagedProfile>,
    imported: Vec<ImportedProfile>,
) -> MergeStats {
    let mut stats = MergeStats::default();
    let mut jump_targets: Vec<(String, String)> = Vec::new();

    for entry in imported {
        let ImportedProfile {
            profile,
            proxy_jump,
        } = entry;
        if let Some(raw) = proxy_jump {
            jump_targets.push((profile.name.clone(), raw));
        }
        match existing.iter_mut().find(|p| p.name == profile.name) {
            Some(current) => {
                // Asked before the assignments below, and only about the fields
                // they write: afterwards every one of them agrees by
                // construction, so the answer would always be "updated".
                let changed = current.host != profile.host
                    || current.port != profile.port
                    || current.user != profile.user
                    || current.identity_files != profile.identity_files
                    || current.proxy_command != profile.proxy_command
                    || current.agent_forward != profile.agent_forward;
                if changed {
                    stats.updated += 1;
                } else {
                    stats.unchanged += 1;
                }
                current.host = profile.host;
                current.port = profile.port;
                current.user = profile.user;
                current.identity_files = profile.identity_files;
                current.proxy_command = profile.proxy_command;
                current.agent_forward = profile.agent_forward;
            }
            None => {
                stats.added += 1;
                existing.push(profile);
            }
        }
    }

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

    stats
}

#[allow(dead_code)]
fn jump_alias(raw: &str) -> Option<String> {
    let first = raw.split(',').next().unwrap_or(raw).trim();
    if first.is_empty() {
        return None;
    }
    crate::core::ssh_profile::parse_quick_connect(first).map(|q| q.host)
}

struct HostBlock {
    patterns: Vec<String>,
    options: Vec<ConfigOption>,
}

/// One `Keyword value` line, kept twice over: `key` is what the resolver
/// matches on, `spelled` is what the import report shows a human.
struct ConfigOption {
    key: String,
    spelled: String,
    value: String,
}

#[derive(Default)]
struct ResolvedHost {
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity_files: Vec<String>,
    proxy_jump: Option<String>,
    proxy_command: Option<String>,
    forward_agent: Option<bool>,
    connect_timeout: Option<u32>,
    keepalive_interval: Option<u32>,
    keepalive_count_max: Option<u32>,
    ciphers: Option<Vec<String>>,
    macs: Option<Vec<String>>,
    kex: Option<Vec<String>>,
    hostkey_algorithms: Option<Vec<String>>,
    compression: Option<bool>,
    forward_x11: Option<bool>,
    verify_host_keys: Option<bool>,
    strict_seen: bool,
    forwards: Vec<ForwardRule>,
}

struct ParsedConfig {
    blocks: Vec<HostBlock>,
    root_read: bool,
    files_read: usize,
}

fn parse_config(root: &Path, home: &Path) -> ParsedConfig {
    let mut blocks = Vec::new();
    let mut seen = HashSet::new();
    let mut files_read = 0;
    let root_read = parse_config_file(root, home, 0, &mut blocks, &mut seen, &mut files_read);
    ParsedConfig {
        blocks,
        root_read,
        files_read,
    }
}

/// Returns whether this file was read, which only the root's answer is worth
/// anything: an include that resolves to nothing is a normal config, a root
/// that does not is a failed import.
fn parse_config_file(
    path: &Path,
    home: &Path,
    depth: usize,
    blocks: &mut Vec<HostBlock>,
    seen: &mut HashSet<PathBuf>,
    files_read: &mut usize,
) -> bool {
    if depth > MAX_INCLUDE_DEPTH || seen.len() >= MAX_CONFIG_FILES {
        return false;
    }
    let path = expand_path(path, home);
    if !seen.insert(path.clone()) {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    *files_read += 1;
    let base = path.parent().unwrap_or(home).to_path_buf();

    let mut current: Option<HostBlock> = None;
    let mut global: Option<HostBlock> = None;
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
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            in_match = true;
        } else if key.eq_ignore_ascii_case("include") {
            if in_match {
                continue;
            }
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            if let Some(block) = global.take() {
                blocks.push(block);
            }
            for token in split_words(rest) {
                for include in expand_include(&token, &base, home) {
                    parse_config_file(&include, home, depth + 1, blocks, seen, files_read);
                }
            }
        } else if !in_match {
            let opt = ConfigOption {
                key: key.to_ascii_lowercase(),
                spelled: key.to_string(),
                value: rest.to_string(),
            };
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
    true
}

/// Group the keywords no `SshProfile` field can hold, by keyword, over the
/// blocks that name at least one host tty7 is actually importing.
///
/// Grouping happens here rather than in `resolve_alias` because `resolve_alias`
/// runs once per alias: an option set under a pattern that matches ten hosts
/// would be reported ten times, and the `Host *` block would drag every
/// keyword in the file into the report for hosts that never set it.
///
/// `Host *` and `Match` blocks are left out for the same reason. A shared
/// config's `SendEnv LANG` at the top is not something the import dropped from
/// anyone's host; it is a line about hosts tty7 was never asked to import.
fn ignored_options(blocks: &[HostBlock]) -> Vec<IgnoredOption> {
    let mut by_keyword: BTreeMap<&str, IgnoredOption> = BTreeMap::new();
    for block in blocks {
        let mut hosts: Vec<&String> = block
            .patterns
            .iter()
            .filter(|pat| concrete_host_alias(pat))
            .collect();
        if hosts.is_empty() {
            continue;
        }
        hosts.sort();
        hosts.dedup();
        for opt in &block.options {
            if option_is_supported(&opt.key) {
                continue;
            }
            // Keyed on the lowercased form so `IdentityAgent` under one host
            // and `identityagent` under another are one entry; the spelling
            // shown is the first one the file uses.
            by_keyword
                .entry(&opt.key)
                .or_insert_with(|| IgnoredOption {
                    option: opt.spelled.clone(),
                    hosts: Vec::new(),
                })
                .hosts
                .extend(hosts.iter().map(|host| (*host).clone()));
        }
    }
    by_keyword
        .into_values()
        .map(|mut entry| {
            entry.hosts.sort();
            entry.hosts.dedup();
            entry
        })
        .collect()
}

/// The keywords `resolve_alias` below knows how to carry into an `SshProfile`.
///
/// A list of its own rather than a second one written out inside
/// `ignored_options`, because the report and the resolver have to answer the
/// same question and a hand-copied pair of lists is a thing that drifts.
/// `every_supported_keyword_is_kept` sets all twenty of these in one config and
/// fails if any of them comes back in the report as dropped.
fn option_is_supported(key: &str) -> bool {
    matches!(
        key,
        "hostname"
            | "user"
            | "port"
            | "identityfile"
            | "proxyjump"
            | "proxycommand"
            | "forwardagent"
            | "connecttimeout"
            | "serveraliveinterval"
            | "serveralivecountmax"
            | "ciphers"
            | "macs"
            | "kexalgorithms"
            | "hostkeyalgorithms"
            | "compression"
            | "forwardx11"
            | "stricthostkeychecking"
            | "localforward"
            | "remoteforward"
            | "dynamicforward"
    )
}

fn resolve_alias(alias: &str, blocks: &[HostBlock]) -> ResolvedHost {
    let mut r = ResolvedHost::default();
    for block in blocks {
        if !block_matches(block, alias) {
            continue;
        }
        for ConfigOption {
            key, value: val, ..
        } in &block.options
        {
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
                    let v = val.trim();
                    if !v.is_empty() && !v.eq_ignore_ascii_case("none") {
                        r.proxy_command = Some(v.to_string());
                    }
                }
                "forwardagent" if r.forward_agent.is_none() => {
                    r.forward_agent = first_word(val).map(|v| yes_no(&v));
                }
                "connecttimeout" if r.connect_timeout.is_none() => {
                    r.connect_timeout = first_word(val).and_then(|v| v.parse::<u32>().ok());
                }
                "serveraliveinterval" if r.keepalive_interval.is_none() => {
                    r.keepalive_interval = first_word(val).and_then(|v| v.parse::<u32>().ok());
                }
                "serveralivecountmax" if r.keepalive_count_max.is_none() => {
                    r.keepalive_count_max = first_word(val).and_then(|v| v.parse::<u32>().ok());
                }
                "ciphers" if r.ciphers.is_none() => {
                    r.ciphers = parse_algorithm_list(val);
                }
                "macs" if r.macs.is_none() => {
                    r.macs = parse_algorithm_list(val);
                }
                "kexalgorithms" if r.kex.is_none() => {
                    r.kex = parse_algorithm_list(val);
                }
                "hostkeyalgorithms" if r.hostkey_algorithms.is_none() => {
                    r.hostkey_algorithms = parse_algorithm_list(val);
                }
                "compression" if r.compression.is_none() => {
                    r.compression = first_word(val).map(|v| yes_no(&v));
                }
                "forwardx11" if r.forward_x11.is_none() => {
                    r.forward_x11 = first_word(val).map(|v| yes_no(&v));
                }
                "stricthostkeychecking" if !r.strict_seen => {
                    r.strict_seen = true;
                    if first_word(val).is_some_and(|v| {
                        matches!(v.to_ascii_lowercase().as_str(), "no" | "off" | "false")
                    }) {
                        r.verify_host_keys = Some(false);
                    }
                }
                "localforward" => {
                    if let Some(rule) = parse_forward_rule(ForwardKind::Local, val) {
                        r.forwards.push(rule);
                    }
                }
                "remoteforward" => {
                    if let Some(rule) = parse_forward_rule(ForwardKind::Remote, val) {
                        r.forwards.push(rule);
                    }
                }
                "dynamicforward" => {
                    if let Some(rule) = parse_forward_rule(ForwardKind::Dynamic, val) {
                        r.forwards.push(rule);
                    }
                }
                _ => {}
            }
        }
    }
    r
}

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

fn first_word(value: &str) -> Option<String> {
    split_words(value).into_iter().next()
}

fn yes_no(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "yes" | "true")
}

fn parse_algorithm_list(value: &str) -> Option<Vec<String>> {
    let token = first_word(value)?;
    if token.starts_with(['+', '-', '^']) {
        return None;
    }
    let list: Vec<String> = token
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    (!list.is_empty()).then_some(list)
}

fn parse_forward_rule(kind: ForwardKind, value: &str) -> Option<ForwardRule> {
    let words = split_words(value);
    let (bind_host, bind_port) = parse_forward_endpoint(words.first()?)?;
    let bind = HostPort::new(forward_bind_host(bind_host), bind_port);
    let target = match kind {
        ForwardKind::Dynamic => HostPort::default(),
        ForwardKind::Local | ForwardKind::Remote => {
            let (target_host, target_port) = parse_forward_endpoint(words.get(1)?)?;
            if target_host.is_empty() {
                return None;
            }
            HostPort::new(target_host, target_port)
        }
    };
    Some(ForwardRule {
        kind,
        bind,
        target,
        description: String::new(),
    })
}

fn parse_forward_endpoint(token: &str) -> Option<(String, u16)> {
    if let Some(rest) = token.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = rest[..close].to_string();
        let port = rest[close + 1..].strip_prefix(':')?.parse::<u16>().ok()?;
        return Some((host, port));
    }
    match token.rfind(':') {
        Some(ix) => {
            let host = token[..ix].to_string();
            let port = token[ix + 1..].parse::<u16>().ok()?;
            Some((host, port))
        }
        None => Some((String::new(), token.parse::<u16>().ok()?)),
    }
}

fn forward_bind_host(host: String) -> String {
    if host.is_empty() {
        "127.0.0.1".to_string()
    } else {
        host
    }
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

        let aliases: Vec<_> = import_profiles_from(ssh.join("config"), &root)
            .into_iter()
            .map(|p| p.profile.name)
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

        let aliases: Vec<_> = import_profiles_from(ssh.join("config"), &root)
            .into_iter()
            .map(|p| p.profile.name)
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
        let names: Vec<_> = imported.iter().map(|i| i.profile.name.as_str()).collect();
        assert_eq!(names, vec!["bastion", "prod"]);

        let prod = &imported[1];
        assert_eq!(prod.profile.host, "10.0.0.5");
        assert_eq!(prod.profile.user, "deploy");
        assert_eq!(prod.profile.port, 2222);
        assert_eq!(
            prod.profile.identity_files,
            vec!["~/.ssh/id_prod".to_string(), "~/.ssh/id_common".to_string()]
        );
        assert!(prod.profile.agent_forward);
        assert_eq!(prod.proxy_jump.as_deref(), Some("bastion"));
        assert_eq!(prod.profile.group.as_deref(), Some(IMPORTED_GROUP));

        let bastion = &imported[0];
        assert_eq!(bastion.profile.host, "jump.example.com");
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
                "Host web !web-staging\n",
                "  HostName web.example.com\n",
            ),
        )
        .unwrap();

        let imported = import_profiles_from(ssh.join("config"), &root);
        let names: Vec<_> = imported.iter().map(|i| i.profile.name.as_str()).collect();
        assert_eq!(names, vec!["secure", "web"]);
        let secure = imported
            .iter()
            .find(|i| i.profile.name == "secure")
            .unwrap();
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
        let stats = merge_imported(&mut existing, imported);
        assert_eq!(
            stats,
            MergeStats {
                added: 1,
                updated: 1,
                unchanged: 0
            }
        );

        assert_eq!(existing.len(), 2);
        let prod = existing.iter().find(|p| p.name == "prod").unwrap();
        assert_eq!(prod.host, "10.0.0.5");
        assert_eq!(prod.id, prod_id);
        assert_eq!(prod.group.as_deref(), Some("My Servers"));
        assert_eq!(prod.auth, AuthMode::Password);
        assert!(prod.credential_ref.is_some());

        let bastion = existing.iter().find(|p| p.name == "bastion").unwrap();
        assert_eq!(bastion.group.as_deref(), Some(IMPORTED_GROUP));

        let snapshot = existing.clone();
        let imported_again = import_profiles_from(ssh.join("config"), &root);
        let stats = merge_imported(&mut existing, imported_again);
        assert_eq!(existing, snapshot);
        // The same invariant the line above pins, said in the words the button
        // reports: a second import of an unedited file updates nothing.
        assert_eq!(
            stats,
            MergeStats {
                added: 0,
                updated: 0,
                unchanged: 2
            }
        );
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
        merge_imported(
            &mut existing,
            import_profiles_from(ssh.join("config"), &root),
        );

        let bastion_id = existing.iter().find(|p| p.name == "bastion").unwrap().id;
        let prod = existing.iter().find(|p| p.name == "prod").unwrap();
        assert_eq!(prod.jump_host, Some(bastion_id));
    }

    #[test]
    fn hostname_percent_h_expands_to_the_alias() {
        let root = temp_root("resolve-percent-h");
        let ssh = root.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("config"),
            "Host web1\n  HostName %h.internal.example\n  User me\n",
        )
        .unwrap();
        let resolved = resolve_alias_to_profile_from(ssh.join("config"), &root, "web1").unwrap();
        assert_eq!(resolved.profile.host, "web1.internal.example");

        assert_eq!(expand_hostname_tokens("%h.x", "a"), "a.x");
        assert_eq!(expand_hostname_tokens("100%%", "a"), "100%");
        assert_eq!(expand_hostname_tokens("%q", "a"), "%q");
    }

    #[test]
    fn hash_only_comments_whole_lines_not_values() {
        let root = temp_root("resolve-hash");
        let ssh = root.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("config"),
            concat!(
                "# a comment\n",
                "Host dev\n",
                "  ProxyCommand connect -H proxy#1 %h %p\n",
                "  # indented comment\n",
                "  User me\n",
            ),
        )
        .unwrap();
        let resolved = resolve_alias_to_profile_from(ssh.join("config"), &root, "dev").unwrap();
        assert_eq!(
            resolved.profile.proxy_command.as_deref(),
            Some("connect -H proxy#1 %h %p")
        );
        assert_eq!(resolved.profile.user, "me");
    }

    #[test]
    fn resolve_alias_maps_native_fields() {
        let root = temp_root("resolve-native");
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
                "  ProxyJump me@bastion:2200\n",
                "  ConnectTimeout 15\n",
                "  ServerAliveInterval 30\n",
                "  ServerAliveCountMax 4\n",
                "  Ciphers aes256-ctr,aes128-ctr\n",
                "  MACs hmac-sha2-256\n",
                "  KexAlgorithms curve25519-sha256\n",
                "  HostKeyAlgorithms ssh-ed25519\n",
                "  Compression yes\n",
                "  ForwardX11 yes\n",
                "  StrictHostKeyChecking no\n",
                "  LocalForward 8080 localhost:80\n",
                "  RemoteForward 9000 127.0.0.1:3000\n",
                "  DynamicForward 1080\n",
            ),
        )
        .unwrap();

        let resolved = resolve_alias_to_profile_from(ssh.join("config"), &root, "prod").unwrap();
        let p = &resolved.profile;
        assert_eq!(p.host, "10.0.0.5");
        assert_eq!(p.user, "deploy");
        assert_eq!(p.port, 2222);
        assert_eq!(p.identity_files, vec!["~/.ssh/id_prod".to_string()]);
        assert_eq!(resolved.proxy_jump.as_deref(), Some("me@bastion:2200"));
        assert_eq!(p.connect_timeout_s, Some(15));
        assert_eq!(p.keepalive_interval_s, Some(30));
        assert_eq!(p.keepalive_count_max, Some(4));
        assert_eq!(p.algorithms.cipher, vec!["aes256-ctr", "aes128-ctr"]);
        assert_eq!(p.algorithms.mac, vec!["hmac-sha2-256"]);
        assert_eq!(p.algorithms.kex, vec!["curve25519-sha256"]);
        assert_eq!(p.algorithms.hostkey, vec!["ssh-ed25519"]);
        assert!(!p.algorithms.compression.is_empty());
        assert!(p.x11);
        assert_eq!(p.verify_host_keys, Some(false));
        assert!(p.group.is_none());
        assert!(p.credential_ref.is_none());

        let forwards = &p.forwards;
        assert_eq!(forwards.len(), 3);
        assert_eq!(forwards[0].kind, ForwardKind::Local);
        assert_eq!(forwards[0].bind, HostPort::new("127.0.0.1", 8080));
        assert_eq!(forwards[0].target, HostPort::new("localhost", 80));
        assert_eq!(forwards[1].kind, ForwardKind::Remote);
        assert_eq!(forwards[1].bind, HostPort::new("127.0.0.1", 9000));
        assert_eq!(forwards[1].target, HostPort::new("127.0.0.1", 3000));
        assert_eq!(forwards[2].kind, ForwardKind::Dynamic);
        assert_eq!(forwards[2].bind, HostPort::new("127.0.0.1", 1080));
    }

    #[test]
    fn resolve_alias_drops_algorithm_modifier_syntax() {
        let root = temp_root("resolve-modifiers");
        let ssh = root.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("config"),
            "Host m\n  HostName h\n  Ciphers +aes256-gcm@openssh.com\n",
        )
        .unwrap();
        let resolved = resolve_alias_to_profile_from(ssh.join("config"), &root, "m").unwrap();
        assert!(resolved.profile.algorithms.cipher.is_empty());
    }

    #[test]
    fn resolve_alias_wildcard_only_and_unknown() {
        let root = temp_root("resolve-wildcard");
        let ssh = root.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(ssh.join("config"), "Host *\n  User fallback\n").unwrap();

        let resolved =
            resolve_alias_to_profile_from(ssh.join("config"), &root, "example.com").unwrap();
        assert_eq!(resolved.profile.host, "example.com");
        assert_eq!(resolved.profile.user, "fallback");

        assert!(resolve_alias_to_profile_from(root.join("missing"), &root, "whatever").is_none());
    }

    #[test]
    fn resolve_alias_proxy_jump_can_resolve_recursively() {
        let root = temp_root("resolve-jump");
        let ssh = root.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("config"),
            "Host prod\n  HostName 10.0.0.5\n  ProxyJump bastion\nHost bastion\n  HostName jump.example.com\n  User jumper\n",
        )
        .unwrap();

        let prod = resolve_alias_to_profile_from(ssh.join("config"), &root, "prod").unwrap();
        assert_eq!(prod.proxy_jump.as_deref(), Some("bastion"));
        let bastion = resolve_alias_to_profile_from(ssh.join("config"), &root, "bastion").unwrap();
        assert_eq!(bastion.profile.host, "jump.example.com");
        assert_eq!(bastion.profile.user, "jumper");
    }

    #[test]
    fn report_groups_ignored_options_by_keyword_as_the_file_spells_them() {
        let root = temp_root("report-ignored");
        let ssh = root.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("config"),
            concat!(
                "Host prod web\n",
                "  HostName 10.0.0.5\n",
                "  IdentityAgent /run/agent.sock\n",
                "  CertificateFile ~/.ssh/id-cert.pub\n",
                "Host db\n",
                "  identityagent /run/other.sock\n",
                "  AddKeysToAgent yes\n",
                "Host *\n",
                "  SendEnv LANG\n",
                "Match host prod\n",
                "  PKCS11Provider /usr/lib/x.so\n",
            ),
        )
        .unwrap();

        let report = import_report_from(ssh.join("config"), &root);
        assert!(report.source_read);
        assert_eq!(report.files_read, 1);
        let ignored: Vec<(&str, Vec<&str>)> = report
            .ignored
            .iter()
            .map(|opt| {
                (
                    opt.option.as_str(),
                    opt.hosts.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            ignored,
            vec![
                ("AddKeysToAgent", vec!["db"]),
                ("CertificateFile", vec!["prod", "web"]),
                // One entry despite the two spellings, under the first of them,
                // and one entry per host rather than one per `Host` line.
                ("IdentityAgent", vec!["db", "prod", "web"]),
            ]
        );
    }

    #[test]
    fn every_supported_keyword_is_kept() {
        let root = temp_root("report-supported");
        let ssh = root.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("config"),
            concat!(
                "Host everything\n",
                "  HostName 10.0.0.5\n",
                "  User deploy\n",
                "  Port 2222\n",
                "  IdentityFile ~/.ssh/id_prod\n",
                "  ProxyJump bastion\n",
                "  ProxyCommand nc %h %p\n",
                "  ForwardAgent yes\n",
                "  ConnectTimeout 15\n",
                "  ServerAliveInterval 30\n",
                "  ServerAliveCountMax 4\n",
                "  Ciphers aes256-ctr\n",
                "  MACs hmac-sha2-256\n",
                "  KexAlgorithms curve25519-sha256\n",
                "  HostKeyAlgorithms ssh-ed25519\n",
                "  Compression yes\n",
                "  ForwardX11 no\n",
                "  StrictHostKeyChecking no\n",
                "  LocalForward 8080 localhost:80\n",
                "  RemoteForward 9000 127.0.0.1:3000\n",
                "  DynamicForward 1080\n",
            ),
        )
        .unwrap();

        let report = import_report_from(ssh.join("config"), &root);
        assert_eq!(report.ignored, Vec::new());
    }

    #[test]
    fn report_says_when_the_root_config_could_not_be_read() {
        let root = temp_root("report-missing");
        let missing = root.join(".ssh/config");

        let report = import_report_from(missing.clone(), &root);
        assert_eq!(report.source, missing);
        assert!(!report.source_read);
        assert_eq!(report.files_read, 0);
        assert!(report.profiles.is_empty());
    }

    #[test]
    fn a_wildcard_only_config_was_still_read() {
        let root = temp_root("report-wildcard");
        let ssh = root.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(ssh.join("config"), "Host *\n  User fallback\n").unwrap();

        let report = import_report_from(ssh.join("config"), &root);
        assert!(report.source_read);
        assert_eq!(report.files_read, 1);
        assert!(report.profiles.is_empty());
        assert!(report.ignored.is_empty());
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

//! The SSH connection-manager profile model (PRD §7.1) plus QuickConnect parsing.
//!
//! A [`SshProfile`] is a full, user-editable connection definition persisted in
//! `config.json` (`Config::ssh_profiles`). Secrets never live here: a profile only
//! carries a [`CredentialRef`] naming its OS keychain entry.
//!
//! This is distinct from [`crate::core::ssh_config`], which does live *discovery*
//! of `~/.ssh/config` aliases for the palette. Profiles are owned by tty7 and can
//! be imported from `ssh_config` (see [`crate::core::ssh_config::import_profiles`]).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::core::keychain::CredentialRef;

/// A saved SSH connection profile. See PRD §7.1 for the field-by-field rationale.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct SshProfile {
    /// Stable identity. Referenced by [`SshProfile::jump_host`] on other profiles.
    #[serde(default = "new_id")]
    pub id: Uuid,
    /// Display name (also the `ssh_config` alias for imported profiles).
    pub name: String,
    /// Optional group/folder label. Imported profiles use
    /// [`crate::core::ssh_config::IMPORTED_GROUP`].
    pub group: Option<String>,

    // ── Connection ───────────────────────────────────────────────────────────
    /// Target host (an IP or DNS name).
    pub host: String,
    /// TCP port. Defaults to 22.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Login user. Empty means "resolve at connect time".
    pub user: String,
    /// Jump host: the id of another profile to tunnel through (multi-level chains
    /// resolve by following each hop's own `jump_host`).
    pub jump_host: Option<Uuid>,
    /// A `ProxyCommand` to spawn as the transport. `%h`/`%p` tokens are substituted
    /// at connect time (not here) — see PRD FR-C1.
    pub proxy_command: Option<String>,
    /// A SOCKS5 proxy to dial through.
    pub socks_proxy: Option<HostPort>,
    /// An HTTP `CONNECT` proxy to dial through.
    pub http_proxy: Option<HostPort>,

    // ── Authentication ───────────────────────────────────────────────────────
    /// How to authenticate. `Auto` (the default) tries every method in order.
    #[serde(deserialize_with = "crate::core::config::de_lenient")]
    pub auth: AuthMode,
    /// Private-key files to try, in order. Each supports `%h`/`%r` placeholders
    /// (see [`expand_identity_placeholders`]).
    pub identity_files: Vec<String>,
    /// Enable ssh-agent forwarding for the session.
    pub agent_forward: bool,
    /// Pointer to the keychain entry holding this profile's saved secret. Never a
    /// secret itself.
    pub credential_ref: Option<CredentialRef>,

    // ── Forwarding ───────────────────────────────────────────────────────────
    /// Port forwards established automatically once connected.
    pub forwards: Vec<ForwardRule>,

    // ── Session ──────────────────────────────────────────────────────────────
    /// Keepalive interval in seconds (`None` = library default).
    pub keepalive_interval_s: Option<u32>,
    /// Max missed keepalives before the connection is considered dead.
    pub keepalive_count_max: Option<u32>,
    /// Connection timeout in seconds.
    pub connect_timeout_s: Option<u32>,
    /// Per-profile override for the "confirm before closing" prompt (`None` =
    /// follow the global setting).
    pub warn_on_close: Option<bool>,
    /// Suppress the server login banner.
    pub skip_banner: bool,
    /// Commands sent automatically right after the shell opens.
    pub login_scripts: Vec<String>,
    /// Request X11 forwarding.
    pub x11: bool,

    // ── Advanced ─────────────────────────────────────────────────────────────
    /// Preferred algorithm lists (empty list = library default for that category).
    pub algorithms: Algorithms,
    /// Per-profile override for host-key verification (`None` = follow the global
    /// setting; `Some(false)` disables verification for this profile).
    pub verify_host_keys: Option<bool>,
}

impl Default for SshProfile {
    fn default() -> Self {
        Self {
            id: new_id(),
            name: String::new(),
            group: None,
            host: String::new(),
            port: default_port(),
            user: String::new(),
            jump_host: None,
            proxy_command: None,
            socks_proxy: None,
            http_proxy: None,
            auth: AuthMode::Auto,
            identity_files: Vec::new(),
            agent_forward: false,
            credential_ref: None,
            forwards: Vec::new(),
            keepalive_interval_s: None,
            keepalive_count_max: None,
            connect_timeout_s: None,
            warn_on_close: None,
            skip_banner: false,
            login_scripts: Vec::new(),
            x11: false,
            algorithms: Algorithms::default(),
            verify_host_keys: None,
        }
    }
}

impl SshProfile {
    /// A fresh profile with a new id and the given name; all else default.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// This profile's `identity_files` with `%h`/`%r` expanded against its own
    /// host/user (see [`expand_identity_placeholders`]).
    pub fn expanded_identity_files(&self) -> Vec<String> {
        self.identity_files
            .iter()
            .map(|f| expand_identity_placeholders(f, &self.host, &self.user))
            .collect()
    }

    /// The `user@host:port` connect string for this profile (see
    /// [`to_connect_string`]).
    pub fn connect_string(&self) -> String {
        to_connect_string(self)
    }
}

/// A host + port pair (used for SOCKS/HTTP proxies and forward endpoints).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct HostPort {
    /// Hostname or IP.
    pub host: String,
    /// Port number.
    pub port: u16,
}

impl Default for HostPort {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 0,
        }
    }
}

impl HostPort {
    /// Construct a `HostPort`.
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }
}

/// How a profile authenticates. `Auto` tries every applicable method in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    /// Try public-key, agent, saved password, keyboard-interactive, prompt — in
    /// order (the default).
    #[default]
    Auto,
    /// Password only (saved, then prompted).
    Password,
    /// Public-key only.
    PublicKey,
    /// ssh-agent only.
    Agent,
    /// keyboard-interactive only (2FA rides this path).
    KeyboardInteractive,
}

/// The direction of a port forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForwardKind {
    /// Local (`-L`): listen locally, tunnel to `target` via the server.
    #[default]
    Local,
    /// Remote (`-R`): the server listens, tunnels back to `target` on our side.
    Remote,
    /// Dynamic (`-D`): a local SOCKS proxy; `target` is unused.
    Dynamic,
}

/// One preconfigured port forward (PRD §7.1 `forwards`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ForwardRule {
    /// Local / Remote / Dynamic.
    #[serde(deserialize_with = "crate::core::config::de_lenient")]
    pub kind: ForwardKind,
    /// The listener endpoint (local side for Local/Dynamic, remote side for Remote).
    pub bind: HostPort,
    /// The endpoint traffic is delivered to. Ignored for [`ForwardKind::Dynamic`].
    pub target: HostPort,
    /// Optional human-readable label.
    pub description: String,
}

impl Default for ForwardRule {
    fn default() -> Self {
        Self {
            kind: ForwardKind::Local,
            bind: HostPort::default(),
            target: HostPort::default(),
            description: String::new(),
        }
    }
}

/// Preferred algorithm lists per category. An empty list means "use the library
/// default set for this category" (PRD §7.1: `空=默认`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Algorithms {
    /// Key-exchange algorithms.
    pub kex: Vec<String>,
    /// Symmetric ciphers.
    pub cipher: Vec<String>,
    /// MAC algorithms.
    pub mac: Vec<String>,
    /// Host-key algorithms.
    pub hostkey: Vec<String>,
    /// Compression algorithms.
    pub compression: Vec<String>,
}

/// A parsed QuickConnect target (PRD FR-P4). `user`/`port` are `None` when the
/// input omitted them; callers apply their own defaults (typically `port` → 22).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickConnect {
    /// The login user, if the input specified one (text before the last `@`).
    pub user: Option<String>,
    /// The host (IPv6 addresses returned without their surrounding brackets).
    pub host: String,
    /// The port, if the input specified one.
    pub port: Option<u16>,
}

impl QuickConnect {
    /// The port, or 22 when unspecified.
    pub fn port_or_default(&self) -> u16 {
        self.port.unwrap_or(22)
    }
}

/// Parse a QuickConnect string: `[ssh://]user@host[:port]`, with IPv6 in bracket
/// form `[::1]:2222` (PRD FR-P4). Mirrors Tabby's semantics (brief §8):
///
/// - the `ssh://` scheme prefix is optional and stripped;
/// - `user` is everything before the **last** `@`, so `@` in usernames works; an
///   empty user (leading `@`) yields `user: None`;
/// - IPv6 must be bracketed; `host` is returned unbracketed;
/// - a port segment that isn't a valid `1..=65535` fails the whole parse (`None`).
///
/// Returns `None` when the host is empty or the port is invalid.
pub fn parse_quick_connect(input: &str) -> Option<QuickConnect> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Optional scheme.
    let body = trimmed
        .strip_prefix("ssh://")
        .or_else(|| trimmed.strip_prefix("SSH://"))
        .unwrap_or(trimmed);

    // Split user off at the LAST '@' so '@' inside a username is preserved.
    let (user, hostport) = match body.rfind('@') {
        Some(ix) => {
            let u = &body[..ix];
            let user = if u.is_empty() {
                None
            } else {
                Some(u.to_string())
            };
            (user, &body[ix + 1..])
        }
        None => (None, body),
    };

    let (host, port) = split_host_port(hostport)?;
    if host.is_empty() {
        return None;
    }
    Some(QuickConnect { user, host, port })
}

/// Split a `host[:port]` / `[ipv6][:port]` fragment. Returns `None` if a present
/// port segment is not a valid `1..=65535`.
fn split_host_port(hostport: &str) -> Option<(String, Option<u16>)> {
    if hostport.is_empty() {
        return Some((String::new(), None));
    }
    // IPv6 bracket form: [host] or [host]:port.
    if let Some(rest) = hostport.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = rest[..close].to_string();
        let after = &rest[close + 1..];
        let port = match after.strip_prefix(':') {
            Some(p) => Some(parse_port(p)?),
            None if after.is_empty() => None,
            // Trailing junk after ']' that isn't a ':port' → reject.
            None => return None,
        };
        return Some((host, port));
    }
    // Non-bracket. A single ':' means `host:port` (the suffix must be a valid
    // port, else reject). Several colons is a bare, unbracketed IPv6 address —
    // ambiguous, so keep it whole as the host rather than guess a port.
    match hostport.matches(':').count() {
        0 => Some((hostport.to_string(), None)),
        1 => {
            let (h, p) = hostport.split_once(':').expect("one colon present");
            Some((h.to_string(), Some(parse_port(p)?)))
        }
        _ => Some((hostport.to_string(), None)),
    }
}

/// Parse a required, valid port; `None` on out-of-range / non-numeric / zero.
fn parse_port(s: &str) -> Option<u16> {
    try_parse_port(s)
}

/// `Some(port)` only for a valid `1..=65535`; `None` otherwise (u16 parse already
/// rejects > 65535, and we additionally reject 0).
fn try_parse_port(s: &str) -> Option<u16> {
    s.parse::<u16>().ok().filter(|&p| p != 0)
}

/// Render a profile as a `user@host:port` connect string (PRD FR-P5). The `user@`
/// is omitted when the user is empty and `:port` is omitted when it's the default
/// 22. IPv6 hosts are re-bracketed so the result round-trips through
/// [`parse_quick_connect`].
pub fn to_connect_string(profile: &SshProfile) -> String {
    let host = if profile.host.contains(':') {
        format!("[{}]", profile.host)
    } else {
        profile.host.clone()
    };
    let mut out = String::new();
    if !profile.user.is_empty() {
        out.push_str(&profile.user);
        out.push('@');
    }
    out.push_str(&host);
    if profile.port != 22 {
        out.push(':');
        out.push_str(&profile.port.to_string());
    }
    out
}

/// Expand `%h` (host) and `%r` (remote user) placeholders in an identity-file path
/// (PRD FR-A2), plus a leading `~/` to the home directory. A single left-to-right
/// pass, so a `%h` that expands to text containing `%r` is not re-expanded. `%%`
/// yields a literal `%`.
///
/// The tilde matters GUI-side: identity paths are overwhelmingly `~/.ssh/...`
/// (every ssh_config import), and the keychain passphrase scheme hashes the key
/// *file contents* — an unexpanded `~` makes that read silently fail, so
/// "remember passphrase" would neither store nor resolve.
pub fn expand_identity_placeholders(path: &str, host: &str, user: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut chars = path.chars();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('h') => out.push_str(host),
            Some('r') => out.push_str(user),
            Some('%') => out.push('%'),
            // Unknown token: keep it verbatim (e.g. "%d" stays "%d").
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    expand_tilde(&out)
}

/// Expand a leading `~/` (or a bare `~`) to the user's home directory; every
/// other path passes through unchanged, as does `~` when no home is known.
pub fn expand_tilde(path: &str) -> String {
    let home = || {
        #[cfg(windows)]
        let var = "USERPROFILE";
        #[cfg(not(windows))]
        let var = "HOME";
        std::env::var(var).ok().filter(|h| !h.is_empty())
    };
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home() {
            let sep = if home.ends_with('/') { "" } else { "/" };
            return format!("{home}{sep}{rest}");
        }
    } else if path == "~" {
        if let Some(home) = home() {
            return home;
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_connect_parses_basic_forms() {
        let q = parse_quick_connect("deploy@10.0.0.5").unwrap();
        assert_eq!(q.user.as_deref(), Some("deploy"));
        assert_eq!(q.host, "10.0.0.5");
        assert_eq!(q.port, None);
        assert_eq!(q.port_or_default(), 22);

        let q = parse_quick_connect("deploy@10.0.0.5:2222").unwrap();
        assert_eq!(q.user.as_deref(), Some("deploy"));
        assert_eq!(q.host, "10.0.0.5");
        assert_eq!(q.port, Some(2222));

        // Host-only.
        let q = parse_quick_connect("example.com").unwrap();
        assert_eq!(q.user, None);
        assert_eq!(q.host, "example.com");
        assert_eq!(q.port, None);

        // Host:port with no user.
        let q = parse_quick_connect("example.com:8022").unwrap();
        assert_eq!(q.user, None);
        assert_eq!(q.host, "example.com");
        assert_eq!(q.port, Some(8022));
    }

    #[test]
    fn quick_connect_strips_scheme_and_whitespace() {
        let q = parse_quick_connect("  ssh://deploy@host:22 ").unwrap();
        assert_eq!(q.user.as_deref(), Some("deploy"));
        assert_eq!(q.host, "host");
        assert_eq!(q.port, Some(22));
    }

    #[test]
    fn quick_connect_at_in_username_uses_last_at() {
        // The user contains an '@' (e.g. an email-style login).
        let q = parse_quick_connect("me@corp.com@bastion").unwrap();
        assert_eq!(q.user.as_deref(), Some("me@corp.com"));
        assert_eq!(q.host, "bastion");
        assert_eq!(q.port, None);

        // Leading '@' → empty user → None, host kept.
        let q = parse_quick_connect("@host").unwrap();
        assert_eq!(q.user, None);
        assert_eq!(q.host, "host");
    }

    #[test]
    fn quick_connect_ipv6_bracket_form() {
        let q = parse_quick_connect("[::1]:2222").unwrap();
        assert_eq!(q.user, None);
        assert_eq!(q.host, "::1");
        assert_eq!(q.port, Some(2222));

        let q = parse_quick_connect("root@[fe80::1]").unwrap();
        assert_eq!(q.user.as_deref(), Some("root"));
        assert_eq!(q.host, "fe80::1");
        assert_eq!(q.port, None);

        let q = parse_quick_connect("[2001:db8::dead:beef]:22").unwrap();
        assert_eq!(q.host, "2001:db8::dead:beef");
        assert_eq!(q.port, Some(22));

        // A bare (unbracketed) IPv6 is ambiguous but must not be split into a port.
        let q = parse_quick_connect("fe80::1").unwrap();
        assert_eq!(q.host, "fe80::1");
        assert_eq!(q.port, None);
    }

    #[test]
    fn quick_connect_rejects_bad_ports_and_empties() {
        assert!(parse_quick_connect("").is_none());
        assert!(parse_quick_connect("   ").is_none());
        // Port out of u16 range.
        assert!(parse_quick_connect("host:70000").is_none());
        // Port zero is invalid.
        assert!(parse_quick_connect("host:0").is_none());
        // Non-numeric single-colon suffix is a malformed port.
        assert!(parse_quick_connect("host:ssh").is_none());
        // Empty host.
        assert!(parse_quick_connect("deploy@").is_none());
        // Max valid port.
        assert_eq!(parse_quick_connect("host:65535").unwrap().port, Some(65535));
    }

    #[test]
    fn connect_string_omits_default_user_and_port() {
        let mut p = SshProfile::new("prod");
        p.host = "10.0.0.5".to_string();
        p.user = "deploy".to_string();
        p.port = 22;
        assert_eq!(to_connect_string(&p), "deploy@10.0.0.5");

        p.port = 2222;
        assert_eq!(to_connect_string(&p), "deploy@10.0.0.5:2222");

        // Empty user → no leading `user@`.
        p.user = String::new();
        p.port = 22;
        assert_eq!(to_connect_string(&p), "10.0.0.5");

        // IPv6 host is re-bracketed and round-trips.
        p.host = "::1".to_string();
        p.user = "root".to_string();
        p.port = 2200;
        let s = to_connect_string(&p);
        assert_eq!(s, "root@[::1]:2200");
        let q = parse_quick_connect(&s).unwrap();
        assert_eq!(q.user.as_deref(), Some("root"));
        assert_eq!(q.host, "::1");
        assert_eq!(q.port, Some(2200));
    }

    #[test]
    fn identity_placeholder_expansion() {
        // `~/` expands to the real home dir (the GUI hashes the key file's
        // contents for the keychain, so the path must be readable as-is).
        let home = expand_tilde("~");
        assert_eq!(
            expand_identity_placeholders("~/.ssh/id_%h", "example.com", "deploy"),
            format!("{home}/.ssh/id_example.com")
        );
        assert_eq!(
            expand_identity_placeholders("~/keys/%r@%h.pem", "host", "alice"),
            format!("{home}/keys/alice@host.pem")
        );
        // A literal %% survives as a single %, and unknown tokens stay verbatim.
        assert_eq!(
            expand_identity_placeholders("100%%-%d-%h", "h", "u"),
            "100%-%d-h"
        );
        // Single left-to-right pass: %h expanding to text with %r is not re-expanded.
        assert_eq!(expand_identity_placeholders("%h", "%r", "u"), "%r");
        // No placeholders and no tilde → unchanged.
        assert_eq!(
            expand_identity_placeholders("/abs/.ssh/id_ed25519", "h", "u"),
            "/abs/.ssh/id_ed25519"
        );
    }

    #[test]
    fn expand_tilde_only_touches_a_leading_tilde() {
        let home = expand_tilde("~");
        assert!(!home.is_empty() && home != "~");
        assert_eq!(expand_tilde("~/.ssh/id"), format!("{home}/.ssh/id"));
        // Not a home reference: mid-path or suffixed tildes stay verbatim.
        assert_eq!(expand_tilde("/a/~/b"), "/a/~/b");
        assert_eq!(expand_tilde("~user/x"), "~user/x");
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
    }

    #[test]
    fn profile_expanded_identity_files_uses_own_host_user() {
        let mut p = SshProfile::new("x");
        p.host = "srv".to_string();
        p.user = "bob".to_string();
        p.identity_files = vec![
            "~/.ssh/id_%r_%h".to_string(),
            "~/.ssh/id_ed25519".to_string(),
        ];
        let home = expand_tilde("~");
        assert_eq!(
            p.expanded_identity_files(),
            vec![
                format!("{home}/.ssh/id_bob_srv"),
                format!("{home}/.ssh/id_ed25519")
            ]
        );
    }

    #[test]
    fn profile_serde_defaults_and_round_trip() {
        // A minimal profile JSON fills everything else from defaults.
        let p: SshProfile = serde_json::from_str(r#"{"name":"min","host":"h"}"#).unwrap();
        assert_eq!(p.name, "min");
        assert_eq!(p.host, "h");
        assert_eq!(p.port, 22);
        assert_eq!(p.auth, AuthMode::Auto);
        assert!(p.credential_ref.is_none());

        // Back-compat: a config.json from a build that still wrote the removed
        // `use_system_ssh` flag loads fine — serde ignores the unknown field
        // (the struct has container-level `#[serde(default)]`, no
        // `deny_unknown_fields`).
        let p: SshProfile =
            serde_json::from_str(r#"{"name":"old","host":"h","use_system_ssh":true}"#).unwrap();
        assert_eq!(p.name, "old");
        assert_eq!(p.host, "h");

        // A bad `auth` value falls back leniently instead of failing the parse.
        let p: SshProfile =
            serde_json::from_str(r#"{"name":"x","host":"h","auth":"bogus"}"#).unwrap();
        assert_eq!(p.auth, AuthMode::Auto);

        // Full round trip preserves the id and every field.
        let mut original = SshProfile::new("full");
        original.host = "10.0.0.9".to_string();
        original.user = "deploy".to_string();
        original.port = 2222;
        original.auth = AuthMode::PublicKey;
        original.identity_files = vec!["~/.ssh/id_%h".to_string()];
        original.forwards = vec![ForwardRule {
            kind: ForwardKind::Remote,
            bind: HostPort::new("127.0.0.1", 8080),
            target: HostPort::new("10.0.0.1", 80),
            description: "web".to_string(),
        }];
        original.socks_proxy = Some(HostPort::new("proxy", 1080));
        original.algorithms.kex = vec!["curve25519-sha256".to_string()];
        original.credential_ref = Some(CredentialRef::password("deploy", "10.0.0.9", 2222));

        let json = serde_json::to_string(&original).unwrap();
        let back: SshProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn auth_mode_kebab_case_serialization() {
        assert_eq!(
            serde_json::to_string(&AuthMode::PublicKey).unwrap(),
            "\"public-key\""
        );
        assert_eq!(
            serde_json::to_string(&AuthMode::KeyboardInteractive).unwrap(),
            "\"keyboard-interactive\""
        );
        let m: AuthMode = serde_json::from_str("\"agent\"").unwrap();
        assert_eq!(m, AuthMode::Agent);
    }
}

/// Serde default for [`SshProfile::id`]: a fresh v4 UUID.
fn new_id() -> Uuid {
    Uuid::new_v4()
}

/// Serde default for [`SshProfile::port`]: the standard SSH port.
fn default_port() -> u16 {
    22
}

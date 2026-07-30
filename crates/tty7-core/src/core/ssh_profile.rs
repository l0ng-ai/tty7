use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::core::keychain::CredentialRef;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct SshProfile {
    #[serde(default = "new_id")]
    pub id: Uuid,
    pub name: String,
    pub group: Option<String>,

    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    pub jump_host: Option<Uuid>,
    pub proxy_command: Option<String>,
    pub socks_proxy: Option<HostPort>,
    pub http_proxy: Option<HostPort>,

    #[serde(deserialize_with = "crate::core::config::de_lenient")]
    pub auth: AuthMode,
    pub identity_files: Vec<String>,
    pub agent_forward: bool,
    pub credential_ref: Option<CredentialRef>,

    pub forwards: Vec<ForwardRule>,

    pub keepalive_interval_s: Option<u32>,
    pub keepalive_count_max: Option<u32>,
    pub connect_timeout_s: Option<u32>,
    pub warn_on_close: Option<bool>,
    pub skip_banner: bool,
    #[serde(default = "default_true")]
    pub shell_integration: bool,
    pub login_scripts: Vec<String>,
    pub x11: bool,

    pub algorithms: Algorithms,
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
            shell_integration: true,
            login_scripts: Vec::new(),
            x11: false,
            algorithms: Algorithms::default(),
            verify_host_keys: None,
        }
    }
}

impl SshProfile {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    pub fn expanded_identity_files(&self) -> Vec<String> {
        self.identity_files
            .iter()
            .map(|f| expand_identity_placeholders(f, &self.host, &self.user))
            .collect()
    }

    pub fn connect_string(&self) -> String {
        to_connect_string(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct HostPort {
    pub host: String,
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
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    #[default]
    Auto,
    Gssapi,
    Password,
    PublicKey,
    Agent,
    KeyboardInteractive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForwardKind {
    #[default]
    Local,
    Remote,
    Dynamic,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ForwardRule {
    #[serde(deserialize_with = "crate::core::config::de_lenient")]
    pub kind: ForwardKind,
    pub bind: HostPort,
    pub target: HostPort,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct Algorithms {
    pub kex: Vec<String>,
    pub cipher: Vec<String>,
    pub mac: Vec<String>,
    pub hostkey: Vec<String>,
    pub compression: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickConnect {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

impl QuickConnect {
    pub fn port_or_default(&self) -> u16 {
        self.port.unwrap_or(22)
    }
}

pub fn parse_quick_connect(input: &str) -> Option<QuickConnect> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let body = trimmed
        .strip_prefix("ssh://")
        .or_else(|| trimmed.strip_prefix("SSH://"))
        .unwrap_or(trimmed);

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

fn split_host_port(hostport: &str) -> Option<(String, Option<u16>)> {
    if hostport.is_empty() {
        return Some((String::new(), None));
    }
    if let Some(rest) = hostport.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = rest[..close].to_string();
        let after = &rest[close + 1..];
        let port = match after.strip_prefix(':') {
            Some(p) => Some(parse_port(p)?),
            None if after.is_empty() => None,
            None => return None,
        };
        return Some((host, port));
    }
    match hostport.matches(':').count() {
        0 => Some((hostport.to_string(), None)),
        1 => {
            let (h, p) = hostport.split_once(':').expect("one colon present");
            Some((h.to_string(), Some(parse_port(p)?)))
        }
        _ => Some((hostport.to_string(), None)),
    }
}

fn parse_port(s: &str) -> Option<u16> {
    try_parse_port(s)
}

fn try_parse_port(s: &str) -> Option<u16> {
    s.parse::<u16>().ok().filter(|&p| p != 0)
}

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
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    expand_tilde(&out)
}

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
    fn profiles_saved_before_the_switch_existed_default_to_integrated() {
        let profile: SshProfile =
            serde_json::from_str(r#"{"name":"prod","host":"h","user":"u"}"#).unwrap();
        assert!(profile.shell_integration);
        let off: SshProfile = serde_json::from_str(
            r#"{"name":"prod","host":"h","user":"u","shell_integration":false}"#,
        )
        .unwrap();
        assert!(!off.shell_integration);
    }

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

        let q = parse_quick_connect("example.com").unwrap();
        assert_eq!(q.user, None);
        assert_eq!(q.host, "example.com");
        assert_eq!(q.port, None);

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
        let q = parse_quick_connect("me@corp.com@bastion").unwrap();
        assert_eq!(q.user.as_deref(), Some("me@corp.com"));
        assert_eq!(q.host, "bastion");
        assert_eq!(q.port, None);

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

        let q = parse_quick_connect("fe80::1").unwrap();
        assert_eq!(q.host, "fe80::1");
        assert_eq!(q.port, None);
    }

    #[test]
    fn quick_connect_rejects_bad_ports_and_empties() {
        assert!(parse_quick_connect("").is_none());
        assert!(parse_quick_connect("   ").is_none());
        assert!(parse_quick_connect("host:70000").is_none());
        assert!(parse_quick_connect("host:0").is_none());
        assert!(parse_quick_connect("host:ssh").is_none());
        assert!(parse_quick_connect("deploy@").is_none());
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

        p.user = String::new();
        p.port = 22;
        assert_eq!(to_connect_string(&p), "10.0.0.5");

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
        let home = expand_tilde("~");
        assert_eq!(
            expand_identity_placeholders("~/.ssh/id_%h", "example.com", "deploy"),
            format!("{home}/.ssh/id_example.com")
        );
        assert_eq!(
            expand_identity_placeholders("~/keys/%r@%h.pem", "host", "alice"),
            format!("{home}/keys/alice@host.pem")
        );
        assert_eq!(
            expand_identity_placeholders("100%%-%d-%h", "h", "u"),
            "100%-%d-h"
        );
        assert_eq!(expand_identity_placeholders("%h", "%r", "u"), "%r");
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
        let p: SshProfile = serde_json::from_str(r#"{"name":"min","host":"h"}"#).unwrap();
        assert_eq!(p.name, "min");
        assert_eq!(p.host, "h");
        assert_eq!(p.port, 22);
        assert_eq!(p.auth, AuthMode::Auto);
        assert!(p.credential_ref.is_none());

        let p: SshProfile =
            serde_json::from_str(r#"{"name":"old","host":"h","use_system_ssh":true}"#).unwrap();
        assert_eq!(p.name, "old");
        assert_eq!(p.host, "h");

        let p: SshProfile =
            serde_json::from_str(r#"{"name":"x","host":"h","auth":"bogus"}"#).unwrap();
        assert_eq!(p.auth, AuthMode::Auto);

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
        assert_eq!(
            serde_json::to_string(&AuthMode::Gssapi).unwrap(),
            "\"gssapi\""
        );
        let m: AuthMode = serde_json::from_str("\"agent\"").unwrap();
        assert_eq!(m, AuthMode::Agent);
    }
}

fn new_id() -> Uuid {
    Uuid::new_v4()
}

fn default_port() -> u16 {
    22
}

fn default_true() -> bool {
    true
}

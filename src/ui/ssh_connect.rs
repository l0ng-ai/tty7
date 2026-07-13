//! Pre-connect credential resolution (WS3): the single place OS-keychain secrets
//! and profile references are resolved into a self-contained [`NativeSshSpec`] for
//! the daemon's native (russh) path.
//!
//! [`build_native_ssh_spec`] turns a stored [`SshProfile`] into the wire spec:
//! it looks up the endpoint password and per-key passphrases from the keychain,
//! resolves the `jump_host` profile chain into nested specs, expands `%h`/`%r`
//! identity-file placeholders, and maps the profile's proxy / forwards / algorithm
//! fields onto the protocol types. The daemon never reads the keychain or the
//! profile store — everything it needs rides this spec once, over the local socket
//! (secrets redacted in `Debug`; see `NativeSshSpec`).
//!
//! WS6 wires the UI entry points to this module: the palette connect flow, the
//! profile editor, QuickConnect, and the reconnect/restore paths all resolve
//! their specs through here (see [`Tty7App::connect_ssh_profile`],
//! [`Tty7App::quick_connect`], and [`resolve_persisted_ssh_spec`]).

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::core::config::Config;
use crate::core::keychain::{CredentialStore, OsCredentialStore, key_account_from_contents};
use crate::core::ssh_profile::{
    Algorithms, AuthMode, ForwardKind, ForwardRule, HostPort, SshProfile,
};
use crate::daemon::protocol::{
    NativeSshSpec, SshAlgorithms, SshAuthMode, SshForwardKind, SshForwardRule, SshProxy,
};

use super::app::Tty7App;

impl Tty7App {
    /// Resolve a stored profile into a fully self-contained [`NativeSshSpec`],
    /// pulling secrets from the OS keychain and the jump chain from the profile
    /// store. The one place secrets enter a spec (WS3). Reads the global
    /// `ssh_profiles` (for jump-host resolution) and `verify_host_keys` fallback.
    pub(crate) fn native_ssh_spec_for_profile(
        &self,
        profile: &SshProfile,
        cx: &gpui::App,
    ) -> NativeSshSpec {
        let cfg = cx.global::<Config>();
        build_native_ssh_spec(
            profile,
            &cfg.ssh_profiles,
            &OsCredentialStore,
            cfg.verify_host_keys,
        )
    }

    /// Connect a saved profile (PRD FR-P3). Honors the per-profile
    /// `use_system_ssh` compat-mode flag (FR-C5): flagged profiles go through the
    /// frozen shell-out path (no SFTP / GUI auth / vault); everything else takes
    /// the native russh path. Bumps the profile's frecency either way.
    pub(crate) fn connect_ssh_profile(
        &mut self,
        profile_id: uuid::Uuid,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(profile) = cx
            .global::<Config>()
            .ssh_profiles
            .iter()
            .find(|p| p.id == profile_id)
            .cloned()
        else {
            return;
        };
        self.bump_ssh_frecency(profile_id, cx);
        if profile.use_system_ssh {
            let spec = compat_ssh_spec(&profile, &cx.global::<Config>().ssh_profiles);
            self.open_managed_ssh_spec(spec, window, cx);
        } else {
            let spec = Box::new(self.native_ssh_spec_for_profile(&profile, cx));
            self.open_native_ssh_tab(spec, window, cx);
        }
    }

    /// QuickConnect to a typed `user@host[:port]` target (PRD FR-P4), always via
    /// the native path. Builds a transient profile so keychain lookup by endpoint
    /// still applies (a QuickConnect can reuse a remembered password).
    pub(crate) fn quick_connect(
        &mut self,
        qc: crate::core::ssh_profile::QuickConnect,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let port = qc.port_or_default();
        let mut profile = SshProfile::new(qc.host.clone());
        profile.host = qc.host;
        profile.port = port;
        if let Some(user) = qc.user {
            profile.user = user;
        }
        let spec = Box::new(self.native_ssh_spec_for_profile(&profile, cx));
        self.open_native_ssh_tab(spec, window, cx);
    }

    /// Reconnect the focused native-SSH pane after it dropped (PRD FR-E4). A
    /// no-op unless the focused pane is a *dead* native-SSH pane. Re-resolves
    /// credentials from the saved profile when the pane's persisted spec names one
    /// (`profile_id`), otherwise reuses the secret-free spec and lets the auth
    /// sheets fill in. Respawns in the same tab/split slot; the daemon rebuilds
    /// the profile's preconfigured forwards on connect.
    pub(crate) fn restart_ssh_session(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(view) = self.focused_pane_view(window, cx) else {
            return;
        };
        let dead_spec = {
            let v = view.read(cx);
            if !v.ssh_disconnected() {
                return;
            }
            v.ssh_spec()
        };
        let Some(spec) = dead_spec else {
            return;
        };
        let resolved = self.resolve_restart_spec(spec, cx);
        self.respawn_native_ssh_in_place(&view, resolved, window, cx);
    }

    /// If the persisted (secret-free) spec names a saved profile that still
    /// exists, rebuild it from the profile so keychain secrets are re-applied;
    /// otherwise return the spec unchanged (the auth sheets will prompt).
    fn resolve_restart_spec(
        &self,
        spec: Box<crate::daemon::protocol::NativeSshSpec>,
        cx: &gpui::App,
    ) -> Box<crate::daemon::protocol::NativeSshSpec> {
        resolve_persisted_ssh_spec(spec, cx)
    }

    /// The focused pane's terminal view, if any.
    fn focused_pane_view(
        &self,
        window: &gpui::Window,
        cx: &gpui::App,
    ) -> Option<gpui::Entity<crate::terminal::view::TerminalView>> {
        self.tabs
            .get(self.active)?
            .pane
            .focused_or_first(window, cx)
    }

    /// Record a connect against a profile's frecency stats (FR-P3).
    fn bump_ssh_frecency(&mut self, profile_id: uuid::Uuid, cx: &mut gpui::Context<Self>) {
        self.update_config(cx, |cfg| {
            let entry = cfg.ssh_profile_frecency.entry(profile_id).or_default();
            entry.count = entry.count.saturating_add(1);
            entry.last_used = crate::core::config::unix_now();
        });
    }
}

/// Re-resolve a persisted (secret-free) [`NativeSshSpec`] for reconnection
/// (FR-E4/C2). When the spec names a saved profile that still exists, rebuild it
/// from that profile so keychain secrets are re-applied; otherwise return the
/// spec unchanged and let the in-pane auth sheets prompt. A free function so both
/// the in-place reconnect and session-restore (which has no `Tty7App` yet) share
/// it.
pub(crate) fn resolve_persisted_ssh_spec(
    spec: Box<crate::daemon::protocol::NativeSshSpec>,
    cx: &gpui::App,
) -> Box<crate::daemon::protocol::NativeSshSpec> {
    let cfg = cx.global::<Config>();
    let profile = spec
        .profile_id
        .as_deref()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .and_then(|id| cfg.ssh_profiles.iter().find(|p| p.id == id).cloned());
    match profile {
        Some(p) => Box::new(build_native_ssh_spec(
            &p,
            &cfg.ssh_profiles,
            &OsCredentialStore,
            cfg.verify_host_keys,
        )),
        None => spec,
    }
}

/// Build a compat-mode [`SshSpec`] (system `ssh` shell-out) from a profile
/// (PRD FR-C5). A best-effort mapping of the common fields — the compat path is
/// frozen (PRD §3.1), so exotic options aren't threaded through here; users who
/// need them keep them in `~/.ssh/config`.
fn compat_ssh_spec(
    profile: &SshProfile,
    profiles: &[SshProfile],
) -> crate::daemon::protocol::SshSpec {
    let target = if profile.user.is_empty() {
        profile.host.clone()
    } else {
        format!("{}@{}", profile.user, profile.host)
    };
    let mut args: Vec<String> = Vec::new();
    if profile.port != 22 {
        args.push("-p".to_string());
        args.push(profile.port.to_string());
    }
    for id in profile.expanded_identity_files() {
        args.push("-i".to_string());
        args.push(id);
    }
    // A jump host resolves to a `-J user@host` hop (single level; deeper chains
    // are rare in compat mode and left to ssh_config).
    if let Some(jump) = profile
        .jump_host
        .and_then(|id| profiles.iter().find(|p| p.id == id))
    {
        let hop = if jump.user.is_empty() {
            jump.host.clone()
        } else {
            format!("{}@{}", jump.user, jump.host)
        };
        args.push("-J".to_string());
        args.push(hop);
    }
    if profile.agent_forward {
        args.push("-A".to_string());
    }
    crate::daemon::protocol::SshSpec { target, args }
}

/// Build a [`NativeSshSpec`] from `profile`, resolving keychain secrets via
/// `store`, jump hosts against `profiles`, and using `global_verify_host_keys`
/// when the profile leaves its `verify_host_keys` unset. Pure and store-injected
/// so it is unit-testable with an in-memory keychain.
pub(crate) fn build_native_ssh_spec(
    profile: &SshProfile,
    profiles: &[SshProfile],
    store: &dyn CredentialStore,
    global_verify_host_keys: bool,
) -> NativeSshSpec {
    let mut visited = HashSet::new();
    visited.insert(profile.id);
    build_spec_inner(
        profile,
        profiles,
        store,
        global_verify_host_keys,
        &mut visited,
    )
}

fn build_spec_inner(
    profile: &SshProfile,
    profiles: &[SshProfile],
    store: &dyn CredentialStore,
    global_verify_host_keys: bool,
    visited: &mut HashSet<Uuid>,
) -> NativeSshSpec {
    let identity_files = profile.expanded_identity_files();

    // Password: only resolve when the auth mode could use one (Auto or Password),
    // so a pure-key profile doesn't pin a stale keychain read into the spec.
    let password = if matches!(profile.auth, AuthMode::Auto | AuthMode::Password) {
        store
            .password_for(&profile.user, &profile.host, profile.port)
            .ok()
            .flatten()
    } else {
        None
    };

    // Key passphrases: keyed by identity-file path (as it appears in the spec's
    // `identity_files`), resolved from the key's content hash (WS1's scheme).
    let mut key_passphrases: HashMap<String, String> = HashMap::new();
    if matches!(profile.auth, AuthMode::Auto | AuthMode::PublicKey) {
        for path in &identity_files {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let account = key_account_from_contents(&bytes);
            if let Ok(Some(passphrase)) = store.passphrase_for_key(&account) {
                key_passphrases.insert(path.clone(), passphrase);
            }
        }
    }

    // Jump chain: resolve the referenced profile and recurse, guarding against
    // cycles (a profile that jumps through itself, directly or transitively).
    let jump = profile
        .jump_host
        .and_then(|id| {
            if visited.contains(&id) {
                return None;
            }
            profiles.iter().find(|p| p.id == id)
        })
        .map(|jp| {
            visited.insert(jp.id);
            Box::new(build_spec_inner(
                jp,
                profiles,
                store,
                global_verify_host_keys,
                visited,
            ))
        });

    NativeSshSpec {
        host: profile.host.clone(),
        port: profile.port,
        user: profile.user.clone(),
        auth_mode: map_auth_mode(profile.auth),
        identity_files,
        agent_forward: profile.agent_forward,
        password,
        key_passphrases: (!key_passphrases.is_empty()).then_some(key_passphrases),
        proxy: map_proxy(profile),
        jump,
        forwards: profile.forwards.iter().map(map_forward).collect(),
        keepalive_interval_s: profile.keepalive_interval_s,
        keepalive_count_max: profile.keepalive_count_max,
        connect_timeout_s: profile.connect_timeout_s,
        algorithms: map_algorithms(&profile.algorithms),
        x11: profile.x11,
        term: "xterm-256color".to_string(),
        verify_host_keys: profile.verify_host_keys.unwrap_or(global_verify_host_keys),
        skip_banner: profile.skip_banner,
        login_script: profile.login_scripts.clone(),
        display_name: (!profile.name.is_empty()).then(|| profile.name.clone()),
        profile_id: Some(profile.id.to_string()),
    }
}

fn map_auth_mode(auth: AuthMode) -> SshAuthMode {
    match auth {
        AuthMode::Auto => SshAuthMode::Auto,
        AuthMode::Password => SshAuthMode::Password,
        AuthMode::PublicKey => SshAuthMode::PublicKey,
        AuthMode::Agent => SshAuthMode::Agent,
        AuthMode::KeyboardInteractive => SshAuthMode::KeyboardInteractive,
    }
}

/// Proxy precedence: an explicit `ProxyCommand` wins, then SOCKS5, then HTTP.
/// (A jump host is carried separately on `NativeSshSpec::jump`.)
fn map_proxy(profile: &SshProfile) -> SshProxy {
    if let Some(cmd) = &profile.proxy_command {
        if !cmd.trim().is_empty() {
            return SshProxy::Command(cmd.clone());
        }
    }
    if let Some(HostPort { host, port }) = &profile.socks_proxy {
        if !host.is_empty() {
            return SshProxy::Socks {
                host: host.clone(),
                port: *port,
            };
        }
    }
    if let Some(HostPort { host, port }) = &profile.http_proxy {
        if !host.is_empty() {
            return SshProxy::Http {
                host: host.clone(),
                port: *port,
            };
        }
    }
    SshProxy::None
}

fn map_forward(rule: &ForwardRule) -> SshForwardRule {
    SshForwardRule {
        kind: match rule.kind {
            ForwardKind::Local => SshForwardKind::Local,
            ForwardKind::Remote => SshForwardKind::Remote,
            ForwardKind::Dynamic => SshForwardKind::Dynamic,
        },
        bind_host: rule.bind.host.clone(),
        bind_port: rule.bind.port,
        target_host: rule.target.host.clone(),
        target_port: rule.target.port,
        description: (!rule.description.is_empty()).then(|| rule.description.clone()),
    }
}

fn map_algorithms(a: &Algorithms) -> SshAlgorithms {
    SshAlgorithms {
        kex: a.kex.clone(),
        cipher: a.cipher.clone(),
        mac: a.mac.clone(),
        host_key: a.hostkey.clone(),
        compression: a.compression.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::keychain::InMemoryCredentialStore;

    fn profile(name: &str, host: &str, user: &str) -> SshProfile {
        let mut p = SshProfile::new(name);
        p.host = host.into();
        p.user = user.into();
        p
    }

    #[test]
    fn resolves_stored_password_for_auto_and_password_modes() {
        let store = InMemoryCredentialStore::new();
        store
            .set_password("deploy", "10.0.0.5", 22, "hunter2")
            .unwrap();
        let mut p = profile("web", "10.0.0.5", "deploy");

        p.auth = AuthMode::Auto;
        let spec = build_native_ssh_spec(&p, &[], &store, true);
        assert_eq!(spec.password.as_deref(), Some("hunter2"));

        p.auth = AuthMode::Password;
        let spec = build_native_ssh_spec(&p, &[], &store, true);
        assert_eq!(spec.password.as_deref(), Some("hunter2"));

        // A key-only profile must not pull the password into the spec.
        p.auth = AuthMode::PublicKey;
        let spec = build_native_ssh_spec(&p, &[], &store, true);
        assert_eq!(spec.password, None);
    }

    #[test]
    fn resolves_jump_chain_into_nested_specs() {
        let bastion = profile("bastion", "bastion.example.com", "jump");
        let mut web = profile("web", "10.0.0.5", "deploy");
        web.jump_host = Some(bastion.id);

        let profiles = vec![bastion.clone(), web.clone()];
        let store = InMemoryCredentialStore::new();
        let spec = build_native_ssh_spec(&web, &profiles, &store, true);

        let jump = spec.jump.expect("jump host should resolve");
        assert_eq!(jump.host, "bastion.example.com");
        assert_eq!(jump.user, "jump");
        assert!(jump.jump.is_none());
    }

    #[test]
    fn jump_cycle_is_broken_not_infinite() {
        // Two profiles that jump through each other.
        let mut a = profile("a", "a.example.com", "u");
        let mut b = profile("b", "b.example.com", "u");
        a.jump_host = Some(b.id);
        b.jump_host = Some(a.id);
        let profiles = vec![a.clone(), b.clone()];
        let store = InMemoryCredentialStore::new();

        // Must terminate; the cycle is cut when a profile is revisited.
        let spec = build_native_ssh_spec(&a, &profiles, &store, true);
        let jump = spec.jump.expect("first hop resolves");
        assert_eq!(jump.host, "b.example.com");
        assert!(jump.jump.is_none(), "cycle back to `a` is cut");
    }

    #[test]
    fn global_verify_host_keys_is_the_fallback() {
        let store = InMemoryCredentialStore::new();
        let mut p = profile("web", "h", "u");

        p.verify_host_keys = None;
        assert!(!build_native_ssh_spec(&p, &[], &store, false).verify_host_keys);
        assert!(build_native_ssh_spec(&p, &[], &store, true).verify_host_keys);

        // A profile override wins over the global.
        p.verify_host_keys = Some(false);
        assert!(!build_native_ssh_spec(&p, &[], &store, true).verify_host_keys);
    }

    #[test]
    fn compat_ssh_spec_maps_common_fields() {
        let bastion = profile("bastion", "bastion.example.com", "jump");
        let mut p = profile("web", "10.0.0.5", "deploy");
        p.port = 2222;
        p.identity_files = vec!["~/.ssh/id_ed25519".to_string()];
        p.agent_forward = true;
        p.jump_host = Some(bastion.id);
        let profiles = vec![bastion.clone(), p.clone()];

        let spec = compat_ssh_spec(&p, &profiles);
        assert_eq!(spec.target, "deploy@10.0.0.5");
        assert!(spec.args.windows(2).any(|w| w == ["-p", "2222"]));
        assert!(spec.args.iter().any(|a| a == "-i"));
        assert!(
            spec.args
                .windows(2)
                .any(|w| w == ["-J", "jump@bastion.example.com"])
        );
        assert!(spec.args.iter().any(|a| a == "-A"));
        // A default-port, user-less profile omits `-p` and `user@`.
        let mut bare = profile("bare", "host", "");
        bare.port = 22;
        let spec = compat_ssh_spec(&bare, &[]);
        assert_eq!(spec.target, "host");
        assert!(!spec.args.iter().any(|a| a == "-p"));
    }

    #[test]
    fn maps_proxy_precedence_command_over_socks_over_http() {
        let store = InMemoryCredentialStore::new();
        let mut p = profile("web", "h", "u");
        p.socks_proxy = Some(HostPort::new("socks", 1080));
        p.http_proxy = Some(HostPort::new("http", 8080));
        assert!(matches!(
            build_native_ssh_spec(&p, &[], &store, true).proxy,
            SshProxy::Socks { .. }
        ));
        p.proxy_command = Some("nc %h %p".into());
        assert!(matches!(
            build_native_ssh_spec(&p, &[], &store, true).proxy,
            SshProxy::Command(_)
        ));
    }
}

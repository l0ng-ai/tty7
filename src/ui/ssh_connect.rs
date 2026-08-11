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
        let spec = Box::new(self.native_ssh_spec_for_profile(&profile, cx));
        self.open_native_ssh_tab(spec, window, cx);
    }

    pub(crate) fn quick_connect(
        &mut self,
        qc: crate::core::ssh_profile::QuickConnect,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if let Some(resolved) = crate::core::ssh_config::resolve_alias_to_profile(&qc.host) {
            let mut profile = resolved.profile;
            if let Some(user) = qc.user {
                profile.user = user;
            }
            if let Some(port) = qc.port {
                profile.port = port;
            }
            let spec = native_spec_from_transient_profile(
                &profile,
                resolved.proxy_jump,
                &OsCredentialStore,
                cx.global::<Config>().verify_host_keys,
                &config_alias_resolver,
            );
            self.open_native_ssh_tab(Box::new(spec), window, cx);
            return;
        }
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

    fn resolve_restart_spec(
        &self,
        spec: Box<crate::daemon::protocol::NativeSshSpec>,
        cx: &gpui::App,
    ) -> Box<crate::daemon::protocol::NativeSshSpec> {
        resolve_persisted_ssh_spec(spec, cx)
    }

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

    fn bump_ssh_frecency(&mut self, profile_id: uuid::Uuid, cx: &mut gpui::Context<Self>) {
        self.update_config(cx, |cfg| {
            let entry = cfg.ssh_profile_frecency.entry(profile_id).or_default();
            entry.count = entry.count.saturating_add(1);
            entry.last_used = crate::core::config::unix_now();
        });
    }
}

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

    let password = if matches!(profile.auth, AuthMode::Auto | AuthMode::Password) {
        store
            .password_for(&profile.user, &profile.host, profile.port)
            .ok()
            .flatten()
    } else {
        None
    };

    let mut key_passphrases: HashMap<String, String> = HashMap::new();
    if matches!(profile.auth, AuthMode::Auto | AuthMode::PublicKey) {
        // Explicit files, then the same `~/.ssh` defaults the daemon probes
        // (#484): it looks passphrases up by the candidate string, so both
        // sides must iterate the one shared list.
        for path in identity_files
            .iter()
            .chain(crate::core::ssh_profile::default_identity_candidates().iter())
        {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let account = key_account_from_contents(&bytes);
            if let Ok(Some(passphrase)) = store.passphrase_for_key(&account) {
                key_passphrases.insert(path.clone(), passphrase);
            }
        }
    }

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
        shell_integration: profile.shell_integration,
        login_script: profile.login_scripts.clone(),
        display_name: (!profile.name.is_empty()).then(|| profile.name.clone()),
        profile_id: Some(profile.id.to_string()),
    }
}

fn map_auth_mode(auth: AuthMode) -> SshAuthMode {
    match auth {
        AuthMode::Auto => SshAuthMode::Auto,
        AuthMode::Gssapi => SshAuthMode::Gssapi,
        AuthMode::Password => SshAuthMode::Password,
        AuthMode::PublicKey => SshAuthMode::PublicKey,
        AuthMode::Agent => SshAuthMode::Agent,
        AuthMode::KeyboardInteractive => SshAuthMode::KeyboardInteractive,
    }
}

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

pub(crate) type AliasResolver<'a> = dyn Fn(&str) -> Option<(SshProfile, Option<String>)> + 'a;

pub(crate) fn config_alias_resolver(alias: &str) -> Option<(SshProfile, Option<String>)> {
    crate::core::ssh_config::resolve_alias_to_profile(alias).map(|r| (r.profile, r.proxy_jump))
}

pub(crate) fn native_spec_from_transient_profile(
    profile: &SshProfile,
    proxy_jump: Option<String>,
    store: &dyn CredentialStore,
    global_verify_host_keys: bool,
    resolve_alias: &AliasResolver<'_>,
) -> NativeSshSpec {
    let mut spec = build_native_ssh_spec(profile, &[], store, global_verify_host_keys);
    if let Some(raw) = proxy_jump {
        let mut visited = HashSet::new();
        visited.insert(profile.name.clone());
        spec.jump = resolve_jump_chain(
            &raw,
            store,
            global_verify_host_keys,
            resolve_alias,
            &mut visited,
        );
    }
    spec
}

fn resolve_jump_chain(
    raw: &str,
    store: &dyn CredentialStore,
    verify: bool,
    resolve_alias: &AliasResolver<'_>,
    visited: &mut HashSet<String>,
) -> Option<Box<NativeSshSpec>> {
    let hops: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    build_jump_from_hops(&hops, store, verify, resolve_alias, visited)
}

fn build_jump_from_hops(
    hops: &[&str],
    store: &dyn CredentialStore,
    verify: bool,
    resolve_alias: &AliasResolver<'_>,
    visited: &mut HashSet<String>,
) -> Option<Box<NativeSshSpec>> {
    let (last, earlier) = hops.split_last()?;
    if !visited.insert((*last).to_string()) {
        return None;
    }
    let (profile, own_jump) = match resolve_alias(last) {
        Some((profile, own_jump)) => (profile, if earlier.is_empty() { own_jump } else { None }),
        None => (transient_profile_from_target(last)?, None),
    };
    let mut spec = build_native_ssh_spec(&profile, &[], store, verify);
    spec.jump = if !earlier.is_empty() {
        build_jump_from_hops(earlier, store, verify, resolve_alias, visited)
    } else if let Some(own_jump) = own_jump {
        resolve_jump_chain(&own_jump, store, verify, resolve_alias, visited)
    } else {
        None
    };
    Some(Box::new(spec))
}

fn transient_profile_from_target(target: &str) -> Option<SshProfile> {
    let qc = crate::core::ssh_profile::parse_quick_connect(target)?;
    let mut profile = SshProfile::new(qc.host.clone());
    profile.port = qc.port_or_default();
    profile.host = qc.host;
    if let Some(user) = qc.user {
        profile.user = user;
    }
    Some(profile)
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
        let mut a = profile("a", "a.example.com", "u");
        let mut b = profile("b", "b.example.com", "u");
        a.jump_host = Some(b.id);
        b.jump_host = Some(a.id);
        let profiles = vec![a.clone(), b.clone()];
        let store = InMemoryCredentialStore::new();

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

        p.verify_host_keys = Some(false);
        assert!(!build_native_ssh_spec(&p, &[], &store, true).verify_host_keys);
    }

    #[test]
    fn transient_profile_maps_and_resolves_alias_jump_chain() {
        let store = InMemoryCredentialStore::new();
        let mut prod = profile("prod", "10.0.0.5", "deploy");
        prod.port = 2222;
        let resolve = |a: &str| -> Option<(SshProfile, Option<String>)> {
            match a {
                "bastion" => Some((profile("bastion", "bastion.example.com", "jump"), None)),
                _ => None,
            }
        };
        let spec = native_spec_from_transient_profile(
            &prod,
            Some("bastion".to_string()),
            &store,
            true,
            &resolve,
        );
        assert_eq!(spec.host, "10.0.0.5");
        assert_eq!(spec.port, 2222);
        let jump = spec.jump.expect("jump resolves from alias");
        assert_eq!(jump.host, "bastion.example.com");
        assert_eq!(jump.user, "jump");
        assert!(jump.jump.is_none());
    }

    #[test]
    fn transient_profile_jump_falls_back_to_user_host_port() {
        let store = InMemoryCredentialStore::new();
        let prod = profile("prod", "10.0.0.5", "deploy");
        let resolve = |_: &str| None;
        let spec = native_spec_from_transient_profile(
            &prod,
            Some("me@jump.example.com:2200".to_string()),
            &store,
            true,
            &resolve,
        );
        let jump = spec.jump.expect("jump parses as target");
        assert_eq!(jump.host, "jump.example.com");
        assert_eq!(jump.user, "me");
        assert_eq!(jump.port, 2200);
    }

    #[test]
    fn transient_profile_jump_cycle_is_broken() {
        let store = InMemoryCredentialStore::new();
        let prod = profile("prod", "10.0.0.5", "deploy");
        let resolve = |a: &str| -> Option<(SshProfile, Option<String>)> {
            match a {
                "bastion" => Some((
                    profile("bastion", "bastion.example.com", "jump"),
                    Some("prod".to_string()),
                )),
                _ => None,
            }
        };
        let spec = native_spec_from_transient_profile(
            &prod,
            Some("bastion".to_string()),
            &store,
            true,
            &resolve,
        );
        let jump = spec.jump.expect("first hop resolves");
        assert_eq!(jump.host, "bastion.example.com");
        assert!(jump.jump.is_none(), "cycle back to prod is cut");
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

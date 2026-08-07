//! Resolve an effective HTTP(S)/SOCKS proxy for tty7's *own* downloads.
//!
//! Scope: the update check and the release / remote-server asset downloads.
//! Programs running inside a pane are deliberately untouched — they inherit
//! whatever their environment says, exactly like in any other terminal.
//!
//! Resolution order:
//! 1. Manual `http_proxy` from `config.json`.
//! 2. Platform system proxy (Windows registry / macOS SCDynamicStore).
//! 3. Environment variables (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`).
//!
//! We read the platform settings by hand because `ureq` has no equivalent of
//! `NSURLSession`'s automatic system-proxy pickup: it ships a Windows-only
//! `win-system-proxy` feature (which mis-reads SOCKS entries, see
//! [`parse_windows_proxy_server`]) and nothing at all for macOS. A GUI launched
//! from Finder or the Dock inherits launchd's environment, not the shell's, so
//! step 3 on its own would leave `HTTP_PROXY` unset for most users.

use ureq::{Proxy, ProxyProtocol};

/// Resolve the proxy that should be used for `target_url`.
pub fn resolve(target_url: &str, manual: Option<&str>) -> Option<Proxy> {
    if let Some(proxy) = manual.and_then(parse_manual) {
        return Some(proxy);
    }
    if let Some(proxy) = system_proxy(target_url) {
        return Some(proxy);
    }
    // Honours `NO_PROXY` as well; the two branches above do not, since neither
    // Windows' `ProxyOverride` nor macOS' `ExceptionsList` is that variable.
    Proxy::try_from_env()
}

/// Turn a manual proxy value into a full URL, or `None` if it is unusable.
///
/// The update check builds a `reqwest` client instead of a `ureq` agent and so
/// parses the value with a different URL parser. Both go through here first, so
/// a bare `host:port` reaches them as the same `http://host:port`.
pub fn normalize_manual(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    // A bare `host:port` is by far the most common thing to paste, and every
    // other tool reads it as HTTP.
    let url = if value.contains("://") {
        value.to_string()
    } else {
        format!("http://{value}")
    };
    // Only hand back something that actually resolves to a proxy.
    proxy_from_url(&url, &[]).ok().map(|_| url)
}

/// Whether `value` would be accepted as a manual proxy, for input validation.
pub fn is_valid_manual(value: &str) -> bool {
    normalize_manual(value).is_some()
}

fn parse_manual(value: &str) -> Option<Proxy> {
    proxy_from_url(&normalize_manual(value)?, &[]).ok()
}

#[cfg(windows)]
fn system_proxy(target_url: &str) -> Option<Proxy> {
    windows::system_proxy(target_url)
}

#[cfg(target_os = "macos")]
fn system_proxy(target_url: &str) -> Option<Proxy> {
    macos::system_proxy(target_url)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn system_proxy(_target_url: &str) -> Option<Proxy> {
    None
}

/// Build a `ureq::Proxy` from a URL string and a no-proxy list.
///
/// `Proxy::new` would handle the URL on its own, but it offers no way to attach
/// a no-proxy list, so we take the authority apart and go through the builder.
/// Note that ureq silently drops no-proxy entries it cannot parse — CIDR blocks
/// such as macOS' default `169.254/16` never match anything.
fn proxy_from_url(url: &str, no_proxy: &[String]) -> Result<Proxy, ureq::Error> {
    let uri: ureq::http::Uri = url.parse().map_err(|_| ureq::Error::InvalidProxyUrl)?;
    let authority = uri.authority().ok_or(ureq::Error::InvalidProxyUrl)?;

    // ureq's own scheme table, so `socks`/`socks4a`/`socks5h` stay in sync.
    let protocol = ProxyProtocol::try_from(uri.scheme_str().unwrap_or("http"))?;

    let mut builder = Proxy::builder(protocol).host(authority.host());
    // Leaving the port unset lets ureq fill in the protocol's own default.
    if let Some(port) = authority.port_u16() {
        builder = builder.port(port);
    }

    // Matches ureq's own handling: the userinfo is passed through as written,
    // without percent-decoding.
    let (username, password) = parse_userinfo(authority.as_str());
    if !username.is_empty() {
        builder = builder.username(username);
        if let Some(password) = password {
            builder = builder.password(password);
        }
    }

    for entry in no_proxy {
        builder = builder.no_proxy(entry.as_str());
    }

    builder.build()
}

fn parse_userinfo(authority: &str) -> (&str, Option<&str>) {
    let Some((userinfo, _)) = authority.split_once('@') else {
        return ("", None);
    };
    let mut parts = userinfo.splitn(2, ':');
    let username = parts.next().unwrap_or("");
    let password = parts.next();
    (username, password)
}

fn target_scheme(target_url: &str) -> &str {
    target_url
        .split_once("://")
        .map(|(scheme, _)| scheme)
        .unwrap_or("http")
}

#[cfg_attr(not(windows), allow(dead_code))]
fn normalize_proxy_addr(addr: &str) -> String {
    if addr.contains("://") {
        addr.to_string()
    } else {
        format!("http://{addr}")
    }
}

/// Parse a Windows `ProxyServer` registry value into a proxy URL.
///
/// Examples:
/// - `127.0.0.1:7890` -> `http://127.0.0.1:7890`
/// - `http=127.0.0.1:7890;https=127.0.0.1:7891`
/// - `socks=127.0.0.1:1080` -> `socks5://127.0.0.1:1080`
///
/// The `socks=` mapping is the whole reason we do not use ureq's
/// `win-system-proxy` feature: it unconditionally builds `http://{ProxyServer}`
/// and so talks HTTP to a SOCKS port.
///
/// Deliberately outside the `windows` module so the parsing is compiled and
/// tested on every host, not just the Windows CI runner.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_windows_proxy_server(server: &str, target_scheme: &str) -> Option<String> {
    if !server.contains('=') {
        let server = server.trim();
        return (!server.is_empty()).then(|| normalize_proxy_addr(server));
    }

    let mut map = std::collections::HashMap::new();
    for part in server.split(';') {
        // Skip anything that isn't `proto=addr`. A trailing `;` or a stray
        // token must not throw the whole setting away.
        let Some((proto, addr)) = part.split_once('=') else {
            continue;
        };
        let addr = addr.trim();
        if addr.is_empty() {
            continue;
        }
        map.insert(proto.trim().to_ascii_lowercase(), addr.to_string());
    }

    if let Some(addr) = map.get(target_scheme).or_else(|| map.get("http")) {
        return Some(normalize_proxy_addr(addr));
    }
    map.get("socks").map(|addr| {
        if addr.contains("://") {
            addr.clone()
        } else {
            format!("socks5://{addr}")
        }
    })
}

#[cfg(windows)]
mod windows {
    use super::{parse_windows_proxy_server, proxy_from_url, target_scheme};
    use ureq::Proxy;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    const INTERNET_SETTINGS: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";

    pub fn system_proxy(target_url: &str) -> Option<Proxy> {
        let key = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(INTERNET_SETTINGS)
            .ok()?;
        let enabled: u32 = key.get_value("ProxyEnable").ok()?;
        if enabled != 1 {
            return None;
        }
        let server: String = key.get_value("ProxyServer").ok()?;
        let overrides: String = key.get_value("ProxyOverride").unwrap_or_default();
        // `<local>` means "any hostname without a dot", which ureq's no-proxy
        // matcher cannot express, so localhost is not bypassed.
        let no_proxy: Vec<String> = overrides
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "<local>")
            .map(String::from)
            .collect();

        let proxy_url = parse_windows_proxy_server(&server, target_scheme(target_url))?;
        proxy_from_url(&proxy_url, &no_proxy).ok()
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{proxy_from_url, target_scheme};
    // Via `system_configuration`'s re-export, so these are the same
    // core-foundation types `get_proxies` hands back. `daemon::pane` keeps its
    // own, newer core-foundation; the two never exchange values.
    use system_configuration::core_foundation::array::CFArray;
    use system_configuration::core_foundation::base::{CFType, TCFType};
    use system_configuration::core_foundation::dictionary::CFDictionary;
    use system_configuration::core_foundation::number::CFNumber;
    use system_configuration::core_foundation::string::CFString;
    use system_configuration::dynamic_store::SCDynamicStoreBuilder;
    use system_configuration_sys::schema_definitions::{
        kSCPropNetProxiesExceptionsList, kSCPropNetProxiesHTTPEnable, kSCPropNetProxiesHTTPPort,
        kSCPropNetProxiesHTTPProxy, kSCPropNetProxiesHTTPSEnable, kSCPropNetProxiesHTTPSPort,
        kSCPropNetProxiesHTTPSProxy, kSCPropNetProxiesSOCKSEnable, kSCPropNetProxiesSOCKSPort,
        kSCPropNetProxiesSOCKSProxy,
    };
    use ureq::Proxy;

    /// The proxy settings dictionary `SCDynamicStore` hands back.
    type Proxies = CFDictionary<CFString, CFType>;

    #[derive(Clone, Copy)]
    enum Kind {
        Http,
        Https,
        Socks,
    }

    impl Kind {
        /// The `(enable, host, port)` schema keys for this entry.
        fn keys(self) -> (CFString, CFString, CFString) {
            unsafe {
                let (enable, host, port) = match self {
                    Kind::Http => (
                        kSCPropNetProxiesHTTPEnable,
                        kSCPropNetProxiesHTTPProxy,
                        kSCPropNetProxiesHTTPPort,
                    ),
                    Kind::Https => (
                        kSCPropNetProxiesHTTPSEnable,
                        kSCPropNetProxiesHTTPSProxy,
                        kSCPropNetProxiesHTTPSPort,
                    ),
                    Kind::Socks => (
                        kSCPropNetProxiesSOCKSEnable,
                        kSCPropNetProxiesSOCKSProxy,
                        kSCPropNetProxiesSOCKSPort,
                    ),
                };
                (
                    CFString::wrap_under_get_rule(enable),
                    CFString::wrap_under_get_rule(host),
                    CFString::wrap_under_get_rule(port),
                )
            }
        }
    }

    pub fn system_proxy(target_url: &str) -> Option<Proxy> {
        let store = SCDynamicStoreBuilder::new("tty7").build();
        let proxies = store.get_proxies()?;

        // Each entry is configured independently in System Settings, so prefer
        // the one matching the target scheme and fall back through the rest.
        let order = if target_scheme(target_url) == "https" {
            [Kind::Https, Kind::Http, Kind::Socks]
        } else {
            [Kind::Http, Kind::Https, Kind::Socks]
        };
        let proxy_url = order
            .into_iter()
            .find_map(|kind| read_proxy(&proxies, kind))?;

        proxy_from_url(&proxy_url, &read_exceptions(&proxies)).ok()
    }

    fn read_proxy(proxies: &Proxies, kind: Kind) -> Option<String> {
        let (enable_key, host_key, port_key) = kind.keys();

        let enabled = proxies
            .find(&enable_key)
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_i64())
            == Some(1);
        if !enabled {
            return None;
        }

        let host = proxies
            .find(&host_key)
            .and_then(|v| v.downcast::<CFString>())?
            .to_string();
        let port = proxies
            .find(&port_key)
            .and_then(|v| v.downcast::<CFNumber>())?
            .to_i64()?;

        Some(proxy_url(kind, &host, port))
    }

    /// macOS' "HTTPS proxy" is the *plain HTTP* proxy used for https traffic,
    /// not a TLS-wrapped one. Emitting `https://` here would land on
    /// `ProxyProtocol::Https` — "CONNECT proxy over HTTPS" — and make ureq
    /// TLS-handshake with the proxy itself, which an ordinary local proxy
    /// listening on 7890 rejects. The Windows path normalises the same way.
    fn proxy_url(kind: Kind, host: &str, port: i64) -> String {
        match kind {
            Kind::Http | Kind::Https => format!("http://{host}:{port}"),
            Kind::Socks => format!("socks5://{host}:{port}"),
        }
    }

    fn read_exceptions(proxies: &Proxies) -> Vec<String> {
        let key = unsafe { CFString::wrap_under_get_rule(kSCPropNetProxiesExceptionsList) };
        let Some(list) = proxies.find(&key).and_then(|v| v.downcast::<CFArray>()) else {
            return Vec::new();
        };
        list.iter()
            .filter_map(|raw| {
                let item = unsafe { CFType::wrap_under_get_rule(*raw) };
                item.downcast::<CFString>().map(|s| s.to_string())
            })
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::{Kind, proxy_url};

        #[test]
        fn https_entries_use_a_plain_http_proxy_url() {
            // Mapping this to `https://` would make ureq TLS-handshake with the
            // proxy and break every download — see `proxy_url`.
            assert_eq!(
                proxy_url(Kind::Https, "127.0.0.1", 7890),
                "http://127.0.0.1:7890"
            );
        }

        #[test]
        fn socks_entries_use_socks5() {
            assert_eq!(
                proxy_url(Kind::Socks, "127.0.0.1", 1080),
                "socks5://127.0.0.1:1080"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows_proxy(server: &str, target: &str) -> Option<String> {
        parse_windows_proxy_server(server, target)
    }

    #[test]
    fn bare_address_defaults_to_http() {
        assert_eq!(
            windows_proxy("127.0.0.1:7890", "https"),
            Some("http://127.0.0.1:7890".to_string())
        );
    }

    #[test]
    fn per_protocol_http_and_https() {
        assert_eq!(
            windows_proxy("http=127.0.0.1:7890;https=127.0.0.1:7891", "https"),
            Some("http://127.0.0.1:7891".to_string())
        );
    }

    #[test]
    fn falls_back_to_http_for_unknown_scheme() {
        assert_eq!(
            windows_proxy("http=127.0.0.1:7890;https=127.0.0.1:7891", "ftp"),
            Some("http://127.0.0.1:7890".to_string())
        );
    }

    #[test]
    fn socks_only_uses_socks5() {
        assert_eq!(
            windows_proxy("socks=127.0.0.1:1080", "https"),
            Some("socks5://127.0.0.1:1080".to_string())
        );
    }

    /// Windows writes `ftp=`/`socks=` entries alongside the ones we understand,
    /// and a trailing `;` is common. Bailing out on the first unparsable part
    /// used to discard the entire proxy setting.
    #[test]
    fn stray_parts_do_not_discard_the_setting() {
        assert_eq!(
            windows_proxy("http=127.0.0.1:7890;", "http"),
            Some("http://127.0.0.1:7890".to_string())
        );
        assert_eq!(
            windows_proxy("ftp=1.2.3.4:21;http=127.0.0.1:7890;<local>", "http"),
            Some("http://127.0.0.1:7890".to_string())
        );
    }

    #[test]
    fn empty_or_valueless_settings_resolve_to_nothing() {
        assert_eq!(windows_proxy("", "http"), None);
        assert_eq!(windows_proxy("   ", "http"), None);
        assert_eq!(windows_proxy("http=", "http"), None);
        assert_eq!(windows_proxy("ftp=1.2.3.4:21", "http"), None);
    }

    #[test]
    fn manual_bare_address_is_http() {
        let proxy = parse_manual("127.0.0.1:7890").expect("parses");
        assert_eq!(proxy.protocol(), ProxyProtocol::Http);
        assert_eq!(proxy.host(), "127.0.0.1");
        assert_eq!(proxy.port(), 7890);
    }

    #[test]
    fn manual_socks5_keeps_its_scheme() {
        let proxy = parse_manual("socks5://127.0.0.1:1080").expect("parses");
        assert_eq!(proxy.protocol(), ProxyProtocol::Socks5);
        assert_eq!(proxy.port(), 1080);
    }

    #[test]
    fn manual_proxy_falls_back_to_the_protocol_default_port() {
        assert_eq!(
            parse_manual("http://proxy.local").expect("parses").port(),
            80
        );
        assert_eq!(
            parse_manual("socks5://proxy.local").expect("parses").port(),
            1080
        );
    }

    #[test]
    fn manual_credentials_are_carried_through() {
        let proxy = parse_manual("http://user:secret@127.0.0.1:7890").expect("parses");
        assert_eq!(proxy.host(), "127.0.0.1");
        assert_eq!(proxy.username(), Some("user"));
        assert_eq!(proxy.password(), Some("secret"));
    }

    #[test]
    fn blank_and_malformed_manual_values_are_rejected() {
        assert!(parse_manual("").is_none());
        assert!(parse_manual("   ").is_none());
        assert!(parse_manual("ftp://127.0.0.1:21").is_none());
        assert!(!is_valid_manual("nonsense://host"));
        assert!(is_valid_manual(" http://127.0.0.1:7890 "));
    }

    #[test]
    fn a_manual_proxy_wins_over_everything_else() {
        let proxy = resolve("https://github.com", Some("socks5://127.0.0.1:1080"))
            .expect("manual proxy resolves");
        assert_eq!(proxy.protocol(), ProxyProtocol::Socks5);
        assert_eq!(proxy.port(), 1080);
    }

    #[test]
    fn target_scheme_reads_the_scheme_or_assumes_http() {
        assert_eq!(target_scheme("https://github.com/x"), "https");
        assert_eq!(target_scheme("http://example.com"), "http");
        assert_eq!(target_scheme("github.com"), "http");
    }
}

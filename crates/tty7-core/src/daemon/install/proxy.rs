//! Resolve an effective HTTP(S)/SOCKS proxy for release downloads.
//!
//! Resolution order:
//! 1. Manual `http_proxy` from `config.json`.
//! 2. Platform system proxy (Windows registry / macOS SCDynamicStore).
//! 3. Environment variables (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`).

use ureq::{Proxy, ProxyProtocol};

/// Resolve the proxy that should be used for `target_url`.
pub fn resolve(target_url: &str, manual: Option<&str>) -> Option<Proxy> {
    if let Some(proxy) = manual.and_then(parse_manual) {
        return Some(proxy);
    }
    if let Some(proxy) = system_proxy(target_url) {
        return Some(proxy);
    }
    ureq::Proxy::try_from_env()
}

fn parse_manual(value: &str) -> Option<Proxy> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let url = if value.contains("://") {
        value.to_string()
    } else {
        format!("http://{value}")
    };
    proxy_from_url(&url, &[]).ok()
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

/// Build a `ureq::Proxy` from a URL string and an optional no-proxy list.
fn proxy_from_url(url: &str, no_proxy: &[String]) -> Result<Proxy, ureq::Error> {
    let uri: ureq::http::Uri = url.parse().map_err(|_| ureq::Error::InvalidProxyUrl)?;
    let authority = uri.authority().ok_or(ureq::Error::InvalidProxyUrl)?.clone();

    let scheme = uri.scheme_str().unwrap_or("http");
    let protocol = proxy_protocol(scheme)?;

    let host = authority.host();
    let port = authority
        .port_u16()
        .unwrap_or_else(|| default_port(protocol));

    let mut builder = Proxy::builder(protocol).host(host).port(port);

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

fn proxy_protocol(scheme: &str) -> Result<ProxyProtocol, ureq::Error> {
    match scheme.to_ascii_lowercase().as_str() {
        "http" => Ok(ProxyProtocol::Http),
        "https" => Ok(ProxyProtocol::Https),
        "socks4" => Ok(ProxyProtocol::Socks4),
        "socks4a" => Ok(ProxyProtocol::Socks4A),
        "socks" | "socks5" => Ok(ProxyProtocol::Socks5),
        "socks5h" => Ok(ProxyProtocol::Socks5h),
        _ => Err(ureq::Error::InvalidProxyUrl),
    }
}

fn default_port(protocol: ProxyProtocol) -> u16 {
    match protocol {
        ProxyProtocol::Http => 80,
        ProxyProtocol::Https => 443,
        ProxyProtocol::Socks4
        | ProxyProtocol::Socks4A
        | ProxyProtocol::Socks5
        | ProxyProtocol::Socks5h => 1080,
        _ => 1080,
    }
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

fn target_scheme(target_url: &str) -> Option<&str> {
    target_url.split("://").next()
}

#[cfg(windows)]
mod windows {
    use super::{proxy_from_url, target_scheme};
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
        let no_proxy: Vec<String> = overrides
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "<local>")
            .map(String::from)
            .collect();

        let target = target_scheme(target_url).unwrap_or("http");
        let proxy_url = parse_proxy_server(&server, target)?;
        proxy_from_url(&proxy_url, &no_proxy).ok()
    }

    /// Parse a Windows `ProxyServer` registry value.
    ///
    /// Examples:
    /// - `127.0.0.1:7890` -> `http://127.0.0.1:7890`
    /// - `http=127.0.0.1:7890;https=127.0.0.1:7890`
    /// - `socks=127.0.0.1:1080`
    fn parse_proxy_server(server: &str, target_scheme: &str) -> Option<String> {
        if server.contains('=') {
            let mut map = std::collections::HashMap::new();
            for part in server.split(';') {
                let mut kv = part.splitn(2, '=');
                let proto = kv.next()?.trim().to_ascii_lowercase();
                let addr = kv.next()?.trim();
                map.insert(proto, normalize_proxy_addr(addr));
            }

            if let Some(addr) = map.get(target_scheme).or_else(|| map.get("http")) {
                return Some(addr.clone());
            }
            if let Some(addr) = map.get("socks") {
                return Some(format!(
                    "socks5://{}",
                    addr.strip_prefix("http://").unwrap_or(addr)
                ));
            }
            None
        } else {
            Some(normalize_proxy_addr(server))
        }
    }

    fn normalize_proxy_addr(addr: &str) -> String {
        if addr.contains("://") {
            addr.to_string()
        } else {
            format!("http://{addr}")
        }
    }

    #[cfg(test)]
    mod tests {
        use super::parse_proxy_server;

        #[test]
        fn bare_address_defaults_to_http() {
            assert_eq!(
                parse_proxy_server("127.0.0.1:7890", "https"),
                Some("http://127.0.0.1:7890".to_string())
            );
        }

        #[test]
        fn per_protocol_http_and_https() {
            assert_eq!(
                parse_proxy_server("http=127.0.0.1:7890;https=127.0.0.1:7891", "https"),
                Some("http://127.0.0.1:7891".to_string())
            );
        }

        #[test]
        fn falls_back_to_http_for_unknown_scheme() {
            assert_eq!(
                parse_proxy_server("http=127.0.0.1:7890;https=127.0.0.1:7891", "ftp"),
                Some("http://127.0.0.1:7890".to_string())
            );
        }

        #[test]
        fn socks_only_uses_socks5() {
            assert_eq!(
                parse_proxy_server("socks=127.0.0.1:1080", "https"),
                Some("socks5://127.0.0.1:1080".to_string())
            );
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{proxy_from_url, target_scheme};
    use core_foundation::base::TCFType;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use system_configuration::dynamic_store::SCDynamicStoreBuilder;
    use system_configuration_sys::schema_definitions::{
        kSCPropNetProxiesExceptionsList, kSCPropNetProxiesHTTPEnable, kSCPropNetProxiesHTTPPort,
        kSCPropNetProxiesHTTPProxy, kSCPropNetProxiesHTTPSEnable, kSCPropNetProxiesHTTPSPort,
        kSCPropNetProxiesHTTPSProxy, kSCPropNetProxiesSOCKSEnable, kSCPropNetProxiesSOCKSPort,
        kSCPropNetProxiesSOCKSProxy,
    };
    use ureq::Proxy;

    pub fn system_proxy(target_url: &str) -> Option<Proxy> {
        let store = SCDynamicStoreBuilder::new("tty7").build();
        let proxies = store.get_proxies()?;

        let target = target_scheme(target_url).unwrap_or("http");
        let proxy_url = if target == "https" {
            read_proxy(&proxies, "https")
                .or_else(|| read_proxy(&proxies, "http"))
                .or_else(|| read_proxy(&proxies, "socks"))
        } else {
            read_proxy(&proxies, "http")
                .or_else(|| read_proxy(&proxies, "https"))
                .or_else(|| read_proxy(&proxies, "socks"))
        }?;

        let no_proxy = read_exceptions(&proxies);
        proxy_from_url(&proxy_url, &no_proxy).ok()
    }

    fn read_proxy(
        proxies: &core_foundation::dictionary::CFDictionary<
            CFString,
            core_foundation::base::CFType,
        >,
        kind: &str,
    ) -> Option<String> {
        let enabled_key = unsafe {
            CFString::wrap_under_get_rule(match kind {
                "http" => kSCPropNetProxiesHTTPEnable,
                "https" => kSCPropNetProxiesHTTPSEnable,
                "socks" => kSCPropNetProxiesSOCKSEnable,
                _ => return None,
            })
        };
        let enabled = proxies
            .find(&enabled_key)
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_i64())
            == Some(1);
        if !enabled {
            return None;
        }

        let (host_key, port_key) = unsafe {
            let host = match kind {
                "http" => kSCPropNetProxiesHTTPProxy,
                "https" => kSCPropNetProxiesHTTPSProxy,
                "socks" => kSCPropNetProxiesSOCKSProxy,
                _ => return None,
            };
            let port = match kind {
                "http" => kSCPropNetProxiesHTTPPort,
                "https" => kSCPropNetProxiesHTTPSPort,
                "socks" => kSCPropNetProxiesSOCKSPort,
                _ => return None,
            };
            (
                CFString::wrap_under_get_rule(host),
                CFString::wrap_under_get_rule(port),
            )
        };

        let host = proxies
            .find(&host_key)
            .and_then(|v| v.downcast::<CFString>())?
            .to_string();
        let port = proxies
            .find(&port_key)
            .and_then(|v| v.downcast::<CFNumber>())?
            .to_i64()?;

        let scheme = if kind == "socks" { "socks5" } else { kind };
        Some(format!("{scheme}://{host}:{port}"))
    }

    fn read_exceptions(
        proxies: &core_foundation::dictionary::CFDictionary<
            CFString,
            core_foundation::base::CFType,
        >,
    ) -> Vec<String> {
        let key = unsafe { CFString::wrap_under_get_rule(kSCPropNetProxiesExceptionsList) };
        proxies
            .find(&key)
            .and_then(|v| v.downcast::<core_foundation::array::CFArray>())
            .map(|arr| arr.iter::<CFString>().map(|s| s.to_string()).collect())
            .unwrap_or_default()
    }
}

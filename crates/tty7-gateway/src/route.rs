//! How the gateway gets out to its relay: straight, or through a proxy.
//!
//! The relay is what lets a phone on another network reach this machine at
//! all — it carries the first packets and coordinates hole punching — and iroh
//! dials it with its own resolver and its own TCP, not through whatever proxy
//! the system is set to. On a machine whose network only works through a local
//! proxy, that dial fails and nothing says so: the gateway still serves phones
//! on its own network, and every other phone times out.
//!
//! So the gateway lists the ways out it knows of, the ones people have set up
//! first, and keeps the first one the relay actually answers on.

use std::fmt;

use tty7_core::core::config::config_path;
use tty7_core::daemon::install::proxy;
use url::Url;

/// One way to reach the relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Direct,
    /// Through an HTTP proxy, with `CONNECT`.
    Proxy(Url),
}

impl Route {
    pub fn proxy(&self) -> Option<&Url> {
        match self {
            Route::Direct => None,
            Route::Proxy(url) => Some(url),
        }
    }
}

impl fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Route::Direct => f.write_str("direct"),
            // Never the userinfo: this ends up in logs.
            Route::Proxy(url) => match url.port_or_known_default() {
                Some(port) => write!(f, "proxy {}:{port}", url.host_str().unwrap_or("?")),
                None => write!(f, "proxy {}", url.host_str().unwrap_or("?")),
            },
        }
    }
}

/// Every way out worth trying, most likely first: tty7's own proxy setting,
/// then the system's, then the environment's, then none. Read afresh on each
/// call — a proxy is switched on and off far more often than the gateway
/// restarts.
pub fn routes() -> Vec<Route> {
    // Relays are dialed over https; the proxy that applies is that scheme's.
    const TARGET: &str = "https://relay.invalid/";
    candidates([
        manual_proxy().as_deref().and_then(proxy::normalize_manual),
        proxy::system_url(TARGET),
        proxy::env_url(TARGET),
    ])
}

/// `http_proxy` from `config.json`, read on its own: loading the whole
/// `Config` may migrate the file, which is the GUI's business, not the
/// gateway's.
fn manual_proxy() -> Option<String> {
    let text = std::fs::read(config_path("config.json")?).ok()?;
    let config: serde_json::Value = serde_json::from_slice(&text).ok()?;
    config.get("http_proxy")?.as_str().map(str::to_owned)
}

fn candidates(proxies: impl IntoIterator<Item = Option<String>>) -> Vec<Route> {
    let mut routes = Vec::new();
    for url in proxies.into_iter().flatten() {
        // iroh tunnels through a proxy with `CONNECT`; a SOCKS one it cannot
        // use, and the relay would stay out of reach through it.
        let Some(url) = Url::parse(&url)
            .ok()
            .filter(|u| u.scheme() == "http" || u.scheme() == "https")
        else {
            continue;
        };
        let route = Route::Proxy(url);
        if !routes.contains(&route) {
            routes.push(route);
        }
    }
    routes.push(Route::Direct);
    routes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(url: &str) -> Route {
        Route::Proxy(Url::parse(url).unwrap())
    }

    #[test]
    fn with_no_proxy_anywhere_the_only_way_is_direct() {
        assert_eq!(candidates([None, None, None]), [Route::Direct]);
    }

    #[test]
    fn proxies_come_first_in_order_and_direct_last() {
        assert_eq!(
            candidates([
                Some("http://10.0.0.1:3128".into()),
                None,
                Some("http://127.0.0.1:7890".into()),
            ]),
            [
                proxy("http://10.0.0.1:3128"),
                proxy("http://127.0.0.1:7890"),
                Route::Direct
            ]
        );
    }

    #[test]
    fn the_same_proxy_named_twice_is_tried_once() {
        // The usual case: the system proxy and `HTTPS_PROXY` both point at the
        // same local proxy.
        assert_eq!(
            candidates([
                None,
                Some("http://127.0.0.1:7890".into()),
                Some("http://127.0.0.1:7890".into()),
            ]),
            [proxy("http://127.0.0.1:7890"), Route::Direct]
        );
    }

    #[test]
    fn socks_proxies_are_skipped() {
        assert_eq!(
            candidates([Some("socks5://127.0.0.1:1080".into()), None, None]),
            [Route::Direct]
        );
    }

    #[test]
    fn a_route_prints_without_credentials() {
        assert_eq!(
            proxy("http://user:secret@127.0.0.1:7890").to_string(),
            "proxy 127.0.0.1:7890"
        );
        assert_eq!(Route::Direct.to_string(), "direct");
    }
}

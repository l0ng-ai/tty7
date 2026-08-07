use std::io::Read as _;
use std::ops::ControlFlow;
use std::time::Duration;

use super::AssetFetcher;
use super::proxy;

// `timeout_global` caps the whole request, so it has to be sized for the
// largest asset over the slowest link anyone actually uses — not for a healthy
// one. The old 180s meant a 28 MB release package needed a sustained 160 KB/s
// just to finish, which a domestic connection to GitHub routinely misses: the
// update then failed for everyone on a slow link, forever. 15 minutes asks for
// 33 KB/s instead, and still bounds a connection that has genuinely died.
//
// Callers who want out sooner cancel — see `get_cancellable`.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

const MAX_ASSET_BYTES: u64 = 128 * 1024 * 1024;

const READ_CHUNK: usize = 64 * 1024;

pub struct HttpsFetcher {
    agent: ureq::Agent,
}

impl HttpsFetcher {
    pub fn new(manual_proxy: Option<&str>) -> Self {
        let mut builder = ureq::Agent::config_builder()
            .timeout_global(Some(DOWNLOAD_TIMEOUT))
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .user_agent(concat!("tty7/", env!("CARGO_PKG_VERSION")));

        if let Some(proxy) = proxy::resolve("https://github.com", manual_proxy) {
            builder = builder.proxy(Some(proxy));
        }

        Self {
            agent: builder.build().into(),
        }
    }
}

impl Default for HttpsFetcher {
    fn default() -> Self {
        Self::new(None)
    }
}

/// Returned when `on_progress` asked to stop. Recognised by callers so a
/// deliberate cancel is not reported to the user as a download failure.
pub const CANCELLED: &str = "the download was cancelled";

impl HttpsFetcher {
    /// `get_with_progress` with a callback that can call the transfer off.
    ///
    /// The in-app updater is the reason this exists: a 30 MB package on a slow
    /// link is precisely the download someone wants to abandon, and without
    /// this the only way out is to kill the app.
    pub fn get_cancellable(
        &self,
        url: &str,
        on_progress: &dyn Fn(u64, Option<u64>) -> ControlFlow<()>,
    ) -> Result<Vec<u8>, String> {
        self.fetch(url, on_progress)
    }
}

impl AssetFetcher for HttpsFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        self.fetch(url, &|_, _| ControlFlow::Continue(()))
    }

    fn get_with_progress(
        &self,
        url: &str,
        on_progress: &dyn Fn(u64, Option<u64>),
    ) -> Result<Vec<u8>, String> {
        self.fetch(url, &|received, total| {
            on_progress(received, total);
            ControlFlow::Continue(())
        })
    }
}

impl HttpsFetcher {
    fn fetch(
        &self,
        url: &str,
        on_progress: &dyn Fn(u64, Option<u64>) -> ControlFlow<()>,
    ) -> Result<Vec<u8>, String> {
        let response = self
            .agent
            .get(url)
            .call()
            .map_err(|e| describe(url, &e.to_string()))?;

        let status = response.status().as_u16();
        if status == 404 {
            return Err(format!(
                "{url} does not exist (404) — this build's release may not be published"
            ));
        }
        if !(200..300).contains(&status) {
            return Err(format!("{url} returned HTTP {status}"));
        }

        let declared = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n <= MAX_ASSET_BYTES);

        let mut body = response.into_body();
        let mut reader = body.as_reader().take(MAX_ASSET_BYTES + 1);
        let mut bytes = Vec::with_capacity(declared.unwrap_or(0) as usize);
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| describe(url, &e.to_string()))?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&buf[..n]);
            if bytes.len() as u64 > MAX_ASSET_BYTES {
                return Err(format!(
                    "{url} is larger than the {MAX_ASSET_BYTES} byte ceiling for a release asset"
                ));
            }
            if on_progress(bytes.len() as u64, declared).is_break() {
                return Err(CANCELLED.to_string());
            }
        }
        Ok(bytes)
    }
}

fn describe(url: &str, reason: &str) -> String {
    let lower = reason.to_ascii_lowercase();
    if lower.contains("dns") || lower.contains("resolve") {
        return format!("could not resolve the host for {url} ({reason})");
    }
    if lower.contains("certificate") || lower.contains("tls") || lower.contains("handshake") {
        return format!(
            "TLS failed fetching {url} ({reason}) — a TLS-intercepting proxy would explain this"
        );
    }
    reason.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_builds() {
        let _ = HttpsFetcher::default();
    }

    #[test]
    #[ignore = "needs the network"]
    fn talks_to_github() {
        let fetcher = HttpsFetcher::default();
        let bytes = fetcher
            .get("https://github.com/l0ng-ai/tty7/raw/main/README.md")
            .expect("github must be reachable over TLS");
        assert!(
            bytes.len() > 500,
            "got {} bytes — an unfollowed redirect looks exactly like this",
            bytes.len()
        );

        let missing = super::super::asset::download_url("v0.0.0-never", "tty7-server-nope");
        let err = fetcher
            .get(&missing)
            .expect_err("a missing release must fail");
        assert!(err.contains("404"), "{err}");
    }

    #[test]
    fn tls_failures_name_the_likely_cause() {
        let msg = describe("https://example/x", "invalid peer certificate");
        assert!(msg.contains("proxy"), "{msg}");
        let msg = describe("https://example/x", "failed to lookup address information");
        assert!(!msg.contains("proxy"), "{msg}");
    }
}

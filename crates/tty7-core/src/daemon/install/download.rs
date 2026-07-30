use std::io::Read as _;
use std::time::Duration;

use super::AssetFetcher;

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(180);

const MAX_ASSET_BYTES: u64 = 128 * 1024 * 1024;

const READ_CHUNK: usize = 64 * 1024;

pub struct HttpsFetcher {
    agent: ureq::Agent,
}

impl Default for HttpsFetcher {
    fn default() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(DOWNLOAD_TIMEOUT))
            .user_agent(concat!("tty7/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl AssetFetcher for HttpsFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        self.get_with_progress(url, &|_, _| {})
    }

    fn get_with_progress(
        &self,
        url: &str,
        on_progress: &dyn Fn(u64, Option<u64>),
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
            on_progress(bytes.len() as u64, declared);
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

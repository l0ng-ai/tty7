//! The HTTPS half of D5: the *client* downloads release assets and pushes them
//! over SSH, because the machines this feature exists for — behind a jump host,
//! on an internal network, in a locked-down VPC — frequently cannot reach GitHub
//! themselves.
//!
//! ## Why `ureq`, and why behind a feature
//!
//! The GUI's update check uses `reqwest_client`, which wraps Zed's reqwest fork
//! behind `gpui::http_client`. `tty7-core` must not depend on gpui, so that stack
//! is unavailable here. `ureq` is blocking (which matches this call path — the
//! installer runs on a daemon std thread, not in an async context), rustls-based
//! (no OpenSSL, so nothing to find at build time), and shares the `rustls` and
//! `http` versions already in the tree.
//!
//! It is optional, behind `remote-install`, which the GUI crate turns on and
//! `tty7-server` does not. `tty7-server` builds as a *static musl* binary that is
//! itself the thing being downloaded; giving it an HTTP client would add size and
//! a TLS backend to every remote install for a code path it can never take. Same
//! mechanism as the existing `gssapi` feature, for the same reason.

use std::io::Read as _;
use std::time::Duration;

use super::AssetFetcher;

/// Overall budget for one asset download. A 6 MB binary on a bad connection is
/// slow but finite; a stalled TLS session is not, and this is what makes the
/// difference visible.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(180);

/// Refuse a body larger than this. A release asset is ~6 MB; anything at this
/// scale means we are downloading something other than what we asked for, and
/// buffering it in memory before finding out is not a good trade.
const MAX_ASSET_BYTES: u64 = 128 * 1024 * 1024;

/// How much body to take per read, and therefore how often progress is
/// reported: ~130 updates over a 8 MB asset. Large enough that the syscall
/// overhead stays irrelevant, small enough that a bar moves smoothly rather
/// than in visible jumps.
const READ_CHUNK: usize = 64 * 1024;

/// Downloads release assets over HTTPS.
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

        // GitHub serves release assets from a redirect to object storage; ureq
        // follows those itself. A 404 here is the interesting one: it means the
        // release tag we derived from our own version was never published (a
        // local dev build, a tag that failed to publish), and saying so beats
        // "download failed".
        let status = response.status().as_u16();
        if status == 404 {
            return Err(format!(
                "{url} does not exist (404) — this build's release may not be published"
            ));
        }
        if !(200..300).contains(&status) {
            return Err(format!("{url} returned HTTP {status}"));
        }

        // Only a hint: it is what the *server* claims, so it sizes the
        // allocation and the progress bar but never the ceiling check below.
        let declared = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|n| *n <= MAX_ASSET_BYTES);

        let mut body = response.into_body();
        // `take` still caps the read, so a lying (or absent) Content-Length
        // cannot make this buffer more than the ceiling — one byte over is
        // enough to detect it, which is why the limit is `+ 1`.
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

/// Turn a transport error into something a user can act on. The distinction
/// worth drawing is "the network is not reachable from here" (retry later, or
/// check the proxy) versus everything else.
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

    /// Constructing the agent must not panic (a rustls provider that fails to
    /// install would, and would do it at the worst possible moment — mid-connect
    /// on someone's first remote workspace).
    #[test]
    fn the_agent_builds() {
        let _ = HttpsFetcher::default();
    }

    /// Talks to the real github.com. `#[ignore]`d because it needs the network,
    /// which no other test here does — run it by hand (`cargo test -p tty7-core
    /// --features remote-install -- --ignored talks_to_github --nocapture`)
    /// after touching the HTTP client.
    ///
    /// Two things only a live server can prove:
    ///
    /// - **Redirects are followed.** Every GitHub download path — `/raw/` and
    ///   `releases/download/…` alike — answers with a 302 to another host. A
    ///   client that does not follow it returns an empty body under a status
    ///   that still reads as success, so the "asset" would sha256 to the digest
    ///   of nothing. Asserting real content is what catches that.
    /// - **The TLS trust anchor works.** ureq's webpki roots must accept
    ///   github.com's chain; that HTTPS connection *is* the security model here
    ///   (`checksums.txt` is not separately signed).
    ///
    /// Deliberately a small file rather than a release asset: assets are ~20 MB
    /// and this is a correctness check, not a bandwidth test.
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

        // And a tag that was never published is reported as such, not as a
        // generic transport failure.
        let missing = super::super::asset::download_url("v0.0.0-never", "tty7-server-nope");
        let err = fetcher
            .get(&missing)
            .expect_err("a missing release must fail");
        assert!(err.contains("404"), "{err}");
    }

    /// The proxy/TLS case gets its own wording because the fix is completely
    /// different from "try again later".
    #[test]
    fn tls_failures_name_the_likely_cause() {
        let msg = describe("https://example/x", "invalid peer certificate");
        assert!(msg.contains("proxy"), "{msg}");
        let msg = describe("https://example/x", "failed to lookup address information");
        assert!(!msg.contains("proxy"), "{msg}");
    }
}

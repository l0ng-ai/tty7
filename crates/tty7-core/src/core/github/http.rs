//! [`Transport`] over HTTPS, with the same stack and proxy resolution the
//! installer and the update check use.
//!
//! Blocking — every call runs on a worker, never on the UI thread.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::api::{ApiError, Reply, Transport, classify, link_has_next};
use super::token::Token;

const API: &str = "https://api.github.com";

/// Interactive reads: long enough for a slow link, short enough that a dead
/// one is reported rather than sat on.
const TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// A pull request's file list with every patch inlined is the largest answer
/// the panel asks for; GitHub itself caps a patch well under this.
const MAX_BODY: u64 = 32 * 1024 * 1024;

/// Past this many remembered answers the cache starts over.
const ETAG_CAP: usize = 512;

pub struct HttpTransport {
    agent: ureq::Agent,
    token: Option<Token>,
    etags: Etags,
}

/// The last answer to each plain GET, by ETag. Re-asking with `If-None-Match`
/// costs nothing when it comes back 304: a signed-in 304 is not counted
/// against the rate limit, which is what keeps the panel's revalidations and
/// polls from eating the hourly allowance it shares with `gh`.
#[derive(Default)]
struct Etags(Mutex<HashMap<String, Arc<(String, Reply)>>>);

impl Etags {
    /// The stored tag and the answer it stands for, read together: a 304
    /// replays the answer that went with the tag that was sent, even if the
    /// entry has since been replaced or the cache has started over.
    fn get(&self, path: &str) -> Option<Arc<(String, Reply)>> {
        let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.get(path).cloned()
    }

    fn store(&self, path: &str, tag: String, reply: &Reply) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        // A wholesale clear rather than an LRU: the panel's working set sits
        // far below the cap.
        if map.len() >= ETAG_CAP && !map.contains_key(path) {
            map.clear();
        }
        map.insert(path.to_string(), Arc::new((tag, reply.clone())));
    }
}

impl HttpTransport {
    /// `manual_proxy` is the `http_proxy` setting, as for tty7's other
    /// downloads.
    pub fn new(token: Option<Token>, manual_proxy: Option<&str>) -> HttpTransport {
        let mut builder = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .timeout_connect(Some(CONNECT_TIMEOUT))
            // Renamed/transferred repositories redirect to /repositories/{id}.
            // Dropping auth there makes a private repository look missing.
            // Never forward the token to another host or downgrade HTTPS.
            .redirect_auth_headers(ureq::config::RedirectAuthHeaders::SameHost)
            // 4xx/5xx come back as responses, so their headers (the rate
            // limit's reset time) can be read.
            .http_status_as_error(false)
            .user_agent(concat!("tty7/", env!("CARGO_PKG_VERSION")));
        if let Some(proxy) = crate::daemon::install::proxy::resolve(API, manual_proxy) {
            builder = builder.proxy(Some(proxy));
        }
        HttpTransport {
            agent: builder.build().into(),
            token,
            etags: Etags::default(),
        }
    }
}

impl Transport for HttpTransport {
    fn get(&self, path: &str) -> Result<Reply, ApiError> {
        self.request(path, "application/vnd.github+json")
    }

    fn get_full(&self, path: &str) -> Result<Reply, ApiError> {
        self.request(path, "application/vnd.github.full+json")
    }

    fn authenticated(&self) -> bool {
        self.token.is_some()
    }
}

impl HttpTransport {
    fn request(&self, path: &str, accept: &str) -> Result<Reply, ApiError> {
        let mut request = self
            .agent
            .get(format!("{API}{path}"))
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            request = request.header("Authorization", format!("Bearer {}", token.expose()));
        }
        // Only a plain GET is revalidated: `get_full` carries signed
        // attachment URLs that expire, so its old body must not be replayed.
        let cacheable = accept == "application/vnd.github+json";
        let known = cacheable.then(|| self.etags.get(path)).flatten();
        if let Some(known) = &known {
            request = request.header("If-None-Match", &known.0);
        }
        // ureq's error text names the URL and the transport failure, never
        // the request headers, so it is safe to show.
        let mut response = request
            .call()
            .map_err(|e| ApiError::Network(e.to_string()))?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        if status == 304
            && let Some(known) = known
        {
            return Ok(known.1.clone());
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_vec()
            .map_err(|e| ApiError::Network(e.to_string()))?;
        classify(status, &header, &body)?;
        let reply = Reply {
            has_next: header("link").is_some_and(|l| link_has_next(&l)),
            body,
        };
        if cacheable && let Some(tag) = header("etag") {
            self.etags.store(path, tag, &reply);
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loopback GitHub stand-in: the old repo redirects to its numeric ID,
    /// whose private contents require the same token as the original URL.
    fn redirected_auth(location_host: &str, status: u16) -> (u16, Vec<bool>) {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        use std::time::Instant;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let location = format!("http://{location_host}:{port}/repositories/123/pulls?page=2");
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut authorized = Vec::new();
            while authorized.len() < 2 {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "redirect request never arrived");
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                // Accepted sockets can inherit nonblocking mode on macOS.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let expected = if authorized.is_empty() {
                    "/repos/org/old/pulls?page=2"
                } else {
                    "/repositories/123/pulls?page=2"
                };
                assert!(line.starts_with(&format!("GET {expected} HTTP/1.1")));
                let mut auth = false;
                loop {
                    line.clear();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    auth |= line
                        .trim()
                        .eq_ignore_ascii_case("authorization: Bearer test-token");
                }
                authorized.push(auth);
                let response = if authorized.len() == 1 {
                    format!("HTTP/1.1 {status} Redirect\r\nLocation: {location}\r\n")
                } else {
                    format!("HTTP/1.1 {} Result\r\n", if auth { 200 } else { 404 })
                };
                write!(
                    stream,
                    "{response}Content-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
            }
            authorized
        });
        let transport = HttpTransport::new(None, None);
        let response = transport
            .agent
            .get(format!(
                "http://127.0.0.1:{port}/repos/org/old/pulls?page=2"
            ))
            .config()
            .proxy(None)
            .build()
            .header("Authorization", "Bearer test-token")
            .call()
            .unwrap();
        (response.status().as_u16(), server.join().unwrap())
    }

    #[test]
    fn a_renamed_private_repository_keeps_auth_on_same_host_redirects() {
        for status in [301, 302, 307, 308] {
            assert_eq!(
                redirected_auth("127.0.0.1", status),
                (200, vec![true, true])
            );
        }
    }

    #[test]
    fn a_redirect_to_another_host_does_not_receive_the_token() {
        assert_eq!(redirected_auth("localhost", 301), (404, vec![true, false]));
    }

    fn reply(body: &str) -> Reply {
        Reply {
            body: body.as_bytes().to_vec(),
            has_next: true,
        }
    }

    #[test]
    fn a_stored_answer_is_replayed_by_its_tag() {
        let etags = Etags::default();
        assert!(etags.get("/a").is_none());
        etags.store("/a", "W/\"1\"".into(), &reply("one"));
        let first = etags.get("/a").unwrap();
        etags.store("/a", "W/\"2\"".into(), &reply("two"));
        let (tag, back) = &*etags.get("/a").unwrap();
        assert_eq!(tag, "W/\"2\"");
        assert_eq!((back.body.as_slice(), back.has_next), (&b"two"[..], true));
        // What was read before the replacement still pairs its own tag and
        // answer.
        assert_eq!(
            (first.0.as_str(), first.1.body.as_slice()),
            ("W/\"1\"", &b"one"[..])
        );
    }

    #[test]
    fn the_cache_starts_over_past_its_cap() {
        let etags = Etags::default();
        for i in 0..ETAG_CAP {
            etags.store(&format!("/{i}"), "t".into(), &reply(""));
        }
        etags.store("/0", "t2".into(), &reply(""));
        assert!(etags.get("/1").is_some());
        let held = etags.get("/1").unwrap();
        etags.store("/new", "t".into(), &reply(""));
        assert!(etags.get("/1").is_none());
        assert_eq!(etags.get("/new").map(|e| e.0.clone()).as_deref(), Some("t"));
        // An answer read before the clear can still be replayed for its 304.
        assert_eq!(held.0, "t");
    }
}

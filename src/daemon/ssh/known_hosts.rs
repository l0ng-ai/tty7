//! OpenSSH `known_hosts` reading + trust decisions for the native russh path.
//!
//! Scope (v1, per PRD §3.4 — WS3 hardens this later): read `~/.ssh/known_hosts`,
//! decide trust for **plaintext** hosts, **hashed** hosts (`|1|salt|hash`, HMAC-
//! SHA1), and `@revoked` lines (hard reject). `@cert-authority` lines are skipped
//! (treated as no-match) so a CA entry never produces a false "changed key"
//! warning — the connection just falls through to the unknown-host confirmation.
//!
//! The parser **never rewrites** the file: [`append_trusted`] only appends a
//! single new line, preserving every existing line (comments, hashed entries, CA
//! and revoked markers) byte-for-byte.
//!
//! SHA-1 / HMAC-SHA1 / base64 are hand-rolled here rather than pulled in as
//! dependencies: it keeps host-key matching self-contained and unit-testable
//! against RFC vectors, and the volume (one HMAC per known_hosts line at connect
//! time) is trivial.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use russh::keys::ssh_key::{HashAlg, PublicKey};

/// The outcome of checking a presented host key against `known_hosts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// An entry for this host + key type matches this exact key: trusted.
    Known,
    /// No entry for this host + key type: a first connection (confirm + maybe add).
    Unknown,
    /// An entry for this host + key type exists but the key differs: possible MITM.
    Changed {
        /// SHA256 fingerprint of the stored (old) key, for the warning UI.
        old_fingerprint_sha256: String,
    },
    /// A matching `@revoked` line: reject hard, never offer to trust.
    Revoked,
}

/// The default OpenSSH user known_hosts path, `~/.ssh/known_hosts`.
pub fn default_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".ssh").join("known_hosts"))
}

#[cfg(unix)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

#[cfg(not(unix))]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// The host token OpenSSH keys a `known_hosts` entry under: the bare host for the
/// default port 22, else the bracketed `[host]:port` form.
pub fn host_token(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// Check `host:port`'s presented `key` against the default known_hosts file.
pub fn check(host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
    match default_path() {
        Some(path) => check_in_file(&path, host, port, key),
        None => HostKeyStatus::Unknown,
    }
}

/// Check against a specific file (the testable core of [`check`]). A missing or
/// unreadable file means "no entries" → `Unknown`.
pub fn check_in_file(path: &Path, host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return HostKeyStatus::Unknown,
    };
    check_in_str(&contents, host, port, key)
}

/// Trust decision over the text of a known_hosts file. Split out so the matcher
/// is unit-testable against fixture strings without touching the filesystem.
pub fn check_in_str(contents: &str, host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
    let token = host_token(host, port);
    let our_alg = key.algorithm();

    // First pass — revocation wins outright. A `@revoked` line matching this exact
    // key anywhere in the file rejects it, even if a trusted line for the same
    // host+key appears earlier: a revoked key must never read as trusted.
    for line in contents.lines() {
        let Some(entry) = KnownHostsLine::parse(line) else {
            continue;
        };
        if entry.marker != Some(Marker::Revoked) || !entry.matches_host(&token) {
            continue;
        }
        if let Some(stored) = entry.key() {
            if &stored == key {
                return HostKeyStatus::Revoked;
            }
        }
    }

    // Second pass — normal known/changed resolution (revocation already handled).
    let mut changed: Option<String> = None;
    let mut changed_other_alg: Option<String> = None;
    for line in contents.lines() {
        let Some(entry) = KnownHostsLine::parse(line) else {
            continue;
        };
        if !entry.matches_host(&token) {
            continue;
        }
        match entry.marker {
            // A host-CA line certifies keys signed by this CA; russh doesn't do
            // host-cert verification here, so skip rather than mis-flag it as a
            // changed key (PRD §3.4). Falls through to Unknown → confirm.
            Some(Marker::CertAuthority) => continue,
            // Revocation was resolved in the first pass; ignore here.
            Some(Marker::Revoked) => continue,
            None => {
                let Some(stored) = entry.key() else { continue };
                if stored.algorithm() != our_alg {
                    // The host is known, just via a different key type. If no
                    // same-type line resolves this below, report Changed, like
                    // OpenSSH: a MITM can present a key of an algorithm absent
                    // from the file precisely to downgrade the changed-key
                    // warning to a benign first-connect prompt.
                    if changed_other_alg.is_none() {
                        changed_other_alg = Some(fingerprint_sha256(&stored));
                    }
                    continue;
                }
                if &stored == key {
                    return HostKeyStatus::Known;
                }
                // Same host + key type, different key: a candidate "changed"
                // result — but keep scanning in case a later line matches
                // exactly (a host can list several keys of the same type).
                if changed.is_none() {
                    changed = Some(fingerprint_sha256(&stored));
                }
            }
        }
    }

    match changed.or(changed_other_alg) {
        Some(old_fingerprint_sha256) => HostKeyStatus::Changed {
            old_fingerprint_sha256,
        },
        None => HostKeyStatus::Unknown,
    }
}

/// Append a trust line for `host:port` + `key` to the default known_hosts file,
/// creating `~/.ssh` (mode 0700) and the file (0600) if needed. Never rewrites
/// existing lines — a plain append.
pub fn append_trusted(host: &str, port: u16, key: &PublicKey) -> std::io::Result<()> {
    let path = default_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no home dir for known_hosts")
    })?;
    append_trusted_to(&path, host, port, key)
}

/// The testable core of [`append_trusted`]: append to a specific path.
pub fn append_trusted_to(
    path: &Path,
    host: &str,
    port: u16,
    key: &PublicKey,
) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    let key_openssh = key
        .to_openssh()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    // `to_openssh` yields `<algo> <base64>` (with the key's comment, if any). Take
    // just the algo + base64 so the appended line is a clean host entry.
    let mut parts = key_openssh.split_whitespace();
    let algo = parts.next().unwrap_or_default();
    let b64 = parts.next().unwrap_or_default();
    let token = host_token(host, port);
    let line = format!("{token} {algo} {b64}\n");

    // Make sure we start on a fresh line so we never join onto a file that lacks a
    // trailing newline (which would corrupt the last existing entry).
    let needs_leading_newline = match std::fs::read(path) {
        Ok(bytes) => !bytes.is_empty() && bytes.last() != Some(&b'\n'),
        Err(_) => false,
    };

    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    if needs_leading_newline {
        f.write_all(b"\n")?;
    }
    f.write_all(line.as_bytes())?;
    Ok(())
}

/// SHA256 fingerprint of a public key in OpenSSH `SHA256:base64` form.
pub fn fingerprint_sha256(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

pub use crate::daemon::protocol::{KnownHostEntry, KnownHostId};

/// List every parseable entry in the default `known_hosts` file, in file order.
/// A missing/unreadable file lists as empty.
pub fn list() -> Vec<KnownHostEntry> {
    match default_path() {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(contents) => list_in_str(&contents),
            Err(_) => Vec::new(),
        },
        None => Vec::new(),
    }
}

/// The testable core of [`list`]: parse entries out of file text.
pub fn list_in_str(contents: &str) -> Vec<KnownHostEntry> {
    let mut out = Vec::new();
    for line in contents.lines() {
        let Some(entry) = KnownHostsLine::parse(line) else {
            continue;
        };
        let fingerprint_sha256 = entry
            .key()
            .map(|k| fingerprint_sha256(&k))
            .unwrap_or_else(|| "?".to_string());
        out.push(KnownHostEntry {
            host: entry.hosts.to_string(),
            marker: entry.marker.map(|m| match m {
                Marker::CertAuthority => "@cert-authority".to_string(),
                Marker::Revoked => "@revoked".to_string(),
            }),
            key_type: entry.keytype.to_string(),
            fingerprint_sha256,
            id: KnownHostId {
                host: entry.hosts.to_string(),
                key_type: entry.keytype.to_string(),
                keyblob: entry.keyblob.to_string(),
            },
        });
    }
    out
}

/// Delete the entry matching `id` from the default `known_hosts` file. Every
/// other line — comments, blanks, unrelated entries, and the file's exact line
/// endings — is preserved verbatim. A no-op (Ok) when the file is absent or the
/// entry isn't found.
pub fn delete(id: &KnownHostId) -> std::io::Result<()> {
    let Some(path) = default_path() else {
        return Ok(());
    };
    delete_in_file(&path, id)
}

/// The testable core of [`delete`]: rewrite `path` without the matching entry.
pub fn delete_in_file(path: &Path, id: &KnownHostId) -> std::io::Result<()> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let (new_contents, removed) = delete_in_str(&contents, id);
    if !removed {
        return Ok(());
    }
    // Write a sibling temp then rename over the original: an in-place truncating
    // write would leave a truncated known_hosts behind a crash mid-write. (A
    // concurrent O_APPEND from another connection's TOFU accept can still be
    // lost to the read-modify-write window — data-loss only; a lost entry fails
    // toward re-prompting, never toward trusting.)
    let tmp = path.with_extension("tty7-tmp");
    std::fs::write(&tmp, new_contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Remove the line matching `id` from `contents`, preserving all other lines and
/// their exact terminators byte-for-byte. Returns the new text and whether a line
/// was removed. Only the first matching line is dropped (ids are unique in
/// practice).
pub fn delete_in_str(contents: &str, id: &KnownHostId) -> (String, bool) {
    let mut out = String::with_capacity(contents.len());
    let mut removed = false;
    // Split keeping terminators so we never alter unrelated bytes (CRLF, a
    // missing final newline, blank lines, comment spacing).
    for segment in split_keep_terminators(contents) {
        if !removed {
            // Match against the line's text without its terminator/leading space.
            let line = segment.trim_end_matches(['\n', '\r']);
            if let Some(entry) = KnownHostsLine::parse(line) {
                if entry.hosts == id.host
                    && entry.keytype == id.key_type
                    && entry.keyblob == id.keyblob
                {
                    removed = true;
                    continue;
                }
            }
        }
        out.push_str(segment);
    }
    (out, removed)
}

/// Split text into segments that each still carry their trailing `\n` (and any
/// `\r`), so rejoining is byte-identical to the input. The final segment has no
/// terminator when the file doesn't end in a newline.
fn split_keep_terminators(text: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            segments.push(&text[start..=i]);
            start = i + 1;
        }
    }
    if start < text.len() {
        segments.push(&text[start..]);
    }
    segments
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    CertAuthority,
    Revoked,
}

/// One parsed known_hosts line: an optional marker, the host field (raw), and the
/// key type + base64 blob. Comment/whitespace/blank lines parse to `None`.
struct KnownHostsLine<'a> {
    marker: Option<Marker>,
    hosts: &'a str,
    keytype: &'a str,
    keyblob: &'a str,
}

impl<'a> KnownHostsLine<'a> {
    fn parse(line: &'a str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let mut rest = line;
        let mut marker = None;
        if let Some(after) = rest.strip_prefix('@') {
            let (m, tail) = after.split_once(char::is_whitespace)?;
            marker = Some(match m {
                "cert-authority" => Marker::CertAuthority,
                "revoked" => Marker::Revoked,
                // Unknown marker: skip the whole line rather than misinterpret it.
                _ => return None,
            });
            rest = tail.trim_start();
        }
        let (hosts, tail) = rest.split_once(char::is_whitespace)?;
        let tail = tail.trim_start();
        let (keytype, keyblob) = tail.split_once(char::is_whitespace)?;
        // The blob may carry a trailing comment; keep only the base64 token.
        let keyblob = keyblob.split_whitespace().next().unwrap_or(keyblob);
        Some(Self {
            marker,
            hosts,
            keytype,
            keyblob,
        })
    }

    /// Reconstruct the stored public key (`<type> <base64>`), or `None` if it
    /// doesn't parse (an entry we can't compare against).
    fn key(&self) -> Option<PublicKey> {
        PublicKey::from_openssh(&format!("{} {}", self.keytype, self.keyblob)).ok()
    }

    /// Does this line's host field cover `token`? Handles plaintext host lists
    /// (comma-separated), OpenSSH glob patterns (`*` / `?`), `!` negations, and
    /// the `|1|salt|hash` hashed form.
    ///
    /// OpenSSH semantics: the field is a comma-separated pattern list; a leading
    /// `!` negates. If *any* negated pattern matches the host, the line does not
    /// apply at all (even when a positive pattern also matches); otherwise the
    /// line applies iff at least one positive pattern matches. Hostname matching
    /// is case-insensitive. A hashed entry carries exactly one host and never
    /// globs.
    fn matches_host(&self, token: &str) -> bool {
        let mut matched = false;
        for pattern in self.hosts.split(',') {
            let pattern = pattern.trim();
            if pattern.is_empty() {
                continue;
            }
            let (negated, pat) = match pattern.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, pattern),
            };
            let hit = if let Some(hashed) = pat.strip_prefix("|1|") {
                hashed_host_matches(hashed, token)
            } else {
                host_glob_matches(pat, token)
            };
            if hit {
                if negated {
                    // A negated match disqualifies the whole line, regardless of
                    // any positive match elsewhere on it.
                    return false;
                }
                matched = true;
            }
        }
        matched
    }
}

/// Match a single OpenSSH host pattern (which may contain `*` / `?` wildcards)
/// against a host token, case-insensitively. `*` matches any run of characters
/// (including empty), `?` matches exactly one character — OpenSSH's `match_pattern`
/// glob, not a regex. Wildcard-free patterns are a plain case-insensitive compare.
fn host_glob_matches(pattern: &str, token: &str) -> bool {
    if !pattern.as_bytes().iter().any(|&b| b == b'*' || b == b'?') {
        return pattern.eq_ignore_ascii_case(token);
    }
    glob_match(pattern.as_bytes(), token.as_bytes())
}

/// Iterative backtracking glob for `*`/`?`, ASCII-case-insensitive (host names
/// fold case in OpenSSH). Linear-ish with a single backtrack pointer for `*`.
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_t = 0usize;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p].eq_ignore_ascii_case(&text[t])) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            star_t = t;
            p += 1;
        } else if let Some(sp) = star {
            // Backtrack: let the last `*` swallow one more character.
            p = sp + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

/// Check a `|1|salt|hash` hashed-host field (base64 salt + base64 HMAC-SHA1)
/// against a host token: OpenSSH stores `HMAC-SHA1(key=salt, msg=token)`.
fn hashed_host_matches(hashed: &str, token: &str) -> bool {
    let Some((salt_b64, hash_b64)) = hashed.split_once('|') else {
        return false;
    };
    let (Some(salt), Some(hash)) = (base64_decode(salt_b64), base64_decode(hash_b64)) else {
        return false;
    };
    hmac_sha1(&salt, token.as_bytes()).as_slice() == hash.as_slice()
}

// --- SHA-1 (FIPS 180-1) --------------------------------------------------

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let ml = (data.len() as u64).wrapping_mul(8);

    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&ml.to_be_bytes());

    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..20].copy_from_slice(&sha1(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Vec::with_capacity(BLOCK + msg.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(msg);
    let inner_hash = sha1(&inner);
    let mut outer = Vec::with_capacity(BLOCK + 20);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_hash);
    sha1(&outer)
}

// --- standard base64 decode (for the hashed-host salt/hash fields) -------

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let s = s.trim();
    let bytes: &[u8] = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc = 0u32;
    let mut nbits = 0u32;
    for &c in bytes {
        if c == b'=' {
            break;
        }
        let v = val(c)? as u32;
        acc = (acc << 6) | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_known_vectors() {
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn hmac_sha1_matches_rfc2202_vector() {
        // RFC 2202 test case 1: key = 0x0b*20, data = "Hi There".
        let key = [0x0bu8; 20];
        assert_eq!(
            hex(&hmac_sha1(&key, b"Hi There")),
            "b617318655057264e28bc0b6fb378c8ef146be00"
        );
    }

    #[test]
    fn base64_decode_round_trips_openssh_salt() {
        // "hello" -> aGVsbG8=
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("").unwrap(), b"");
    }

    #[test]
    fn host_token_brackets_only_non_default_ports() {
        assert_eq!(host_token("example.com", 22), "example.com");
        assert_eq!(host_token("example.com", 2222), "[example.com]:2222");
    }

    // A fixed ed25519 public key and a second, different one, both valid OpenSSH.
    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPXO/kBX63iuiTczoR6uNdl3wAFK7tGWz70jCKkKlw5r";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEUVe8YNCi/DX61b+J6+ou0f0kCiuYE2/+p0qCIU6fN4";

    fn key(s: &str) -> PublicKey {
        PublicKey::from_openssh(s).unwrap()
    }

    #[test]
    fn plaintext_known_and_unknown_and_changed() {
        let ka = key(KEY_A);
        let kb = key(KEY_B);
        let file = format!("example.com {KEY_A}\n");

        assert_eq!(
            check_in_str(&file, "example.com", 22, &ka),
            HostKeyStatus::Known
        );
        assert_eq!(
            check_in_str(&file, "other.com", 22, &ka),
            HostKeyStatus::Unknown
        );
        match check_in_str(&file, "example.com", 22, &kb) {
            HostKeyStatus::Changed { .. } => {}
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn different_key_type_for_a_known_host_reports_changed_not_unknown() {
        // The host is known via ed25519 only; a presented ECDSA key must raise
        // the changed-key warning, not the benign first-connect prompt — a MITM
        // can pick an algorithm absent from the file to get the softer dialog.
        const KEY_ECDSA: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBCdv5xfuuCGyVbYZSTqcFjQWE7YtIsx8fqlXF1+v728j1RUnELLVrmgsC6gZ0zObXAzJ39JEynaQv9tf/v16V58=";
        let file = format!("example.com {KEY_A}\n");
        match check_in_str(&file, "example.com", 22, &key(KEY_ECDSA)) {
            HostKeyStatus::Changed { .. } => {}
            other => panic!("expected Changed, got {other:?}"),
        }
        // A same-type exact match elsewhere still wins over the mismatch.
        let file = format!("example.com {KEY_A}\nexample.com {KEY_ECDSA}\n");
        assert_eq!(
            check_in_str(&file, "example.com", 22, &key(KEY_ECDSA)),
            HostKeyStatus::Known
        );
    }

    #[test]
    fn non_default_port_uses_bracket_syntax() {
        let ka = key(KEY_A);
        let file = format!("[example.com]:2222 {KEY_A}\n");
        assert_eq!(
            check_in_str(&file, "example.com", 2222, &ka),
            HostKeyStatus::Known
        );
        // Same host on the default port is a different token → unknown.
        assert_eq!(
            check_in_str(&file, "example.com", 22, &ka),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn revoked_line_hard_rejects_the_matching_key() {
        let ka = key(KEY_A);
        let file = format!("@revoked example.com {KEY_A}\n");
        assert_eq!(
            check_in_str(&file, "example.com", 22, &ka),
            HostKeyStatus::Revoked
        );
    }

    #[test]
    fn revoked_takes_precedence_over_an_earlier_trusted_line() {
        // A trusted line for the exact key appears FIRST, then a `@revoked` line
        // for the same host+key. Revocation must win — the key is never trusted.
        let ka = key(KEY_A);
        let file = format!("example.com {KEY_A}\n@revoked example.com {KEY_A}\n");
        assert_eq!(
            check_in_str(&file, "example.com", 22, &ka),
            HostKeyStatus::Revoked
        );
    }

    #[test]
    fn cert_authority_line_is_skipped_not_flagged_as_changed() {
        let ka = key(KEY_A);
        // A CA line whose key differs from the presented key must NOT read as
        // "changed" — it should fall through to Unknown.
        let file = format!("@cert-authority example.com {KEY_B}\n");
        assert_eq!(
            check_in_str(&file, "example.com", 22, &ka),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn comment_lines_are_ignored() {
        let ka = key(KEY_A);
        let file = format!("# a comment\n\nexample.com {KEY_A}\n");
        assert_eq!(
            check_in_str(&file, "example.com", 22, &ka),
            HostKeyStatus::Known
        );
    }

    #[test]
    fn hashed_host_matches_via_hmac_sha1() {
        // Build a hashed entry the way OpenSSH would: salt is arbitrary bytes,
        // hash = HMAC-SHA1(salt, token). Encode both with our base64.
        let token = "example.com";
        let salt = b"0123456789abcdef1234"; // 20 bytes
        let hash = hmac_sha1(salt, token.as_bytes());
        let line = format!("|1|{}|{} {KEY_A}\n", b64(salt), b64(&hash),);
        let ka = key(KEY_A);
        assert_eq!(
            check_in_str(&line, "example.com", 22, &ka),
            HostKeyStatus::Known
        );
        assert_eq!(
            check_in_str(&line, "nope.com", 22, &ka),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn append_preserves_existing_lines_and_adds_one() {
        let dir = std::env::temp_dir().join(format!("tty7-kh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known_hosts");
        // Pre-seed a file WITHOUT a trailing newline to prove we don't corrupt it.
        std::fs::write(&path, format!("first.com {KEY_B}")).unwrap();

        let ka = key(KEY_A);
        append_trusted_to(&path, "example.com", 2222, &ka).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        // The original line is intact...
        assert!(contents.contains(&format!("first.com {KEY_B}")));
        // ...and the new host is trusted at its bracketed token.
        assert_eq!(
            check_in_str(&contents, "example.com", 2222, &ka),
            HostKeyStatus::Known
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wildcard_star_matches_hostname_glob() {
        let ka = key(KEY_A);
        let file = format!("*.example.com {KEY_A}\n");
        assert_eq!(
            check_in_str(&file, "web1.example.com", 22, &ka),
            HostKeyStatus::Known
        );
        assert_eq!(
            check_in_str(&file, "a.b.example.com", 22, &ka),
            HostKeyStatus::Known
        );
        // `*` does not cross into a different domain suffix.
        assert_eq!(
            check_in_str(&file, "web1.example.org", 22, &ka),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn wildcard_question_matches_single_char() {
        let ka = key(KEY_A);
        let file = format!("host? {KEY_A}\n");
        assert_eq!(check_in_str(&file, "host1", 22, &ka), HostKeyStatus::Known);
        // `?` is exactly one char — "host" (zero) and "host12" (two) don't match.
        assert_eq!(check_in_str(&file, "host", 22, &ka), HostKeyStatus::Unknown);
        assert_eq!(
            check_in_str(&file, "host12", 22, &ka),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn negated_pattern_disqualifies_the_line() {
        let ka = key(KEY_A);
        // Matches the whole domain except the negated host — even though the
        // positive `*.example.com` would otherwise cover it.
        let file = format!("*.example.com,!secret.example.com {KEY_A}\n");
        assert_eq!(
            check_in_str(&file, "web.example.com", 22, &ka),
            HostKeyStatus::Known
        );
        assert_eq!(
            check_in_str(&file, "secret.example.com", 22, &ka),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn host_match_is_case_insensitive() {
        let ka = key(KEY_A);
        let file = format!("Example.COM {KEY_A}\n");
        assert_eq!(
            check_in_str(&file, "example.com", 22, &ka),
            HostKeyStatus::Known
        );
    }

    #[test]
    fn comma_list_of_hosts_matches_any_member() {
        let ka = key(KEY_A);
        let file = format!("alpha.example.com,10.0.0.5 {KEY_A}\n");
        assert_eq!(
            check_in_str(&file, "10.0.0.5", 22, &ka),
            HostKeyStatus::Known
        );
    }

    #[test]
    fn list_reports_host_type_and_fingerprint() {
        let file = format!("example.com {KEY_A}\n@revoked bad.example.com {KEY_B}\n# comment\n");
        let entries = list_in_str(&file);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].host, "example.com");
        assert_eq!(entries[0].marker, None);
        assert_eq!(entries[0].key_type, "ssh-ed25519");
        assert!(entries[0].fingerprint_sha256.starts_with("SHA256:"));
        assert_eq!(entries[1].marker.as_deref(), Some("@revoked"));
    }

    #[test]
    fn delete_removes_only_the_matching_entry_byte_for_byte() {
        // A file with CRLF, a comment, a blank line, and no trailing newline on
        // the last entry — deletion must preserve every unrelated byte.
        let contents =
            format!("# my hosts\r\nkeep.example.com {KEY_B}\n\ndrop.example.com {KEY_A}");
        let entries = list_in_str(&contents);
        let target = entries
            .iter()
            .find(|e| e.host == "drop.example.com")
            .unwrap()
            .id
            .clone();
        let (after, removed) = delete_in_str(&contents, &target);
        assert!(removed);
        // Everything except the dropped line is preserved exactly.
        let expected = format!("# my hosts\r\nkeep.example.com {KEY_B}\n\n");
        assert_eq!(after, expected);
        // And the dropped host is now unknown.
        let ka = key(KEY_A);
        assert_eq!(
            check_in_str(&after, "drop.example.com", 22, &ka),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn delete_of_absent_entry_is_a_noop() {
        let contents = format!("keep.example.com {KEY_B}\n");
        let missing = KnownHostId {
            host: "nope.example.com".into(),
            key_type: "ssh-ed25519".into(),
            keyblob: "AAAA".into(),
        };
        let (after, removed) = delete_in_str(&contents, &missing);
        assert!(!removed);
        assert_eq!(after, contents);
    }

    #[test]
    fn glob_matcher_edge_cases() {
        assert!(glob_match(b"*", b""));
        assert!(glob_match(b"*", b"anything"));
        assert!(glob_match(b"a*c", b"ac"));
        assert!(glob_match(b"a*c", b"abbbc"));
        assert!(!glob_match(b"a*c", b"abbb"));
        assert!(glob_match(b"a?c", b"abc"));
        assert!(!glob_match(b"a?c", b"ac"));
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    // A minimal standard-base64 encoder for the test fixtures only.
    fn b64(data: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
            out.push(T[(n >> 18 & 63) as usize] as char);
            out.push(T[(n >> 12 & 63) as usize] as char);
            if chunk.len() > 1 {
                out.push(T[(n >> 6 & 63) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(T[(n & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }
}

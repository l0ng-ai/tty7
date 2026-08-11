use std::io::Write as _;
use std::path::{Path, PathBuf};

use russh::keys::ssh_key::{Algorithm, HashAlg, PublicKey};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyStatus {
    Known,
    Unknown,
    Changed {
        old_fingerprint_sha256: String,
    },
    /// The host is on file, but only under some *other* key algorithm.
    ///
    /// A server that grows an ed25519 key beside the ssh-rsa one it has always
    /// had has not been tampered with, and OpenSSH says so: it treats a key of
    /// an algorithm the host has no entry for as simply unknown, and saves the
    /// man-in-the-middle warning for a key that contradicts one on file. The
    /// algorithm travels with the status so the confirmation can name what the
    /// host was known by.
    ChangedAlgorithm {
        known_fingerprint_sha256: String,
        known_algorithm: String,
    },
    Revoked,
}

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

pub fn host_token(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

pub fn check(host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
    match default_path() {
        Some(path) => check_in_file(&path, host, port, key),
        None => HostKeyStatus::Unknown,
    }
}

pub fn check_in_file(path: &Path, host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return HostKeyStatus::Unknown,
    };
    check_in_str(&contents, host, port, key)
}

pub fn check_in_str(contents: &str, host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
    let token = host_token(host, port);
    let our_alg = key.algorithm();

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

    let mut changed: Option<String> = None;
    let mut changed_other_alg: Option<(String, String)> = None;
    for line in contents.lines() {
        let Some(entry) = KnownHostsLine::parse(line) else {
            continue;
        };
        if !entry.matches_host(&token) {
            continue;
        }
        match entry.marker {
            Some(Marker::CertAuthority) => continue,
            Some(Marker::Revoked) => continue,
            None => {
                let Some(stored) = entry.key() else { continue };
                let stored_alg = stored.algorithm();
                if stored_alg != our_alg {
                    if changed_other_alg.is_none() {
                        changed_other_alg =
                            Some((fingerprint_sha256(&stored), stored_alg.as_str().to_string()));
                    }
                    continue;
                }
                if &stored == key {
                    return HostKeyStatus::Known;
                }
                if changed.is_none() {
                    changed = Some(fingerprint_sha256(&stored));
                }
            }
        }
    }

    // A same-algorithm contradiction outranks everything else on file: the host
    // has an entry that says this key is wrong, and no amount of other-algorithm
    // company softens that.
    if let Some(old_fingerprint_sha256) = changed {
        return HostKeyStatus::Changed {
            old_fingerprint_sha256,
        };
    }
    match changed_other_alg {
        Some((known_fingerprint_sha256, known_algorithm)) => HostKeyStatus::ChangedAlgorithm {
            known_fingerprint_sha256,
            known_algorithm,
        },
        None => HostKeyStatus::Unknown,
    }
}

/// The host-key algorithms this host already has entries for, in file order.
///
/// Negotiation reads this so the algorithms already on file are offered first —
/// see `connect::build_preferred`. Markers are skipped: a `@cert-authority` line
/// names the authority's key rather than the host's, and a `@revoked` one names
/// a key that would be refused, so neither predicts what the host will present.
pub fn known_algorithms(host: &str, port: u16) -> Vec<Algorithm> {
    match default_path() {
        Some(path) => known_algorithms_in_file(&path, host, port),
        None => Vec::new(),
    }
}

pub fn known_algorithms_in_file(path: &Path, host: &str, port: u16) -> Vec<Algorithm> {
    match std::fs::read_to_string(path) {
        Ok(contents) => known_algorithms_in_str(&contents, host, port),
        Err(_) => Vec::new(),
    }
}

pub fn known_algorithms_in_str(contents: &str, host: &str, port: u16) -> Vec<Algorithm> {
    let token = host_token(host, port);
    let mut out: Vec<Algorithm> = Vec::new();
    for line in contents.lines() {
        let Some(entry) = KnownHostsLine::parse(line) else {
            continue;
        };
        if entry.marker.is_some() || !entry.matches_host(&token) {
            continue;
        }
        let Some(stored) = entry.key() else { continue };
        let alg = stored.algorithm();
        if !out.contains(&alg) {
            out.push(alg);
        }
    }
    out
}

pub fn append_trusted(host: &str, port: u16, key: &PublicKey) -> std::io::Result<()> {
    let path = default_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no home dir for known_hosts")
    })?;
    append_trusted_to(&path, host, port, key)
}

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
    let mut parts = key_openssh.split_whitespace();
    let algo = parts.next().unwrap_or_default();
    let b64 = parts.next().unwrap_or_default();
    let token = host_token(host, port);
    let line = format!("{token} {algo} {b64}\n");

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

pub fn fingerprint_sha256(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

pub use crate::daemon::protocol::{KnownHostEntry, KnownHostId};

pub fn list() -> Vec<KnownHostEntry> {
    match default_path() {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(contents) => list_in_str(&contents),
            Err(_) => Vec::new(),
        },
        None => Vec::new(),
    }
}

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

pub fn delete(id: &KnownHostId) -> std::io::Result<()> {
    let Some(path) = default_path() else {
        return Ok(());
    };
    delete_in_file(&path, id)
}

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

pub fn delete_in_str(contents: &str, id: &KnownHostId) -> (String, bool) {
    let mut out = String::with_capacity(contents.len());
    let mut removed = false;
    for segment in split_keep_terminators(contents) {
        if !removed {
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

/// Drop the lines `key` replaces before it is appended for `host`.
///
/// Appending on its own is not enough when the user overrides a *changed* key:
/// `check_in_str` answers `Known` on any same-algorithm match, so the line the
/// new key contradicts — the one the user just decided was wrong, and which in
/// the case the warning exists for is an attacker's — would go on being trusted
/// forever, with no warning ever shown again. OpenSSH rewrites the file for the
/// same reason.
///
/// A no-op for a host that was merely unknown, which by definition has no
/// same-algorithm line to drop.
pub fn forget_superseded(host: &str, port: u16, key: &PublicKey) -> std::io::Result<()> {
    match default_path() {
        Some(path) => forget_superseded_in_file(&path, host, port, key),
        None => Ok(()),
    }
}

pub fn forget_superseded_in_file(
    path: &Path,
    host: &str,
    port: u16,
    key: &PublicKey,
) -> std::io::Result<()> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for id in superseded_ids_in_str(&contents, host, port, key) {
        delete_in_file(path, &id)?;
    }
    Ok(())
}

pub fn superseded_ids_in_str(
    contents: &str,
    host: &str,
    port: u16,
    key: &PublicKey,
) -> Vec<KnownHostId> {
    let token = host_token(host, port);
    let our_alg = key.algorithm();
    let mut out = Vec::new();
    for line in contents.lines() {
        let Some(entry) = KnownHostsLine::parse(line) else {
            continue;
        };
        // Only a plain line for this one host: a `@revoked` line is a standing
        // refusal that an override of a different key must not lift, and a glob
        // or a comma list also speaks for hosts nobody is connecting to — the
        // rest of `*.example.com` should not be forgotten because one machine
        // behind it rotated its key.
        if entry.marker.is_some() || !entry.names_only_host(&token) {
            continue;
        }
        let Some(stored) = entry.key() else { continue };
        if stored.algorithm() != our_alg || &stored == key {
            continue;
        }
        out.push(KnownHostId {
            host: entry.hosts.to_string(),
            key_type: entry.keytype.to_string(),
            keyblob: entry.keyblob.to_string(),
        });
    }
    out
}

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
                _ => return None,
            });
            rest = tail.trim_start();
        }
        let (hosts, tail) = rest.split_once(char::is_whitespace)?;
        let tail = tail.trim_start();
        let (keytype, keyblob) = tail.split_once(char::is_whitespace)?;
        let keyblob = keyblob.split_whitespace().next().unwrap_or(keyblob);
        Some(Self {
            marker,
            hosts,
            keytype,
            keyblob,
        })
    }

    fn key(&self) -> Option<PublicKey> {
        PublicKey::from_openssh(&format!("{} {}", self.keytype, self.keyblob)).ok()
    }

    /// Whether this line's host field names `token` and nothing else.
    ///
    /// `matches_host` is the right question for "does this entry apply here";
    /// this is the stricter one to ask before *deleting* a line, because a glob
    /// or a comma list carries other hosts with it. A hashed pattern names
    /// exactly one token, so it passes.
    fn names_only_host(&self, token: &str) -> bool {
        if self.hosts.contains(',') {
            return false;
        }
        let pattern = self.hosts.trim();
        match pattern.strip_prefix("|1|") {
            Some(hashed) => hashed_host_matches(hashed, token),
            None => {
                !pattern
                    .as_bytes()
                    .iter()
                    .any(|&b| b == b'*' || b == b'?' || b == b'!')
                    && pattern.eq_ignore_ascii_case(token)
            }
        }
    }

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
                    return false;
                }
                matched = true;
            }
        }
        matched
    }
}

fn host_glob_matches(pattern: &str, token: &str) -> bool {
    if !pattern.as_bytes().iter().any(|&b| b == b'*' || b == b'?') {
        return pattern.eq_ignore_ascii_case(token);
    }
    glob_match(pattern.as_bytes(), token.as_bytes())
}

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

fn hashed_host_matches(hashed: &str, token: &str) -> bool {
    let Some((salt_b64, hash_b64)) = hashed.split_once('|') else {
        return false;
    };
    let (Some(salt), Some(hash)) = (base64_decode(salt_b64), base64_decode(hash_b64)) else {
        return false;
    };
    hmac_sha1(&salt, token.as_bytes()).as_slice() == hash.as_slice()
}

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
        let key = [0x0bu8; 20];
        assert_eq!(
            hex(&hmac_sha1(&key, b"Hi There")),
            "b617318655057264e28bc0b6fb378c8ef146be00"
        );
    }

    #[test]
    fn base64_decode_round_trips_openssh_salt() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("").unwrap(), b"");
    }

    #[test]
    fn host_token_brackets_only_non_default_ports() {
        assert_eq!(host_token("example.com", 22), "example.com");
        assert_eq!(host_token("example.com", 2222), "[example.com]:2222");
    }

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

    const KEY_ECDSA: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBCdv5xfuuCGyVbYZSTqcFjQWE7YtIsx8fqlXF1+v728j1RUnELLVrmgsC6gZ0zObXAzJ39JEynaQv9tf/v16V58=";

    /// Not `Changed`, which is the man-in-the-middle alarm: a host that has
    /// grown a second key type has not contradicted anything on file, and
    /// OpenSSH asks the same mild question it asks about any unseen key.
    #[test]
    fn a_key_of_a_new_algorithm_is_flagged_as_the_algorithm_being_new() {
        let file = format!("example.com {KEY_A}\n");
        match check_in_str(&file, "example.com", 22, &key(KEY_ECDSA)) {
            HostKeyStatus::ChangedAlgorithm {
                known_algorithm, ..
            } => assert_eq!(known_algorithm, "ssh-ed25519"),
            other => panic!("expected ChangedAlgorithm, got {other:?}"),
        }
        let file = format!("example.com {KEY_A}\nexample.com {KEY_ECDSA}\n");
        assert_eq!(
            check_in_str(&file, "example.com", 22, &key(KEY_ECDSA)),
            HostKeyStatus::Known
        );
    }

    #[test]
    fn a_different_key_of_the_same_algorithm_is_still_a_change() {
        let file = format!("example.com {KEY_A}\n");
        match check_in_str(&file, "example.com", 22, &key(KEY_B)) {
            HostKeyStatus::Changed {
                old_fingerprint_sha256,
            } => assert_eq!(old_fingerprint_sha256, fingerprint_sha256(&key(KEY_A))),
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    /// The downgrade this split has to not open: an attacker offering a key of
    /// an algorithm the host also has an entry for gets the full alarm, however
    /// many other-algorithm lines sit alongside it.
    #[test]
    fn a_same_algorithm_mismatch_outranks_an_other_algorithm_entry() {
        let file = format!("example.com {KEY_ECDSA}\nexample.com {KEY_A}\n");
        match check_in_str(&file, "example.com", 22, &key(KEY_B)) {
            HostKeyStatus::Changed { .. } => {}
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn known_algorithms_lists_each_algorithm_once_in_file_order() {
        let file = format!(
            "example.com {KEY_ECDSA}\nexample.com {KEY_A}\nexample.com {KEY_B}\nother.com {KEY_A}\n"
        );
        let algs = known_algorithms_in_str(&file, "example.com", 22);
        assert_eq!(
            algs.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
            vec!["ecdsa-sha2-nistp256", "ssh-ed25519"]
        );
        assert!(known_algorithms_in_str(&file, "nowhere.com", 22).is_empty());
    }

    #[test]
    fn known_algorithms_ignores_revoked_and_cert_authority_lines() {
        let file = format!("@revoked example.com {KEY_A}\n@cert-authority example.com {KEY_B}\n");
        assert!(known_algorithms_in_str(&file, "example.com", 22).is_empty());
    }

    #[test]
    fn non_default_port_uses_bracket_syntax() {
        let ka = key(KEY_A);
        let file = format!("[example.com]:2222 {KEY_A}\n");
        assert_eq!(
            check_in_str(&file, "example.com", 2222, &ka),
            HostKeyStatus::Known
        );
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
        let token = "example.com";
        let salt = b"0123456789abcdef1234";
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
        std::fs::write(&path, format!("first.com {KEY_B}")).unwrap();

        let ka = key(KEY_A);
        append_trusted_to(&path, "example.com", 2222, &ka).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains(&format!("first.com {KEY_B}")));
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
        assert_eq!(check_in_str(&file, "host", 22, &ka), HostKeyStatus::Unknown);
        assert_eq!(
            check_in_str(&file, "host12", 22, &ka),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn negated_pattern_disqualifies_the_line() {
        let ka = key(KEY_A);
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
        let expected = format!("# my hosts\r\nkeep.example.com {KEY_B}\n\n");
        assert_eq!(after, expected);
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

    /// Overriding a changed key used to append and leave the old line in
    /// place, and `check_in_str` answers `Known` on any same-algorithm match —
    /// so the key the user had just refused stayed trusted, silently, forever.
    #[test]
    fn overriding_a_changed_key_stops_trusting_the_one_it_replaces() {
        let dir = std::env::temp_dir().join(format!("tty7-kh-supersede-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known_hosts");
        std::fs::write(&path, format!("example.com {KEY_A}\n")).unwrap();

        let kb = key(KEY_B);
        forget_superseded_in_file(&path, "example.com", 22, &kb).unwrap();
        append_trusted_to(&path, "example.com", 22, &kb).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            !contents.contains(KEY_A.split_whitespace().nth(1).unwrap()),
            "the superseded key is still on file: {contents}"
        );
        assert_eq!(
            check_in_str(&contents, "example.com", 22, &kb),
            HostKeyStatus::Known
        );
        assert!(matches!(
            check_in_str(&contents, "example.com", 22, &key(KEY_A)),
            HostKeyStatus::Changed { .. }
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nothing_is_superseded_by_a_key_of_another_algorithm_or_another_host() {
        let file = format!("example.com {KEY_A}\nother.com {KEY_B}\n");
        assert!(superseded_ids_in_str(&file, "example.com", 22, &key(KEY_ECDSA)).is_empty());
        assert!(superseded_ids_in_str(&file, "example.com", 22, &key(KEY_A)).is_empty());
        assert_eq!(
            superseded_ids_in_str(&file, "example.com", 22, &key(KEY_B)).len(),
            1
        );
    }

    /// A wildcard or comma-list line speaks for hosts nobody is connecting to,
    /// and a `@revoked` line is a standing refusal — one host's override must
    /// not quietly drop either.
    #[test]
    fn superseding_never_touches_a_shared_or_revoked_line() {
        let file = format!(
            "*.example.com {KEY_A}\nweb.example.com,db.example.com {KEY_A}\n@revoked web.example.com {KEY_A}\n"
        );
        assert!(superseded_ids_in_str(&file, "web.example.com", 22, &key(KEY_B)).is_empty());
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

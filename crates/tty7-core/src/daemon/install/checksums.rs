//! `checksums.txt` parsing and asset verification.
//!
//! The release publishes one GNU coreutils `sha256sum`-format manifest covering
//! every asset. HTTPS to github.com is the trust anchor — the manifest is not
//! separately signed — so this module's whole job is to make sure the bytes we
//! are about to write onto someone else's machine are the bytes that release
//! actually published.
//!
//! Pure: no network, no filesystem. The bytes come in as a slice.

use std::fmt;

use sha2::{Digest as _, Sha256};

/// A parsed sha256 digest: 32 raw bytes, compared by value rather than by
/// string so casing and whitespace can never make a comparison accidentally
/// succeed.
pub type Digest = [u8; 32];

/// Why an asset failed verification. Every variant aborts the install; none of
/// them is retried, and there is no unverified fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChecksumError {
    /// The manifest has no line for this asset. Either the release is
    /// incomplete or we derived an asset name the release does not carry — both
    /// are "stop", never "install it anyway".
    Missing { asset: String },
    /// The asset's line exists but is not `<64 hex>  <name>`.
    Malformed { asset: String, line: String },
    /// The manifest and the downloaded bytes disagree. The one variant that can
    /// mean something is actively wrong (a corrupted download, a proxy that
    /// rewrote the body, a compromised mirror), so it reports both digests.
    Mismatch {
        asset: String,
        expected: String,
        actual: String,
    },
}

impl fmt::Display for ChecksumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { asset } => write!(
                f,
                "checksums.txt has no entry for {asset}; refusing to install an unverified binary"
            ),
            Self::Malformed { asset, line } => write!(
                f,
                "checksums.txt entry for {asset} is malformed ({line:?}); \
                 refusing to install an unverified binary"
            ),
            Self::Mismatch {
                asset,
                expected,
                actual,
            } => write!(
                f,
                "{asset} failed sha256 verification: release says {expected}, downloaded bytes are \
                 {actual}. Install aborted; nothing was written to the remote machine"
            ),
        }
    }
}

impl std::error::Error for ChecksumError {}

/// The sha256 of some bytes.
pub fn sha256(bytes: &[u8]) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Render a digest as lowercase hex, for messages.
pub fn hex(digest: &Digest) -> String {
    use fmt::Write as _;
    digest.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Parse 64 hex characters into a digest. Case-insensitive; any
/// other length or a non-hex character is a parse failure.
fn parse_hex(s: &str) -> Option<Digest> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// The digest `manifest` records for `asset`.
///
/// **The filename field is matched whole, never by substring.**
/// `tty7-server-x86_64-unknown-linux-musl` happens not to be a substring of any
/// other asset today, but that is an accident of the current release contents,
/// not a property anyone maintains — and a substring match that drifted would
/// silently verify one binary's bytes against another's digest.
///
/// The coreutils format is `<digest><two spaces><name>`; the second space is `*`
/// in binary mode (`<digest> *<name>`), which some tools emit, so a leading `*`
/// on the name is stripped. Blank lines and `#` comments are skipped.
pub fn expected_digest(manifest: &str, asset: &str) -> Result<Digest, ChecksumError> {
    for line in manifest.lines() {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        // Split once on whitespace: everything before is the digest field,
        // everything after (minus the binary-mode marker) is the filename field.
        let Some((digest_field, name_field)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let name = name_field.trim_start().trim_start_matches('*');
        if name != asset {
            continue;
        }
        return parse_hex(digest_field).ok_or_else(|| ChecksumError::Malformed {
            asset: asset.to_string(),
            line: line.to_string(),
        });
    }
    Err(ChecksumError::Missing {
        asset: asset.to_string(),
    })
}

/// Verify downloaded `bytes` against the manifest. `Ok(())` is the only outcome
/// that permits an install.
pub fn verify(manifest: &str, asset: &str, bytes: &[u8]) -> Result<(), ChecksumError> {
    let expected = expected_digest(manifest, asset)?;
    let actual = sha256(bytes);
    if expected == actual {
        return Ok(());
    }
    Err(ChecksumError::Mismatch {
        asset: asset.to_string(),
        expected: hex(&expected),
        actual: hex(&actual),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::install::asset::{ASSET_AARCH64, ASSET_X86_64};

    /// A manifest shaped exactly like the release workflow's, with digests that
    /// really are the sha256 of the payloads below.
    fn manifest_for(payloads: &[(&str, &[u8])]) -> String {
        payloads
            .iter()
            .map(|(name, bytes)| format!("{}  {name}\n", hex(&sha256(bytes))))
            .collect()
    }

    #[test]
    fn matching_bytes_verify() {
        let bytes = b"\x7fELF pretend this is a server".as_slice();
        let manifest = manifest_for(&[(ASSET_X86_64, bytes), (ASSET_AARCH64, b"other")]);
        verify(&manifest, ASSET_X86_64, bytes).expect("the published bytes must verify");
    }

    /// Uppercase hex in the manifest is still the same digest.
    #[test]
    fn digest_comparison_is_case_insensitive() {
        let bytes = b"payload".as_slice();
        // Only the digest is uppercased — the filename field stays exact-match.
        let manifest = format!("{}  {ASSET_X86_64}\n", hex(&sha256(bytes)).to_uppercase());
        verify(&manifest, ASSET_X86_64, bytes).expect("case must not matter");
    }

    /// **The failure path.** Bytes that do not match must abort with
    /// both digests reported — not retry, not install anyway.
    #[test]
    fn mismatched_bytes_abort_with_both_digests() {
        let published = b"the real server binary".as_slice();
        let tampered = b"the real server binary!".as_slice();
        let manifest = manifest_for(&[(ASSET_X86_64, published)]);

        let err = verify(&manifest, ASSET_X86_64, tampered).unwrap_err();
        match err {
            ChecksumError::Mismatch {
                ref asset,
                ref expected,
                ref actual,
            } => {
                assert_eq!(asset, ASSET_X86_64);
                assert_eq!(*expected, hex(&sha256(published)));
                assert_eq!(*actual, hex(&sha256(tampered)));
                assert_ne!(expected, actual);
            }
            other => panic!("a mismatch must report both digests, got {other:?}"),
        }
        // The message has to be actionable on its own — it is what the user sees.
        let msg = err.to_string();
        assert!(msg.contains("sha256"), "{msg}");
        assert!(msg.contains("aborted"), "{msg}");
    }

    /// A one-bit difference is caught. Cheap to assert, and the property the
    /// whole verification exists for.
    #[test]
    fn a_single_flipped_bit_fails() {
        let mut payload = vec![0u8; 4096];
        payload[1234] = 0x5a;
        let manifest = manifest_for(&[(ASSET_X86_64, &payload)]);
        let mut flipped = payload.clone();
        flipped[1234] ^= 0x01;
        assert!(matches!(
            verify(&manifest, ASSET_X86_64, &flipped),
            Err(ChecksumError::Mismatch { .. })
        ));
    }

    /// No line for our asset → abort. This is the "release is missing the
    /// architecture we need" case, and installing the other architecture (or
    /// nothing-checked) would both be worse than stopping.
    #[test]
    fn a_missing_entry_aborts() {
        let manifest = manifest_for(&[(ASSET_AARCH64, b"arm bytes")]);
        assert!(matches!(
            verify(&manifest, ASSET_X86_64, b"anything"),
            Err(ChecksumError::Missing { .. })
        ));
        assert!(matches!(
            verify("", ASSET_X86_64, b"anything"),
            Err(ChecksumError::Missing { .. })
        ));
    }

    /// A line whose digest field is not 64 hex characters is malformed, not
    /// "close enough". Truncated digests are exactly what a partially-uploaded
    /// manifest looks like.
    #[test]
    fn a_malformed_entry_aborts() {
        for bad in [
            "abc  tty7-server-x86_64-unknown-linux-musl",
            "zz786850e387550fdab836ed7e6dc881de23001b4b4d8ec3a1a0b9d5e0d5c0f1x  tty7-server-x86_64-unknown-linux-musl",
            "  tty7-server-x86_64-unknown-linux-musl",
        ] {
            let err = expected_digest(bad, ASSET_X86_64).unwrap_err();
            assert!(
                matches!(
                    err,
                    ChecksumError::Malformed { .. } | ChecksumError::Missing { .. }
                ),
                "{bad:?} produced {err:?}"
            );
        }
    }

    /// **Whole-field match, not substring.** A manifest carrying a longer name
    /// that *contains* ours must not satisfy the lookup — this is the guard
    /// whole-field matching exists for.
    #[test]
    fn filename_matching_is_exact_not_substring() {
        let payload = b"decoy".as_slice();
        let manifest = format!(
            "{}  {ASSET_X86_64}.sig\n{}  old-{ASSET_X86_64}\n",
            hex(&sha256(payload)),
            hex(&sha256(payload)),
        );
        assert!(
            matches!(
                expected_digest(&manifest, ASSET_X86_64),
                Err(ChecksumError::Missing { .. })
            ),
            "neither a suffixed nor a prefixed name may satisfy the lookup"
        );
    }

    /// Binary-mode (`*name`) lines, CRLF line endings, comments and blank lines
    /// are all shapes a checksum file can legitimately arrive in.
    #[test]
    fn tolerates_binary_mode_crlf_and_comments() {
        let payload = b"payload".as_slice();
        let digest = hex(&sha256(payload));
        let manifest =
            format!("# generated by the release workflow\r\n\r\n{digest} *{ASSET_X86_64}\r\n");
        verify(&manifest, ASSET_X86_64, payload).expect("binary-mode CRLF lines must parse");
    }

    /// Empty input hashes to the well-known empty sha256; a fixed vector keeps
    /// the hashing itself honest rather than only self-consistent.
    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}

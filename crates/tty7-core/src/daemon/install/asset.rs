//! The pure half of the installer: `uname -sm` → release asset, client version →
//! release tag → download URL, and the remote paths a server binary lives at.
//!
//! Everything here is a total function of its arguments — no network, no SFTP, no
//! clock — which is the point: the asset naming here is a *literal* contract
//! with the release workflow (`.github/workflows/release.yml`), and a contract
//! is only worth having if both sides can be tested without standing up the
//! other one.

use std::fmt;

/// The release asset for a 64-bit x86 Linux box.
///
/// **`<os>-<arch>-musl`, not the Rust target triple.** These names used to be
/// `${{ matrix.target }}` pasted into a filename, which put `unknown` — the
/// triple's *vendor* field, meaning "no particular vendor" — in front of anyone
/// reading the releases page. Of the triple's four fields only two say anything
/// to whoever downloads this: the architecture, which is what `asset_for_uname`
/// picks by, and `musl`, which is why one file runs on any distribution. The
/// order matches the GUI assets the same release publishes
/// (`tty7-<version>-linux-x86_64.tar.gz`), so one release is one naming scheme.
///
/// The build target keeps the triple wherever it really is one — `cargo
/// zigbuild --target`, the `target/<triple>/release` path, the cache key. This
/// is a *download* name, and the two are no longer spelled the same on purpose.
pub const ASSET_X86_64: &str = "tty7-server-linux-x86_64-musl";
/// The release asset for a 64-bit ARM Linux box. See [`ASSET_X86_64`] for the
/// naming.
pub const ASSET_AARCH64: &str = "tty7-server-linux-aarch64-musl";
/// The sha256 manifest published beside every asset in a release.
pub const CHECKSUMS_ASSET: &str = "checksums.txt";

/// Where release assets are downloaded from. The tag and asset name are appended
/// (`{RELEASE_BASE}/{tag}/{asset}`); HTTPS to github.com is the trust anchor for
/// the checksum file itself.
pub const RELEASE_BASE: &str = "https://github.com/l0ng-ai/tty7/releases/download";

/// The `XDG_DATA_HOME`-shaped directory tty7 owns on a remote machine, relative
/// to `$HOME`. Split into components because the installer has to `mkdir` each
/// level (SFTP has no `mkdir -p`) and because joining is `/`-only regardless of
/// the *client's* OS — a Windows client must not produce `.local\share`.
pub const INSTALL_DIR_COMPONENTS: [&str; 4] = [".local", "share", "tty7", "bin"];

/// Why a machine cannot be served a `tty7-server`.
///
/// Both variants carry the raw `uname -sm` output: the whole value of refusing
/// instead of guessing is that the user can read the string we refused and either
/// recognise their box or paste it into an issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsupportedTarget {
    /// `uname -s` is not `Linux`. A remote tty7-server is a Linux binary; there
    /// is no macOS/BSD/Solaris asset to fall back to.
    NotLinux { raw: String },
    /// `uname -s` is `Linux` but `uname -m` is not one we publish for — 32-bit
    /// arm, i686, riscv64, or something we have never seen.
    UnknownMachine { raw: String },
    /// `uname -sm` did not produce the two whitespace-separated words it is
    /// specified to. Almost always means the command did not run at all (a login
    /// shell that printed a banner, a restricted shell) rather than a real
    /// answer, so it gets its own variant with the raw text.
    Unparseable { raw: String },
}

impl UnsupportedTarget {
    /// The `uname -sm` text this refusal is about, as the remote printed it.
    pub fn raw(&self) -> &str {
        match self {
            Self::NotLinux { raw } | Self::UnknownMachine { raw } | Self::Unparseable { raw } => {
                raw
            }
        }
    }
}

impl fmt::Display for UnsupportedTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLinux { raw } => write!(
                f,
                "a remote tty7 workspace needs a Linux host; this machine reports `uname -sm` = {raw:?}"
            ),
            Self::UnknownMachine { raw } => write!(
                f,
                "no tty7-server is published for this architecture (`uname -sm` = {raw:?}); \
                 supported: x86_64/amd64 and aarch64/arm64"
            ),
            Self::Unparseable { raw } => write!(
                f,
                "`uname -sm` did not answer with a system and machine name (got {raw:?})"
            ),
        }
    }
}

impl std::error::Error for UnsupportedTarget {}

/// Map raw `uname -sm` output to the release asset that runs on that machine.
///
/// **Exact string match, then fail.** No prefix matching, no "starts with `arm`
/// so it is probably aarch64" heuristic. Guessing wrong here installs a binary
/// that dies with `Exec format error` at first exec — an error with no visible
/// connection to the architecture detection that caused it, on a machine the user
/// may not be able to inspect. An unknown machine string is a clean, explainable
/// refusal that names itself.
///
/// `amd64` / `arm64` are accepted alongside the values Linux actually reports
/// because some container images and BSD-flavoured userlands normalise to them.
pub fn asset_for_uname(uname_sm: &str) -> Result<&'static str, UnsupportedTarget> {
    let raw = uname_sm.trim().to_string();
    let mut words = raw.split_whitespace();
    let (Some(system), Some(machine), None) = (words.next(), words.next(), words.next()) else {
        return Err(UnsupportedTarget::Unparseable { raw });
    };
    if system != "Linux" {
        return Err(UnsupportedTarget::NotLinux { raw });
    }
    match machine {
        "x86_64" | "amd64" => Ok(ASSET_X86_64),
        "aarch64" | "arm64" | "armv8l" | "armv8b" => Ok(ASSET_AARCH64),
        _ => Err(UnsupportedTarget::UnknownMachine { raw }),
    }
}

/// Resolve an asset name that arrived over a wire back to a `&'static str`.
///
/// [`super::InstallRequest::asset`] is `&'static str` because on the producing
/// side it is always one of the two consts above. A decoder cannot promise that,
/// and the relay in `daemon::router` has to rebuild the request a *different
/// process* raised — so the two known names map to themselves, and anything else
/// (a client older or newer than the daemon that named it) is leaked.
///
/// Leaking is bounded in the way that matters: the value comes from tty7's own
/// daemon naming one of its own release assets, and a session sees at most a
/// handful of distinct machines. It is preferred to guessing one of the two
/// consts, which would show the user a prompt naming the wrong architecture.
pub fn interned(name: &str) -> &'static str {
    if name == ASSET_X86_64 {
        ASSET_X86_64
    } else if name == ASSET_AARCH64 {
        ASSET_AARCH64
    } else {
        Box::leak(name.to_string().into_boxed_str())
    }
}

/// The release tag whose assets a client of `version` must download.
///
/// The nightly channel republishes a single rolling `nightly` tag every night, so
/// a nightly client must not ask for `v26.7.6-nightly.20260727` — that tag does
/// not exist and never will. Rule: the version contains `-nightly.` → `nightly`;
/// otherwise `v` + version.
pub fn release_tag(version: &str) -> String {
    if version.contains("-nightly.") {
        "nightly".to_string()
    } else {
        format!("v{version}")
    }
}

/// The download URL for one asset of one release.
pub fn download_url(tag: &str, asset: &str) -> String {
    format!("{RELEASE_BASE}/{tag}/{asset}")
}

/// Absolute remote paths for one *dialect*'s server binary.
///
/// Built with explicit `/` joins from an absolute `$HOME` the remote resolved for
/// us (SFTP does not expand `~`, and `PathBuf::join` would emit `\` on a Windows
/// client).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePaths {
    /// `$HOME/.local/share/tty7/bin`.
    pub bin_dir: String,
    /// `$HOME/.local/share/tty7/bin/tty7-server-c<control>p<protocol>` — the
    /// atomically published binary. See [`binary_name`] for why the dialects, and
    /// not the version, are what the name carries.
    pub binary: String,
    /// `$HOME/.local/share/tty7/bin/.tty7-server-c<control>p<protocol>.tmp` —
    /// where the bytes land before `chmod`, the `--protocol` check, and `rename`.
    ///
    /// A dotfile, so a half-written upload is not mistaken for an installed
    /// server by anything reading the directory. The installer adds a per-process
    /// suffix (`super::unique_temp`) before writing: one file per dialect means
    /// two clients installing the same dialect at once would otherwise interleave
    /// their bytes into one name.
    pub temp: String,
    /// Every directory that must exist before the upload, outermost first. SFTP
    /// has no recursive mkdir, so the installer walks this.
    pub dir_chain: Vec<String>,
}

/// Build the remote paths for a server speaking `control`/`protocol` under an
/// absolute remote `home`.
pub fn remote_paths(home: &str, control: u32, protocol: u32) -> RemotePaths {
    let home = home.trim_end_matches('/');
    let mut dir_chain = Vec::with_capacity(INSTALL_DIR_COMPONENTS.len());
    let mut cursor = home.to_string();
    for part in INSTALL_DIR_COMPONENTS {
        cursor = format!("{cursor}/{part}");
        dir_chain.push(cursor.clone());
    }
    let bin_dir = cursor;
    let name = binary_name(control, protocol);
    RemotePaths {
        binary: format!("{bin_dir}/{name}"),
        temp: format!("{bin_dir}/.{name}.tmp"),
        dir_chain,
        bin_dir,
    }
}

/// The filename a server speaking `control`/`protocol` is installed under.
///
/// **The dialects are the name, and the version is nowhere in it.** Everything
/// the installer decides — is there something usable here, can the daemon that
/// is running talk to us — is a question about dialects, and a name built from
/// them answers it with a `stat` the client can address without asking the
/// remote anything. A name built from the version answers a *different*
/// question, and answers this one wrong in both directions: two builds that
/// share a version string but not a dialect (any two dev builds between
/// releases) look interchangeable, and two builds that share a dialect but not a
/// version look incompatible and cost an 8 MB upload that changes nothing.
///
/// One file per dialect, so a machine accumulates at most one binary per wire
/// break rather than one per release. Which *build* is sitting behind a given
/// dialect is a separate question, answered by [`PROTOCOL_FLAG`][flag] and by
/// the control handshake — not by the filename.
///
/// [flag]: super::PROTOCOL_FLAG
pub fn binary_name(control: u32, protocol: u32) -> String {
    format!("tty7-server-c{control}p{protocol}")
}

/// [`RemotePaths`] pointing at a binary that is **already on the machine**,
/// found rather than named — the server a connect adopted because it speaks our
/// dialects (`Installer::adoptable_running_server`).
///
/// `binary` is the path as the remote reported it, verbatim: it is what the
/// transport must connect to, and rebuilding it from a version parsed out of the
/// filename would turn a binary installed somewhere unexpected into a path that
/// does not exist.
///
/// `temp` and `dir_chain` still describe *our* install location, because that is
/// where a later install would write. Nothing writes anything on the adoption
/// path, so they are unused there; keeping them well-formed means a caller that
/// falls back to installing does not need a second `RemotePaths`.
pub fn remote_paths_for_binary(
    home: &str,
    binary: &str,
    control: u32,
    protocol: u32,
) -> RemotePaths {
    let mut paths = remote_paths(home, control, protocol);
    paths.binary = binary.to_string();
    paths
}

/// The dialects encoded in an installed binary's *path*, if it is one of ours.
///
/// This is how the running daemon is identified without asking it: the install
/// path carries the dialects by construction, so `readlink /proc/<pid>/exe` on
/// the remote answers "can the thing serving this machine talk to us" in the
/// round trip that found it.
///
/// `None` for anything else, and that deliberately includes every binary
/// installed by a client that named files after versions: an old name carries no
/// dialect, so it gets no opinion, and the probe (`--protocol`) is what settles
/// it. Guessing a dialect from a version string is the exact inference this
/// naming exists to make impossible.
pub fn dialect_from_path(path: &str) -> Option<(u32, u32)> {
    let name = path.rsplit('/').next()?;
    let (control, protocol) = name.strip_prefix("tty7-server-c")?.split_once('p')?;
    Some((control.parse().ok()?, protocol.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract's mapping table, row for row. This test *is* the client half
    /// of the asset naming contract: if the release workflow ever renames an
    /// asset, this is where the two sides stop agreeing.
    #[test]
    fn uname_maps_to_the_published_assets() {
        for raw in ["Linux x86_64", "Linux amd64"] {
            assert_eq!(asset_for_uname(raw).unwrap(), ASSET_X86_64, "{raw}");
        }
        for raw in [
            "Linux aarch64",
            "Linux arm64",
            "Linux armv8l",
            "Linux armv8b",
        ] {
            assert_eq!(asset_for_uname(raw).unwrap(), ASSET_AARCH64, "{raw}");
        }
    }

    /// Real `uname` output ends in a newline, and a shell may pad it. Trimming is
    /// the only normalisation allowed — the *words* are matched exactly.
    #[test]
    fn uname_output_is_trimmed_before_matching() {
        assert_eq!(asset_for_uname("Linux x86_64\n").unwrap(), ASSET_X86_64);
        assert_eq!(
            asset_for_uname("  Linux x86_64  \r\n").unwrap(),
            ASSET_X86_64
        );
    }

    /// The refusal path, which is the whole reason this function exists. Every
    /// one of these would be a plausible prefix/fuzzy match — `x86_64-v2` starts
    /// with `x86_64`, `armv7l` starts with `arm`, `Linux` appears inside
    /// `GNU/Linux` — and each would install a binary that cannot exec.
    #[test]
    fn unknown_machines_are_refused_not_guessed() {
        for raw in [
            "Linux i686",
            "Linux i386",
            "Linux armv7l",
            "Linux armv6l",
            "Linux riscv64",
            "Linux ppc64le",
            "Linux s390x",
            "Linux x86_64-v2",
            "Linux aarch64_be",
            "Linux ARM64",
            "Linux X86_64",
        ] {
            let err = asset_for_uname(raw).unwrap_err();
            assert!(
                matches!(err, UnsupportedTarget::UnknownMachine { .. }),
                "{raw} must be refused as an unknown machine, got {err:?}"
            );
            assert_eq!(err.raw(), raw, "the refusal must quote what it refused");
        }
    }

    /// A non-Linux host is refused with its own variant so the message can say
    /// "needs Linux" rather than "unknown architecture" — the user's next step is
    /// completely different.
    #[test]
    fn non_linux_systems_are_refused() {
        for raw in [
            "Darwin arm64",
            "FreeBSD amd64",
            "SunOS i86pc",
            "linux x86_64",
        ] {
            assert!(
                matches!(
                    asset_for_uname(raw).unwrap_err(),
                    UnsupportedTarget::NotLinux { .. }
                ),
                "{raw}"
            );
        }
    }

    /// Anything that is not exactly two words never reaches the mapping. In
    /// practice this catches the common failure where the command did not run and
    /// we got a shell banner, an error message, or nothing at all — and it is the
    /// guard that keeps a three-word string from silently matching on its first
    /// two words.
    #[test]
    fn output_that_is_not_two_words_is_unparseable() {
        for raw in [
            "",
            "   ",
            "Linux",
            "x86_64",
            "Linux x86_64 GNU/Linux",
            "bash: uname: command not found",
        ] {
            assert!(
                matches!(
                    asset_for_uname(raw).unwrap_err(),
                    UnsupportedTarget::Unparseable { .. }
                ),
                "{raw:?}"
            );
        }
    }

    /// Stable releases resolve to their own tag; nightlies resolve to the single
    /// rolling `nightly` tag, because per-night tags are never created.
    #[test]
    fn release_tag_sends_nightlies_to_the_rolling_tag() {
        assert_eq!(release_tag("26.7.5"), "v26.7.5");
        assert_eq!(release_tag("0.1.0"), "v0.1.0");
        assert_eq!(release_tag("26.7.6-nightly.20260727"), "nightly");
        // A pre-release that is *not* a nightly keeps its own tag: only the
        // nightly channel republishes under a rolling name.
        assert_eq!(release_tag("26.8.0-rc.1"), "v26.8.0-rc.1");
    }

    #[test]
    fn download_urls_point_at_the_release_the_tag_names() {
        assert_eq!(
            download_url(&release_tag("26.7.5"), ASSET_X86_64),
            "https://github.com/l0ng-ai/tty7/releases/download/v26.7.5/tty7-server-linux-x86_64-musl"
        );
        assert_eq!(
            download_url(&release_tag("26.7.6-nightly.20260727"), CHECKSUMS_ASSET),
            "https://github.com/l0ng-ai/tty7/releases/download/nightly/checksums.txt"
        );
    }

    /// **The asset names, pinned as literals.**
    ///
    /// They are one half of a contract whose other half is a `cp` in two
    /// workflow files, and checking them against the consts they come from
    /// would assert nothing. A literal here is what makes changing one side
    /// without the other a failing test rather than a 404 on a user's machine.
    ///
    /// Including the absence of `unknown`: that word only ever reached these
    /// names by way of `${{ matrix.target }}`, and a build triple pasted into a
    /// download name is worth failing on rather than explaining again.
    #[test]
    fn asset_names_are_the_ones_the_release_workflow_publishes() {
        assert_eq!(ASSET_X86_64, "tty7-server-linux-x86_64-musl");
        assert_eq!(ASSET_AARCH64, "tty7-server-linux-aarch64-musl");
        for asset in [ASSET_X86_64, ASSET_AARCH64] {
            assert!(
                !asset.contains("unknown"),
                "{asset} carries the triple's vendor field"
            );
        }
        // `checksums::expected_digest` matches the filename field whole, and
        // says outright that it relies on no asset name being a substring of
        // another. Two names is the whole set, so check it here.
        assert!(!ASSET_X86_64.contains(ASSET_AARCH64));
        assert!(!ASSET_AARCH64.contains(ASSET_X86_64));
    }

    /// Path construction, including the `mkdir` chain. Asserted literally: these
    /// strings are what an SFTP server sees, and a `\` in any of them (which is
    /// what `PathBuf::join` would produce on a Windows client) would create a file
    /// named `.local\share\tty7\bin` in the remote home directory.
    #[test]
    fn remote_paths_are_posix_and_named_by_dialect() {
        let p = remote_paths("/home/me", 3, 4);
        assert_eq!(p.bin_dir, "/home/me/.local/share/tty7/bin");
        assert_eq!(p.binary, "/home/me/.local/share/tty7/bin/tty7-server-c3p4");
        assert_eq!(
            p.temp,
            "/home/me/.local/share/tty7/bin/.tty7-server-c3p4.tmp"
        );
        assert_eq!(
            p.dir_chain,
            vec![
                "/home/me/.local",
                "/home/me/.local/share",
                "/home/me/.local/share/tty7",
                "/home/me/.local/share/tty7/bin",
            ]
        );
        assert!(
            !p.temp.contains('\\') && !p.binary.contains('\\'),
            "remote paths are POSIX regardless of the client's OS"
        );
    }

    /// The temp name is a sibling dotfile of the target, so the finishing rename
    /// is same-directory (same filesystem → atomic) and a partial upload is not
    /// mistaken for an installed server.
    #[test]
    fn temp_path_is_a_hidden_sibling_of_the_binary() {
        let p = remote_paths("/home/me", 3, 4);
        let dir = |s: &str| s.rsplit_once('/').unwrap().0.to_string();
        assert_eq!(dir(&p.temp), dir(&p.binary));
        assert!(p.temp.rsplit('/').next().unwrap().starts_with('.'));
        assert!(!p.binary.rsplit('/').next().unwrap().starts_with('.'));
    }

    /// A trailing slash on the resolved home (some SFTP servers return `/root/`)
    /// must not produce a doubled separator.
    #[test]
    fn trailing_slash_on_home_is_absorbed() {
        assert_eq!(
            remote_paths("/root/", 1, 1).binary,
            "/root/.local/share/tty7/bin/tty7-server-c1p1"
        );
        // Root as home is degenerate but must still be well-formed.
        assert_eq!(remote_paths("/", 1, 1).bin_dir, "/.local/share/tty7/bin");
    }

    /// The inverse used to identify a *running* daemon from its executable path.
    #[test]
    fn dialects_are_recoverable_from_an_install_path() {
        assert_eq!(
            dialect_from_path("/home/me/.local/share/tty7/bin/tty7-server-c3p4"),
            Some((3, 4))
        );
        assert_eq!(dialect_from_path("tty7-server-c12p30"), Some((12, 30)));
        // Not ours, or not dialect-named: no opinion rather than a wrong one.
        assert_eq!(dialect_from_path("/usr/bin/tty7-server"), None);
        assert_eq!(dialect_from_path("/bin/bash"), None);
        assert_eq!(dialect_from_path("tty7-server-c3"), None);
        assert_eq!(dialect_from_path("tty7-server-cxpy"), None);
    }

    /// Every name a version-naming client ever installed reads as "no opinion".
    ///
    /// The whole point of the rename is that a version string can no longer be
    /// mistaken for a dialect; a parser that squeezed `3` out of `26.7.3` would
    /// reintroduce exactly that, and on the paths of binaries already sitting on
    /// users' machines.
    #[test]
    fn legacy_version_named_binaries_carry_no_dialect() {
        for legacy in [
            "/home/me/.local/share/tty7/bin/tty7-server-26.7.4",
            "tty7-server-26.7.6-nightly.20260727",
            "tty7-server-0.1.0",
            "/usr/local/bin/tty7-server-",
        ] {
            assert_eq!(dialect_from_path(legacy), None, "{legacy}");
        }
    }

    /// Round-trip: the name we install under is the name we recognise later.
    #[test]
    fn install_path_and_dialect_extraction_round_trip() {
        for (c, p) in [(1u32, 1u32), (3, 4), (26, 7)] {
            let paths = remote_paths("/home/me", c, p);
            assert_eq!(dialect_from_path(&paths.binary), Some((c, p)));
        }
    }
}

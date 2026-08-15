use std::fmt;

pub const ASSET_LINUX_X86_64: &str = "tty7-server-linux-x86_64-musl";
pub const ASSET_LINUX_AARCH64: &str = "tty7-server-linux-aarch64-musl";

/// The macOS servers carry no libc suffix because there is nothing to choose:
/// they link the system libSystem every macOS has, which is as portable there
/// as static musl is on Linux. Same flat, version-free shape as the others —
/// the tag in the download URL carries the version.
pub const ASSET_MACOS_X86_64: &str = "tty7-server-macos-x86_64";
pub const ASSET_MACOS_AARCH64: &str = "tty7-server-macos-aarch64";

pub const CHECKSUMS_ASSET: &str = "checksums.txt";

pub const RELEASE_BASE: &str = "https://github.com/l0ng-ai/tty7/releases/download";

pub const INSTALL_DIR_COMPONENTS: [&str; 4] = [".local", "share", "tty7", "bin"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsupportedTarget {
    UnsupportedSystem { raw: String },
    UnknownMachine { raw: String },
    Unparseable { raw: String },
}

impl UnsupportedTarget {
    pub fn raw(&self) -> &str {
        match self {
            Self::UnsupportedSystem { raw }
            | Self::UnknownMachine { raw }
            | Self::Unparseable { raw } => raw,
        }
    }
}

impl fmt::Display for UnsupportedTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSystem { raw } => write!(
                f,
                "a remote tty7 workspace needs a Linux or macOS host; this machine reports \
                 `uname -sm` = {raw:?}"
            ),
            Self::UnknownMachine { raw } => write!(
                f,
                "no tty7-server is published for this architecture (`uname -sm` = {raw:?}); \
                 supported: Linux on x86_64/amd64 and aarch64/arm64, macOS on x86_64 and arm64"
            ),
            Self::Unparseable { raw } => write!(
                f,
                "`uname -sm` did not answer with a system and machine name (got {raw:?})"
            ),
        }
    }
}

impl std::error::Error for UnsupportedTarget {}

pub(crate) fn asset_for_uname(uname_sm: &str) -> Result<&'static str, UnsupportedTarget> {
    let raw = uname_sm.trim().to_string();
    let mut words = raw.split_whitespace();
    let (Some(system), Some(machine), None) = (words.next(), words.next(), words.next()) else {
        return Err(UnsupportedTarget::Unparseable { raw });
    };
    // Matched per system rather than by machine alone: the two do not share a
    // vocabulary. Linux answers `arm64` on some distributions and `aarch64` on
    // others, while macOS only ever says `arm64` — accepting Linux's spellings
    // under Darwin would be guessing at output no Mac produces, and the machine
    // names that would reach it are the ones worth refusing loudly.
    match (system, machine) {
        ("Linux", "x86_64" | "amd64") => Ok(ASSET_LINUX_X86_64),
        ("Linux", "aarch64" | "arm64" | "armv8l" | "armv8b") => Ok(ASSET_LINUX_AARCH64),
        // A Rosetta shell reports `x86_64` on Apple Silicon, and taking it at
        // its word is right: the x86_64 server runs under the same translation
        // the shell asking for it is already running under.
        ("Darwin", "x86_64") => Ok(ASSET_MACOS_X86_64),
        ("Darwin", "arm64") => Ok(ASSET_MACOS_AARCH64),
        ("Linux" | "Darwin", _) => Err(UnsupportedTarget::UnknownMachine { raw }),
        _ => Err(UnsupportedTarget::UnsupportedSystem { raw }),
    }
}

pub(crate) fn interned(name: &str) -> &'static str {
    match name {
        _ if name == ASSET_LINUX_X86_64 => ASSET_LINUX_X86_64,
        _ if name == ASSET_LINUX_AARCH64 => ASSET_LINUX_AARCH64,
        _ if name == ASSET_MACOS_X86_64 => ASSET_MACOS_X86_64,
        _ if name == ASSET_MACOS_AARCH64 => ASSET_MACOS_AARCH64,
        _ => Box::leak(name.to_string().into_boxed_str()),
    }
}

pub(crate) fn release_tag(version: &str) -> String {
    if version.contains("-nightly.") {
        "nightly".to_string()
    } else {
        format!("v{version}")
    }
}

pub(crate) fn download_url(tag: &str, asset: &str) -> String {
    format!("{RELEASE_BASE}/{tag}/{asset}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePaths {
    pub bin_dir: String,
    pub binary: String,
    pub temp: String,
    pub dir_chain: Vec<String>,
}

pub(crate) fn remote_paths(home: &str, control: u32, protocol: u32) -> RemotePaths {
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

pub(crate) fn binary_name(control: u32, protocol: u32) -> String {
    format!("tty7-server-c{control}p{protocol}")
}

pub(crate) fn remote_paths_for_binary(
    home: &str,
    binary: &str,
    control: u32,
    protocol: u32,
) -> RemotePaths {
    let mut paths = remote_paths(home, control, protocol);
    paths.binary = binary.to_string();
    paths
}

pub(crate) fn dialect_from_path(path: &str) -> Option<(u32, u32)> {
    let name = path.rsplit('/').next()?;
    let (control, protocol) = name.strip_prefix("tty7-server-c")?.split_once('p')?;
    Some((control.parse().ok()?, protocol.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uname_maps_to_the_published_assets() {
        for raw in ["Linux x86_64", "Linux amd64"] {
            assert_eq!(asset_for_uname(raw).unwrap(), ASSET_LINUX_X86_64, "{raw}");
        }
        for raw in [
            "Linux aarch64",
            "Linux arm64",
            "Linux armv8l",
            "Linux armv8b",
        ] {
            assert_eq!(asset_for_uname(raw).unwrap(), ASSET_LINUX_AARCH64, "{raw}");
        }
        assert_eq!(
            asset_for_uname("Darwin arm64").unwrap(),
            ASSET_MACOS_AARCH64
        );
        assert_eq!(
            asset_for_uname("Darwin x86_64").unwrap(),
            ASSET_MACOS_X86_64
        );
    }

    /// The two systems are matched as pairs, so a machine name that means one
    /// thing on Linux must not be honoured under Darwin just because it appears
    /// in the same function.
    #[test]
    fn a_machine_name_does_not_carry_across_systems() {
        for raw in ["Darwin aarch64", "Darwin amd64", "Darwin armv8l"] {
            assert!(
                matches!(
                    asset_for_uname(raw).unwrap_err(),
                    UnsupportedTarget::UnknownMachine { .. }
                ),
                "{raw} is not something a Mac reports"
            );
        }
    }

    #[test]
    fn uname_output_is_trimmed_before_matching() {
        assert_eq!(
            asset_for_uname("Linux x86_64\n").unwrap(),
            ASSET_LINUX_X86_64
        );
        assert_eq!(
            asset_for_uname("  Linux x86_64  \r\n").unwrap(),
            ASSET_LINUX_X86_64
        );
    }

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

    #[test]
    fn systems_we_publish_nothing_for_are_refused() {
        for raw in [
            "FreeBSD amd64",
            "OpenBSD amd64",
            "SunOS i86pc",
            "linux x86_64",
            "darwin arm64",
        ] {
            assert!(
                matches!(
                    asset_for_uname(raw).unwrap_err(),
                    UnsupportedTarget::UnsupportedSystem { .. }
                ),
                "{raw}"
            );
        }
    }

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

    #[test]
    fn release_tag_sends_nightlies_to_the_rolling_tag() {
        assert_eq!(release_tag("26.7.5"), "v26.7.5");
        assert_eq!(release_tag("0.1.0"), "v0.1.0");
        assert_eq!(release_tag("26.7.6-nightly.20260727"), "nightly");
        assert_eq!(release_tag("26.8.0-rc.1"), "v26.8.0-rc.1");
    }

    #[test]
    fn download_urls_point_at_the_release_the_tag_names() {
        assert_eq!(
            download_url(&release_tag("26.7.5"), ASSET_LINUX_X86_64),
            "https://github.com/l0ng-ai/tty7/releases/download/v26.7.5/tty7-server-linux-x86_64-musl"
        );
        assert_eq!(
            download_url(&release_tag("26.7.6-nightly.20260727"), CHECKSUMS_ASSET),
            "https://github.com/l0ng-ai/tty7/releases/download/nightly/checksums.txt"
        );
    }

    #[test]
    fn asset_names_are_the_ones_the_release_workflow_publishes() {
        assert_eq!(ASSET_LINUX_X86_64, "tty7-server-linux-x86_64-musl");
        assert_eq!(ASSET_LINUX_AARCH64, "tty7-server-linux-aarch64-musl");
        assert_eq!(ASSET_MACOS_X86_64, "tty7-server-macos-x86_64");
        assert_eq!(ASSET_MACOS_AARCH64, "tty7-server-macos-aarch64");

        let all = [
            ASSET_LINUX_X86_64,
            ASSET_LINUX_AARCH64,
            ASSET_MACOS_X86_64,
            ASSET_MACOS_AARCH64,
        ];
        for asset in all {
            assert!(
                !asset.contains("unknown") && !asset.contains("apple"),
                "{asset} carries the triple's vendor field"
            );
            assert_eq!(asset, interned(asset), "{asset} must intern to itself");
        }
        // No name may contain another: `checksums` looks a line up by filename,
        // and a name that is a suffix of its neighbour would let one asset's
        // digest answer for the other's.
        for a in all {
            for b in all {
                assert!(a == b || !a.contains(b), "{a} contains {b}");
            }
        }
    }

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

    #[test]
    fn temp_path_is_a_hidden_sibling_of_the_binary() {
        let p = remote_paths("/home/me", 3, 4);
        let dir = |s: &str| s.rsplit_once('/').unwrap().0.to_string();
        assert_eq!(dir(&p.temp), dir(&p.binary));
        assert!(p.temp.rsplit('/').next().unwrap().starts_with('.'));
        assert!(!p.binary.rsplit('/').next().unwrap().starts_with('.'));
    }

    #[test]
    fn trailing_slash_on_home_is_absorbed() {
        assert_eq!(
            remote_paths("/root/", 1, 1).binary,
            "/root/.local/share/tty7/bin/tty7-server-c1p1"
        );
        assert_eq!(remote_paths("/", 1, 1).bin_dir, "/.local/share/tty7/bin");
    }

    #[test]
    fn dialects_are_recoverable_from_an_install_path() {
        assert_eq!(
            dialect_from_path("/home/me/.local/share/tty7/bin/tty7-server-c3p4"),
            Some((3, 4))
        );
        assert_eq!(dialect_from_path("tty7-server-c12p30"), Some((12, 30)));
        assert_eq!(dialect_from_path("/usr/bin/tty7-server"), None);
        assert_eq!(dialect_from_path("/bin/bash"), None);
        assert_eq!(dialect_from_path("tty7-server-c3"), None);
        assert_eq!(dialect_from_path("tty7-server-cxpy"), None);
    }

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

    #[test]
    fn install_path_and_dialect_extraction_round_trip() {
        for (c, p) in [(1u32, 1u32), (3, 4), (26, 7)] {
            let paths = remote_paths("/home/me", c, p);
            assert_eq!(dialect_from_path(&paths.binary), Some((c, p)));
        }
    }
}

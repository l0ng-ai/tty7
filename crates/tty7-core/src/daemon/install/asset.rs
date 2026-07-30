use std::fmt;

pub const ASSET_X86_64: &str = "tty7-server-linux-x86_64-musl";
pub const ASSET_AARCH64: &str = "tty7-server-linux-aarch64-musl";
pub const CHECKSUMS_ASSET: &str = "checksums.txt";

pub const RELEASE_BASE: &str = "https://github.com/l0ng-ai/tty7/releases/download";

pub const INSTALL_DIR_COMPONENTS: [&str; 4] = [".local", "share", "tty7", "bin"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsupportedTarget {
    NotLinux { raw: String },
    UnknownMachine { raw: String },
    Unparseable { raw: String },
}

impl UnsupportedTarget {
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

pub fn interned(name: &str) -> &'static str {
    if name == ASSET_X86_64 {
        ASSET_X86_64
    } else if name == ASSET_AARCH64 {
        ASSET_AARCH64
    } else {
        Box::leak(name.to_string().into_boxed_str())
    }
}

pub fn release_tag(version: &str) -> String {
    if version.contains("-nightly.") {
        "nightly".to_string()
    } else {
        format!("v{version}")
    }
}

pub fn download_url(tag: &str, asset: &str) -> String {
    format!("{RELEASE_BASE}/{tag}/{asset}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePaths {
    pub bin_dir: String,
    pub binary: String,
    pub temp: String,
    pub dir_chain: Vec<String>,
}

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

pub fn binary_name(control: u32, protocol: u32) -> String {
    format!("tty7-server-c{control}p{protocol}")
}

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

pub fn dialect_from_path(path: &str) -> Option<(u32, u32)> {
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

    #[test]
    fn uname_output_is_trimmed_before_matching() {
        assert_eq!(asset_for_uname("Linux x86_64\n").unwrap(), ASSET_X86_64);
        assert_eq!(
            asset_for_uname("  Linux x86_64  \r\n").unwrap(),
            ASSET_X86_64
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
            download_url(&release_tag("26.7.5"), ASSET_X86_64),
            "https://github.com/l0ng-ai/tty7/releases/download/v26.7.5/tty7-server-linux-x86_64-musl"
        );
        assert_eq!(
            download_url(&release_tag("26.7.6-nightly.20260727"), CHECKSUMS_ASSET),
            "https://github.com/l0ng-ai/tty7/releases/download/nightly/checksums.txt"
        );
    }

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
        assert!(!ASSET_X86_64.contains(ASSET_AARCH64));
        assert!(!ASSET_AARCH64.contains(ASSET_X86_64));
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

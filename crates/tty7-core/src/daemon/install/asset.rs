use std::fmt;

pub const ASSET_LINUX_X86_64: &str = "tty7-server-linux-x86_64-musl";
pub const ASSET_LINUX_AARCH64: &str = "tty7-server-linux-aarch64-musl";

/// The macOS servers carry no libc suffix because there is nothing to choose:
/// they link the system libSystem every macOS has, which is as portable there
/// as static musl is on Linux. Same flat, version-free shape as the others —
/// the tag in the download URL carries the version.
pub const ASSET_MACOS_X86_64: &str = "tty7-server-macos-x86_64";
pub const ASSET_MACOS_AARCH64: &str = "tty7-server-macos-aarch64";

/// Windows servers keep the `.exe` suffix in the published name as well as on
/// disk: it is what makes the file runnable over there, and a checksum line is
/// looked up by exactly this name.
pub const ASSET_WINDOWS_X86_64: &str = "tty7-server-windows-x86_64.exe";
pub const ASSET_WINDOWS_AARCH64: &str = "tty7-server-windows-aarch64.exe";

pub const CHECKSUMS_ASSET: &str = "checksums.txt";

pub const RELEASE_BASE: &str = "https://github.com/l0ng-ai/tty7/releases/download";

pub const INSTALL_DIR_COMPONENTS: [&str; 4] = [".local", "share", "tty7", "bin"];

/// `%LOCALAPPDATA%\tty7\bin`, spelled from the profile directory SFTP reports
/// as home rather than read from the environment: the installer only ever sees
/// the far end through SFTP paths, and `%LOCALAPPDATA%` is this directory on
/// every profile that has not been redirected by policy.
pub const WINDOWS_INSTALL_DIR_COMPONENTS: [&str; 4] = ["AppData", "Local", "tty7", "bin"];

/// Which family of machine a remote workspace installs onto. It decides the
/// install directory, the binary's file name, and — in `install` — the shell
/// every command is phrased for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RemotePlatform {
    /// Linux and macOS: a POSIX `sh`, `uname`, and SFTP paths that are the
    /// machine's own paths.
    #[default]
    Unix,
    /// Windows OpenSSH: no POSIX shell to count on, and SFTP spells every path
    /// with a leading slash before the drive (`/C:/Users/me`).
    Windows,
}

impl RemotePlatform {
    /// What an SFTP home directory says about the machine behind it.
    ///
    /// Windows OpenSSH's SFTP server is the only one that answers `realpath .`
    /// with a drive letter, so the shape of the home alone tells the two apart
    /// without a round trip.
    pub fn of_sftp_home(home: &str) -> RemotePlatform {
        if is_windows_sftp_path(home) {
            RemotePlatform::Windows
        } else {
            RemotePlatform::Unix
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsupportedTarget {
    UnsupportedSystem {
        raw: String,
    },
    UnknownMachine {
        raw: String,
    },
    /// A Windows host whose `PROCESSOR_ARCHITECTURE` names something no server
    /// is published for (32-bit x86, Itanium).
    UnknownWindowsMachine {
        raw: String,
    },
    Unparseable {
        raw: String,
    },
}

impl UnsupportedTarget {
    pub fn raw(&self) -> &str {
        match self {
            Self::UnsupportedSystem { raw }
            | Self::UnknownMachine { raw }
            | Self::UnknownWindowsMachine { raw }
            | Self::Unparseable { raw } => raw,
        }
    }
}

impl fmt::Display for UnsupportedTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSystem { raw } => write!(
                f,
                "a remote tty7 workspace needs a Linux, macOS or Windows host; this machine \
                 reports `uname -sm` = {raw:?}"
            ),
            Self::UnknownWindowsMachine { raw } => write!(
                f,
                "no tty7-server is published for this Windows architecture \
                 (`PROCESSOR_ARCHITECTURE` = {raw:?}); supported: AMD64 and ARM64"
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

pub fn asset_for_uname(uname_sm: &str) -> Result<&'static str, UnsupportedTarget> {
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

/// Whether `uname -sm` came from a POSIX layer on top of Windows — Git for
/// Windows, MSYS2 or Cygwin on the `PATH` of the account being logged into.
/// The machine is still Windows, and the server it needs is the Windows one.
pub fn uname_reports_windows(uname_sm: &str) -> bool {
    let system = uname_sm.split_whitespace().next().unwrap_or("");
    ["MINGW", "MSYS_NT", "CYGWIN_NT", "Windows_NT"]
        .iter()
        .any(|prefix| system.starts_with(prefix))
}

/// The server for a Windows host, from `PROCESSOR_ARCHITECTURE` (or
/// `PROCESSOR_ARCHITEW6432` when the probing shell is a 32-bit process on a
/// 64-bit machine).
///
/// An x64 shell on an ARM64 machine reports `AMD64`, and taking it at its word
/// is right for the same reason as a Rosetta shell on a Mac: the x64 server
/// runs under the emulation the shell asking for it already runs under.
pub fn asset_for_windows_arch(arch: &str) -> Result<&'static str, UnsupportedTarget> {
    let raw = arch.trim().to_string();
    match raw.to_ascii_uppercase().as_str() {
        "AMD64" | "X64" | "EM64T" => Ok(ASSET_WINDOWS_X86_64),
        "ARM64" => Ok(ASSET_WINDOWS_AARCH64),
        "" => Err(UnsupportedTarget::Unparseable { raw }),
        _ => Err(UnsupportedTarget::UnknownWindowsMachine { raw }),
    }
}

pub fn interned(name: &str) -> &'static str {
    match name {
        _ if name == ASSET_LINUX_X86_64 => ASSET_LINUX_X86_64,
        _ if name == ASSET_LINUX_AARCH64 => ASSET_LINUX_AARCH64,
        _ if name == ASSET_MACOS_X86_64 => ASSET_MACOS_X86_64,
        _ if name == ASSET_MACOS_AARCH64 => ASSET_MACOS_AARCH64,
        _ if name == ASSET_WINDOWS_X86_64 => ASSET_WINDOWS_X86_64,
        _ if name == ASSET_WINDOWS_AARCH64 => ASSET_WINDOWS_AARCH64,
        _ => Box::leak(name.to_string().into_boxed_str()),
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
    /// The machine these paths are on, which is also how every command that
    /// names one of them has to be phrased.
    pub platform: RemotePlatform,
}

pub fn remote_paths(home: &str, control: u32, protocol: u32) -> RemotePaths {
    remote_paths_on(RemotePlatform::Unix, home, control, protocol)
}

/// [`remote_paths`] for either kind of machine. The paths are always in the
/// SFTP spelling — forward slashes, and on Windows a leading `/C:` — because
/// SFTP is what writes them; [`windows_native_path`] turns one into what a
/// Windows command line wants.
pub fn remote_paths_on(
    platform: RemotePlatform,
    home: &str,
    control: u32,
    protocol: u32,
) -> RemotePaths {
    let home = home.trim_end_matches('/');
    let components = match platform {
        RemotePlatform::Unix => INSTALL_DIR_COMPONENTS,
        RemotePlatform::Windows => WINDOWS_INSTALL_DIR_COMPONENTS,
    };
    let mut dir_chain = Vec::with_capacity(components.len());
    let mut cursor = home.to_string();
    for part in components {
        cursor = format!("{cursor}/{part}");
        dir_chain.push(cursor.clone());
    }
    let bin_dir = cursor;
    let name = binary_name(control, protocol);
    let (binary, temp) = match platform {
        RemotePlatform::Unix => (
            format!("{bin_dir}/{name}"),
            format!("{bin_dir}/.{name}.tmp"),
        ),
        // The temp name keeps `.exe` last: a Windows shell hands a file with
        // any other extension to its file association instead of running it,
        // and the upload is run (`--protocol`) before it is renamed into place.
        RemotePlatform::Windows => (
            format!("{bin_dir}/{name}.exe"),
            format!("{bin_dir}/.{name}.tmp.exe"),
        ),
    };
    RemotePaths {
        binary,
        temp,
        dir_chain,
        bin_dir,
        platform,
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
    remote_paths_for_binary_on(RemotePlatform::Unix, home, binary, control, protocol)
}

pub fn remote_paths_for_binary_on(
    platform: RemotePlatform,
    home: &str,
    binary: &str,
    control: u32,
    protocol: u32,
) -> RemotePaths {
    let mut paths = remote_paths_on(platform, home, control, protocol);
    paths.binary = binary.to_string();
    paths
}

/// Whether `path` is a Windows path as Windows OpenSSH's SFTP server spells
/// it: `/C:` alone or followed by `/`.
pub fn is_windows_sftp_path(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() >= 3
        && b[0] == b'/'
        && b[1].is_ascii_alphabetic()
        && b[2] == b':'
        && (b.len() == 3 || b[3] == b'/')
}

/// `/C:/Users/me/x.exe` → `C:\Users\me\x.exe`, the spelling a Windows command
/// line takes. `None` for anything that is not an SFTP Windows path, so a
/// caller can never hand a POSIX path to a Windows shell by accident.
pub fn windows_native_path(sftp: &str) -> Option<String> {
    if !is_windows_sftp_path(sftp) {
        return None;
    }
    let mut native = sftp[1..].replace('/', "\\");
    if native.len() == 2 {
        native.push('\\');
    }
    Some(native)
}

/// The inverse of [`windows_native_path`]: `C:\x\y.exe` → `/C:/x/y.exe`, for
/// the paths a Windows command reports (the running server's image), so they
/// compare equal to the SFTP paths the installer keeps.
pub fn sftp_path_from_windows(native: &str) -> Option<String> {
    let native = native.trim();
    let b = native.as_bytes();
    if b.len() < 2 || !b[0].is_ascii_alphabetic() || b[1] != b':' {
        return None;
    }
    if b.len() > 2 && b[2] != b'\\' && b[2] != b'/' {
        return None;
    }
    let sftp = format!("/{}", native.replace('\\', "/"));
    Some(if sftp.len() > 4 {
        sftp.trim_end_matches('/').to_string()
    } else {
        sftp
    })
}

fn strip_exe(name: &str) -> &str {
    match name.len().checked_sub(4) {
        Some(i) if name.is_char_boundary(i) && name[i..].eq_ignore_ascii_case(".exe") => &name[..i],
        _ => name,
    }
}

pub fn dialect_from_path(path: &str) -> Option<(u32, u32)> {
    let name = strip_exe(path.rsplit(['/', '\\']).next()?);
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
            let paths = remote_paths_on(RemotePlatform::Windows, "/C:/Users/me", c, p);
            assert_eq!(dialect_from_path(&paths.binary), Some((c, p)));
        }
    }

    #[test]
    fn windows_architectures_map_to_the_published_assets() {
        for raw in ["AMD64", "amd64", "x64", "EM64T", " AMD64\r\n"] {
            assert_eq!(
                asset_for_windows_arch(raw).unwrap(),
                ASSET_WINDOWS_X86_64,
                "{raw:?}"
            );
        }
        assert_eq!(
            asset_for_windows_arch("ARM64").unwrap(),
            ASSET_WINDOWS_AARCH64
        );
        for raw in ["x86", "IA64", "ARM"] {
            let err = asset_for_windows_arch(raw).unwrap_err();
            assert!(
                matches!(err, UnsupportedTarget::UnknownWindowsMachine { .. }),
                "{raw}: {err:?}"
            );
            assert_eq!(err.raw(), raw);
        }
        assert!(matches!(
            asset_for_windows_arch("  ").unwrap_err(),
            UnsupportedTarget::Unparseable { .. }
        ));
    }

    /// Git for Windows, MSYS2 and Cygwin all answer `uname` with a system
    /// name that is not `Linux`, and what they sit on is still Windows.
    #[test]
    fn a_posix_layer_on_windows_is_recognised_as_windows() {
        for raw in [
            "MINGW64_NT-10.0-19045 x86_64",
            "MSYS_NT-10.0-22631 x86_64",
            "CYGWIN_NT-10.0 x86_64",
            "Windows_NT x86_64",
        ] {
            assert!(uname_reports_windows(raw), "{raw}");
            assert!(
                matches!(
                    asset_for_uname(raw).unwrap_err(),
                    UnsupportedTarget::UnsupportedSystem { .. }
                ),
                "{raw}: uname alone must not pick a Windows server"
            );
        }
        for raw in ["Linux x86_64", "Darwin arm64", "FreeBSD amd64", ""] {
            assert!(!uname_reports_windows(raw), "{raw}");
        }
    }

    #[test]
    fn windows_remote_paths_live_under_local_app_data_and_end_in_exe() {
        let p = remote_paths_on(RemotePlatform::Windows, "/C:/Users/me/", 3, 4);
        assert_eq!(p.platform, RemotePlatform::Windows);
        assert_eq!(p.bin_dir, "/C:/Users/me/AppData/Local/tty7/bin");
        assert_eq!(
            p.binary,
            "/C:/Users/me/AppData/Local/tty7/bin/tty7-server-c3p4.exe"
        );
        assert_eq!(
            p.temp,
            "/C:/Users/me/AppData/Local/tty7/bin/.tty7-server-c3p4.tmp.exe"
        );
        assert_eq!(
            p.dir_chain,
            vec![
                "/C:/Users/me/AppData",
                "/C:/Users/me/AppData/Local",
                "/C:/Users/me/AppData/Local/tty7",
                "/C:/Users/me/AppData/Local/tty7/bin",
            ]
        );
        assert_eq!(
            remote_paths("/home/me", 3, 4).platform,
            RemotePlatform::Unix,
            "the unix spelling is unchanged and says so"
        );
    }

    #[test]
    fn an_sftp_home_says_which_platform_it_is_on() {
        for home in ["/C:/Users/me", "/c:/Users/me", "/D:", "/C:/"] {
            assert_eq!(
                RemotePlatform::of_sftp_home(home),
                RemotePlatform::Windows,
                "{home}"
            );
        }
        for home in ["/home/me", "/", "/C", "/CD:/x", "C:/Users/me", "/Users/c:"] {
            assert_eq!(
                RemotePlatform::of_sftp_home(home),
                RemotePlatform::Unix,
                "{home}"
            );
        }
    }

    #[test]
    fn windows_paths_convert_between_sftp_and_native_spelling() {
        assert_eq!(
            windows_native_path("/C:/Users/me/AppData/Local/tty7/bin/x.exe").as_deref(),
            Some(r"C:\Users\me\AppData\Local\tty7\bin\x.exe")
        );
        assert_eq!(windows_native_path("/D:").as_deref(), Some(r"D:\"));
        assert_eq!(windows_native_path("/home/me/x"), None);

        assert_eq!(
            sftp_path_from_windows(r"C:\Users\me\AppData\Local\tty7\bin\x.exe").as_deref(),
            Some("/C:/Users/me/AppData/Local/tty7/bin/x.exe")
        );
        assert_eq!(
            sftp_path_from_windows("C:/Users/me/\r\n").as_deref(),
            Some("/C:/Users/me")
        );
        assert_eq!(sftp_path_from_windows(r"C:\").as_deref(), Some("/C:/"));
        for not_a_drive in [
            "",
            "/home/me",
            r"\\server\share\x.exe",
            "C",
            "CD:\\x",
            "C:x",
        ] {
            assert_eq!(sftp_path_from_windows(not_a_drive), None, "{not_a_drive:?}");
        }

        let sftp = "/C:/Users/me/AppData/Local/tty7/bin/tty7-server-c3p4.exe";
        assert_eq!(
            sftp_path_from_windows(&windows_native_path(sftp).unwrap()).as_deref(),
            Some(sftp)
        );
    }

    #[test]
    fn dialects_are_recoverable_from_a_windows_install_path() {
        assert_eq!(
            dialect_from_path(r"C:\Users\me\AppData\Local\tty7\bin\tty7-server-c3p4.exe"),
            Some((3, 4))
        );
        assert_eq!(
            dialect_from_path("/C:/Users/me/AppData/Local/tty7/bin/tty7-server-c3p4.EXE"),
            Some((3, 4))
        );
        assert_eq!(dialect_from_path(r"C:\tools\tty7-server.exe"), None);
    }

    #[test]
    fn windows_asset_names_are_distinct_and_intern_to_themselves() {
        assert_eq!(ASSET_WINDOWS_X86_64, "tty7-server-windows-x86_64.exe");
        assert_eq!(ASSET_WINDOWS_AARCH64, "tty7-server-windows-aarch64.exe");
        let all = [
            ASSET_LINUX_X86_64,
            ASSET_LINUX_AARCH64,
            ASSET_MACOS_X86_64,
            ASSET_MACOS_AARCH64,
            ASSET_WINDOWS_X86_64,
            ASSET_WINDOWS_AARCH64,
        ];
        for a in all {
            assert_eq!(a, interned(a));
            for b in all {
                assert!(a == b || !a.contains(b), "{a} contains {b}");
            }
        }
    }
}

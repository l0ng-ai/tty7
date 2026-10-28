//! macOS default-terminal integration.
//!
//! LaunchServices has no system-wide "default terminal" switch. Instead it
//! remembers a handler per document type and URL scheme, which is exactly the
//! narrow promise tty59 can make: folders, runnable local files, SSH links, and
//! man-page links.

use std::path::PathBuf;

#[cfg(target_os = "macos")]
pub const BUNDLE_ID: &str = "com.github.tty59";
#[cfg(target_os = "macos")]
const URL_SCHEMES: &[&str] = &["ssh", "x-man-page"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalOpen {
    Folder(PathBuf),
    Runnable(PathBuf),
    Ssh(tty59_core::core::ssh_profile::QuickConnect),
    /// `man [section] page`, from `x-man-page://page` or the sectioned
    /// `x-man-page://section/page` that Apple's own man-page links use.
    ManPage {
        section: Option<String>,
        page: String,
    },
}

/// Parses the strings supplied by gpui's `Application::on_open_urls`. Finder
/// represents document opens as `file:` URLs on macOS, so paths and custom
/// schemes deliberately share this one entry point.
pub fn parse_open_url(raw: &str) -> Result<ExternalOpen, String> {
    let url = url::Url::parse(raw).map_err(|error| format!("invalid URL: {error}"))?;
    match url.scheme() {
        "file" => {
            let path = url
                .to_file_path()
                .map_err(|_| "the file URL does not name a local path".to_string())?;
            if path.is_dir() {
                Ok(ExternalOpen::Folder(path))
            } else {
                Ok(ExternalOpen::Runnable(path))
            }
        }
        "ssh" => quick_connect_from(&url).map(ExternalOpen::Ssh),
        "x-man-page" => man_page_from(&url),
        scheme => Err(format!("unsupported URL scheme: {scheme}")),
    }
}

/// Reads the authority `Url` has already validated rather than handing the raw
/// string to [`tty59_core::core::ssh_profile::parse_quick_connect`]. That parser
/// takes a bare `user@host:port` typed into Quick Connect, so everything a URL
/// may carry past the authority lands in the wrong field: `ssh://h:22/` parses
/// its port as `22/` and is dropped, and `ssh://h/srv` becomes the host
/// `h/srv`.
fn quick_connect_from(
    url: &url::Url,
) -> Result<tty59_core::core::ssh_profile::QuickConnect, String> {
    let host = match url.host() {
        // `Host`'s own `Display` brackets an IPv6 address for use in a URL.
        // `QuickConnect` holds the bare form and brackets it again when it
        // writes one out, so unwrap it here.
        Some(url::Host::Ipv6(address)) => address.to_string(),
        Some(host) => host.to_string(),
        None => return Err("the SSH URL has no host".to_string()),
    };
    if host.is_empty() {
        return Err("the SSH URL has no host".to_string());
    }
    let user = decode(url.username(), "user name")?;
    Ok(tty59_core::core::ssh_profile::QuickConnect {
        user: (!user.is_empty()).then_some(user),
        host,
        // Port 0 is what `Url` gives back for `:0`, and no SSH server listens
        // there; the Quick Connect parser rejects it the same way.
        port: url.port().filter(|port| *port != 0),
    })
}

/// Apple writes these two ways: `x-man-page://ls`, and `x-man-page://3/printf`
/// where the authority is the *section*. Taking the host as the page name
/// turns the second form into `man 3`, which asks the user what page they
/// wanted.
fn man_page_from(url: &url::Url) -> Result<ExternalOpen, String> {
    let mut parts = Vec::new();
    if let Some(host) = url.host_str() {
        parts.push(decode(host, "page name")?);
    }
    for segment in url.path().split('/') {
        parts.push(decode(segment, "page name")?);
    }
    parts.retain(|part| !part.is_empty());
    let mut parts = parts.into_iter();
    let first = parts
        .next()
        .ok_or_else(|| "the man-page URL has no page name".to_string())?;
    Ok(match parts.next() {
        Some(page) => ExternalOpen::ManPage {
            section: Some(first),
            page,
        },
        None => ExternalOpen::ManPage {
            section: None,
            page: first,
        },
    })
}

fn decode(raw: &str, what: &str) -> Result<String, String> {
    percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .map(|decoded| decoded.into_owned())
        .map_err(|_| format!("the URL has a {what} that is not UTF-8"))
}

#[cfg(target_os = "macos")]
pub fn set_as_default_terminal() -> Result<(), String> {
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;

    // LaunchServices is a subframework of CoreServices. Linking the parent is
    // portable across both the full Xcode SDK and Command Line Tools SDK; the
    // latter has no standalone `LaunchServices.framework` linker path.
    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn LSSetDefaultRoleHandlerForContentType(
            content_type: core_foundation::string::CFStringRef,
            role: u32,
            handler: core_foundation::string::CFStringRef,
        ) -> i32;
        fn LSSetDefaultHandlerForURLScheme(
            scheme: core_foundation::string::CFStringRef,
            handler: core_foundation::string::CFStringRef,
        ) -> i32;
    }

    // This is the conventional macOS definition of "default terminal":
    // iTerm2 makes the same `public.unix-executable` / `Shell` association. A
    // folder is a Viewer/Editor item rather than something a terminal executes,
    // so asking LaunchServices to assign its Shell role is invalid (-50).
    const ROLE_SHELL: u32 = 0x0000_0008;
    let handler = CFString::new(BUNDLE_ID);
    let executable = CFString::new("public.unix-executable");
    let status = unsafe {
        LSSetDefaultRoleHandlerForContentType(
            executable.as_concrete_TypeRef(),
            ROLE_SHELL,
            handler.as_concrete_TypeRef(),
        )
    };
    if status != 0 {
        return Err(format!(
            "could not set the Unix executable handler (LaunchServices status {status})"
        ));
    }
    for scheme in URL_SCHEMES {
        let scheme = CFString::new(scheme);
        let status = unsafe {
            LSSetDefaultHandlerForURLScheme(
                scheme.as_concrete_TypeRef(),
                handler.as_concrete_TypeRef(),
            )
        };
        if status != 0 {
            return Err(format!(
                "could not set the {scheme} URL handler (LaunchServices status {status})"
            ));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn set_as_default_terminal() -> Result<(), String> {
    Err("setting a default terminal is only available on macOS".to_string())
}

#[cfg(test)]
mod tests {
    use super::{ExternalOpen, parse_open_url};

    #[test]
    fn parses_finder_file_urls_and_percent_decodes_paths() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a folder");
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            parse_open_url(&url::Url::from_file_path(&path).unwrap().to_string()).unwrap(),
            ExternalOpen::Folder(path)
        );
    }

    fn ssh(raw: &str) -> tty59_core::core::ssh_profile::QuickConnect {
        let ExternalOpen::Ssh(ssh) = parse_open_url(raw).unwrap() else {
            panic!("expected SSH request from {raw:?}");
        };
        ssh
    }

    #[test]
    fn parses_ssh_authority_and_port() {
        let parsed = ssh("ssh://me@example.test:2200");
        assert_eq!(parsed.user.as_deref(), Some("me"));
        assert_eq!(parsed.host, "example.test");
        assert_eq!(parsed.port, Some(2200));
    }

    /// The authority is read off the parsed URL, so what follows it cannot
    /// leak into the host or the port the way it does when the raw string is
    /// re-parsed as a bare `user@host:port`.
    #[test]
    fn ssh_paths_and_trailing_slashes_stay_out_of_the_authority() {
        let trailing = ssh("ssh://me@example.test:2200/");
        assert_eq!(trailing.host, "example.test");
        assert_eq!(trailing.port, Some(2200));

        let with_path = ssh("ssh://example.test/srv/app");
        assert_eq!(with_path.host, "example.test");
        assert_eq!(with_path.port, None);

        // `Url` lowercases the scheme, so the arm fires whatever case the
        // link was written in.
        assert_eq!(ssh("SSH://example.test").host, "example.test");
    }

    #[test]
    fn ssh_decodes_the_user_and_unwraps_ipv6() {
        assert_eq!(
            ssh("ssh://user%40corp@example.test").user.as_deref(),
            Some("user@corp")
        );
        let numeric = ssh("ssh://[fe80::1]:2200");
        assert_eq!(numeric.host, "fe80::1");
        assert_eq!(numeric.port, Some(2200));
    }

    #[test]
    fn ssh_without_a_host_is_rejected() {
        assert!(parse_open_url("ssh://").is_err());
    }

    #[test]
    fn parses_man_page_host_or_path() {
        assert_eq!(
            parse_open_url("x-man-page://printf").unwrap(),
            ExternalOpen::ManPage {
                section: None,
                page: "printf".into()
            }
        );
        assert_eq!(
            parse_open_url("x-man-page:/ls").unwrap(),
            ExternalOpen::ManPage {
                section: None,
                page: "ls".into()
            }
        );
    }

    /// Apple's sectioned form puts the section in the authority. Reading the
    /// host as the page name ran `man 3` and asked what page was wanted.
    #[test]
    fn a_sectioned_man_page_keeps_its_page_name() {
        assert_eq!(
            parse_open_url("x-man-page://3/printf").unwrap(),
            ExternalOpen::ManPage {
                section: Some("3".into()),
                page: "printf".into()
            }
        );
    }

    #[test]
    fn a_man_page_url_with_no_name_is_rejected() {
        assert!(parse_open_url("x-man-page://").is_err());
    }
}

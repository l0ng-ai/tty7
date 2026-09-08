//! The one place a path is shortened to start from `~`.
//!
//! Three rows shorten a path for display — the Info panel's cwd, the tab
//! strip's title, the home picker's recent list — and all three used to spell
//! the check their own way: read `HOME`, compare byte prefixes. On Windows
//! that missed twice over. `HOME` is often unset there (the variable is
//! `USERPROFILE`), and even a set home never matched a pane cwd that spells
//! itself with the other separator or different case — a PowerShell pane
//! reports `C:/Users/x/…` while `USERPROFILE` is `C:\Users\x` (#544).
//!
//! The comparison below normalizes both sides to `/` and folds case before
//! comparing, so every spelling of the same directory shortens. What comes
//! back is for reading only — a `~`-rooted path is spelled the way `~` paths
//! are spelled everywhere, with `/`. Nothing feeds it back to an API: the
//! Info panel's Copy Path and Reveal both carry the untouched `PathBuf`, and
//! the tab strip and picker only ever draw it.
//!
//! *Which* home a path is measured against is the caller's to say, because
//! only the caller knows which machine the path is on. A pane, a workspace
//! row or a tab title can name a directory on another host, and this
//! machine's `$HOME` answers for nothing over there: `/home/deploy/app` on a
//! server shortened to `~/app` on a laptop that happens to log in as
//! `deploy`, and stayed long on one that does not, so the `~` meant the
//! wrong machine either way (#580). [`home_for_host`] is where that question
//! is answered — the same borrow #568 took out of the file-link resolver.

use crate::ui::host_ops::HostId;
use gpui::App;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// The directory `~` stands for on the machine tty7 is running on, or `None`
/// when it won't say.
///
/// `USERPROFILE` is the fallback rather than the only source on Windows so
/// the MSYS/Git-Bash environments that do export `HOME` keep working, and
/// the two agree in every case that matters.
pub(crate) fn local_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))
        .map(PathBuf::from)
}

/// The directory `~` stands for in a path that lives on `host`.
///
/// A remote host reports its home during the control handshake and it is
/// kept per host in [`HostLinks`](crate::ui::remote_connect::HostLinks), so
/// this costs a map lookup and never a round trip. `None` means nothing here
/// can say — no link to that host yet — and a path measured against nothing
/// is shown whole, which is the honest answer and the one #568 settled on
/// for the same question about file links.
pub(crate) fn home_for_host(cx: &App, host: HostId) -> Option<PathBuf> {
    match host.is_local() {
        true => local_home(),
        false => crate::ui::remote_connect::HostLinks::home(cx, host),
    }
}

/// `/`-spelled, case-folded, trailing separators dropped — the form two
/// paths are compared in, never the form either is shown in. Case folding is
/// ASCII-only: drive letters and the ASCII half of real paths are where
/// Windows case instability actually lives, and a full Unicode fold would
/// fold a Unix filename that happened to differ only in case into a match it
/// is not.
fn normalized(s: &str) -> String {
    s.replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

/// Re-spells a path on **this** machine with the separators this OS expects,
/// so the Win32 shell will take it.
///
/// `IShellFolder::ParseDisplayName` bails out with `E_INVALIDARG` on a
/// mixed-separator path — a forward-slash prefix joined with backslash
/// entries — and `reveal_path` swallows that failure (it only logs), so
/// handing it native separators is what makes "open folder" actually open.
///
/// The rule itself lives in [`tty7_core::core::path_spelling`], next to the
/// prefix rule the SCM caches need, because a path spelled two ways is one
/// problem and it must not have two answers in two crates. This is the
/// separators half on its own: a `\\?\` path is already something
/// `ParseDisplayName` will not take, and re-spelling one here would be a
/// silent change of subject rather than a fix.
///
/// **Only for paths on the machine this window runs on.** A remote host's
/// `/home/u/src` is already native over there; re-spelling it would put a
/// path on the clipboard that names nothing on either machine. Every caller
/// sits behind a locality check for that reason.
pub(crate) fn native_separators(path: &Path) -> Cow<'_, Path> {
    tty7_core::core::path_spelling::native_separators(path)
}

/// Shortens `path` to start from `~` when it is (inside) `home` — the home
/// directory of the machine `path` is on, not of this one.
///
/// A `None` home is a path this process cannot place: a pane on a host with
/// no link, or one whose shell has ssh'd somewhere tty7 never spoke to. It
/// comes back untouched rather than measured against a home that belongs to
/// somebody else (#580).
///
/// The `~` replaces the home prefix and the remainder is re-spelled with
/// `/` separators (a `~\work` hybrid reads as a root the path never had),
/// but its case and component spelling are the path's own. A path that is
/// exactly home shortens to `~`, and one whose next character is not a
/// separator (`/home/xavier` under `/home/xa`) does not match at all.
pub(crate) fn abbreviate_home<'a>(path: &'a str, home: Option<&Path>) -> Cow<'a, str> {
    let Some(home) = home else {
        return Cow::Borrowed(path);
    };
    abbreviate_under(path, &home.to_string_lossy())
}

/// `abbreviate_home` with the home as a plain string, so the tests below can
/// pin one — including a Windows-spelled home on a Unix build, which no
/// `Path` on that platform round-trips.
fn abbreviate_under<'a>(path: &'a str, home: &str) -> Cow<'a, str> {
    let home_norm = normalized(home);
    if home_norm.is_empty() {
        return Cow::Borrowed(path);
    }
    let path_norm = normalized(path);
    if path_norm == home_norm {
        return Cow::Owned("~".to_string());
    }
    if !path_norm.starts_with(&home_norm) {
        return Cow::Borrowed(path);
    }
    // The byte after the home prefix has to be a separator. Where it sits in
    // the *original* string is derived from the normalized one rather than
    // from `home.len()`: a trailing-separator difference (`C:\Users\xa\`
    // recorded as home) makes the two lengths disagree, and slicing by the
    // wrong one can split a UTF-8 boundary. Separator and case substitutions
    // preserve byte length, so the boundary found in the normalized string is
    // the boundary in the original. The remainder is re-spelled with `/`:
    // `~\work` reads as a root the path never had.
    let boundary = home_norm.len();
    if !path_norm[boundary..].starts_with('/') {
        return Cow::Borrowed(path);
    }
    Cow::Owned(format!("~/{}", path[boundary + 1..].replace('\\', "/")))
}

/// Splits a path into the part that may be eaten by truncation and the
/// segment that must survive it.
///
/// A path identifies a thing by its *last* segment, and plain end-truncation
/// eats exactly that: a deep checkout reads "/private/tmp/claude-501…" and
/// tells you nothing. Drawn as two elements — a head that shrinks first and
/// a leaf that shrinks last — the filename stays legible however narrow the
/// row gets, the way a file manager shows a path. `head + leaf` rejoins into
/// the original string, so nothing is invented at either end.
pub(crate) fn split_path_leaf(s: &str) -> (String, String) {
    // The larger of the two separator positions, not cfg-gated by platform:
    // the Info panel shows remote paths too, so a Windows build describes
    // Unix paths and vice versa — and a mixed-spelling path (`C:\Users\dev/
    // project`, which agent-reported cwds arrive as) still cuts at its true
    // leaf (#544). A Unix filename containing a literal `\` loses a shorter
    // leaf; head + leaf still rejoins exactly, so the cost is decorative.
    let leaf_at = s.rfind('/').max(s.rfind('\\'));
    match leaf_at {
        // Keep the separator with the head: "~/a/b/" + "c" rejoins exactly.
        Some(i) if i + 1 < s.len() => (s[..=i].to_string(), s[i + 1..].to_string()),
        _ => (String::new(), s.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_under_home_shortens_to_tilde() {
        assert_eq!(abbreviate_under("/home/xa", "/home/xa"), "~");
        assert_eq!(abbreviate_under("/home/xa/work", "/home/xa"), "~/work");
        // A home recorded with its trailing separator matches the same paths,
        // and the slice that follows it is found in the *normalized* string,
        // so the two spellings cannot disagree about where the cut is.
        assert_eq!(abbreviate_under("/home/xa/work", "/home/xa/"), "~/work");
        // A longer name that merely starts with home is not under it.
        assert_eq!(abbreviate_under("/home/xavier", "/home/xa"), "/home/xavier");
        assert_eq!(abbreviate_under("/var/tmp", "/home/xa"), "/var/tmp");
        // A home the environment reports as empty leaves every path alone,
        // rather than turning `/` into `~`.
        assert_eq!(abbreviate_under("/var/tmp", ""), "/var/tmp");
        assert_eq!(abbreviate_under("/var/tmp", "/"), "/var/tmp");
    }

    #[test]
    fn separators_and_case_do_not_change_what_counts_as_home() {
        // The Windows miss: USERPROFILE spells `C:\Users\xa`, a PowerShell
        // pane reports `C:/Users/xa/…`, and neither matched the other.
        let home = "C:\\Users\\xa";
        assert_eq!(abbreviate_under("C:/Users/xa/work", home), "~/work");
        assert_eq!(abbreviate_under("c:\\Users\\XA\\work", home), "~/work");
        assert_eq!(abbreviate_under("C:\\Users\\xa", home), "~");
        // The remainder is re-spelled with `/`, case untouched.
        assert_eq!(abbreviate_under("C:/Users/xa/Mix\\ed", home), "~/Mix/ed");
    }

    /// The #580 borrow: a path whose machine is unknown keeps its full
    /// spelling instead of being read against this one's home.
    #[test]
    fn a_path_with_no_home_to_measure_against_is_left_alone() {
        let deploy = "/home/deploy/app";
        assert_eq!(abbreviate_home(deploy, None), deploy);
        // The same path *does* shorten once the host that owns it has said
        // what its home is.
        assert_eq!(
            abbreviate_home(deploy, Some(Path::new("/home/deploy"))),
            "~/app"
        );
        // And this machine's home is not offered as a stand-in: a laptop
        // that logs in as `deploy` used to shorten a server's path by
        // accident, purely because the two names matched.
        assert_eq!(
            abbreviate_home(deploy, Some(Path::new("/Users/thomas"))),
            deploy
        );
    }

    #[test]
    fn a_non_ascii_component_is_sliced_on_a_character_boundary() {
        // The cut is taken from the normalized string; `replace` and the
        // ASCII case fold both preserve byte length, so a multi-byte
        // component before or after the home prefix cannot move it.
        assert_eq!(abbreviate_under("/home/日本/work", "/home/日本"), "~/work");
        assert_eq!(abbreviate_under("/home/xa/日本語", "/home/xa"), "~/日本語");
    }

    #[test]
    fn native_separators_passes_through_a_path_with_nothing_to_fix() {
        // No forward slashes → nothing to rewrite, and no allocation: the
        // borrowed path is the caller's, handed back untouched.
        assert!(matches!(
            native_separators(Path::new("README.md")),
            Cow::Borrowed(_)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn native_separators_rewrites_forward_slashes_to_backslashes_on_windows() {
        // The bug: a repo root from `git rev-parse` (forward slashes) joined
        // with backslash-joined entries yields a mixed path, which
        // `ParseDisplayName` rejects. Every `/` must become `\`.
        assert_eq!(
            native_separators(Path::new("D:/code/tty7\\skills")),
            Path::new("D:\\code\\tty7\\skills")
        );
        assert_eq!(
            native_separators(Path::new("D:/code/tty7")),
            Path::new("D:\\code\\tty7")
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_separators_leaves_unc_and_backslash_paths_alone() {
        // A UNC path (`\\wsl$\…`, `\\?\…`) or an already-native path has no
        // `/`, so it passes through borrowed — the replace is a no-op and
        // must not allocate, nor touch the leading `\\`.
        for p in [
            "\\\\wsl$\\Ubuntu\\home",
            "\\\\?\\C:\\code",
            "C:\\code\\tty7",
        ] {
            let got = native_separators(Path::new(p));
            assert_eq!(got.as_ref(), Path::new(p), "{p:?}");
            assert!(matches!(got, Cow::Borrowed(_)), "{p:?} should not allocate");
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_separators_keeps_a_name_a_string_cannot_hold() {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        // `0xD800` is a lone high surrogate — legal in an NTFS name, and not
        // representable in a Rust `str`. Rewriting through
        // `to_string_lossy` would swap it for `U+FFFD` and hand back a path
        // naming a *different* file; because `reveal_path` only logs its
        // failures, that reads to the user as the same silent no-op this fix
        // is here to remove. Working on the UTF-16 units keeps the name.
        let raw: Vec<u16> = "C:/a"
            .encode_utf16()
            .chain([0xD800])
            .chain("/b".encode_utf16())
            .collect();
        let path = PathBuf::from(OsString::from_wide(&raw));
        let want: Vec<u16> = "C:\\a"
            .encode_utf16()
            .chain([0xD800])
            .chain("\\b".encode_utf16())
            .collect();
        assert_eq!(
            native_separators(&path)
                .as_os_str()
                .encode_wide()
                .collect::<Vec<_>>(),
            want
        );
        // The round-trip this replaced really did destroy it.
        assert!(path.to_string_lossy().contains('\u{FFFD}'));
    }

    #[cfg(not(windows))]
    #[test]
    fn native_separators_is_a_no_op_off_windows() {
        // On Unix the OS separator is `/`; a path that happens to contain
        // backslashes is legitimate and must be left alone.
        for p in ["/home/u/tty7", "C:\\Users\\dev", "mixed/path\\here"] {
            let got = native_separators(Path::new(p));
            assert_eq!(got.as_ref(), Path::new(p), "{p:?}");
            assert!(matches!(got, Cow::Borrowed(_)));
        }
    }

    #[test]
    fn the_head_and_leaf_rejoin_into_the_path_they_came_from() {
        for p in [
            "~/repo/tty7",
            "/private/tmp/claude-501/a-very-long-directory/and-another-level",
            "/",
            "relative",
            "",
            "C:\\Users\\dev\\project",
            "C:\\Users\\dev/project",
            "\\\\server\\share\\dir",
        ] {
            let (head, leaf) = split_path_leaf(p);
            assert_eq!(format!("{head}{leaf}"), p, "rejoining {p:?}");
        }
    }

    #[test]
    fn the_leaf_is_the_segment_that_names_the_directory() {
        let (head, leaf) = split_path_leaf("/a/b/c");
        assert_eq!((head.as_str(), leaf.as_str()), ("/a/b/", "c"));
        // A trailing slash has no leaf to keep, so the whole thing is head.
        let (head, leaf) = split_path_leaf("/a/b/");
        assert_eq!((head.as_str(), leaf.as_str()), ("", "/a/b/"));
        // Root is one segment with nothing before it.
        let (head, leaf) = split_path_leaf("/");
        assert_eq!((head.as_str(), leaf.as_str()), ("", "/"));
    }

    #[test]
    fn the_leaf_survives_windows_and_mixed_spellings() {
        // Backslash-native, the shape an agent-reported cwd arrives in.
        let (head, leaf) = split_path_leaf("C:\\Users\\dev\\project");
        assert_eq!(
            (head.as_str(), leaf.as_str()),
            ("C:\\Users\\dev\\", "project")
        );
        // Mixed separators cut at the *last* one of either kind.
        let (head, leaf) = split_path_leaf("C:\\Users\\dev/project");
        assert_eq!(
            (head.as_str(), leaf.as_str()),
            ("C:\\Users\\dev/", "project")
        );
        let (head, leaf) = split_path_leaf("C:/Users/dev\\project");
        assert_eq!(
            (head.as_str(), leaf.as_str()),
            ("C:/Users/dev\\", "project")
        );
        // A drive root has no leaf to keep.
        let (head, leaf) = split_path_leaf("C:\\");
        assert_eq!((head.as_str(), leaf.as_str()), ("", "C:\\"));
        // A UNC path splits at its last component, head keeping the share.
        let (head, leaf) = split_path_leaf("\\\\server\\share\\dir");
        assert_eq!(
            (head.as_str(), leaf.as_str()),
            ("\\\\server\\share\\", "dir")
        );
    }
}

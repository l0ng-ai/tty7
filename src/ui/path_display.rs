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

/// Normalizes a path's separators to what the local OS expects. On Windows the
/// shell's `IShellFolder::ParseDisplayName` bails out with `E_INVALIDARG` on a
/// mixed-separator path — a forward-slash prefix joined with backslash
/// entries. The forward slashes get in from two routes: the shell's PWD
/// (OSC 7 from Git Bash / MSYS bash reports `/`, and that string survives
/// `Path::ancestors()` when the file tree walks up to find `.git`), and
/// `git rev-parse --show-toplevel` from Git for Windows (MSYS2), which always
/// prints `/` regardless of the calling shell. `reveal_path` swallows that
/// failure (it only logs), so handing it native separators is what makes
/// "open folder" actually open.
pub(crate) fn native_separators(path: &Path) -> Cow<'_, Path> {
    #[cfg(windows)]
    if path.as_os_str().to_string_lossy().contains('/') {
        return Cow::Owned(path.as_os_str().to_string_lossy().replace('/', "\\").into());
    }
    Cow::Borrowed(path)
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
        for p in ["\\\\wsl$\\Ubuntu\\home", "\\\\?\\C:\\code", "C:\\code\\tty7"] {
            let got = native_separators(Path::new(p));
            assert_eq!(got.as_ref(), Path::new(p), "{p:?}");
            assert!(
                matches!(got, Cow::Borrowed(_)),
                "{p:?} should not allocate"
            );
        }
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
}

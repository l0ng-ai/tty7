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

use std::borrow::Cow;

/// The directory `~` stands for, or `None` when this machine won't say.
///
/// `USERPROFILE` is the fallback rather than the only source on Windows so
/// the MSYS/Git-Bash environments that do export `HOME` keep working, and
/// the two agree in every case that matters.
fn home_dir() -> Option<std::ffi::OsString> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))
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

/// Shortens `path` to start from `~` when it is (inside) the home directory.
///
/// The `~` replaces the home prefix and the remainder is re-spelled with
/// `/` separators (a `~\work` hybrid reads as a root the path never had),
/// but its case and component spelling are the path's own. A path that is
/// exactly home shortens to `~`, and one whose next character is not a
/// separator (`/home/xavier` under `/home/xa`) does not match at all.
pub(crate) fn abbreviate_home(path: &str) -> Cow<'_, str> {
    let Some(home) = home_dir() else {
        return Cow::Borrowed(path);
    };
    abbreviate_under(path, &home.to_string_lossy())
}

/// `abbreviate_home` with the home handed in rather than read from the
/// environment, so the tests below pin one without touching a process-global
/// the rest of the binary is also reading (`ui::home`'s own test sets `HOME`
/// and expects to see it, and everything runs in one process).
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

    #[test]
    fn a_non_ascii_component_is_sliced_on_a_character_boundary() {
        // The cut is taken from the normalized string; `replace` and the
        // ASCII case fold both preserve byte length, so a multi-byte
        // component before or after the home prefix cannot move it.
        assert_eq!(abbreviate_under("/home/日本/work", "/home/日本"), "~/work");
        assert_eq!(abbreviate_under("/home/xa/日本語", "/home/xa"), "~/日本語");
    }
}

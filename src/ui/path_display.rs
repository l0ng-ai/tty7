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
//! comparing, so every spelling of the same directory shortens. What is
//! returned keeps the path's own spelling — only the matched home prefix is
//! swapped for `~` — because the rest of the string is what a copy or a
//! tooltip shows, and it should read the way the pane wrote it.

use std::borrow::Cow;

/// The directory `~` stands for, or `None` when this machine won't say.
///
/// `USERPROFILE` is the fallback rather than the only source on Windows so
/// the MSYS/Git-Bash environments that do export `HOME` keep working, and
/// the two agree in every case that matters. In tests the process env is
/// shared with neighbours that legitimately read the real home (ssh_connect
/// writes a key into it), so the home is injected rather than mutated.
fn home_dir() -> Option<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(home) = test_home() {
        return home;
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))
        .map(std::path::PathBuf::from)
}

/// The home a test has pinned, shared by `home_dir` and `set_test_home`.
/// `Some(None)` means "no home" — a test for the unset case still needs the
/// override, or it would read the machine's real one.
#[cfg(test)]
fn test_home_slot() -> &'static std::sync::Mutex<Option<Option<std::path::PathBuf>>> {
    use std::sync::{Mutex, OnceLock};
    static HOME: OnceLock<Mutex<Option<Option<std::path::PathBuf>>>> = OnceLock::new();
    HOME.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn test_home() -> Option<Option<std::path::PathBuf>> {
    test_home_slot().lock().unwrap().clone()
}

#[cfg(test)]
fn set_test_home(home: Option<std::path::PathBuf>) {
    *test_home_slot().lock().unwrap() = Some(home);
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
    let home = home.to_string_lossy();
    let home_norm = normalized(&home);
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
    use std::sync::{Mutex, OnceLock};

    // The pinned home is process-global, so the tests that set it serialize
    // on one lock — a neighbour pinning a different home mid-assertion is the
    // only way these can lie. Tests elsewhere that read the *real* home
    // (ssh_connect writes a key into it) are untouched: they never call
    // set_test_home, and home_dir only consults the slot once a test set it.
    fn home_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn a_path_under_home_shortens_to_tilde() {
        let _g = home_lock();
        set_test_home(Some(std::path::PathBuf::from("/home/xa")));
        assert_eq!(abbreviate_home("/home/xa"), "~");
        assert_eq!(abbreviate_home("/home/xa/work"), "~/work");
        assert_eq!(abbreviate_home("/home/xavier"), "/home/xavier");
        assert_eq!(abbreviate_home("/var/tmp"), "/var/tmp");
    }

    #[test]
    fn separators_and_case_do_not_change_what_counts_as_home() {
        // The Windows miss: USERPROFILE spells `C:\Users\xa`, a PowerShell
        // pane reports `C:/Users/xa/…`, and neither matched the other.
        let _g = home_lock();
        set_test_home(Some(std::path::PathBuf::from("C:\\Users\\xa")));
        assert_eq!(abbreviate_home("C:/Users/xa/work"), "~/work");
        assert_eq!(abbreviate_home("c:\\Users\\XA\\work"), "~/work");
        assert_eq!(abbreviate_home("C:\\Users\\xa"), "~");
        // The remainder is re-spelled with `/`, case untouched.
        assert_eq!(abbreviate_home("C:/Users/xa/Mix\\ed"), "~/Mix/ed");
    }

    #[test]
    fn without_a_home_the_path_stands_as_spelled() {
        let _g = home_lock();
        set_test_home(None);
        assert_eq!(abbreviate_home("/any/where"), "/any/where");
    }
}

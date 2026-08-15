//! Scratch directories for tests, removed when the test ends.
//!
//! Every fixture here used to be `env::temp_dir().join(format!("tty7-x-{pid}"))`,
//! wiped on the way *in* and left behind on the way out. That reads as
//! self-cleaning and is not: the pid is in the name so two concurrent
//! `cargo test` runs cannot share a fixture, which also means a run never
//! finds the previous run's directory to wipe. One directory per fixture per
//! run, kept forever — this working copy had 25,639 of them.
//!
//! Dropping the guard is what removes the tree, so a test that panics still
//! cleans up: unwinding runs destructors, and the assertion failure is the
//! output that matters rather than the litter left beside it.

use std::path::{Path, PathBuf};

/// A directory under the system temp dir that removes itself when dropped.
///
/// Derefs to [`Path`], so it stands in for the `PathBuf` these fixtures used
/// to hand back: `root.join("x")` and `&root` both keep working.
///
/// Bind it to a name — `let root = temp_root("x")` — rather than calling a
/// method on it directly. A temporary would be dropped at the end of the
/// statement that made it, taking the directory with it before the test
/// looked at it.
pub struct TempRoot(PathBuf);

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for TempRoot {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempRoot {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// A fresh, empty `tty7-<name>-<pid>` directory that lasts as long as the
/// returned guard.
///
/// The pid stays in the name: it is what keeps two concurrent `cargo test`
/// runs off each other's fixture. It is the missing *removal*, not the
/// unique name, that made these accumulate.
pub fn temp_root(name: &str) -> TempRoot {
    let dir = std::env::temp_dir().join(format!("tty7-{name}-{}", std::process::id()));
    // Still wiped on the way in: a previous run that was killed hard enough
    // to skip its destructors must not leave a fixture behind that this one
    // would then read as its own.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    TempRoot(dir)
}

#[cfg(test)]
mod tests {
    use super::temp_root;

    #[test]
    fn a_scratch_directory_is_gone_once_its_guard_is() {
        let path = {
            let root = temp_root("testutil-selfcheck");
            let path = root.to_path_buf();
            assert!(path.is_dir(), "the fixture exists while the guard is held");
            std::fs::write(root.join("f"), b"x").unwrap();
            path
        };
        assert!(!path.exists(), "and is removed with the guard: {path:?}");
    }

    #[test]
    fn a_second_call_starts_from_an_empty_directory() {
        let first = temp_root("testutil-reuse");
        std::fs::write(first.join("stale"), b"x").unwrap();
        let path = first.to_path_buf();
        std::mem::forget(first); // simulate a run that never dropped its guard
        let again = temp_root("testutil-reuse");
        assert_eq!(*again, *path, "same name, same pid, same directory");
        assert!(!again.join("stale").exists(), "wiped on the way in");
    }
}

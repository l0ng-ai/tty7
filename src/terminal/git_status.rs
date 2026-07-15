//! A lightweight git snapshot for a pane's working directory — the current
//! branch and the working-tree diff size — rendered as the sidebar row's third
//! line (`⎇ feat/x  +6 −5`): each session fronted with its branch and change
//! count.
//!
//! Deliberately shell-out simple: one `git` invocation per field, run on a
//! background thread by the caller (see [`crate::terminal::view`]) so the UI
//! never blocks on a slow repo. Read-only — `GIT_OPTIONAL_LOCKS=0` keeps status
//! polling from ever taking `index.lock` and fighting a real git command the
//! user is running. Returns `None` when the cwd isn't inside a git work tree,
//! so the sidebar simply omits the line.

use std::path::Path;
use std::process::{Command, Stdio};

/// A pane's git snapshot: the branch it's on and how much the working tree has
/// changed against `HEAD`. `added`/`removed` sum the per-file line counts from
/// `git diff --numstat HEAD` (tracked staged + unstaged changes); binary files
/// and untracked files don't contribute a line count.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitStatus {
    /// The branch name (`main`, `feat/x`), or a short commit sha when the HEAD
    /// is detached. Never empty.
    pub branch: String,
    /// Lines added across the working tree vs `HEAD`.
    pub added: u32,
    /// Lines removed across the working tree vs `HEAD`.
    pub removed: u32,
}

/// Compute the git snapshot for `cwd`, or `None` when it isn't a git work tree
/// (or the path is gone). Blocking — call it on a background executor.
pub fn compute(cwd: &Path) -> Option<GitStatus> {
    if !cwd.exists() {
        return None;
    }
    let branch = branch_name(cwd)?;
    let (added, removed) = diff_numstat(cwd).unwrap_or((0, 0));
    Some(GitStatus {
        branch,
        added,
        removed,
    })
}

/// The current branch name, or a short sha for a detached HEAD. Doubles as the
/// "is this a git repo" gate: both probes failing (not a work tree) yields
/// `None`.
fn branch_name(cwd: &Path) -> Option<String> {
    // On a branch — even before the first commit — `symbolic-ref` names it.
    if let Some(out) = git(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        let name = out.trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    // Detached HEAD (or a rebase/bisect): fall back to the short commit sha.
    let sha = git(cwd, &["rev-parse", "--short", "HEAD"])?;
    let sha = sha.trim();
    (!sha.is_empty()).then(|| sha.to_string())
}

/// Sum added/removed lines across the working tree vs `HEAD` from
/// `git diff --numstat HEAD`. Binary files (`-\t-`) contribute nothing.
fn diff_numstat(cwd: &Path) -> Option<(u32, u32)> {
    let out = git(cwd, &["diff", "--numstat", "HEAD"])?;
    let mut added = 0u32;
    let mut removed = 0u32;
    for line in out.lines() {
        let mut fields = line.split('\t');
        if let Some(n) = fields.next().and_then(|s| s.parse::<u32>().ok()) {
            added += n;
        }
        if let Some(n) = fields.next().and_then(|s| s.parse::<u32>().ok()) {
            removed += n;
        }
    }
    Some((added, removed))
}

/// Run `git -C <cwd> <args>` and return stdout on success, `None` on a
/// non-zero exit or a missing `git`. `GIT_OPTIONAL_LOCKS=0` makes the read
/// truly read-only; stdin is nulled so a misconfigured git can't block on a
/// prompt.
fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tmp path that is not a git repo yields no status (and never panics).
    #[test]
    fn non_repo_is_none() {
        let dir = std::env::temp_dir().join("tty7-git-status-not-a-repo-xyz");
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(compute(&dir), None);
    }

    /// A path that doesn't exist is `None`, not a panic.
    #[test]
    fn missing_path_is_none() {
        assert_eq!(compute(Path::new("/no/such/tty7/path/here")), None);
    }

    /// This repo (the crate root is inside the tty7 work tree) reports a branch,
    /// exercising the real `git` probe end-to-end.
    #[test]
    fn own_repo_has_a_branch() {
        let here = env!("CARGO_MANIFEST_DIR");
        if let Some(status) = compute(Path::new(here)) {
            assert!(!status.branch.is_empty());
        }
        // If the crate is built outside a work tree (e.g. a vendored tarball),
        // `None` is the correct answer and the assertion above is skipped.
    }
}

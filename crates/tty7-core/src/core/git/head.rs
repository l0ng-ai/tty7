//! Which repository a directory is in, read off the disk.
//!
//! The sidebar asks this of every pane, and asking `git` meant three processes
//! a pane — each of which can fail for reasons that have nothing to do with the
//! directory. On macOS `/usr/bin/git` is a shim that hands off to whichever
//! Xcode is selected, and dies while that selection is mid-switch; every probe
//! in that window used to come back "not a repository" and pull its tab out of
//! its group. Nothing here spawns anything: the answer is a handful of small
//! files, which is all `git rev-parse` reads for it too.
//!
//! Only the *where*: root, home and the branch name. The line counts beside
//! the branch still come from `git diff --numstat`, the same CLI the diff views
//! read, so the two never disagree about a number.

use std::path::{Path, PathBuf};

/// Where a directory's repository is, as far as the files on disk say.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RepoHead {
    /// The working tree the directory is in — `git rev-parse --show-toplevel`.
    pub root: PathBuf,
    /// The main working tree a linked one belongs to; `root` otherwise.
    pub home: PathBuf,
    /// The checked-out branch, when HEAD names one plainly. `None` for
    /// anything `git` should spell instead — a detached HEAD, whose short sha
    /// `git` lengthens until it is unambiguous, and a reftable repository,
    /// whose HEAD file is a placeholder.
    pub branch: Option<String>,
}

/// The repository `cwd` is in, or `None` when it is in none.
///
/// A bare repository counts as none, and so does a `.git` directory itself —
/// `git rev-parse --show-toplevel` refuses both, and there is no working tree
/// for a tab to be grouped under. So does a directory that does not exist.
pub fn read_head(cwd: &Path) -> Option<RepoHead> {
    let options = gix_discover::upwards::Options::default().apply_environment();
    let (path, _trust) = gix_discover::upwards_opts(cwd, options).ok()?;
    let (git_dir, work_dir) = path.into_repository_and_work_tree_directories();
    let work_dir = work_dir?;
    // `git` answers with the physical path, and the root is the key the diff
    // views file their own `rev-parse` answer under: a pane that reached its
    // repository through a symlink has to land on the same spelling.
    let root = std::fs::canonicalize(&work_dir).ok()?;
    let git_dir = std::fs::canonicalize(&git_dir).ok()?;
    // Standing in the git dir itself: discovery walks up to the tree it
    // belongs to, but `git` refuses the question there, and so does this.
    if std::fs::canonicalize(cwd).is_ok_and(|cwd| cwd.starts_with(&git_dir)) {
        return None;
    }
    let home = home_of(&root, &git_dir);
    let branch = std::fs::read_to_string(git_dir.join("HEAD"))
        .ok()
        .and_then(|head| branch_of(&head));
    Some(RepoHead { root, home, branch })
}

/// The main working tree `root` belongs to, given its git directory.
///
/// A linked worktree's private git dir names the shared one in `commondir`;
/// that shared dir is the main tree's `.git`, whose parent is the home. A
/// shared dir with another name is a bare repository with worktrees hung off
/// it, and the repository itself is the home — the same rule `repo_home`
/// applies to what `rev-parse --git-common-dir` prints.
fn home_of(root: &Path, git_dir: &Path) -> PathBuf {
    let Ok(common) = std::fs::read_to_string(git_dir.join("commondir")) else {
        return root.to_path_buf();
    };
    let Ok(common) = std::fs::canonicalize(git_dir.join(common.trim_end_matches(['\n', '\r'])))
    else {
        return root.to_path_buf();
    };
    if common == git_dir {
        return root.to_path_buf();
    }
    match (common.file_name(), common.parent()) {
        (Some(name), Some(parent)) if name == ".git" => parent.to_path_buf(),
        _ => common,
    }
}

/// The branch a HEAD file names, spelled the way `git symbolic-ref --short`
/// would, or `None` where that is not a plain read.
fn branch_of(head: &str) -> Option<String> {
    let name = head
        .trim_end_matches(['\n', '\r'])
        .strip_prefix("ref: refs/heads/")?;
    // Reftable keeps HEAD only for older tools to trip over.
    (!name.is_empty() && name != ".invalid").then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::git::test_support::{PINS, one_spelling};

    fn git(cwd: &Path, args: &[&str]) -> Option<String> {
        let mut full = PINS.to_vec();
        full.extend_from_slice(args);
        let out = crate::core::git::git_output(cwd, &full).ok()?;
        out.success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    #[test]
    fn head_files_spell_branches_like_git() {
        assert_eq!(branch_of("ref: refs/heads/main\n").as_deref(), Some("main"));
        assert_eq!(
            branch_of("ref: refs/heads/feat/x\r\n").as_deref(),
            Some("feat/x")
        );
        assert_eq!(
            branch_of("0123456789abcdef0123456789abcdef01234567\n"),
            None
        );
        assert_eq!(branch_of("ref: refs/heads/.invalid\n"), None);
        assert_eq!(branch_of("ref: refs/remotes/origin/main\n"), None);
    }

    /// The whole point: every answer here is the one `git` gives, for a main
    /// tree, a subdirectory, a linked worktree and a plain directory.
    #[test]
    fn agrees_with_git_about_repos_and_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir(&main).unwrap();
        if git(&main, &["init", "--quiet"]).is_none() {
            return; // no git on this machine
        }
        git(&main, &["symbolic-ref", "HEAD", "refs/heads/trunk"]).unwrap();
        git(&main, &["commit", "--quiet", "--allow-empty", "-m", "one"]).unwrap();
        let sub = main.join("src/deep");
        std::fs::create_dir_all(&sub).unwrap();
        let linked = dir.path().join("linked");
        git(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "feat/x",
                linked.to_str().unwrap(),
            ],
        )
        .unwrap();

        let canon = |p: &Path| std::fs::canonicalize(p).unwrap();
        for (cwd, root, home, branch) in [
            (&main, &main, &main, "trunk"),
            (&sub, &main, &main, "trunk"),
            (&linked, &linked, &main, "feat/x"),
        ] {
            let got = read_head(cwd).unwrap_or_else(|| panic!("{} is a repo", cwd.display()));
            assert_eq!(got.root, canon(root), "root of {}", cwd.display());
            assert_eq!(got.home, canon(home), "home of {}", cwd.display());
            assert_eq!(got.branch.as_deref(), Some(branch));
            // In one spelling: `canonicalize` answers `\\?\C:\…` on Windows and
            // git `C:/…`. `probe_repo` folds the first into the local spelling
            // before anything keys on it.
            assert_eq!(
                one_spelling(&got.root),
                one_spelling(Path::new(
                    &git(cwd, &["rev-parse", "--show-toplevel"]).unwrap()
                )),
                "and git names the same root"
            );
        }

        git(&main, &["checkout", "--quiet", "--detach"]).unwrap();
        assert_eq!(
            read_head(&main).unwrap().branch,
            None,
            "git spells a detached HEAD"
        );

        assert_eq!(
            read_head(&main.join(".git")),
            None,
            "a git dir has no working tree"
        );
        assert_eq!(read_head(&dir.path().join("gone")), None);
    }
}

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

#[derive(Default, Clone)]
pub(crate) struct GitignoreChain {
    matchers: HashMap<PathBuf, Option<Arc<Gitignore>>>,
    /// `<repo>/.git/info/exclude`, per repository root.
    excludes: HashMap<PathBuf, Option<Arc<Gitignore>>>,
    /// `core.excludesFile`, or the XDG default. One per process.
    global: Option<Option<Arc<Gitignore>>>,
}

impl GitignoreChain {
    pub(crate) fn is_ignored(&mut self, path: &Path, is_dir: bool, root: &Path) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        // "It is not possible to re-include a file if a parent directory of
        // that file is excluded" — gitignore(5). git never descends into an
        // excluded directory, so no `!pattern` below one can bring anything
        // back. Applying each level to the whole path missed that: with
        // `build/` ignored at the root and `!keep.txt` inside `build/`,
        // `git check-ignore` says build/keep.txt is ignored and this said it
        // was not, so an expanded directory showed a file git would not.
        if parent != root && parent.starts_with(root) && self.is_ignored(parent, true, root) {
            return true;
        }
        let mut state = false;
        // git reads these below every `.gitignore`, so they are consulted
        // first and a `.gitignore` further down can still whitelist what they
        // exclude — checked against `git check-ignore`, which calls a file
        // `!both.txt` re-includes not ignored even with `both.txt` in
        // `info/exclude`.
        let global = self
            .global
            .get_or_insert_with(|| {
                let (gi, _err) = Gitignore::global();
                (gi.num_ignores() > 0 || gi.num_whitelists() > 0).then(|| Arc::new(gi))
            })
            .clone();
        let exclude = self
            .excludes
            .entry(root.to_path_buf())
            .or_insert_with(|| {
                let file = root.join(".git/info/exclude");
                if !file.is_file() {
                    return None;
                }
                // Rooted at the repository, not at `.git/info`, so its patterns
                // are read against the paths they are written about.
                let mut builder = GitignoreBuilder::new(root);
                builder.add(&file);
                builder.build().ok().map(Arc::new)
            })
            .clone();
        for gi in [global, exclude].into_iter().flatten() {
            match gi.matched(path, is_dir) {
                ignore::Match::Ignore(_) => state = true,
                ignore::Match::Whitelist(_) => state = false,
                ignore::Match::None => {}
            }
        }
        let mut chain: Vec<&Path> = parent
            .ancestors()
            .take_while(|a| a.starts_with(root))
            .collect();
        chain.reverse();
        for dir in chain {
            let gi = self
                .matchers
                .entry(dir.to_path_buf())
                .or_insert_with(|| {
                    let file = dir.join(".gitignore");
                    file.is_file().then(|| {
                        let (gi, _err) = Gitignore::new(&file);
                        Arc::new(gi)
                    })
                })
                .clone();
            let Some(gi) = gi else { continue };
            let Ok(rel) = path.strip_prefix(dir) else {
                continue;
            };
            match gi.matched(rel, is_dir) {
                ignore::Match::Ignore(_) => state = true,
                ignore::Match::Whitelist(_) => state = false,
                ignore::Match::None => {}
            }
        }
        state
    }

    /// Unused: the matcher sets are built whole rather than merged. Kept beside
    /// `len`/`is_empty`, which are the same story -- a spare accessor is better
    /// than half a type.
    #[allow(dead_code)]
    pub fn absorb(&mut self, other: Self) {
        self.matchers.extend(other.matchers);
    }

    pub fn clear(&mut self) {
        self.matchers.clear();
        self.excludes.clear();
        self.global = None;
    }

    /// Unused, like `absorb` above and for the same reason.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.matchers.len()
    }

    /// Unused, like `absorb` above and for the same reason.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.matchers.is_empty()
    }
}

#[cfg(test)]
mod tests {

    /// `.git/info/exclude` counts, and a `.gitignore` still outranks it.
    ///
    /// It is where a person puts an ignore they do not want in the shared
    /// file, so a tree that reads only `.gitignore` marks files git does not.
    /// Checked against `git check-ignore` on this layout, including the
    /// precedence: `!both.txt` in `.gitignore` re-includes a name that
    /// `info/exclude` lists, because git reads the exclude file below every
    /// `.gitignore`.
    ///
    /// The global `core.excludesFile` is the third source and is left to
    /// `Gitignore::global()`, which reads `GIT_CONFIG_GLOBAL` and the XDG
    /// path the way git does. It is not exercised here: proving it means
    /// setting an environment variable, and this suite runs in parallel in
    /// one process.
    #[test]
    fn a_repo_exclude_file_is_read_and_a_gitignore_still_wins() {
        let root = std::env::temp_dir().join(format!("tty7-exclude-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        std::fs::write(root.join(".git/info/exclude"), "secret.txt\nboth.txt\n").unwrap();
        std::fs::write(root.join(".gitignore"), "!both.txt\n").unwrap();
        for name in ["secret.txt", "both.txt", "normal.txt"] {
            std::fs::write(root.join(name), "x").unwrap();
        }

        let mut chain = GitignoreChain::default();
        let ignored = |chain: &mut GitignoreChain, name: &str| {
            chain.is_ignored(&root.join(name), false, &root)
        };

        assert!(
            ignored(&mut chain, "secret.txt"),
            "a name in .git/info/exclude is ignored"
        );
        assert!(
            !ignored(&mut chain, "both.txt"),
            "a .gitignore whitelist outranks the exclude file"
        );
        assert!(!ignored(&mut chain, "normal.txt"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Nothing under an excluded directory can be brought back.
    ///
    /// gitignore(5): "It is not possible to re-include a file if a parent
    /// directory of that file is excluded." git never descends into one, so a
    /// `!pattern` below it never runs. Applying each level of the chain to the
    /// whole path missed that, and an expanded `build/` showed a file git
    /// ignores.
    ///
    /// The three answers here are `git check-ignore`'s own, against this
    /// layout: a whitelist under an ordinary directory still wins, and one
    /// under an excluded directory does not.
    #[test]
    fn a_whitelist_under_an_excluded_directory_does_not_re_include() {
        let root = std::env::temp_dir().join(format!("tty7-reinclude-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::create_dir_all(root.join("keep")).unwrap();
        std::fs::write(root.join(".gitignore"), "*.log\nbuild/\n").unwrap();
        std::fs::write(root.join("keep/.gitignore"), "!important.log\n").unwrap();
        std::fs::write(root.join("build/.gitignore"), "!keep.txt\n").unwrap();

        let mut chain = GitignoreChain::default();
        let ignored = |chain: &mut GitignoreChain, rel: &str, is_dir: bool| {
            chain.is_ignored(&root.join(rel), is_dir, &root)
        };

        assert!(ignored(&mut chain, "build", true), "build/ is excluded");
        assert!(
            ignored(&mut chain, "build/keep.txt", false),
            "a file under an excluded directory cannot be re-included"
        );
        assert!(
            !ignored(&mut chain, "keep/important.log", false),
            "a whitelist under an ordinary directory still wins"
        );
        assert!(ignored(&mut chain, "keep/other.log", false));

        let _ = std::fs::remove_dir_all(&root);
    }

    use super::*;

    fn write_ignore(dir: &Path, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(".gitignore"), body).unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tty7-gitignore-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_deepest_match_wins() {
        let root = scratch("deepest");
        write_ignore(&root, "*.log\n");
        write_ignore(&root.join("keep"), "!important.log\n");

        let mut chain = GitignoreChain::default();
        assert!(chain.is_ignored(&root.join("a.log"), false, &root));
        assert!(chain.is_ignored(&root.join("keep/other.log"), false, &root));
        assert!(!chain.is_ignored(&root.join("keep/important.log"), false, &root));
        assert!(!chain.is_ignored(&root.join("a.txt"), false, &root));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn directory_only_patterns_need_is_dir() {
        let root = scratch("dironly");
        write_ignore(&root, "build/\n");

        let mut chain = GitignoreChain::default();
        assert!(chain.is_ignored(&root.join("build"), true, &root));
        assert!(!chain.is_ignored(&root.join("build"), false, &root));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clear_lets_an_edited_gitignore_take_effect() {
        let root = scratch("clear");
        write_ignore(&root, "*.log\n");

        let mut chain = GitignoreChain::default();
        assert!(chain.is_ignored(&root.join("a.log"), false, &root));

        write_ignore(&root, "*.tmp\n");
        assert!(
            chain.is_ignored(&root.join("a.log"), false, &root),
            "cached"
        );
        chain.clear();
        assert!(!chain.is_ignored(&root.join("a.log"), false, &root));
        assert!(chain.is_ignored(&root.join("a.tmp"), false, &root));

        let _ = std::fs::remove_dir_all(&root);
    }
}

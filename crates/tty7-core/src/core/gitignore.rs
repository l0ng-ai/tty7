use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::Gitignore;

#[derive(Default, Clone)]
pub(crate) struct GitignoreChain {
    matchers: HashMap<PathBuf, Option<Arc<Gitignore>>>,
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

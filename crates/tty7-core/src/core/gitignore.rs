use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::Gitignore;

#[derive(Default, Clone)]
pub struct GitignoreChain {
    matchers: HashMap<PathBuf, Option<Arc<Gitignore>>>,
}

impl GitignoreChain {
    pub fn is_ignored(&mut self, path: &Path, is_dir: bool, root: &Path) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
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

    pub fn absorb(&mut self, other: Self) {
        self.matchers.extend(other.matchers);
    }

    pub fn clear(&mut self) {
        self.matchers.clear();
    }

    pub fn len(&self) -> usize {
        self.matchers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.matchers.is_empty()
    }
}

#[cfg(test)]
mod tests {
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

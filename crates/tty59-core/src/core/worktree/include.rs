//! `.worktreeinclude`: the gitignored files a fresh checkout cannot run
//! without — `.env`, `*.local.*`, a machine's own settings — carried over
//! from the main checkout when a worktree is created.
//!
//! The rules: gitignore syntax, and a path is copied only if it matches *and*
//! git ignores it. A tracked
//! file is already in the checkout, and an untracked-but-unignored one would
//! show up as a spurious change there.
//!
//! Copies are copy-on-write where the filesystem can: `clonefile` on macOS
//! (whole directory trees in one call), `std::fs::copy` elsewhere, which is
//! `copy_file_range` — a reflink on btrfs and XFS — on Linux. A cloned
//! `node_modules` costs no disk until one side writes to it. Nothing is ever
//! symlinked: two worktrees sharing one dependency tree break each other the
//! moment their lockfiles disagree.
//!
//! A remote host has no copy of its own to ask for, so there the bytes go
//! through `Host::read_file` / `write_file` under a budget — enough for the
//! config files this is for, not for a dependency tree.

use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::host::Host;

pub const FILE: &str = ".worktreeinclude";

/// What a remote copy may move in total, and per file.
const REMOTE_BUDGET: u64 = 64 << 20;
const REMOTE_FILE_MAX: u64 = 16 << 20;
/// How many entries the walk under an ignored directory may look at.
const WALK_MAX: usize = 10_000;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Carried {
    /// Paths (relative, `/`-separated) now in the worktree.
    pub copied: Vec<String>,
    /// Paths that matched but did not make it, and why.
    pub skipped: Vec<(String, String)>,
}

/// Copy what `main_root`'s `.worktreeinclude` asks for into `worktree`.
pub fn carry(host: &dyn Host, main_root: &Path, worktree: &Path) -> Carried {
    let mut out = Carried::default();
    let Ok(bytes) = host.read_file(&host.join(main_root, FILE), 1 << 20) else {
        return out;
    };
    let text = String::from_utf8_lossy(&bytes);
    let rules = Rules::parse(&text);
    if rules.is_empty() {
        return out;
    }
    let picked = picks(host, main_root, &rules);
    let mut budget = REMOTE_BUDGET;
    for rel in picked {
        let src = join_rel(host, main_root, &rel);
        let dst = join_rel(host, worktree, &rel);
        if host.stat(&dst).is_ok() {
            continue;
        }
        let result = if host.id().is_local() {
            copy_local(&src, &dst)
        } else {
            copy_remote(host, &src, &dst, &mut budget)
        };
        match result {
            Ok(()) => out.copied.push(rel),
            Err(e) => out.skipped.push((rel, e)),
        }
    }
    out
}

struct Rules {
    matcher: Gitignore,
    /// Directory prefixes (`a/b/`) that anchored patterns point into. An
    /// ignored directory git reports whole is only walked when one of these
    /// leads inside it — a bare `.env` rule must not send the walk through
    /// every `node_modules`.
    anchors: Vec<String>,
}

impl Rules {
    fn parse(text: &str) -> Rules {
        let mut builder = GitignoreBuilder::new("");
        let mut anchors = Vec::new();
        for line in text.lines() {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let _ = builder.add_line(None, line);
            if line.starts_with('!') {
                continue;
            }
            let body = line.trim_start_matches('/');
            // Only a pattern with a slash before its last component is
            // anchored; a leading `**` matches anywhere.
            let Some(slash) = body.trim_end_matches('/').rfind('/') else {
                continue;
            };
            if body.starts_with("**") {
                continue;
            }
            let dir = &body[..=slash];
            let literal = dir.find(['*', '?', '[']).map_or(dir, |glob| {
                &dir[..dir[..glob].rfind('/').map_or(0, |s| s + 1)]
            });
            if !literal.is_empty() {
                anchors.push(literal.to_string());
            }
        }
        Rules {
            matcher: builder.build().unwrap_or_else(|_| Gitignore::empty()),
            anchors,
        }
    }

    fn is_empty(&self) -> bool {
        self.matcher.is_empty()
    }

    fn wants(&self, rel: &str, is_dir: bool) -> bool {
        self.matcher
            .matched_path_or_any_parents(rel, is_dir)
            .is_ignore()
    }

    /// Whether some anchored pattern reaches inside directory `dir/`.
    fn leads_into(&self, dir: &str) -> bool {
        self.anchors
            .iter()
            .any(|a| a.starts_with(dir) || dir.starts_with(a.as_str()))
    }
}

/// The ignored paths in `main_root` the rules select, outermost only.
fn picks(host: &dyn Host, main_root: &Path, rules: &Rules) -> Vec<String> {
    let Ok(out) = host.git(
        main_root,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
    ) else {
        return Vec::new();
    };
    if !out.success() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut walked = 0;
    for entry in out.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let entry = String::from_utf8_lossy(entry);
        let (rel, is_dir) = match entry.strip_suffix('/') {
            Some(dir) => (dir.to_string(), true),
            None => (entry.to_string(), false),
        };
        if rules.wants(&rel, is_dir) {
            found.push(rel);
        } else if is_dir && rules.leads_into(&format!("{rel}/")) {
            walk(host, main_root, &rel, rules, &mut found, &mut walked);
        }
    }
    // `--directory` also lists an untracked directory whose only contents are
    // ignored, though the directory itself is not. Git has the last word.
    let found = confirm_ignored(host, main_root, found);
    outermost(found)
}

fn walk(
    host: &dyn Host,
    root: &Path,
    dir: &str,
    rules: &Rules,
    found: &mut Vec<String>,
    walked: &mut usize,
) {
    let Ok(entries) = host.read_dir(&join_rel(host, root, dir), None) else {
        return;
    };
    for e in entries {
        *walked += 1;
        if *walked > WALK_MAX {
            return;
        }
        let rel = format!("{dir}/{}", e.name);
        let is_dir = e.is_dir && !e.is_symlink;
        if rules.wants(&rel, is_dir) {
            found.push(rel);
        } else if is_dir && rules.leads_into(&format!("{rel}/")) {
            walk(host, root, &rel, rules, found, walked);
        }
    }
}

fn confirm_ignored(host: &dyn Host, root: &Path, paths: Vec<String>) -> Vec<String> {
    if paths.is_empty() {
        return paths;
    }
    // `-z` needs `--stdin`, which `Host::git` cannot feed; unquoted lines it is.
    let mut args = vec!["-c", "core.quotePath=false", "check-ignore", "--"];
    args.extend(paths.iter().map(String::as_str));
    let Ok(out) = host.git(root, &args) else {
        return Vec::new();
    };
    let ignored: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    paths.into_iter().filter(|p| ignored.contains(p)).collect()
}

/// Drop every path that sits inside another one on the list.
fn outermost(mut paths: Vec<String>) -> Vec<String> {
    paths.sort();
    paths.dedup();
    let mut kept: Vec<String> = Vec::new();
    for p in paths {
        if !kept.iter().any(|k| p.starts_with(&format!("{k}/"))) {
            kept.push(p);
        }
    }
    kept
}

fn join_rel(host: &dyn Host, root: &Path, rel: &str) -> PathBuf {
    rel.split('/')
        .fold(root.to_path_buf(), |p, c| host.join(&p, c))
}

fn copy_local(src: &Path, dst: &Path) -> Result<(), String> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "macos")]
    if clonefile(src, dst).is_ok() {
        return Ok(());
    }
    copy_tree(src, dst).map_err(|e| e.to_string())
}

/// One `clonefile(2)` — a whole directory tree included — or an error for the
/// caller to fall back from (another volume, a filesystem that is not APFS).
#[cfg(target_os = "macos")]
fn clonefile(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let src = std::ffi::CString::new(src.as_os_str().as_bytes())?;
    let dst = std::ffi::CString::new(dst.as_os_str().as_bytes())?;
    // A symlink is cloned as the link, not what it points at. `<sys/clonefile.h>`;
    // the libc crate does not export it.
    const CLONE_NOFOLLOW: u32 = 0x0001;
    match unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), CLONE_NOFOLLOW) } {
        0 => Ok(()),
        _ => Err(std::io::Error::last_os_error()),
    }
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(std::fs::read_link(src)?, dst);
        #[cfg(not(unix))]
        return Err(std::io::Error::other("symlinks are not copied here"));
    }
    if meta.is_dir() {
        std::fs::create_dir(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        return Ok(());
    }
    std::fs::copy(src, dst).map(|_| ())
}

fn copy_remote(host: &dyn Host, src: &Path, dst: &Path, budget: &mut u64) -> Result<(), String> {
    let meta = host.stat(src).map_err(|e| e.to_string())?;
    if meta.is_symlink {
        return Err("symlinks are not copied to a remote worktree".into());
    }
    if meta.is_dir {
        host.create_dir(dst, true).map_err(|e| e.to_string())?;
        for e in host.read_dir(src, None).map_err(|e| e.to_string())? {
            copy_remote(
                host,
                &host.join(src, &e.name),
                &host.join(dst, &e.name),
                budget,
            )?;
        }
        return Ok(());
    }
    if meta.len > REMOTE_FILE_MAX.min(*budget) {
        return Err("too large to copy to a remote worktree".into());
    }
    let bytes = host
        .read_file(src, REMOTE_FILE_MAX)
        .map_err(|e| e.to_string())?;
    *budget -= bytes.len() as u64;
    if let Some(parent) = dst.parent() {
        host.create_dir(parent, true).map_err(|e| e.to_string())?;
    }
    host.write_file(dst, &bytes).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_are_the_literal_directories_a_pattern_starts_in() {
        let r = Rules::parse("# c\n.env\n/config/local/*.json\nsecrets/**\n**/x/y\n!keep\n");
        assert_eq!(r.anchors, vec!["config/local/", "secrets/"]);
        assert!(r.leads_into("config/"));
        assert!(r.leads_into("config/local/"));
        assert!(!r.leads_into("node_modules/"));
        assert!(r.wants(".env", false));
        assert!(r.wants("pkg/a/.env", false));
        assert!(r.wants("config/local/a.json", false));
        assert!(!r.wants("config/local/a.txt", false));
    }

    #[test]
    fn outermost_drops_nested_paths() {
        let got = outermost(vec![
            "a/b".into(),
            "a".into(),
            "ab".into(),
            "c/d".into(),
            "a".into(),
        ]);
        assert_eq!(got, vec!["a", "ab", "c/d"]);
    }
}

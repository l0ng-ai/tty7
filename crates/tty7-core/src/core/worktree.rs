use std::path::{Path, PathBuf};

use crate::core::codename::Names;
use crate::host::Host;

#[derive(Debug)]
pub struct NewWorktree {
    pub path: PathBuf,
    pub branch: String,
}

#[derive(Debug, Clone)]
pub struct WorktreeRequest {
    pub name: String,
    pub branch: String,
    pub base: String,
}

#[derive(Debug, Clone)]
pub struct WorktreeDefaults {
    pub name: String,
    pub base: String,
    pub dir: PathBuf,
}

fn managed_root(host: &dyn Host, main_root: &Path) -> PathBuf {
    host.join(&host.join(main_root, ".tty7"), "worktrees")
}

fn git(host: &dyn Host, dir: &Path, args: &[&str]) -> Result<String, String> {
    match host.git(dir, args) {
        Ok(out) if out.success() => Ok(out.stdout_trimmed()),
        Ok(out) => Err(out.stderr_trimmed()),
        Err(e) => Err(format!("failed to run git: {e}")),
    }
}

fn branch_exists(host: &dyn Host, repo_root: &Path, name: &str) -> bool {
    git(
        host,
        repo_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
    )
    .is_ok()
}

#[derive(Debug, Clone)]
pub struct ManagedWorktree {
    pub path: PathBuf,
    pub branch: String,
    pub main_root: PathBuf,
    pub dirty: bool,
}

pub fn managed(host: &dyn Host, cwd: &Path) -> Option<ManagedWorktree> {
    let cwd = host.canonicalize(cwd).ok()?;
    let suffix = host.join(Path::new(".tty7"), "worktrees");
    if !cwd.ancestors().any(|a| a.ends_with(&suffix)) {
        return None;
    }
    let path = PathBuf::from(git(host, &cwd, &["rev-parse", "--show-toplevel"]).ok()?);
    let main_root = git(
        host,
        &path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()
    .map(PathBuf::from)?
    .parent()?
    .to_path_buf();
    if !path.starts_with(managed_root(host, &main_root)) {
        return None;
    }
    let branch = git(host, &path, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
    let dirty = !git(host, &path, &["status", "--porcelain"])
        .ok()?
        .is_empty();
    Some(ManagedWorktree {
        path,
        branch,
        main_root,
        dirty,
    })
}

pub fn occupied(host: &dyn Host, path: &Path, cwds: &[PathBuf]) -> bool {
    let Ok(path) = host.canonicalize(path) else {
        return false;
    };
    cwds.iter()
        .any(|c| host.canonicalize(c).is_ok_and(|c| c.starts_with(&path)))
}

pub fn remove(host: &dyn Host, wt: &ManagedWorktree, force: bool) -> Result<(), String> {
    let path = wt.path.to_str().ok_or("worktree path is not valid UTF-8")?;
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path);
    git(host, &wt.main_root, &args)?;
    let _ = git(host, &wt.main_root, &["branch", "-d", &wt.branch]);
    Ok(())
}

fn repo_dir(host: &dyn Host, cwd: &Path) -> Result<(PathBuf, PathBuf), String> {
    let repo_root = git(host, cwd, &["rev-parse", "--show-toplevel"])
        .map_err(|_| "not inside a git repository".to_string())?;
    let repo_root = PathBuf::from(repo_root);
    let main_root = git(
        host,
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()
    .map(PathBuf::from)
    .and_then(|d| d.parent().map(Path::to_path_buf))
    .unwrap_or_else(|| repo_root.clone());
    let dir = managed_root(host, &main_root);
    Ok((repo_root, dir))
}

pub fn defaults(host: &dyn Host, cwd: &Path) -> Result<WorktreeDefaults, String> {
    let (repo_root, dir) = repo_dir(host, cwd)?;

    let name = Names::new().unique(|name| {
        branch_exists(host, &repo_root, name) || host.exists(&host.join(&dir, name))
    });

    let base = git(host, &repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "HEAD".to_string());
    Ok(WorktreeDefaults { name, base, dir })
}

pub fn create(host: &dyn Host, cwd: &Path, req: &WorktreeRequest) -> Result<NewWorktree, String> {
    if req.name.is_empty() || req.name == "." || req.name == ".." || req.name.contains(['/', '\\'])
    {
        return Err(format!("invalid worktree name \"{}\"", req.name));
    }
    let (repo_root, dir) = repo_dir(host, cwd)?;
    host.create_dir(&dir, true)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let ignore = host.join(
        dir.parent().expect(".tty7/worktrees has a parent"),
        ".gitignore",
    );
    if !host.exists(&ignore) {
        let _ = host.write_file(&ignore, b"*\n");
    }

    let path = host.join(&dir, &req.name);
    if host.exists(&path) {
        return Err(format!("{} already exists", path.display()));
    }
    git(
        host,
        &repo_root,
        &[
            "worktree",
            "add",
            "-b",
            &req.branch,
            path.to_str().ok_or("worktree path is not valid UTF-8")?,
            &req.base,
        ],
    )?;
    Ok(NewWorktree {
        path,
        branch: req.branch.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-wt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn plain(p: &Path) -> PathBuf {
        let s = p.to_string_lossy();
        PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
    }

    fn sh(dir: &Path, args: &[&str]) {
        assert!(
            std::process::Command::new(args[0])
                .args(&args[1..])
                .current_dir(dir)
                .output()
                .unwrap()
                .status
                .success(),
            "command failed: {args:?}"
        );
    }

    fn temp_repo(name: &str) -> PathBuf {
        let dir = scratch(name);
        sh(&dir, &["git", "init", "-q"]);
        sh(&dir, &["git", "config", "user.email", "t@t"]);
        sh(&dir, &["git", "config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        sh(&dir, &["git", "add", "."]);
        sh(&dir, &["git", "commit", "-q", "-m", "init"]);
        dir
    }

    fn h() -> crate::host::SharedHost {
        crate::host::local::LocalHost::new()
    }

    fn req(name: &str) -> WorktreeRequest {
        WorktreeRequest {
            name: name.into(),
            branch: name.into(),
            base: "HEAD".into(),
        }
    }

    #[test]
    fn defaults_proposes_fresh_name_current_branch_and_target_dir() {
        let h = h();
        let repo = temp_repo("dflt");
        let d = defaults(&*h, &repo).unwrap();
        assert!(!branch_exists(&*h, &repo, &d.name));
        let head = git(&*h, &repo, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(d.base, head);
        let canon = plain(&std::fs::canonicalize(&repo).unwrap());
        assert_eq!(plain(&d.dir), canon.join(".tty7").join("worktrees"));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_makes_worktree_on_new_branch_inside_the_repo() {
        let h = h();
        let repo = temp_repo("repo");
        let wt = create(&*h, &repo, &req("quiet-otter")).unwrap();
        assert!(wt.path.join("a.txt").exists());
        assert!(branch_exists(&*h, &repo, &wt.branch));
        let canon = plain(&std::fs::canonicalize(&repo).unwrap());
        assert_eq!(plain(&wt.path), canon.join(".tty7/worktrees/quiet-otter"));
        let head = git(&*h, &wt.path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(head, wt.branch);
        assert_eq!(
            std::fs::read_to_string(canon.join(".tty7/.gitignore")).unwrap(),
            "*\n"
        );
        assert_eq!(git(&*h, &repo, &["status", "--porcelain"]).unwrap(), "");
        assert!(
            create(&*h, &repo, &req("quiet-otter"))
                .unwrap_err()
                .contains("already exists")
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_honors_custom_branch_and_base() {
        let h = h();
        let repo = temp_repo("base");
        sh(&repo, &["git", "branch", "stable"]);
        std::fs::write(repo.join("b.txt"), "b").unwrap();
        sh(&repo, &["git", "add", "."]);
        sh(&repo, &["git", "commit", "-q", "-m", "second"]);
        let wt = create(
            &*h,
            &repo,
            &WorktreeRequest {
                name: "my-dir".into(),
                branch: "feat/my-branch".into(),
                base: "stable".into(),
            },
        )
        .unwrap();
        assert_eq!(wt.path.file_name().unwrap().to_str().unwrap(), "my-dir");
        let head = git(&*h, &wt.path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        assert_eq!(head, "feat/my-branch");
        assert!(wt.path.join("a.txt").exists());
        assert!(!wt.path.join("b.txt").exists());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_rejects_escaping_names() {
        let h = h();
        let repo = temp_repo("names");
        for bad in ["", ".", "..", "a/b", "a\\b"] {
            let mut r = req("x");
            r.name = bad.into();
            assert!(
                create(&*h, &repo, &r)
                    .unwrap_err()
                    .contains("invalid worktree name"),
                "{bad:?} should be rejected"
            );
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_from_a_linked_worktree_lands_in_the_main_repo() {
        let h = h();
        let repo = temp_repo("nest");
        let first = create(&*h, &repo, &req("first-wt")).unwrap();
        let second = create(&*h, &first.path, &req("second-wt")).unwrap();
        assert_eq!(second.path.parent().unwrap(), first.path.parent().unwrap());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn managed_resolves_managed_checkouts_and_remove_cleans_up() {
        let h = h();
        let repo = temp_repo("mg");
        let wt = create(&*h, &repo, &req("mg-wt")).unwrap();
        assert!(managed(&*h, &repo).is_none());
        let own = scratch("mg-own");
        let _ = std::fs::remove_dir_all(&own);
        sh(
            &repo,
            &[
                "git",
                "worktree",
                "add",
                "-b",
                "own-branch",
                own.to_str().unwrap(),
            ],
        );
        assert!(managed(&*h, &own).is_none());
        let sub = wt.path.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let m = managed(&*h, &sub).unwrap();
        assert_eq!(m.branch, wt.branch);
        assert_eq!(m.path, wt.path);
        assert!(!m.dirty);
        std::fs::write(wt.path.join("b.txt"), "b").unwrap();
        let m = managed(&*h, &wt.path).unwrap();
        assert!(m.dirty);
        assert!(remove(&*h, &m, false).is_err());
        remove(&*h, &m, true).unwrap();
        assert!(!wt.path.exists());
        assert!(!branch_exists(&*h, &repo, &wt.branch));
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&own);
    }

    #[test]
    fn writes_survive_the_optional_locks_invariant() {
        let h = h();
        let repo = temp_repo("locks");

        let wt = create(&*h, &repo, &req("lock-wt")).unwrap();
        assert!(wt.path.join("a.txt").exists());

        let list = git(&*h, &repo, &["worktree", "list", "--porcelain"]).unwrap();
        assert!(
            list.lines()
                .any(|l| l.starts_with("branch ") && l.ends_with(&wt.branch)),
            "worktree list must show the new checkout: {list}"
        );

        std::fs::write(wt.path.join("c.txt"), "c").unwrap();
        git(&*h, &wt.path, &["add", "."]).unwrap();
        git(
            &*h,
            &wt.path,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "c",
            ],
        )
        .unwrap();
        assert_eq!(git(&*h, &wt.path, &["status", "--porcelain"]).unwrap(), "");

        let m = managed(&*h, &wt.path).unwrap();
        remove(&*h, &m, false).unwrap();
        assert!(!wt.path.exists());
        assert!(branch_exists(&*h, &repo, &wt.branch));
        let plain_wt = create(&*h, &repo, &req("lock-wt2")).unwrap();
        let m = managed(&*h, &plain_wt.path).unwrap();
        remove(&*h, &m, false).unwrap();
        assert!(!branch_exists(&*h, &repo, &plain_wt.branch));

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn occupied_detects_live_cwds_inside_the_worktree() {
        let h = h();
        let repo = temp_repo("occ");
        let wt = create(&*h, &repo, &req("occ-wt")).unwrap();
        let inside = wt.path.join("deep");
        std::fs::create_dir_all(&inside).unwrap();
        assert!(occupied(&*h, &wt.path, &[repo.clone(), inside]));
        assert!(!occupied(&*h, &wt.path, std::slice::from_ref(&repo)));
        assert!(!occupied(&*h, &wt.path, &[wt.path.join("gone")]));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_outside_a_repo_errors() {
        let h = h();
        let plain = scratch("plain");
        let err = create(&*h, &plain, &req("x")).unwrap_err();
        assert_eq!(err, "not inside a git repository");
        assert_eq!(
            defaults(&*h, &plain).unwrap_err(),
            "not inside a git repository"
        );
        let _ = std::fs::remove_dir_all(&plain);
    }
}

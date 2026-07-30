use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{Host, MTime, SearchHit};
use crate::daemon::control::WATCH_COALESCE_WINDOW;

pub type Case = fn(h: &dyn Host, sandbox: &dyn Sandbox);

pub trait Sandbox {
    fn path(&self) -> &Path;

    fn symlink(&self, target: &Path, link: &Path) -> Option<io::Result<()>> {
        let _ = (target, link);
        None
    }
}

#[macro_export]
macro_rules! for_each_host_case {
    ($cb:ident $(, $extra:tt)*) => {
        $cb! {
            $($extra)*
            @cases
            read_dir_lists_and_sorts,
            read_dir_includes_hidden,
            read_dir_missing_is_not_found,
            read_dir_on_a_file_errors,
            read_dir_marks_dotgit_ignored,
            read_dir_applies_gitignore_chain,
            read_dir_without_root_ignores_nothing,
            read_dir_symlink_to_dir_is_dir,
            stat_reports_len_and_mtime,
            stat_missing_is_not_found,
            exists_matches_stat,
            canonicalize_resolves_dotdot,
            read_file_roundtrips_bytes,
            read_file_over_max_bytes_errors,
            read_file_on_a_dir_errors,
            write_file_creates_and_overwrites,
            write_file_reports_its_own_metadata,
            write_file_to_missing_parent_errors,
            create_file_new_rejects_existing,
            create_dir_non_recursive_needs_parent,
            create_dir_recursive_makes_chain,
            rename_moves_and_rejects_existing_target,
            rename_across_dirs_works,
            remove_file_then_missing,
            remove_dir_non_recursive_needs_empty,
            remove_dir_recursive_clears_tree,
            repo_root_finds_nearest_git,
            repo_root_handles_worktree_file,
            git_status_porcelain_reflects_changes,
            git_nonzero_exit_is_ok_not_err,
            git_that_cannot_run_is_err,
            git_optional_locks_env_is_set,
            git_stdin_is_null,
            join_uses_host_separator,
            is_absolute_matches_host_semantics,
            search_is_breadth_first,
            search_skips_ignored_dirs,
            search_respects_limit,
            search_respects_max_dirs,
            shells_are_named_and_have_a_default,
            watch_reports_create_and_delete,
            watch_is_non_recursive,
            watch_set_dirs_adds_and_drops,
            watch_coalesces_within_window,
            watch_drop_unsubscribes,
            is_connected_is_true_when_healthy,
            id_is_stable_across_calls,
            separator_matches_hello,
        }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __host_case_table {
    (@cases $($name:ident),* $(,)?) => {
                                pub const CASES: &[(&str, $crate::host::conformance::Case)] = &[
            $((stringify!($name), $name as $crate::host::conformance::Case)),*
        ];
    };
}

crate::for_each_host_case!(__host_case_table);

#[doc(hidden)]
#[macro_export]
macro_rules! __host_case_tests {
    (($factory:expr) @cases $($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                let (host, sandbox) = ($factory)();
                $crate::host::conformance::$name(&*host, &sandbox);
            }
        )*
    };
}

#[macro_export]
macro_rules! host_conformance_suite {
    ($modname:ident, $factory:expr) => {
        mod $modname {
            #[allow(unused_imports)]
            use super::*;
            use $crate::__host_case_tests;

            $crate::for_each_host_case!(__host_case_tests, ($factory));
        }
    };
}

const WATCH_TIMEOUT: Duration = Duration::from_secs(4);

const WATCH_QUIET: Duration = Duration::from_millis(1200);

fn write(h: &dyn Host, p: &Path, body: &str) {
    h.write_file(p, body.as_bytes())
        .unwrap_or_else(|e| panic!("write {}: {e}", p.display()));
}

fn put(h: &dyn Host, p: &Path, bytes: &[u8]) {
    h.write_file(p, bytes)
        .unwrap_or_else(|e| panic!("write {}: {e}", p.display()));
}

fn mkdir(h: &dyn Host, p: &Path) {
    h.create_dir(p, true)
        .unwrap_or_else(|e| panic!("mkdir {}: {e}", p.display()));
}

fn names(entries: &[super::Entry]) -> Vec<&str> {
    entries.iter().map(|e| e.name.as_str()).collect()
}

fn hit_names(hits: &[SearchHit]) -> Vec<&str> {
    hits.iter().map(|h| h.name.as_str()).collect()
}

fn git_repo(h: &dyn Host, dir: &Path) -> Option<()> {
    let out = h.git(dir, &["init", "--quiet"]).ok()?;
    out.success().then_some(())
}

fn drain(sub: &super::WatchSub) {
    while sub.events().try_recv().is_ok() {}
}

fn await_event(sub: &super::WatchSub, name: &str) -> Option<Vec<PathBuf>> {
    let deadline = Instant::now() + WATCH_TIMEOUT;
    while Instant::now() < deadline {
        match sub.events().try_recv() {
            Ok(batch) => {
                if batch
                    .iter()
                    .any(|p| p.file_name().is_some_and(|n| n == name))
                {
                    return Some(batch);
                }
            }
            Err(smol::channel::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(25))
            }
            Err(smol::channel::TryRecvError::Closed) => return None,
        }
    }
    None
}

fn collect_batches(sub: &super::WatchSub, window: Duration) -> Vec<Vec<PathBuf>> {
    let deadline = Instant::now() + window;
    let mut out = Vec::new();
    while Instant::now() < deadline {
        match sub.events().try_recv() {
            Ok(batch) => out.push(batch),
            Err(smol::channel::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(smol::channel::TryRecvError::Closed) => break,
        }
    }
    out
}

pub fn read_dir_lists_and_sorts(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    mkdir(h, &h.join(sandbox, "src"));
    mkdir(h, &h.join(sandbox, "Assets"));
    write(h, &h.join(sandbox, "main.rs"), "");
    write(h, &h.join(sandbox, "Cargo.toml"), "");
    write(h, &h.join(sandbox, "zeta.txt"), "");

    let listed = h.read_dir(sandbox, None).unwrap();
    assert_eq!(
        names(&listed),
        vec!["Assets", "src", "Cargo.toml", "main.rs", "zeta.txt"],
        "directories first, then case-insensitive by name"
    );
}

pub fn read_dir_includes_hidden(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    write(h, &h.join(sandbox, ".hidden"), "");
    write(h, &h.join(sandbox, "visible"), "");
    let listed = h.read_dir(sandbox, None).unwrap();
    assert!(names(&listed).contains(&".hidden"), "{:?}", names(&listed));
}

pub fn read_dir_missing_is_not_found(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let err = h.read_dir(&h.join(sandbox, "nope"), None).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
}

pub fn read_dir_on_a_file_errors(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let f = h.join(sandbox, "file.txt");
    write(h, &f, "hi");
    assert!(h.read_dir(&f, None).is_err());
}

pub fn read_dir_marks_dotgit_ignored(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    mkdir(h, &h.join(sandbox, ".git"));
    write(h, &h.join(sandbox, "a.txt"), "");
    let listed = h.read_dir(sandbox, Some(sandbox)).unwrap();
    let git = listed
        .iter()
        .find(|e| e.name == ".git")
        .expect(".git listed");
    assert!(git.ignored, ".git is always ignored");
    let a = listed.iter().find(|e| e.name == "a.txt").unwrap();
    assert!(!a.ignored);
}

pub fn read_dir_applies_gitignore_chain(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    mkdir(h, &h.join(sandbox, "src"));
    write(h, &h.join(sandbox, ".gitignore"), "*.log\nbuild/\n");
    write(
        h,
        &h.join(&h.join(sandbox, "src"), ".gitignore"),
        "!keep.log\n",
    );
    write(h, &h.join(sandbox, "drop.log"), "");
    write(h, &h.join(&h.join(sandbox, "src"), "keep.log"), "");
    write(h, &h.join(&h.join(sandbox, "src"), "main.rs"), "");

    let ignored = |entries: &[super::Entry], name: &str| {
        entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .ignored
    };

    let top = h.read_dir(sandbox, Some(sandbox)).unwrap();
    assert!(ignored(&top, "drop.log"), "root pattern applies");
    assert!(!ignored(&top, "src"));

    let nested = h.read_dir(&h.join(sandbox, "src"), Some(sandbox)).unwrap();
    assert!(!ignored(&nested, "keep.log"), "the deeper whitelist wins");
    assert!(!ignored(&nested, "main.rs"));
}

pub fn read_dir_without_root_ignores_nothing(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    mkdir(h, &h.join(sandbox, ".git"));
    write(h, &h.join(sandbox, ".gitignore"), "*.log\n");
    write(h, &h.join(sandbox, "drop.log"), "");

    let listed = h.read_dir(sandbox, None).unwrap();
    for e in &listed {
        if e.name == ".git" {
            assert!(e.ignored, ".git is ignored even with no root");
        } else {
            assert!(
                !e.ignored,
                "{} should not be ignored without a root",
                e.name
            );
        }
    }
}

pub fn read_dir_symlink_to_dir_is_dir(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let target = h.join(sandbox, "real");
    let link = h.join(sandbox, "link");
    mkdir(h, &target);
    let Some(made) = sb.symlink(&target, &link) else {
        return;
    };
    if made.is_err() {
        return;
    }

    let listed = h.read_dir(sandbox, None).unwrap();
    let l = listed
        .iter()
        .find(|e| e.name == "link")
        .expect("link listed");
    assert!(l.is_dir, "a link to a directory resolves as a directory");
    assert!(l.is_symlink, "and still reports as a link");
}

pub fn stat_reports_len_and_mtime(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let f = h.join(sandbox, "sized.txt");
    write(h, &f, "hello");
    let m = h.stat(&f).unwrap();
    assert_eq!(m.len, 5);
    assert!(!m.is_dir);
    assert!(!m.is_symlink);
    let mtime = m.mtime.expect("a real filesystem has mtimes");
    assert!(mtime > MTime { secs: 0, nanos: 0 }, "{mtime:?}");

    let d = h.join(sandbox, "adir");
    mkdir(h, &d);
    assert!(h.stat(&d).unwrap().is_dir);
}

pub fn stat_missing_is_not_found(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let err = h.stat(&h.join(sandbox, "ghost")).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
}

pub fn exists_matches_stat(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let f = h.join(sandbox, "there.txt");
    write(h, &f, "x");
    assert_eq!(h.exists(&f), h.stat(&f).is_ok());
    assert!(h.exists(&f));

    let missing = h.join(sandbox, "not-there");
    assert_eq!(h.exists(&missing), h.stat(&missing).is_ok());
    assert!(!h.exists(&missing));

    let nested = h.join(&f, "child");
    assert_eq!(h.exists(&nested), h.stat(&nested).is_ok());
}

pub fn canonicalize_resolves_dotdot(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let a = h.join(sandbox, "a");
    let b = h.join(sandbox, "b");
    mkdir(h, &a);
    mkdir(h, &b);

    let round_about = h.join(&h.join(&a, ".."), "b");
    let canon = h.canonicalize(&round_about).unwrap();
    let direct = h.canonicalize(&b).unwrap();
    assert_eq!(canon, direct, "a/../b is b");
}

pub fn read_file_roundtrips_bytes(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let f = h.join(sandbox, "bytes.bin");
    let mut body: Vec<u8> = vec![0x00, 0xff, 0xfe, b'a', 0x00, 0x80];
    put(h, &f, &body);
    assert_eq!(h.read_file(&f, 1024).unwrap(), body);

    let big = h.join(sandbox, "big.bin");
    body = (0..10 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    put(h, &big, &body);
    let back = h.read_file(&big, 32 * 1024 * 1024).unwrap();
    assert_eq!(back.len(), body.len());
    assert!(back == body, "10MB body round-tripped byte for byte");
}

pub fn read_file_over_max_bytes_errors(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let f = h.join(sandbox, "fat.bin");
    put(h, &f, &vec![b'x'; 4096]);
    let err = h.read_file(&f, 1024).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::FileTooLarge, "{err}");
    assert_eq!(h.read_file(&f, 4096).unwrap().len(), 4096);
}

pub fn read_file_on_a_dir_errors(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let d = h.join(sandbox, "adir");
    mkdir(h, &d);
    assert!(h.read_file(&d, 1024 * 1024).is_err());
}

pub fn write_file_creates_and_overwrites(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let f = h.join(sandbox, "doc.txt");
    let wrote = h.write_file(&f, b"first").unwrap();
    assert_eq!(h.read_file(&f, 1024).unwrap(), b"first");
    let first = h.stat(&f).unwrap();
    assert_eq!(first.len, 5);
    assert!(first.mtime.is_some());
    assert!(!first.is_dir);
    assert_eq!(
        wrote, first,
        "the write answers with the file it just wrote"
    );

    let wrote = h.write_file(&f, b"second-and-longer").unwrap();
    assert_eq!(h.read_file(&f, 1024).unwrap(), b"second-and-longer");
    let second = h.stat(&f).unwrap();
    assert_eq!(second.len, 17);
    assert!(second.mtime.is_some());
    assert_eq!(wrote.len, 17, "the overwrite reports the new length");
    assert_eq!(wrote, second);
}

pub fn write_file_reports_its_own_metadata(h: &dyn Host, sb: &dyn Sandbox) {
    let f = h.join(sb.path(), "baseline.txt");
    let wrote = h.write_file(&f, b"mine").unwrap();
    assert_eq!(wrote.len, 4);
    assert!(
        wrote.mtime.is_some(),
        "a save needs an mtime to compare with"
    );
    assert!(!wrote.is_dir);
    assert!(!wrote.is_symlink);

    let after = h.write_file(&f, b"theirs, longer").unwrap();
    assert_eq!(after.len, 14);
    assert_ne!(
        after, wrote,
        "a subsequent write must be distinguishable from ours"
    );
}

pub fn write_file_to_missing_parent_errors(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let parent = h.join(sandbox, "no-such-dir");
    let f = h.join(&parent, "doc.txt");
    let err = h.write_file(&f, b"x").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
    assert!(!h.exists(&parent), "the parent must not have been created");
}

pub fn create_file_new_rejects_existing(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let f = h.join(sandbox, "fresh.txt");
    h.create_file_new(&f).unwrap();
    assert_eq!(h.stat(&f).unwrap().len, 0);

    h.write_file(&f, b"content").unwrap();
    let err = h.create_file_new(&f).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
    assert_eq!(
        h.read_file(&f, 1024).unwrap(),
        b"content",
        "the rejected create must not have truncated it"
    );
}

pub fn create_dir_non_recursive_needs_parent(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let deep = h.join(&h.join(sandbox, "a"), "b");
    let err = h.create_dir(&deep, false).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");

    let a = h.join(sandbox, "a");
    h.create_dir(&a, false).unwrap();
    h.create_dir(&deep, false).unwrap();
    assert!(h.stat(&deep).unwrap().is_dir);

    let err = h.create_dir(&a, false).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
}

pub fn create_dir_recursive_makes_chain(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let deep = h.join(&h.join(&h.join(sandbox, "x"), "y"), "z");
    h.create_dir(&deep, true).unwrap();
    assert!(h.stat(&deep).unwrap().is_dir);
    h.create_dir(&deep, true).unwrap();
    assert!(h.stat(&deep).unwrap().is_dir);
}

pub fn rename_moves_and_rejects_existing_target(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let a = h.join(sandbox, "a.txt");
    let b = h.join(sandbox, "b.txt");
    write(h, &a, "a");
    h.rename(&a, &b).unwrap();
    assert!(!h.exists(&a));
    assert_eq!(h.read_file(&b, 64).unwrap(), b"a");

    let c = h.join(sandbox, "c.txt");
    write(h, &c, "c");
    let err = h.rename(&c, &b).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
    assert_eq!(h.read_file(&b, 64).unwrap(), b"a", "target untouched");
    assert!(h.exists(&c), "source untouched");
}

pub fn rename_across_dirs_works(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let from_dir = h.join(sandbox, "from");
    let to_dir = h.join(sandbox, "to");
    mkdir(h, &from_dir);
    mkdir(h, &to_dir);
    let src = h.join(&from_dir, "f.txt");
    let dst = h.join(&to_dir, "f.txt");
    write(h, &src, "moved");

    h.rename(&src, &dst).unwrap();
    assert!(!h.exists(&src));
    assert_eq!(h.read_file(&dst, 64).unwrap(), b"moved");

    let sub = h.join(&from_dir, "sub");
    mkdir(h, &sub);
    let sub_dst = h.join(&to_dir, "sub");
    h.rename(&sub, &sub_dst).unwrap();
    assert!(h.stat(&sub_dst).unwrap().is_dir);
}

pub fn remove_file_then_missing(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let f = h.join(sandbox, "doomed.txt");
    write(h, &f, "x");
    h.remove(&f, false).unwrap();
    assert!(!h.exists(&f));
    let err = h.remove(&f, false).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
}

pub fn remove_dir_non_recursive_needs_empty(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let d = h.join(sandbox, "full");
    mkdir(h, &d);
    write(h, &h.join(&d, "child.txt"), "x");
    let err = h.remove(&d, false).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::DirectoryNotEmpty, "{err}");
    assert!(h.exists(&d));

    let empty = h.join(sandbox, "empty");
    mkdir(h, &empty);
    h.remove(&empty, false).unwrap();
    assert!(!h.exists(&empty));
}

pub fn remove_dir_recursive_clears_tree(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let root = h.join(sandbox, "tree");
    let deep = h.join(&h.join(&root, "a"), "b");
    mkdir(h, &deep);
    write(h, &h.join(&deep, "leaf.txt"), "x");
    write(h, &h.join(&root, "top.txt"), "x");

    h.remove(&root, true).unwrap();
    assert!(!h.exists(&root));
}

pub fn repo_root_finds_nearest_git(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let outside = h.join(sandbox, "outside");
    mkdir(h, &outside);
    let sandbox_root = h.repo_root(sandbox).unwrap();
    if sandbox_root.is_none() {
        assert_eq!(h.repo_root(&outside).unwrap(), None, "no repo, no root");
    }

    let repo = h.join(sandbox, "repo");
    mkdir(h, &h.join(&repo, ".git"));
    let deep = h.join(&h.join(&repo, "src"), "nested");
    mkdir(h, &deep);
    assert_eq!(h.repo_root(&deep).unwrap(), Some(repo.clone()));
    assert_eq!(h.repo_root(&repo).unwrap(), Some(repo));
}

pub fn repo_root_handles_worktree_file(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let wt = h.join(sandbox, "worktree");
    mkdir(h, &wt);
    write(
        h,
        &h.join(&wt, ".git"),
        "gitdir: /elsewhere/.git/worktrees/wt\n",
    );
    let deep = h.join(&wt, "src");
    mkdir(h, &deep);
    assert_eq!(h.repo_root(&deep).unwrap(), Some(wt));
}

pub fn git_status_porcelain_reflects_changes(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let repo = h.join(sandbox, "repo");
    mkdir(h, &repo);
    let Some(()) = git_repo(h, &repo) else { return };

    write(h, &h.join(&repo, "new.txt"), "hello");
    let out = h.git(&repo, &["status", "--porcelain"]).unwrap();
    assert!(out.success(), "status exited {:?}", out.status);
    assert!(
        out.stdout_trimmed().contains("new.txt"),
        "porcelain: {:?}",
        out.stdout_trimmed()
    );
}

pub fn git_nonzero_exit_is_ok_not_err(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let plain = h.join(sandbox, "not-a-repo");
    mkdir(h, &plain);
    let out = match h.git(&plain, &["rev-parse", "--show-toplevel"]) {
        Ok(out) => out,
        Err(_) => return,
    };
    if out.success() {
        return;
    }
    assert!(
        out.status.is_some(),
        "a failing git still exited with a code"
    );
    assert!(
        !out.stderr.is_empty(),
        "stderr is captured, not discarded — worktree errors are read from it"
    );
}

pub fn git_that_cannot_run_is_err(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let gone = h.join(sandbox, "no-such-directory");
    let err = h.git(&gone, &["status"]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
}

pub fn git_optional_locks_env_is_set(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let repo = h.join(sandbox, "repo");
    mkdir(h, &repo);
    let Some(()) = git_repo(h, &repo) else { return };

    let configured = h.git(
        &repo,
        &[
            "config",
            "alias.tty7probe",
            "!echo LOCKS=[$GIT_OPTIONAL_LOCKS]",
        ],
    );
    let Ok(out) = configured else { return };
    if !out.success() {
        return;
    }
    let Ok(out) = h.git(&repo, &["tty7probe"]) else {
        return;
    };
    if !out.success() {
        return;
    }
    assert!(
        out.stdout_trimmed().contains("LOCKS=[0]"),
        "GIT_OPTIONAL_LOCKS must reach git: {:?}",
        out.stdout_trimmed()
    );
}

pub fn git_stdin_is_null(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let repo = h.join(sandbox, "repo");
    mkdir(h, &repo);

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            let _ = tx.send(h.git(&repo, &["stripspace"]).map(|o| o.stdout));
        });
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(stdout)) => assert!(stdout.is_empty(), "stripspace read something from stdin"),
            Ok(Err(_)) => {}
            Err(_) => panic!("git blocked on stdin — it must be nulled"),
        }
    });
}

pub fn join_uses_host_separator(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let sep = h.separator();
    let joined = h.join(sandbox, "child");
    let text = joined.to_string_lossy();
    assert!(text.ends_with(&format!("{sep}child")), "{text}");
    assert!(
        text.starts_with(&*sandbox.to_string_lossy()),
        "{text} should extend {}",
        sandbox.display()
    );
    let deep = h.join(&joined, "grand");
    assert!(
        deep.to_string_lossy()
            .ends_with(&format!("child{sep}grand"))
    );
}

pub fn is_absolute_matches_host_semantics(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    assert!(
        h.is_absolute(sandbox),
        "a sandbox path is absolute on its own host: {}",
        sandbox.display()
    );
    assert!(h.is_absolute(&h.join(sandbox, "child")));
    assert!(!h.is_absolute(Path::new("relative/path")));
    assert!(!h.is_absolute(Path::new("child.txt")));
}

pub fn search_is_breadth_first(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    write(h, &h.join(sandbox, "target-top.txt"), "");
    let deep = h.join(&h.join(sandbox, "a"), "b");
    mkdir(h, &deep);
    write(h, &h.join(&deep, "target-deep.txt"), "");

    let hits = h
        .search(&[sandbox.to_path_buf()], "target", 100, 2000, false)
        .unwrap();
    let names = hit_names(&hits);
    let top = names.iter().position(|n| *n == "target-top.txt");
    let deep_pos = names.iter().position(|n| *n == "target-deep.txt");
    assert!(top.is_some() && deep_pos.is_some(), "{names:?}");
    assert!(top < deep_pos, "shallow before deep: {names:?}");
}

pub fn search_skips_ignored_dirs(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    write(h, &h.join(sandbox, ".gitignore"), "node_modules/\n");
    let nm = h.join(sandbox, "node_modules");
    mkdir(h, &nm);
    write(h, &h.join(&nm, "needle.js"), "");
    write(h, &h.join(sandbox, "needle.rs"), "");

    let hits = h
        .search(&[sandbox.to_path_buf()], "needle", 100, 2000, false)
        .unwrap();
    assert_eq!(
        hit_names(&hits),
        vec!["needle.rs"],
        "{:?}",
        hit_names(&hits)
    );

    let hits = h
        .search(&[sandbox.to_path_buf()], "needle", 100, 2000, true)
        .unwrap();
    let mut names = hit_names(&hits);
    names.sort_unstable();
    assert_eq!(names, vec!["needle.js", "needle.rs"]);
}

pub fn search_respects_limit(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    for i in 0..10 {
        write(h, &h.join(sandbox, &format!("match{i}.txt")), "");
    }
    let hits = h
        .search(&[sandbox.to_path_buf()], "match", 3, 2000, false)
        .unwrap();
    assert_eq!(hits.len(), 3, "{:?}", hit_names(&hits));
}

pub fn search_respects_max_dirs(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let mut dir = sandbox.to_path_buf();
    for i in 0..12 {
        dir = h.join(&dir, &format!("d{i}"));
    }
    mkdir(h, &dir);
    write(h, &h.join(&dir, "needle.txt"), "");

    let hits = h
        .search(&[sandbox.to_path_buf()], "needle", 100, 2, false)
        .unwrap();
    assert!(
        hits.is_empty(),
        "the budget should stop the walk long before the leaf: {:?}",
        hit_names(&hits)
    );

    let hits = h
        .search(&[sandbox.to_path_buf()], "needle", 100, 2000, false)
        .unwrap();
    assert_eq!(hit_names(&hits), vec!["needle.txt"]);
}

pub fn shells_are_named_and_have_a_default(h: &dyn Host, _sb: &dyn Sandbox) {
    let inv = h.shells().expect("a host can list its shells");
    assert!(
        !inv.default_name.trim().is_empty(),
        "no default shell name to tag the menu with"
    );
    let mut seen = std::collections::HashSet::new();
    for shell in &inv.shells {
        assert!(!shell.label.trim().is_empty(), "a shell row with no label");
        assert!(
            !shell.program.trim().is_empty(),
            "shell {:?} has nothing to spawn",
            shell.label
        );
        assert!(
            seen.insert(shell.label.clone()),
            "{:?} is listed twice",
            shell.label
        );
    }
}

pub fn watch_reports_create_and_delete(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let sub = h.watch(&[sandbox.to_path_buf()]).unwrap();
    drain(&sub);

    let f = h.join(sandbox, "watched.txt");
    write(h, &f, "hello");
    assert!(
        await_event(&sub, "watched.txt").is_some(),
        "no event for the create"
    );

    drain(&sub);
    h.remove(&f, false).unwrap();
    assert!(
        await_event(&sub, "watched.txt").is_some(),
        "no event for the delete"
    );
}

pub fn watch_is_non_recursive(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let sub_dir = h.join(sandbox, "child");
    mkdir(h, &sub_dir);

    let sub = h.watch(&[sandbox.to_path_buf()]).unwrap();
    drain(&sub);

    write(h, &h.join(&sub_dir, "deep.txt"), "x");
    std::thread::sleep(WATCH_QUIET);
    let batches = collect_batches(&sub, Duration::from_millis(200));
    let leaked: Vec<&PathBuf> = batches
        .iter()
        .flatten()
        .filter(|p| p.file_name().is_some_and(|n| n == "deep.txt"))
        .collect();
    assert!(
        leaked.is_empty(),
        "a change inside an unwatched subdirectory leaked: {leaked:?}"
    );
}

pub fn watch_set_dirs_adds_and_drops(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let a = h.join(sandbox, "a");
    let b = h.join(sandbox, "b");
    mkdir(h, &a);
    mkdir(h, &b);

    let sub = h.watch(&[a.clone()]).unwrap();
    drain(&sub);

    sub.set_dirs(&[b.clone()]).unwrap();
    drain(&sub);

    write(h, &h.join(&b, "in-b.txt"), "x");
    assert!(
        await_event(&sub, "in-b.txt").is_some(),
        "the added directory should report"
    );

    drain(&sub);
    write(h, &h.join(&a, "in-a.txt"), "x");
    std::thread::sleep(WATCH_QUIET);
    let batches = collect_batches(&sub, Duration::from_millis(200));
    let leaked: Vec<&PathBuf> = batches
        .iter()
        .flatten()
        .filter(|p| p.file_name().is_some_and(|n| n == "in-a.txt"))
        .collect();
    assert!(
        leaked.is_empty(),
        "the dropped directory still reports: {leaked:?}"
    );
}

pub fn watch_coalesces_within_window(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let sub = h.watch(&[sandbox.to_path_buf()]).unwrap();
    drain(&sub);

    let f = h.join(sandbox, "busy.txt");
    let started = Instant::now();
    for i in 0..50 {
        put(h, &f, format!("{i}").as_bytes());
    }
    let burst = started.elapsed();

    let batches = collect_batches(&sub, Duration::from_secs(2));
    let with_file: Vec<&Vec<PathBuf>> = batches
        .iter()
        .filter(|b| {
            b.iter()
                .any(|p| p.file_name().is_some_and(|n| n == "busy.txt"))
        })
        .collect();
    assert!(!with_file.is_empty(), "the burst produced no events at all");
    let windows = burst
        .as_millis()
        .div_ceil(WATCH_COALESCE_WINDOW.as_millis())
        .max(1) as usize;
    let allowed = windows + 2;
    assert!(
        with_file.len() <= allowed,
        "50 writes taking {burst:?} coalesced into {} batches, more than the \
         {allowed} the elapsed windows allow",
        with_file.len()
    );
    for batch in &with_file {
        let hits = batch
            .iter()
            .filter(|p| p.file_name().is_some_and(|n| n == "busy.txt"))
            .count();
        assert_eq!(hits, 1, "paths are deduplicated within a batch");
    }
}

pub fn watch_drop_unsubscribes(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let sub = h.watch(&[sandbox.to_path_buf()]).unwrap();
    drain(&sub);
    let rx = sub.events().clone();
    drop(sub);

    write(h, &h.join(sandbox, "after.txt"), "x");
    std::thread::sleep(WATCH_QUIET);

    loop {
        match rx.try_recv() {
            Ok(batch) => assert!(
                !batch
                    .iter()
                    .any(|p| p.file_name().is_some_and(|n| n == "after.txt")),
                "events kept coming after the subscription was dropped"
            ),
            Err(smol::channel::TryRecvError::Empty) => break,
            Err(smol::channel::TryRecvError::Closed) => break,
        }
    }
}

pub fn is_connected_is_true_when_healthy(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    assert!(h.is_connected());
    let _ = h.read_dir(sandbox, None).unwrap();
    assert!(h.is_connected());
}

pub fn id_is_stable_across_calls(h: &dyn Host, _sb: &dyn Sandbox) {
    let first = h.id();
    for _ in 0..8 {
        assert_eq!(h.id(), first);
    }
}

pub fn separator_matches_hello(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let sep = h.separator();
    assert_eq!(sep, h.separator());
    assert!(
        h.join(sandbox, "x").to_string_lossy().contains(sep),
        "join must use the reported separator"
    );
}

#[cfg(test)]
mod tests {
    use super::CASES;

    #[test]
    fn every_case_is_registered() {
        let src = include_str!("conformance.rs");
        let defined: Vec<&str> = src
            .lines()
            .filter_map(|l| l.strip_prefix("pub fn "))
            .filter(|l| l.contains("&dyn Sandbox"))
            .filter_map(|l| l.split('(').next())
            .collect();

        let registered: Vec<&str> = CASES.iter().map(|(n, _)| *n).collect();
        for name in &defined {
            assert!(
                registered.contains(name),
                "case `{name}` is defined but missing from for_each_host_case!"
            );
        }
        for name in &registered {
            assert!(
                defined.contains(name),
                "case `{name}` is registered but has no `pub fn`"
            );
        }
        assert_eq!(defined.len(), registered.len(), "duplicate registration");
        assert!(
            defined.len() >= 46,
            "the suite lost cases: {}",
            defined.len()
        );
    }
}

//! The suite every [`Host`] implementation has to pass, unchanged.
//!
//! A remote workspace is only worth having if "the files are over there" is
//! invisible. That invisibility is not something a design document can enforce
//! — it is a property of two implementations agreeing on several dozen small
//! behaviours: what a listing is sorted by, whether a non-zero `git` exit is an
//! error, what happens when you rename onto an existing file, how long a
//! watcher batches for. So the behaviours live here, once, as functions over
//! `&dyn Host`, and [`LocalHost`](super::local::LocalHost), `RemoteHost` and the
//! `--stdio` server all run the same list.
//!
//! # Shape
//!
//! Each case is a `pub fn(&dyn Host, &dyn Sandbox)`. `&dyn Host` rather than a generic
//! is deliberate twice over: it keeps the suite from monomorphizing per
//! implementation, and it makes the suite itself the proof that the trait stayed
//! object-safe — which the whole tree depends on, since a workspace holds
//! `Arc<dyn Host>`.
//!
//! [`for_each_host_case!`](crate::for_each_host_case) lists every case;
//! [`host_conformance_suite!`](crate::host_conformance_suite) expands that list
//! into one `#[test]` per case for a given host factory, so a failure names the
//! behaviour that broke instead of arriving as one opaque red suite.
//!
//! Adding a case means writing the `pub fn` *and* adding a line to the macro.
//! `every_case_is_registered` fails if you do only the first.
//!
//! # What a case may assume
//!
//! Only the sandbox and the `Host`. Cases build their fixtures through the host
//! being tested — `h.write_file`, `h.create_dir` — never through `std::fs`,
//! because for a remote host the sandbox is a directory on *another machine*
//! and `std::fs` would quietly test the wrong computer.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{Host, MTime, SearchHit};
use crate::daemon::control::WATCH_COALESCE_WINDOW;

/// One conformance case.
pub type Case = fn(h: &dyn Host, sandbox: &dyn Sandbox);

/// An empty, disposable directory in the host's own namespace.
///
/// The factory that produces one has to guarantee: it is empty; it is cleaned up
/// when dropped; its path is meaningful *to the host under test* (for a remote
/// host that means a path on the server, not on the client); and `git` can run
/// inside it.
pub trait Sandbox {
    /// The directory's path, in the host's own vocabulary.
    fn path(&self) -> &Path;

    /// Create a symbolic link at `link` pointing to `target`, or `None` if this
    /// sandbox cannot make symlinks (unprivileged Windows). Cases that need one
    /// skip rather than fail when this is `None`, because "this platform has no
    /// symlinks" is not a host bug.
    fn symlink(&self, target: &Path, link: &Path) -> Option<io::Result<()>> {
        let _ = (target, link);
        None
    }
}

/// Every case, one per line. The single source of truth for what the suite is.
///
/// The whole list goes to `$cb` in one brace-delimited invocation, plus whatever
/// extra token trees the caller passes ahead of the `@cases` marker. Two
/// callbacks use it — one builds [`CASES`], the other builds a run of `#[test]`s
/// — and neither can drift from the other, because there is only one list.
#[macro_export]
macro_rules! for_each_host_case {
    ($cb:ident $(, $extra:tt)*) => {
        $cb! {
            $($extra)*
            @cases
            // fs: reading
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
            // fs: writing
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
            // git
            repo_root_finds_nearest_git,
            repo_root_handles_worktree_file,
            git_status_porcelain_reflects_changes,
            git_nonzero_exit_is_ok_not_err,
            git_that_cannot_run_is_err,
            git_optional_locks_env_is_set,
            git_stdin_is_null,
            // path arithmetic
            join_uses_host_separator,
            is_absolute_matches_host_semantics,
            // search
            search_is_breadth_first,
            search_skips_ignored_dirs,
            search_respects_limit,
            search_respects_max_dirs,
            // machine inventory
            shells_are_named_and_have_a_default,
            // watch
            watch_reports_create_and_delete,
            watch_is_non_recursive,
            watch_set_dirs_adds_and_drops,
            watch_coalesces_within_window,
            watch_drop_unsubscribes,
            // connection semantics
            is_connected_is_true_when_healthy,
            id_is_stable_across_calls,
            separator_matches_hello,
        }
    };
}

/// Builds [`CASES`]. Internal to [`for_each_host_case!`].
#[doc(hidden)]
#[macro_export]
macro_rules! __host_case_table {
    (@cases $($name:ident),* $(,)?) => {
        /// Every case as `(name, fn)`, for a runner that cannot use the
        /// `#[test]` expansion — an integration test in another crate driving a
        /// real `tty7-server --stdio`, say.
        pub const CASES: &[(&str, $crate::host::conformance::Case)] = &[
            $((stringify!($name), $name as $crate::host::conformance::Case)),*
        ];
    };
}

crate::for_each_host_case!(__host_case_table);

/// Builds one `#[test]` per case. Internal to [`host_conformance_suite!`].
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

/// Expand the whole suite into `#[test]`s for one host factory.
///
/// `$factory` is any expression callable with no arguments returning
/// `(SharedHost, impl Sandbox)`. Each case gets a *fresh* host and sandbox, so
/// one case's leftovers can never explain another's failure.
///
/// ```ignore
/// tty7_core::host_conformance_suite!(local, || (LocalHost::new(), TempSandbox::new()));
/// ```
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

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// How long a case waits for a watcher event before calling it absent. Four
/// coalescing windows plus slack — long enough that a loaded CI box does not
/// flake, short enough that a genuinely dead watcher fails the run promptly.
const WATCH_TIMEOUT: Duration = Duration::from_secs(4);

/// How long a case waits to be sure an event is *not* coming.
const WATCH_QUIET: Duration = Duration::from_millis(1200);

fn write(h: &dyn Host, p: &Path, body: &str) {
    h.write_file(p, body.as_bytes())
        .unwrap_or_else(|e| panic!("write {}: {e}", p.display()));
}

/// [`write`] for a body that is not a `&str`.
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

/// A git repository in `dir`, or `None` when this host has no usable git — in
/// which case the git cases skip rather than fail, because "no git installed"
/// is an environment fact and not a conformance violation.
fn git_repo(h: &dyn Host, dir: &Path) -> Option<()> {
    let out = h.git(dir, &["init", "--quiet"]).ok()?;
    out.success().then_some(())
}

/// Drain whatever the watcher already queued, so a case's assertions are about
/// the change it just made and not about the fixture it built.
fn drain(sub: &super::WatchSub) {
    while sub.events().try_recv().is_ok() {}
}

/// The next batch containing a path whose file name is `name`, or `None` if
/// none arrives within [`WATCH_TIMEOUT`].
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

/// Collect every batch that arrives over `window`.
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

// ---------------------------------------------------------------------------
// fs: reading
// ---------------------------------------------------------------------------

/// Directories first, then case-insensitively by name — the order the file tree
/// renders, computed by the host so a remote listing needs no client-side sort
/// (and so the two can never drift).
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

/// Hidden files come back. Whether to *show* them is a UI preference, and a host
/// that filtered them would make that preference unimplementable for the tree
/// while still costing a listing.
pub fn read_dir_includes_hidden(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    write(h, &h.join(sandbox, ".hidden"), "");
    write(h, &h.join(sandbox, "visible"), "");
    let listed = h.read_dir(sandbox, None).unwrap();
    assert!(names(&listed).contains(&".hidden"), "{:?}", names(&listed));
}

/// A directory that isn't there is `NotFound`, so the tree can tell "gone" from
/// "unreadable" and drop the row instead of showing an error.
pub fn read_dir_missing_is_not_found(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let err = h.read_dir(&h.join(sandbox, "nope"), None).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
}

/// Listing a file is an error. Which error varies by platform (`NotADirectory`
/// where it exists), so the assertion is only that it fails rather than
/// pretending to be an empty directory.
pub fn read_dir_on_a_file_errors(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let f = h.join(sandbox, "file.txt");
    write(h, &f, "hi");
    assert!(h.read_dir(&f, None).is_err());
}

/// `.git` is ignored unconditionally — no `.gitignore` mentions it, and the tree
/// has always dimmed it.
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

/// The gitignore chain, scored the way git scores it: walk from the root down,
/// deepest match wins, a nested `!pattern` un-ignores what an ancestor ignored.
/// The fixture is the file tree's own, so a regression here is a visible change
/// in what the sidebar dims.
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

/// Without a root there is no chain to score against, so nothing is ignored —
/// except `.git`, which is not a pattern match.
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

/// A symlink to a directory reads as a directory *and* as a link: the tree
/// expands it like a directory, and the sort puts it with the directories, but
/// callers that care (a delete, say) can still tell.
pub fn read_dir_symlink_to_dir_is_dir(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    // The sandbox owns symlink creation because the host trait has no method
    // for it — and on unprivileged Windows there is nothing to test.
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

/// Size is exact and a modification time is present — the two fields the editor
/// builds its external-change detection on.
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

/// A missing path is `NotFound`, not some generic failure — call sites branch on
/// it to tell "deleted" from "broken".
pub fn stat_missing_is_not_found(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let err = h.stat(&h.join(sandbox, "ghost")).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
}

/// `exists` is allowed to be a cheaper path than `stat`, but it must never be a
/// *different* answer.
pub fn exists_matches_stat(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let f = h.join(sandbox, "there.txt");
    write(h, &f, "x");
    assert_eq!(h.exists(&f), h.stat(&f).is_ok());
    assert!(h.exists(&f));

    let missing = h.join(sandbox, "not-there");
    assert_eq!(h.exists(&missing), h.stat(&missing).is_ok());
    assert!(!h.exists(&missing));

    // A path *under* a file is neither a file nor a directory.
    let nested = h.join(&f, "child");
    assert_eq!(h.exists(&nested), h.stat(&nested).is_ok());
}

/// `..` is resolved by the host, not by the client's `std::path` — which on a
/// Windows client would resolve a remote POSIX path against the wrong
/// filesystem entirely.
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

/// Bytes are bytes: NULs, invalid UTF-8 and a multi-megabyte body all come back
/// exactly as written. The editor reads files this way and would corrupt a
/// binary it merely *opened* if any of it were lossy.
pub fn read_file_roundtrips_bytes(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let f = h.join(sandbox, "bytes.bin");
    let mut body: Vec<u8> = vec![0x00, 0xff, 0xfe, b'a', 0x00, 0x80];
    put(h, &f, &body);
    assert_eq!(h.read_file(&f, 1024).unwrap(), body);

    // Big enough to cross any chunking a transport might do.
    let big = h.join(sandbox, "big.bin");
    body = (0..10 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    put(h, &big, &body);
    let back = h.read_file(&big, 32 * 1024 * 1024).unwrap();
    assert_eq!(back.len(), body.len());
    assert!(back == body, "10MB body round-tripped byte for byte");
}

/// The limit is the *host's*: an oversized file fails without its contents
/// being read or transferred, which is the difference between an instant "too
/// big" and a minute of transatlantic transfer thrown away on arrival.
pub fn read_file_over_max_bytes_errors(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let f = h.join(sandbox, "fat.bin");
    put(h, &f, &vec![b'x'; 4096]);
    let err = h.read_file(&f, 1024).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::FileTooLarge, "{err}");
    // Exactly at the limit is fine — the check is `>`, not `>=`.
    assert_eq!(h.read_file(&f, 4096).unwrap().len(), 4096);
}

/// Reading a directory fails rather than returning something.
pub fn read_file_on_a_dir_errors(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let d = h.join(sandbox, "adir");
    mkdir(h, &d);
    assert!(h.read_file(&d, 1024 * 1024).is_err());
}

// ---------------------------------------------------------------------------
// fs: writing
// ---------------------------------------------------------------------------

/// Creating and overwriting both work, and the file the host reports after the
/// write is the file that is actually there.
pub fn write_file_creates_and_overwrites(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let f = h.join(sandbox, "doc.txt");
    let wrote = h.write_file(&f, b"first").unwrap();
    assert_eq!(h.read_file(&f, 1024).unwrap(), b"first");
    // The metadata visible straight after the write describes the bytes that
    // were written — the editor's whole external-change detection rests on
    // being able to record "this mtime is mine" the moment a save lands.
    let first = h.stat(&f).unwrap();
    assert_eq!(first.len, 5);
    assert!(first.mtime.is_some());
    assert!(!first.is_dir);
    // …and the write reports that same file itself, so the caller never has to
    // ask again. This is the guard on the round trip `write_file -> Meta` saves.
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

/// The mtime a save records must come from the write itself.
///
/// The editor tells its own save apart from someone else's edit by comparing
/// against a `disk_mtime` baseline. If that baseline came from a `stat` issued
/// *after* the write, an edit landing in the gap would be stamped as ours and
/// the editor would never report it — a silent lost-update, and the user never
/// gets the conflict prompt. So the write has to answer for itself.
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

    // A later external write moves the mtime forward; the value we recorded is
    // still the one describing *our* bytes, which is what makes the comparison
    // meaningful.
    let after = h.write_file(&f, b"theirs, longer").unwrap();
    assert_eq!(after.len, 14);
    assert_ne!(
        after, wrote,
        "a subsequent write must be distinguishable from ours"
    );
}

/// A missing parent is an error and stays missing. Silently creating it would
/// turn a typo in a save dialog into a directory tree nobody asked for.
pub fn write_file_to_missing_parent_errors(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let parent = h.join(sandbox, "no-such-dir");
    let f = h.join(&parent, "doc.txt");
    let err = h.write_file(&f, b"x").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
    assert!(!h.exists(&parent), "the parent must not have been created");
}

/// Exclusive creation: the file tree's "new file" row must not silently
/// truncate a file that is already there.
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

/// Without `recursive`, a missing parent is an error rather than an implicit
/// `mkdir -p`.
pub fn create_dir_non_recursive_needs_parent(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let deep = h.join(&h.join(sandbox, "a"), "b");
    let err = h.create_dir(&deep, false).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");

    let a = h.join(sandbox, "a");
    h.create_dir(&a, false).unwrap();
    h.create_dir(&deep, false).unwrap();
    assert!(h.stat(&deep).unwrap().is_dir);

    // And a second non-recursive create of the same directory is a conflict.
    let err = h.create_dir(&a, false).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
}

/// With `recursive`, the whole chain appears at once and an existing directory
/// is success — the `mkdir -p` semantics the worktree setup depends on.
pub fn create_dir_recursive_makes_chain(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let deep = h.join(&h.join(&h.join(sandbox, "x"), "y"), "z");
    h.create_dir(&deep, true).unwrap();
    assert!(h.stat(&deep).unwrap().is_dir);
    h.create_dir(&deep, true).unwrap();
    assert!(h.stat(&deep).unwrap().is_dir);
}

/// An occupied destination is `AlreadyExists`, guaranteed by the host.
///
/// This is the case that keeps the file tree's inline rename from needing an
/// `exists` probe first: the probe would be an extra round trip *and* racy, and
/// on Unix a bare `rename(2)` would have silently destroyed the other file.
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

/// Moving between directories on the same host works — a drag in the tree is
/// this call.
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

    // Directories move too.
    let sub = h.join(&from_dir, "sub");
    mkdir(h, &sub);
    let sub_dst = h.join(&to_dir, "sub");
    h.rename(&sub, &sub_dst).unwrap();
    assert!(h.stat(&sub_dst).unwrap().is_dir);
}

/// Deleting twice is `NotFound` the second time, so the tree's optimistic row
/// removal can tell "already gone" from "could not delete".
pub fn remove_file_then_missing(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let f = h.join(sandbox, "doomed.txt");
    write(h, &f, "x");
    h.remove(&f, false).unwrap();
    assert!(!h.exists(&f));
    let err = h.remove(&f, false).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
}

/// A non-empty directory needs `recursive`. Without it the host refuses, which
/// is what lets a delete confirm before it destroys a subtree.
pub fn remove_dir_non_recursive_needs_empty(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let d = h.join(sandbox, "full");
    mkdir(h, &d);
    write(h, &h.join(&d, "child.txt"), "x");
    let err = h.remove(&d, false).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::DirectoryNotEmpty, "{err}");
    assert!(h.exists(&d));

    // Empty, it goes.
    let empty = h.join(sandbox, "empty");
    mkdir(h, &empty);
    h.remove(&empty, false).unwrap();
    assert!(!h.exists(&empty));
}

/// With `recursive`, a whole tree goes in one call rather than one round trip
/// per file.
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

// ---------------------------------------------------------------------------
// git
// ---------------------------------------------------------------------------

/// The nearest ancestor with a `.git`, found in one call rather than one round
/// trip per level — and `Ok(None)`, not an error, outside any repository.
pub fn repo_root_finds_nearest_git(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let outside = h.join(sandbox, "outside");
    mkdir(h, &outside);
    // The sandbox itself may sit inside somebody's repository (a checkout under
    // a repo-shaped temp dir), so the "outside" assertion is only meaningful
    // when the sandbox is genuinely outside one.
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

/// A linked worktree's `.git` is a *file*, not a directory. A root probe that
/// only looked for directories would treat every worktree as "not a repo".
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

/// A real `git` invocation against a real repository: the sidebar's status line
/// is this call, and it has to see a change the host just made.
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

/// **The load-bearing one.** A non-zero exit is `Ok`, with the code in
/// `Output::status`.
///
/// Everything downstream is built on this split: `Err` means git never ran, so a
/// caller can keep the previous status instead of blanking it, while an exit
/// 128 is just git's answer to a question about a directory that isn't a repo.
/// Collapse the two and the sidebar starts showing errors for ordinary states.
pub fn git_nonzero_exit_is_ok_not_err(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let plain = h.join(sandbox, "not-a-repo");
    mkdir(h, &plain);
    let out = match h.git(&plain, &["rev-parse", "--show-toplevel"]) {
        Ok(out) => out,
        // No git on this host at all: nothing to assert about exit codes.
        Err(_) => return,
    };
    // If the sandbox happens to live inside a repository, git succeeds — then
    // the case has nothing to say, and saying it anyway would be a false red.
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

/// `Err` is reserved for "it could not run". A `cwd` that does not exist is
/// exactly that: the question was never about the repository.
///
/// (The other way to reach `Err` — no `git` on `PATH` — cannot be provoked
/// in-process without mutating the environment out from under every other test
/// in the binary, so this is the deterministic half of that contract.)
pub fn git_that_cannot_run_is_err(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let gone = h.join(sandbox, "no-such-directory");
    let err = h.git(&gone, &["status"]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
}

/// `GIT_OPTIONAL_LOCKS=0` reaches the git process.
///
/// Probed through a `!`-alias, which git runs in a shell that inherits git's own
/// environment — the only way to observe the variable without mutating this
/// process's `PATH`. Without it, every background status probe can take
/// `index.lock` and lose a race against a git command the user is running.
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
    // A host without a shell for `!`-aliases (some Windows layouts) cannot run
    // the probe; that is an environment limit, not a conformance failure.
    if !out.success() {
        return;
    }
    assert!(
        out.stdout_trimmed().contains("LOCKS=[0]"),
        "GIT_OPTIONAL_LOCKS must reach git: {:?}",
        out.stdout_trimmed()
    );
}

/// git's stdin is closed, so a subcommand that reads it gets EOF immediately
/// instead of blocking a background thread forever on a terminal nobody is
/// attached to.
///
/// Hard-bounded: if the invariant is broken the call hangs, and a hung test that
/// eventually times out the whole suite is a far worse failure report than a
/// named assertion.
pub fn git_stdin_is_null(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let repo = h.join(sandbox, "repo");
    mkdir(h, &repo);

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| {
            // `stripspace` reads stdin to EOF and writes it out. With stdin
            // nulled it returns instantly and empty; with stdin inherited from
            // an interactive terminal it never returns at all.
            let _ = tx.send(h.git(&repo, &["stripspace"]).map(|o| o.stdout));
        });
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(stdout)) => assert!(stdout.is_empty(), "stripspace read something from stdin"),
            // No git here: nothing to assert.
            Ok(Err(_)) => {}
            Err(_) => panic!("git blocked on stdin — it must be nulled"),
        }
    });
}

// ---------------------------------------------------------------------------
// path arithmetic
// ---------------------------------------------------------------------------

/// `join` uses the *host's* separator, not the client's. On a Windows client
/// talking to Linux, `PathBuf::join` would produce `/home/me\src`, which the
/// remote has never heard of.
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
    // Joining twice is joining a path, not concatenating two roots.
    let deep = h.join(&joined, "grand");
    assert!(
        deep.to_string_lossy()
            .ends_with(&format!("child{sep}grand"))
    );
}

/// Absoluteness is the host's judgement. A Windows client asked about
/// `/home/me` would say "relative" — which would send every remote path down
/// the wrong branch of every call site that checks.
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

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

/// Breadth-first: the shallow hit is the one you meant, so it comes first.
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

/// Ignored directories are not walked at all. `node_modules` and `target` are
/// where the file count explodes and never where anyone is searching — walking
/// them would burn the whole directory budget before reaching real code.
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

    // With hidden/ignored shown, the walk does go in — the flag is the switch.
    let hits = h
        .search(&[sandbox.to_path_buf()], "needle", 100, 2000, true)
        .unwrap();
    let mut names = hit_names(&hits);
    names.sort_unstable();
    assert_eq!(names, vec!["needle.js", "needle.rs"]);
}

/// `limit` stops the walk, so a query like "e" cannot crawl a monorepo.
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

/// `max_dirs` bounds the walk even when nothing matches, so a typo cannot turn
/// into a full-disk crawl.
pub fn search_respects_max_dirs(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    // A chain deep enough that visiting it all would be obvious, with the only
    // match at the bottom.
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

    // With room, it is found — proving the fixture, not just the bound.
    let hits = h
        .search(&[sandbox.to_path_buf()], "needle", 100, 2000, false)
        .unwrap();
    assert_eq!(hit_names(&hits), vec!["needle.txt"]);
}

// ---------------------------------------------------------------------------
// machine inventory
// ---------------------------------------------------------------------------

/// Every row of the new-tab dropdown is launchable and labelled, and the menu
/// knows which one is the default.
///
/// Deliberately not "the list is non-empty": a host with no shell registered
/// anywhere is a strange machine, not a broken `Host` implementation. What the
/// dropdown cannot survive is a blank row, a row with nothing to spawn, or two
/// rows with the same name — the dedupe the local probe does is part of the
/// contract, not an implementation detail of `/etc/shells` parsing.
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

// ---------------------------------------------------------------------------
// watch
// ---------------------------------------------------------------------------

/// Creating and deleting a file in a watched directory both surface.
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

/// Non-recursive, always. The tree watches the directories it has expanded; a
/// recursive watch on a repository root would report every file a build touches
/// and repaint the sidebar continuously.
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

/// The watched set is replaceable in place — the file tree changes it on every
/// expand, and rebuilding the subscription each time would cost a round trip
/// and a fresh server-side watcher per disclosure triangle.
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

    // The newly watched directory reports.
    write(h, &h.join(&b, "in-b.txt"), "x");
    assert!(
        await_event(&sub, "in-b.txt").is_some(),
        "the added directory should report"
    );

    // The dropped one does not.
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

/// A burst becomes a batch. Fifty writes arrive as a handful of deduplicated
/// batches, not fifty repaints — and identically on every host, so where the
/// files live cannot change how busy the UI looks.
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

    // Give the window time to close, plus slack for a loaded machine.
    let batches = collect_batches(&sub, Duration::from_secs(2));
    let with_file: Vec<&Vec<PathBuf>> = batches
        .iter()
        .filter(|b| {
            b.iter()
                .any(|p| p.file_name().is_some_and(|n| n == "busy.txt"))
        })
        .collect();
    assert!(!with_file.is_empty(), "the burst produced no events at all");
    // The guarantee is one batch per window, not a fixed batch count. A loaded
    // runner can spend well over a window just issuing the writes, and a burst
    // spread over N windows is *allowed* to arrive as N batches — bounding by a
    // constant would be testing how fast the machine writes files, not whether
    // the coalescer coalesces.
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

/// Dropping the subscription unsubscribes — the watcher goes away rather than
/// living on and (remotely) leaking a server-side watch per expanded directory.
pub fn watch_drop_unsubscribes(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    let sub = h.watch(&[sandbox.to_path_buf()]).unwrap();
    drain(&sub);
    // Keep the receiving end so we can prove nothing arrives after the drop.
    let rx = sub.events().clone();
    drop(sub);

    write(h, &h.join(sandbox, "after.txt"), "x");
    std::thread::sleep(WATCH_QUIET);

    loop {
        match rx.try_recv() {
            // A batch already in flight when the drop happened is fine; a batch
            // describing the change made *after* it is not.
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

// ---------------------------------------------------------------------------
// connection semantics
// ---------------------------------------------------------------------------

/// A host handed to a test is a working host. (Trivially true locally; the
/// point is that a remote one has to agree, so call sites can trust the flag to
/// mean "showing stale data is the right move".)
pub fn is_connected_is_true_when_healthy(h: &dyn Host, sb: &dyn Sandbox) {
    let sandbox = sb.path();
    assert!(h.is_connected());
    // And it stays true across real work.
    let _ = h.read_dir(sandbox, None).unwrap();
    assert!(h.is_connected());
}

/// The id never changes under a live host. Caches key on it; a shifting id would
/// silently orphan every entry they hold.
pub fn id_is_stable_across_calls(h: &dyn Host, _sb: &dyn Sandbox) {
    let first = h.id();
    for _ in 0..8 {
        assert_eq!(h.id(), first);
    }
}

/// The separator is stable and is the one `join` actually uses — for a remote
/// host it comes from the handshake, so a mismatch here means every path the
/// client builds is wrong.
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

    /// Every `pub fn` case in this file appears in the registry.
    ///
    /// The failure mode this guards is silent: write a case, forget the list
    /// entry, and it simply never runs — a green suite that tests one behaviour
    /// less than it claims to.
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

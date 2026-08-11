//! Copying dropped files into a directory of the Files panel's tree.
//!
//! The panel has always been a drag *source* — a row can be dragged into a
//! terminal to insert its path. This is the other direction: whatever the
//! desktop (or another row) drops on the tree gets copied into the directory
//! under the cursor.
//!
//! Everything here runs on a `HostOps` worker thread, never on the UI thread,
//! and the destination is reached through the [`Host`] the tree is listing —
//! which is this machine for a local workspace and the far end of a control
//! connection for a remote one. The *sources* are always local paths: a file
//! drop from the desktop can only name files on the desktop's own machine.

use std::io;
use std::path::{Path, PathBuf};

use crate::ui::host_ops::Host;
use crate::ui::i18n::{L10nKey, t, t_fmt};

/// How deep a dropped folder is followed before the copy gives up.
///
/// `metadata` follows symlinks, which is what makes a dropped alias copy the
/// thing it points at rather than a broken link — and what makes a link back
/// up its own tree an infinite walk. This is the floor that walk stops on.
const MAX_DEPTH: usize = 64;

/// Ceiling on one file copied to a host that is not this machine.
///
/// `write_file` puts the whole file in a single control frame, alongside the
/// JSON naming the path, and the frame as a whole has to fit in `MAX_FRAME`.
/// The megabyte of slack is for that JSON and the framing around it.
const REMOTE_FILE_MAX: u64 = (crate::daemon::protocol::MAX_FRAME - 1024 * 1024) as u64;

/// What one drop did, in the terms the panel has to answer in: rows to
/// refresh, names to ask about, failures to report.
#[derive(Default)]
pub(crate) struct DropReport {
    pub copied: Vec<String>,
    /// Names that already exist in the destination. Non-empty only when the
    /// caller asked without `overwrite`, and then nothing at all was written —
    /// the answer to "replace?" governs every name in the drop, so a half-done
    /// copy would have to be undone to honour a "no".
    pub conflicts: Vec<String>,
    pub errors: Vec<(String, io::Error)>,
}

impl DropReport {
    fn fail(&mut self, name: &str, message: String) {
        self.errors
            .push((name.to_string(), io::Error::other(message)));
    }
}

/// Copy `sources` into `dir` on `host`.
///
/// Two passes: name and vet every source first, then write. That is what lets
/// the panel ask about name collisions before anything has been overwritten.
pub(crate) fn copy_into_dir(
    host: &dyn Host,
    sources: &[PathBuf],
    dir: &Path,
    overwrite: bool,
) -> DropReport {
    let mut report = DropReport::default();
    let mut planned: Vec<(PathBuf, PathBuf, String)> = Vec::new();
    // Basenames already claimed by this drop: two sources from different
    // folders can share one, and both would land on the same path (#490).
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();

    for src in sources {
        let Some(name) = src.file_name().map(|n| n.to_string_lossy().to_string()) else {
            // A root directory, or a path ending in `..`: nothing to name the
            // copy after.
            continue;
        };
        // A row dropped back where it already is: on the folder holding it, or
        // — since every row is both a drag source and a drop target — on
        // itself, which is where an abandoned drag lands. Both are misses, and
        // a miss is not worth a notification.
        if src.parent() == Some(dir) || dir == src {
            continue;
        }
        if dir.starts_with(src) {
            report.fail(&name, t(L10nKey::FileDropIntoItself).to_string());
            continue;
        }
        // The drag carries whatever `on_drag` put in it, and a row of a remote
        // tree carries a path on the *far* machine. Reading it here would
        // either fail or, worse, find a local file of the same name.
        if !src.exists() {
            report.fail(&name, t(L10nKey::FileDropNotHere).to_string());
            continue;
        }
        if !claimed.insert(name.clone()) {
            // Copying both would mean the second silently overwrites the
            // first. Keep the first and say why its namesake did not come
            // along — there is no answer to "replace?" that lands both.
            report.fail(&name, t(L10nKey::FileDropNameClash).to_string());
            continue;
        }
        if host.exists(&host.join(dir, &name)) {
            report.conflicts.push(name.clone());
        }
        planned.push((src.clone(), host.join(dir, &name), name));
    }

    if !overwrite && !report.conflicts.is_empty() {
        return report;
    }

    for (src, dest, name) in planned {
        // Replace rather than merge: a folder dropped onto a folder of the
        // same name should end up as what was dropped, not as the union of the
        // two, which is what writing into it one file at a time would leave.
        let result = if host.exists(&dest) {
            replace_tree(host, dir, &src, &dest, &name)
        } else {
            copy_tree(host, &src, &dest, 0)
        };
        match result {
            Ok(()) => report.copied.push(name),
            Err(e) => report.errors.push((name, e)),
        }
    }
    report
}

/// Replace `dest` with a copy of `src` leaving no window in which neither is
/// there: the new tree is staged under a temporary name and takes the old
/// one's place only once it is complete, and a failure anywhere along the way
/// puts the old one back. Deleting the destination first — the order this
/// replaced — loses the old file to any mid-copy failure (#490).
fn replace_tree(
    host: &dyn Host,
    dir: &Path,
    src: &Path,
    dest: &Path,
    name: &str,
) -> io::Result<()> {
    let partial = host.join(dir, &format!(".tty7-partial-{name}"));
    let replaced = host.join(dir, &format!(".tty7-replaced-{name}"));
    // Leftovers of an interrupted earlier replace.
    let _ = host.remove(&partial, true);
    let _ = host.remove(&replaced, true);
    if let Err(e) = copy_tree(host, src, &partial, 0) {
        let _ = host.remove(&partial, true);
        return Err(e);
    }
    host.rename(dest, &replaced)?;
    if let Err(e) = host.rename(&partial, dest) {
        // The old tree stepped aside and the new one would not take its
        // place — put the old one back.
        let _ = host.rename(&replaced, dest);
        let _ = host.remove(&partial, true);
        return Err(e);
    }
    // The swap is done; the old tree going away is housekeeping, not part of
    // the guarantee.
    let _ = host.remove(&replaced, true);
    Ok(())
}

fn copy_tree(host: &dyn Host, src: &Path, dest: &Path, depth: usize) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(io::Error::other(t_fmt(
            L10nKey::FileDropTooDeep,
            &[("n", &MAX_DEPTH.to_string())],
        )));
    }
    let meta = std::fs::metadata(src)?;
    if !meta.is_dir() {
        return copy_file(host, src, dest, meta.len());
    }
    host.create_dir(dest, true)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        // `Host::join` builds paths out of `&str`, so a name that is not UTF-8
        // would land under a lossy spelling of itself. Locally there is no
        // reason to go through it at all; over the wire the path is a String
        // either way, and lossy is the best that can be done.
        let child = match host.id().is_local() {
            true => dest.join(&name),
            false => host.join(dest, &name.to_string_lossy()),
        };
        copy_tree(host, &entry.path(), &child, depth + 1)?;
    }
    Ok(())
}

fn copy_file(host: &dyn Host, src: &Path, dest: &Path, len: u64) -> io::Result<()> {
    // One syscall path locally, and the one that keeps the mode bits:
    // `write_file` would drop the executable bit off every script copied in.
    if host.id().is_local() {
        std::fs::copy(src, dest)?;
        return Ok(());
    }
    if len > REMOTE_FILE_MAX {
        return Err(io::Error::other(t_fmt(
            L10nKey::FileDropTooLarge,
            &[("limit", &(REMOTE_FILE_MAX / (1024 * 1024)).to_string())],
        )));
    }
    let bytes = std::fs::read(src)?;
    host.write_file(dest, &bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tty7_core::host::local::LocalHost;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-drop-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn a_dropped_file_lands_in_the_directory_it_was_dropped_on() {
        let root = scratch("one-file");
        let src = root.join("from/note.txt");
        write(&src, "hello");
        let dest_dir = root.join("into");
        std::fs::create_dir_all(&dest_dir).unwrap();

        let report = copy_into_dir(&*LocalHost::shared(), &[src], &dest_dir, false);

        assert_eq!(report.copied, vec!["note.txt".to_string()]);
        assert!(report.errors.is_empty());
        assert_eq!(
            std::fs::read_to_string(dest_dir.join("note.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn a_dropped_folder_brings_everything_under_it() {
        let root = scratch("folder");
        write(&root.join("from/pkg/a.txt"), "a");
        write(&root.join("from/pkg/deep/b.txt"), "b");
        let dest_dir = root.join("into");
        std::fs::create_dir_all(&dest_dir).unwrap();

        let report = copy_into_dir(
            &*LocalHost::shared(),
            &[root.join("from/pkg")],
            &dest_dir,
            false,
        );

        assert_eq!(report.copied, vec!["pkg".to_string()]);
        assert_eq!(
            std::fs::read_to_string(dest_dir.join("pkg/deep/b.txt")).unwrap(),
            "b"
        );
    }

    #[test]
    fn an_executable_stays_executable() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let root = scratch("mode");
            let src = root.join("from/run.sh");
            write(&src, "#!/bin/sh\n");
            std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();
            let dest_dir = root.join("into");
            std::fs::create_dir_all(&dest_dir).unwrap();

            copy_into_dir(&*LocalHost::shared(), &[src], &dest_dir, false);

            let mode = std::fs::metadata(dest_dir.join("run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "the executable bit did not survive");
        }
    }

    #[test]
    fn a_name_already_there_is_reported_and_nothing_is_written() {
        let root = scratch("conflict");
        let src = root.join("from/note.txt");
        write(&src, "new");
        let dest_dir = root.join("into");
        write(&dest_dir.join("note.txt"), "old");
        let other = root.join("from/fresh.txt");
        write(&other, "fresh");

        let report = copy_into_dir(
            &*LocalHost::shared(),
            &[src.clone(), other],
            &dest_dir,
            false,
        );

        assert_eq!(report.conflicts, vec!["note.txt".to_string()]);
        assert!(
            report.copied.is_empty(),
            "the answer governs the whole drop"
        );
        assert_eq!(
            std::fs::read_to_string(dest_dir.join("note.txt")).unwrap(),
            "old"
        );
        assert!(!dest_dir.join("fresh.txt").exists());
    }

    #[test]
    fn replacing_a_folder_leaves_what_was_dropped_and_not_the_union() {
        let root = scratch("replace");
        write(&root.join("from/pkg/new.txt"), "new");
        let dest_dir = root.join("into");
        write(&dest_dir.join("pkg/stale.txt"), "stale");

        let report = copy_into_dir(
            &*LocalHost::shared(),
            &[root.join("from/pkg")],
            &dest_dir,
            true,
        );

        assert_eq!(report.copied, vec!["pkg".to_string()]);
        assert!(dest_dir.join("pkg/new.txt").exists());
        assert!(
            !dest_dir.join("pkg/stale.txt").exists(),
            "replacing a folder must not merge into it"
        );
    }

    #[test]
    fn two_sources_sharing_a_name_do_not_overwrite_each_other() {
        // #490: the batch itself can collide even when the destination holds
        // nothing yet, and the second copy silently winning was the bug.
        let root = scratch("name-clash");
        let a = root.join("from/a/readme.txt");
        let b = root.join("from/b/readme.txt");
        write(&a, "from a");
        write(&b, "from b");
        let dest_dir = root.join("into");
        std::fs::create_dir_all(&dest_dir).unwrap();

        let report = copy_into_dir(&*LocalHost::shared(), &[a, b], &dest_dir, false);

        assert_eq!(
            report.copied,
            vec!["readme.txt".to_string()],
            "the first of the two still lands"
        );
        assert_eq!(report.errors.len(), 1, "the second is reported, not silent");
        assert_eq!(report.errors[0].0, "readme.txt");
        assert_eq!(
            std::fs::read_to_string(dest_dir.join("readme.txt")).unwrap(),
            "from a",
            "and it is the first source, not whichever copied last"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_replace_keeps_the_file_that_was_there() {
        // #490: delete-then-copy lost the old file to any mid-copy failure;
        // stage-then-swap must leave it exactly where it was.
        use std::os::unix::fs::PermissionsExt as _;
        let root = scratch("failed-replace");
        write(&root.join("from/pkg/ok.txt"), "new ok");
        let unreadable = root.join("from/pkg/no.txt");
        write(&unreadable, "new secret");
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        let dest_dir = root.join("into");
        write(&dest_dir.join("pkg/old.txt"), "old");

        let report = copy_into_dir(
            &*LocalHost::shared(),
            &[root.join("from/pkg")],
            &dest_dir,
            true,
        );

        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(report.errors.len(), 1);
        assert!(report.copied.is_empty());
        assert_eq!(
            std::fs::read_to_string(dest_dir.join("pkg/old.txt")).unwrap(),
            "old",
            "the failed copy must not have taken the old file with it"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dest_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".tty7-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "staging names are cleaned up: {leftovers:?}"
        );
    }

    #[test]
    fn a_row_dropped_back_on_its_own_folder_does_nothing() {
        let root = scratch("same-dir");
        let src = root.join("here/note.txt");
        write(&src, "hello");

        let report = copy_into_dir(&*LocalHost::shared(), &[src], &root.join("here"), false);

        assert!(report.copied.is_empty());
        assert!(report.errors.is_empty(), "a miss is not an error");
        assert!(report.conflicts.is_empty());
    }

    #[test]
    fn a_drag_abandoned_on_the_folder_it_started_from_says_nothing() {
        let root = scratch("abandoned");
        let pkg = root.join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();

        let report = copy_into_dir(&*LocalHost::shared(), &[pkg.clone()], &pkg, false);

        assert!(report.copied.is_empty());
        assert!(
            report.errors.is_empty(),
            "letting go where you picked it up is not an error"
        );
    }

    #[test]
    fn a_folder_cannot_be_copied_into_itself() {
        let root = scratch("into-itself");
        write(&root.join("pkg/deep/a.txt"), "a");

        let report = copy_into_dir(
            &*LocalHost::shared(),
            &[root.join("pkg")],
            &root.join("pkg/deep"),
            false,
        );

        assert_eq!(report.errors.len(), 1);
        assert!(report.copied.is_empty());
    }

    #[test]
    fn a_path_that_is_not_on_this_machine_is_refused_rather_than_guessed_at() {
        let root = scratch("elsewhere");
        let dest_dir = root.join("into");
        std::fs::create_dir_all(&dest_dir).unwrap();

        let report = copy_into_dir(
            &*LocalHost::shared(),
            &[PathBuf::from("/srv/on-the-far-end/note.txt")],
            &dest_dir,
            false,
        );

        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].0, "note.txt");
        assert!(report.copied.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cycle_cannot_make_the_copy_walk_forever() {
        let root = scratch("cycle");
        let src = root.join("from/pkg");
        std::fs::create_dir_all(&src).unwrap();
        std::os::unix::fs::symlink(&src, src.join("loop")).unwrap();
        let dest_dir = root.join("into");
        std::fs::create_dir_all(&dest_dir).unwrap();

        let report = copy_into_dir(&*LocalHost::shared(), &[src], &dest_dir, false);

        assert_eq!(report.errors.len(), 1, "the walk has to stop and say so");
        assert!(report.copied.is_empty());
    }
}

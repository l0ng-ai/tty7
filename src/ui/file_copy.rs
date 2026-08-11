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

/// How many working names beside a destination are tried before a replacement
/// gives up.
///
/// The first choice is normally free. It is not free when a copy was killed
/// outright and left its half-written tree behind, and — the reason these are
/// probed rather than cleared out of the way — it is not free when the name
/// happens to belong to a file of somebody's own.
const WORKING_NAME_TRIES: usize = 16;

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
        let dest = host.join(dir, &name);
        // Two sources of one drop can carry the same name: `~/a/notes.md` and
        // `~/b/notes.md` dragged in together. Nothing in the destination
        // objects to either of them, so neither is a conflict — they collide
        // with each other, and the second would be written straight over the
        // first with the panel reporting both as copied. One name is one file:
        // the first claim on it stands, and the rest are refused out loud
        // rather than allowed to eat it.
        if planned.iter().any(|(_, taken, _)| *taken == dest) {
            report.fail(&name, t(L10nKey::FileDropNameTaken).to_string());
            continue;
        }
        // Only ever on the pass that asks. `conflicts` is what re-opens the
        // "replace?" dialog, so filling it on the pass that carries the answer
        // asks the same question a second time — and because the panel reports
        // conflicts *or* errors and never both, it also swallows every error
        // that pass produced, including a replacement that failed.
        if !overwrite && host.exists(&dest) {
            report.conflicts.push(name.clone());
        }
        planned.push((src.clone(), dest, name));
    }

    if !overwrite && !report.conflicts.is_empty() {
        return report;
    }

    for (src, dest, name) in planned {
        // Replace rather than merge: a folder dropped onto a folder of the
        // same name should end up as what was dropped, not as the union of the
        // two, which is what writing into it one file at a time would leave.
        //
        // What is already there is not cleared away to make room, though. A
        // copy can fail halfway — a full disk, a control connection that drops
        // mid-tree — and clearing the way first is what turns that into a
        // destination holding neither the old thing nor a whole new one. The
        // copy lands beside it instead, and only takes its place once it is
        // whole.
        let done = match host.exists(&dest) {
            true => copy_over(host, &src, dir, &dest, &name),
            false => copy_tree(host, &src, &dest, 0),
        };
        match done {
            Ok(()) => report.copied.push(name),
            Err(e) => report.errors.push((name, e)),
        }
    }
    report
}

/// Copy `src` onto a `dest` that is already there, without `dest` ever being
/// the thing that is missing.
///
/// Three steps, and what was there survives all of them: the copy lands on a
/// working name beside it, the old thing is moved aside rather than removed,
/// and a rename — one metadata operation, not a tree walk — puts the new copy
/// in its place. A failure anywhere puts the old thing back, and in the one
/// case where even that fails it is still on disk under the name it was moved
/// to — which the panel is told, so it is a name somebody can find rather than
/// one they have to go looking for.
fn copy_over(host: &dyn Host, src: &Path, dir: &Path, dest: &Path, name: &str) -> io::Result<()> {
    let (staged, _) = free_name_beside(host, dir, "partial", name)?;
    if let Err(e) = copy_tree(host, src, &staged, 0) {
        let _ = host.remove(&staged, true);
        return Err(e);
    }
    let (aside, aside_name) = match free_name_beside(host, dir, "replaced", name) {
        Ok(aside) => aside,
        Err(e) => {
            let _ = host.remove(&staged, true);
            return Err(e);
        }
    };
    if let Err(e) = host.rename(dest, &aside) {
        let _ = host.remove(&staged, true);
        return Err(e);
    }
    if let Err(e) = host.rename(&staged, dest) {
        let _ = host.remove(&staged, true);
        if let Err(back) = host.rename(&aside, dest) {
            // Both renames are one control round trip each on a remote host, so
            // a link that drops between them lands here. Nothing is lost, but
            // it is under a name nobody chose — which is only better than lost
            // if the panel says so rather than the log.
            log::warn!(
                "{} could not be put back after a failed replacement and is at {}: {back}",
                dest.display(),
                aside.display()
            );
            return Err(io::Error::other(t_fmt(
                L10nKey::FileDropLeftAside,
                &[("name", &aside_name)],
            )));
        }
        return Err(e);
    }
    if let Err(e) = host.remove(&aside, true) {
        // The copy is in place and the drop succeeded; all that is left is the
        // old thing under a name nobody asked for.
        log::warn!(
            "{} outlived the copy that replaced it: {e}",
            aside.display()
        );
    }
    Ok(())
}

/// A name in `dir` that nothing is using yet, for a copy to land on before it
/// takes the destination's place. Returned with the bare name as well as the
/// path, because the one message that has to name it is a sentence in the
/// panel, where a whole path — on a remote host, someone else's whole path —
/// is not what the sentence wants.
///
/// The leading dot keeps the working copy out of the way of a tree that hides
/// dotfiles, and the tag says what it is to anyone who finds one that outlived
/// the copy that made it.
fn free_name_beside(
    host: &dyn Host,
    dir: &Path,
    tag: &str,
    name: &str,
) -> io::Result<(PathBuf, String)> {
    for n in 0..WORKING_NAME_TRIES {
        let candidate = match n {
            0 => format!(".tty7-{tag}-{name}"),
            n => format!(".tty7-{tag}-{n}-{name}"),
        };
        let path = host.join(dir, &candidate);
        if !host.exists(&path) {
            return Ok((path, candidate));
        }
    }
    Err(io::Error::other(
        t(L10nKey::FileDropNoWorkingName).to_string(),
    ))
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

    /// The working files a replacement makes, and is supposed to take away
    /// again whichever way it ends.
    fn leavings(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with(".tty7-"))
            .collect();
        names.sort();
        names
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
    fn two_dropped_items_of_the_same_name_do_not_land_on_top_of_each_other() {
        let root = scratch("same-name");
        let first = root.join("from/a/notes.md");
        let second = root.join("from/b/notes.md");
        write(&first, "first");
        write(&second, "second");
        let dest_dir = root.join("into");
        std::fs::create_dir_all(&dest_dir).unwrap();

        let report = copy_into_dir(&*LocalHost::shared(), &[first, second], &dest_dir, false);

        assert_eq!(report.copied, vec!["notes.md".to_string()]);
        assert_eq!(
            report.errors.len(),
            1,
            "the one that could not be written has to be said out loud"
        );
        assert_eq!(report.errors[0].0, "notes.md");
        assert_eq!(
            std::fs::read_to_string(dest_dir.join("notes.md")).unwrap(),
            "first",
            "the first claim on the name stands"
        );
    }

    #[test]
    fn the_pass_that_carries_the_answer_does_not_ask_again() {
        let root = scratch("no-re-ask");
        let src = root.join("from/note.txt");
        write(&src, "new");
        let dest_dir = root.join("into");
        write(&dest_dir.join("note.txt"), "old");

        let asked = copy_into_dir(&*LocalHost::shared(), &[src.clone()], &dest_dir, false);
        assert_eq!(asked.conflicts, vec!["note.txt".to_string()]);

        let answered = copy_into_dir(&*LocalHost::shared(), &[src], &dest_dir, true);

        assert_eq!(answered.copied, vec!["note.txt".to_string()]);
        assert!(
            answered.conflicts.is_empty(),
            "the question was already answered: asking it again re-opens the dialog, \
             and the panel reports conflicts instead of errors, so it also hides \
             whatever went wrong on this pass"
        );
    }

    #[test]
    fn a_finished_replacement_leaves_no_working_files_behind() {
        let root = scratch("replace-clean");
        let src = root.join("from/note.txt");
        write(&src, "new");
        let dest_dir = root.join("into");
        write(&dest_dir.join("note.txt"), "old");

        let report = copy_into_dir(&*LocalHost::shared(), &[src], &dest_dir, true);

        assert_eq!(report.copied, vec!["note.txt".to_string()]);
        assert_eq!(
            std::fs::read_to_string(dest_dir.join("note.txt")).unwrap(),
            "new"
        );
        assert!(
            leavings(&dest_dir).is_empty(),
            "the replacement left its working files behind: {:?}",
            leavings(&dest_dir)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_replacement_that_fails_partway_leaves_what_was_there() {
        let root = scratch("replace-fails");
        // A walk that cannot finish: the copy gets deep enough to have written
        // part of itself before it gives up, which is the shape of the full
        // disk and the dropped connection this path exists for.
        let src = root.join("from/pkg");
        write(&src.join("a.txt"), "a");
        std::os::unix::fs::symlink(&src, src.join("loop")).unwrap();
        let dest_dir = root.join("into");
        write(&dest_dir.join("pkg/keep.txt"), "keep");

        let report = copy_into_dir(&*LocalHost::shared(), &[src], &dest_dir, true);

        assert_eq!(report.errors.len(), 1);
        assert!(report.copied.is_empty());
        assert_eq!(
            std::fs::read_to_string(dest_dir.join("pkg/keep.txt")).unwrap(),
            "keep",
            "a copy that failed took the destination down with it"
        );
        assert!(
            leavings(&dest_dir).is_empty(),
            "the failed copy was left behind: {:?}",
            leavings(&dest_dir)
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

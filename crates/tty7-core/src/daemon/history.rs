//! A history file per pane, instead of one everybody writes into.
//!
//! Shells share history by sharing a file. Two panes running zsh with
//! `share_history`, or bash with `histappend`, are appending to the same
//! `~/.zsh_history` and reading each other's lines back — which is either the
//! feature or the problem, depending on what the panes are for. Someone with a
//! pane per task wants Up to walk *this* task's commands, not an interleaving
//! of four.
//!
//! So each pane gets its own file, named after the pane, and the shell is told
//! to use it. Two consequences follow from that being all it is:
//!
//! - **The pane does not start blank.** The file is seeded from whatever the
//!   shell's real history file was, so Up still reaches last week's commands.
//!   Only what is typed from now on stays local.
//! - **Nothing is lost when the pane closes.** What the pane added is appended
//!   back to the file it was seeded from. A per-pane history that evaporates
//!   would be a way of losing commands, not of organising them.
//!
//! The seeding is done by the shell rather than by the daemon, and that is the
//! only reason this works at all: the daemon cannot know where a given shell
//! keeps its history. `HISTFILE` is set by the user's rc file, to anywhere they
//! like, long after the pane's environment was decided. tty7's integration
//! snippet runs *after* that rc — it is appended to the rc it wraps — so it is
//! the one place where the real path is known. It copies the tail, records what
//! it copied, and repoints `HISTFILE`; this module only has to clean up.
//!
//! A shell with no tty7 integration is simply not covered: nothing sets the
//! variable, nothing reads it, and the pane keeps the history it always had.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The pane's own history file, as handed to the shell.
pub const FILE_ENV: &str = "TTY7_HISTFILE";

/// Shells whose integration snippet knows what to do with [`FILE_ENV`].
///
/// The list is exactly the shells that get a `HISTFILE` *and* a tty7 snippet to
/// repoint it in. fish keeps history in its own format under its own directory
/// and names sessions rather than files; PowerShell's history belongs to
/// PSReadLine, not to the shell. Plain `sh` gets no snippet at all, so naming a
/// file for it would produce a variable nothing ever reads.
fn understands(shell: &str) -> bool {
    let stem = Path::new(shell)
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    matches!(stem.as_str(), "zsh" | "bash")
}

pub fn enabled() -> bool {
    crate::core::config::Config::load().per_pane_history
}

fn dir() -> Option<PathBuf> {
    crate::core::config::config_path("history")
}

pub fn path_for(pane: u64) -> Option<PathBuf> {
    Some(dir()?.join(format!("pane-{pane}")))
}

/// What to put in a pane's environment, if it should have a history of its own.
pub fn env_for(pane: u64, shell: &str) -> Option<(String, String)> {
    if !understands(shell) || !enabled() {
        return None;
    }
    let path = path_for(pane)?;
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::debug!("no history directory ({e}); pane {pane} shares the usual one");
            return None;
        }
        // The files are written by the shell, under the user's umask, and a
        // command history is not something to leave to that. Closing the
        // directory is what makes the mode of what is inside it moot.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    Some((FILE_ENV.to_string(), path.to_string_lossy().into_owned()))
}

/// Hand a dead pane's history to the pane that replaces it.
///
/// A pane restored after the daemon died is a new pane with a new id, and
/// without this its Up key would start from whatever the global file held when
/// it was seeded — losing exactly the commands its predecessor ran, which are
/// the ones the user is most likely to want back.
pub fn carry(from: u64, to: u64) {
    let (Some(old), Some(new)) = (path_for(from), path_for(to)) else {
        return;
    };
    if !old.exists() {
        return;
    }
    for suffix in ["", ".seed", ".origin"] {
        let (a, b) = (with_suffix(&old, suffix), with_suffix(&new, suffix));
        let _ = std::fs::rename(&a, &b);
    }
}

/// Give a closed pane's commands back to the history file they came from.
///
/// Only what the pane added: the shell recorded how many bytes it seeded, and
/// everything past that mark is this pane's own. Appending the whole file would
/// duplicate the seed, and a pane that ran nothing would rewrite the user's
/// history for no reason at all.
pub fn retire(pane: u64) {
    let Some(path) = path_for(pane) else {
        return;
    };
    if !path.exists() {
        return;
    }
    merge_back(&path);
    for suffix in ["", ".seed", ".origin"] {
        let _ = std::fs::remove_file(with_suffix(&path, suffix));
    }
}

fn merge_back(path: &Path) {
    use std::io::Write as _;

    let Some(origin) = read_origin(&with_suffix(path, ".origin")) else {
        return;
    };
    let seeded: usize = std::fs::read_to_string(with_suffix(path, ".seed"))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0);
    let Ok(body) = std::fs::read(path) else {
        return;
    };
    // Shorter than its own seed means the file was replaced under us — by a
    // shell that rewrote it wholesale on exit, say. There is no way to tell
    // which part is new, and appending all of it would duplicate history the
    // user already has.
    let Some(added) = body.get(seeded..).filter(|added| !added.is_empty()) else {
        return;
    };
    let appended = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&origin)
        .and_then(|mut file| file.write_all(added));
    if let Err(e) = appended {
        log::debug!(
            "pane history could not be merged into {}: {e}",
            origin.display()
        );
    }
}

/// The path the shell said it seeded from, if it is one worth writing to.
///
/// It arrives from inside a pane, which is a place a user's rc file can put
/// anything at all. A relative path would be resolved against the daemon's
/// working directory rather than the pane's, so it would name a file nobody
/// meant. A path that does not exist yet is fine and is the first-ever shell's
/// case — its history file is created by this very append — but its directory
/// has to be one already, so a typo cannot make this build a tree.
fn read_origin(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path).ok()?;
    let origin = PathBuf::from(text.trim());
    if !origin.is_absolute() || origin.is_dir() {
        return None;
    }
    let parent_exists = origin.parent().is_some_and(Path::is_dir);
    (origin.is_file() || parent_exists).then_some(origin)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    if suffix.is_empty() {
        return path.to_path_buf();
    }
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Drop the files of panes that no longer exist.
///
/// A daemon killed outright never gets to retire anything, so its panes' files
/// are still here the next time one starts. Their commands are lost either way
/// — the mark that says which of them were new is only meaningful while the
/// shell that wrote them is alive — so what is left is to not accumulate them.
pub fn sweep(keep: &HashSet<u64>) {
    let Some(dir) = dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let stem = name
            .split_once('.')
            .map(|(stem, _)| stem)
            .unwrap_or(name.as_ref());
        let Some(id) = stem
            .strip_prefix("pane-")
            .and_then(|id| id.parse::<u64>().ok())
        else {
            continue;
        };
        if !keep.contains(&id) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared temp directory the rest of the suite pins, by the same name:
    /// the override is a process-wide `OnceLock`, so agreeing on the path is
    /// what keeps whoever gets there first from mattering.
    fn pin_config_dir() {
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::core::config::set_config_dir(dir);
    }

    /// Stand in for the shell's half: seed the pane's file from `origin` and
    /// record what was seeded, exactly as the integration snippet does.
    fn seed(pane: u64, origin: &Path) -> PathBuf {
        let path = path_for(pane).expect("a path under the config dir");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let seeded = std::fs::read(origin).unwrap_or_default();
        std::fs::write(&path, &seeded).unwrap();
        std::fs::write(with_suffix(&path, ".seed"), seeded.len().to_string()).unwrap();
        std::fs::write(
            with_suffix(&path, ".origin"),
            format!("{}\n", origin.display()),
        )
        .unwrap();
        path
    }

    fn a_global_history(name: &str, body: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("tty7-history-{}-{name}", std::process::id()));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn a_closed_pane_gives_back_only_what_it_added() {
        pin_config_dir();
        let origin = a_global_history("added", "old command\n");
        let pane = seed(80_001, &origin);
        std::fs::write(&pane, "old command\nnew command\n").unwrap();

        retire(80_001);

        assert_eq!(
            std::fs::read_to_string(&origin).unwrap(),
            "old command\nnew command\n",
            "the seed is already in the origin; appending it again would duplicate history"
        );
        assert!(!pane.exists(), "a closed pane leaves no file behind");
        assert!(!with_suffix(&pane, ".seed").exists());
        assert!(!with_suffix(&pane, ".origin").exists());
        let _ = std::fs::remove_file(&origin);
    }

    #[test]
    fn a_pane_that_ran_nothing_does_not_touch_the_users_history() {
        pin_config_dir();
        let origin = a_global_history("untouched", "old command\n");
        seed(80_002, &origin);

        retire(80_002);

        assert_eq!(
            std::fs::read_to_string(&origin).unwrap(),
            "old command\n",
            "opening a pane and closing it must not rewrite anything"
        );
        let _ = std::fs::remove_file(&origin);
    }

    #[test]
    fn a_file_that_shrank_under_us_is_left_alone() {
        pin_config_dir();
        let origin = a_global_history("shrunk", "old command\n");
        let pane = seed(80_003, &origin);
        // What a shell that rewrites its history wholesale on exit leaves.
        std::fs::write(&pane, "x\n").unwrap();

        retire(80_003);

        assert_eq!(
            std::fs::read_to_string(&origin).unwrap(),
            "old command\n",
            "with no way to tell which lines are new, adding any of them is a guess"
        );
        let _ = std::fs::remove_file(&origin);
    }

    #[test]
    fn an_origin_that_is_not_a_real_absolute_file_is_refused() {
        pin_config_dir();
        let path = path_for(80_004).expect("a path");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "some command\n").unwrap();
        std::fs::write(with_suffix(&path, ".seed"), "0").unwrap();
        std::fs::write(with_suffix(&path, ".origin"), "../relative/history\n").unwrap();

        // The point is that it does not panic and does not create anything: the
        // value came from inside a pane, where an rc file can put anything.
        retire(80_004);
        assert!(!Path::new("../relative/history").exists());
    }

    #[test]
    fn a_restored_pane_inherits_its_predecessors_history() {
        pin_config_dir();
        let origin = a_global_history("carried", "old command\n");
        let before = seed(80_005, &origin);
        std::fs::write(&before, "old command\nfrom the dead pane\n").unwrap();

        carry(80_005, 80_006);

        assert!(
            !before.exists(),
            "the old name is not left behind as a copy"
        );
        let after = path_for(80_006).expect("a path");
        assert_eq!(
            std::fs::read_to_string(&after).unwrap(),
            "old command\nfrom the dead pane\n",
            "the replacement pane's Up key has to reach what its predecessor ran"
        );
        assert_eq!(
            std::fs::read_to_string(with_suffix(&after, ".seed")).unwrap(),
            "12",
            "the mark has to travel too, or the merge would hand back the seed as new"
        );

        retire(80_006);
        assert_eq!(
            std::fs::read_to_string(&origin).unwrap(),
            "old command\nfrom the dead pane\n"
        );
        let _ = std::fs::remove_file(&origin);
    }

    #[test]
    fn the_sweep_keeps_only_the_panes_that_still_exist() {
        pin_config_dir();
        let origin = a_global_history("swept", "old\n");
        let kept = seed(80_007, &origin);
        let dropped = seed(80_008, &origin);

        sweep(&HashSet::from([80_007]));

        assert!(kept.exists(), "a live pane keeps its history");
        assert!(!dropped.exists(), "a pane that is gone does not");
        assert!(
            !with_suffix(&dropped, ".seed").exists(),
            "the sidecars go with it"
        );
        retire(80_007);
        let _ = std::fs::remove_file(&origin);
    }

    #[test]
    fn only_shells_whose_integration_knows_the_variable_are_offered_it() {
        assert!(understands("/bin/zsh"));
        assert!(understands("/opt/homebrew/bin/bash"));
        assert!(
            !understands("/bin/sh"),
            "no integration snippet reaches a plain sh, so nothing would read the variable"
        );
        assert!(
            !understands("/usr/local/bin/fish"),
            "fish keeps sessions, not a HISTFILE; naming one would do nothing"
        );
        assert!(!understands("pwsh.exe"));
        assert!(!understands("C:\\Windows\\System32\\cmd.exe"));
    }
}

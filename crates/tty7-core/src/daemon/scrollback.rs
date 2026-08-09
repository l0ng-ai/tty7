//! What a pane looked like, kept where the daemon's own death cannot reach it.
//!
//! The replay ring already holds every pane's recent output so a reattaching
//! client can be shown the screen it left. It holds it in this process, which
//! makes it exactly as durable as this process: a crash, a `kill -9`, or a
//! reboot takes the panes and their scrollback together, and the window comes
//! back to a row of blank shells with no trace of what was in them.
//!
//! The planned upgrade path does not lose anything — see `daemon::handoff`,
//! which carries the live ptys and their rings into the new binary without the
//! shells ever noticing. This module is for the paths a handoff cannot cover,
//! where the process does not get to run any code on its way out. It cannot
//! keep the *processes*; nothing written to a file can. It keeps the picture.
//!
//! Consequences of that being the goal:
//!
//! - **The snapshot is periodic, not write-through.** Every pty byte reaching
//!   the disk would be an enormous amount of write amplification to buy a few
//!   seconds of freshness at the tail of a crash. The writer runs on a timer
//!   and skips panes whose ring has not changed.
//! - **It is capped far below the ring.** [`SNAPSHOT_CAP`] keeps what fills a
//!   screen or two, not the ring's whole 8 MiB. The value of scrollback decays
//!   steeply with distance from the bottom, and every byte here is a byte of
//!   someone's terminal sitting on disk.
//! - **It is off unless asked for.** These bytes include whatever was echoed
//!   into the pane: tokens, `env` output, an agent's transcript. In memory
//!   they die with the daemon. On disk they outlive it, which is the whole
//!   point and also the whole risk, so the choice is the user's.
//! - **It is dropped as soon as it is meaningless.** A pane that is closed, or
//!   that no workspace refers to any more, has its file removed. Retention is
//!   by relevance, not by calendar: a snapshot of a pane nobody will reopen is
//!   not worth keeping for a month, or for an hour.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::daemon::protocol::WinSize;

/// How much of a pane's ring reaches the disk, per pane.
///
/// A screenful of dense output is on the order of 20 KiB once escape sequences
/// are counted, so this is "the last several screens" rather than "everything".
/// The in-memory ring stays at its own, much larger, cap: this bound is about
/// what is worth persisting, not what is worth keeping while running.
pub const SNAPSHOT_CAP: usize = 256 * 1024;

/// How often the writer looks for panes whose ring has moved.
pub const SNAPSHOT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

const MAGIC: &[u8; 8] = b"TTY7SB\x01\x00";

/// One geometry-homogeneous run of pane output, the unit the replay ring is
/// segmented into: replaying a segment means telling the client the size those
/// bytes were written at, then handing it the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub size: WinSize,
    pub bytes: Vec<u8>,
}

/// Drop bytes from the oldest segments until the total fits `cap`.
///
/// Trimming from the front is what makes a truncated snapshot still make sense:
/// terminal output is only interpretable forwards, so the tail can stand on its
/// own in a way the head cannot. A partially eaten segment keeps its geometry —
/// the bytes that remain were still written at that size.
pub fn trim_to(segments: &mut Vec<Segment>, cap: usize) {
    let total: usize = segments.iter().map(|s| s.bytes.len()).sum();
    let mut over = total.saturating_sub(cap);
    while over > 0 {
        let Some(head) = segments.first_mut() else {
            return;
        };
        let drop = over.min(head.bytes.len());
        head.bytes.drain(..drop);
        over -= drop;
        if !head.bytes.is_empty() {
            continue;
        }
        // The last segment is kept even when it is empty: the ring this feeds
        // back into is defined by always having a tail to append to.
        if segments.len() == 1 {
            return;
        }
        segments.remove(0);
    }
}

pub fn encode(segments: &[Segment]) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        MAGIC.len() + 4 + segments.iter().map(|s| s.bytes.len() + 12).sum::<usize>(),
    );
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(segments.len() as u32).to_le_bytes());
    for seg in segments {
        out.extend_from_slice(&seg.size.cols.to_le_bytes());
        out.extend_from_slice(&seg.size.rows.to_le_bytes());
        out.extend_from_slice(&seg.size.cell_w.to_le_bytes());
        out.extend_from_slice(&seg.size.cell_h.to_le_bytes());
        out.extend_from_slice(&(seg.bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&seg.bytes);
    }
    out
}

/// `None` for anything that is not a snapshot this build wrote.
///
/// A truncated file is the ordinary failure here — the writer renames into
/// place, but a filesystem that reordered the rename against the data, or a
/// half-written file from an older scheme, both land as a short read. There is
/// nothing to salvage and nothing at stake in refusing: the pane comes back
/// blank, which is what it did before this module existed.
pub fn decode(raw: &[u8]) -> Option<Vec<Segment>> {
    let mut cur = raw.strip_prefix(MAGIC.as_slice())?;
    let count = u32::from_le_bytes(take(&mut cur, 4)?.try_into().ok()?) as usize;
    let mut segments = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let cols = u16::from_le_bytes(take(&mut cur, 2)?.try_into().ok()?);
        let rows = u16::from_le_bytes(take(&mut cur, 2)?.try_into().ok()?);
        let cell_w = u16::from_le_bytes(take(&mut cur, 2)?.try_into().ok()?);
        let cell_h = u16::from_le_bytes(take(&mut cur, 2)?.try_into().ok()?);
        let len = u32::from_le_bytes(take(&mut cur, 4)?.try_into().ok()?) as usize;
        let bytes = take(&mut cur, len)?.to_vec();
        segments.push(Segment {
            size: WinSize {
                cols,
                rows,
                cell_w,
                cell_h,
            },
            bytes,
        });
    }
    Some(segments)
}

fn take<'a>(cur: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    if cur.len() < n {
        return None;
    }
    let (head, rest) = cur.split_at(n);
    *cur = rest;
    Some(head)
}

/// Whether the user has asked for scrollback to outlive the daemon.
pub fn enabled() -> bool {
    crate::core::config::Config::load().persist_scrollback
}

fn dir() -> Option<PathBuf> {
    crate::core::config::config_path("scrollback")
}

fn path_for(pane_id: u64) -> Option<PathBuf> {
    Some(dir()?.join(format!("{pane_id}.bin")))
}

/// Write one pane's snapshot, replacing whatever was there.
///
/// Renamed into place so a reader never sees a half-written file, and mode 0600
/// from creation rather than after the fact — a window in which someone else's
/// terminal output is world-readable is not one worth leaving open.
pub fn save(pane_id: u64, segments: &[Segment]) {
    let Some(path) = path_for(pane_id) else {
        return;
    };
    let Some(parent) = path.parent().map(|p| p.to_path_buf()) else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&parent) {
        log::debug!("no scrollback directory ({e}); pane {pane_id} is not persisted");
        return;
    }
    let temp = parent.join(format!("{pane_id}.{}.tmp", std::process::id()));
    if let Err(e) = write_private(&temp, &encode(segments)) {
        log::debug!("could not stage pane {pane_id}'s scrollback: {e}");
        let _ = std::fs::remove_file(&temp);
        return;
    }
    if let Err(e) = std::fs::rename(&temp, &path) {
        log::debug!("could not store pane {pane_id}'s scrollback: {e}");
        let _ = std::fs::remove_file(&temp);
    }
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    // Windows inherits the config directory's ACL, which is already per-user.
    std::fs::write(path, bytes)
}

pub fn load(pane_id: u64) -> Option<Vec<Segment>> {
    let raw = std::fs::read(path_for(pane_id)?).ok()?;
    match decode(&raw) {
        Some(segments) => Some(segments),
        None => {
            log::debug!("pane {pane_id}'s stored scrollback is not readable by this build");
            None
        }
    }
}

pub fn forget(pane_id: u64) {
    if let Some(path) = path_for(pane_id) {
        let _ = std::fs::remove_file(path);
    }
}

/// Drop snapshots for panes that nothing refers to any more.
///
/// Called with the set of pane ids the machine tree still names. A file outside
/// it belongs to a pane that was closed, or that lived in a workspace since
/// deleted; either way nobody can ask to restore it, so keeping it is only a
/// way to leave terminal output on disk indefinitely.
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
        let Some(id) = name
            .strip_suffix(".bin")
            .and_then(|stem| stem.parse::<u64>().ok())
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

    fn size(cols: u16, rows: u16) -> WinSize {
        WinSize {
            cols,
            rows,
            cell_w: 8,
            cell_h: 17,
        }
    }

    fn seg(cols: u16, bytes: &[u8]) -> Segment {
        Segment {
            size: size(cols, 24),
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn segments_survive_the_round_trip_with_their_geometry() {
        let segments = vec![seg(80, b"before the resize"), seg(120, b"after it")];
        let decoded = decode(&encode(&segments)).expect("what we wrote is readable");
        assert_eq!(
            decoded, segments,
            "a replayed snapshot has to say which size each run of bytes was written at, \
             or the client rewraps old output to the current width"
        );
    }

    #[test]
    fn a_truncated_or_foreign_file_decodes_to_nothing() {
        let raw = encode(&[seg(80, b"hello")]);
        assert!(
            decode(&raw[..raw.len() - 2]).is_none(),
            "a short read must not be mistaken for a shorter snapshot"
        );
        assert!(decode(b"").is_none(), "an empty file is not a snapshot");
        assert!(
            decode(b"PLAIN TEXT, NOT OURS").is_none(),
            "only files this scheme wrote are read back"
        );
    }

    #[test]
    fn trimming_keeps_the_tail_and_the_geometry_of_what_it_keeps() {
        let mut segments = vec![seg(80, b"0123456789"), seg(120, b"abcdefghij")];
        trim_to(&mut segments, 12);
        let kept: Vec<u8> = segments.iter().flat_map(|s| s.bytes.clone()).collect();
        assert_eq!(
            kept, b"89abcdefghij",
            "terminal output only reads forwards, so a cap has to eat the head"
        );
        assert_eq!(
            segments[0].size,
            size(80, 24),
            "the bytes left in a partly eaten segment were still written at its size"
        );
    }

    #[test]
    fn trimming_below_one_segment_still_leaves_something_to_replay() {
        let mut segments = vec![seg(80, b"0123456789")];
        trim_to(&mut segments, 0);
        assert_eq!(
            segments.len(),
            1,
            "the ring always has a tail; so does this"
        );
        assert!(segments[0].bytes.is_empty());
    }

    /// The same shared temp directory every other module's tests pin, by the
    /// same name: the override is a process-wide `OnceLock`, so the first
    /// caller decides for all of them and agreeing on the path is what keeps
    /// that harmless.
    fn pin_config_dir() {
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::core::config::set_config_dir(dir);
    }

    #[test]
    fn a_stored_screen_comes_back_and_can_be_dropped() {
        pin_config_dir();
        let pane = 90_001;
        save(pane, &[seg(80, b"what the pane had on it")]);
        assert_eq!(
            load(pane).expect("a saved screen is readable"),
            vec![seg(80, b"what the pane had on it")],
        );
        forget(pane);
        assert!(
            load(pane).is_none(),
            "a pane the user closed leaves nothing behind to restore"
        );
    }

    #[test]
    fn saving_twice_replaces_rather_than_appends() {
        pin_config_dir();
        let pane = 90_002;
        save(pane, &[seg(80, b"first")]);
        save(pane, &[seg(80, b"second")]);
        assert_eq!(
            load(pane).expect("still readable"),
            vec![seg(80, b"second")],
            "each write is the pane's current screen, not another slice of history"
        );
        forget(pane);
    }

    #[test]
    fn the_sweep_keeps_only_panes_something_can_still_ask_for() {
        pin_config_dir();
        let (kept, dropped) = (90_003, 90_004);
        save(kept, &[seg(80, b"in a workspace")]);
        save(dropped, &[seg(80, b"in no workspace")]);
        sweep(&HashSet::from([kept]));
        assert!(
            load(kept).is_some(),
            "a pane a tree still names is restorable"
        );
        assert!(
            load(dropped).is_none(),
            "nobody can ask to restore a pane no tree refers to, so its output must not sit on disk"
        );
        forget(kept);
    }

    #[cfg(unix)]
    #[test]
    fn a_stored_screen_is_not_readable_by_anyone_else() {
        use std::os::unix::fs::PermissionsExt as _;
        pin_config_dir();
        let pane = 90_005;
        save(pane, &[seg(80, b"a token someone echoed")]);
        let mode = std::fs::metadata(path_for(pane).expect("a path under the config dir"))
            .expect("the file exists")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "this file is a copy of someone's terminal; group and other get nothing"
        );
        forget(pane);
    }

    #[test]
    fn an_empty_snapshot_round_trips() {
        assert_eq!(
            decode(&encode(&[])).expect("still a valid snapshot"),
            Vec::new(),
            "a pane that has printed nothing is not a corrupt file"
        );
    }
}

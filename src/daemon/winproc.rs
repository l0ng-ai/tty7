//! Windows-only process-table helpers.
//!
//! Windows has no ConPTY analogue of a Unix "foreground process group", so the
//! daemon can't ask the pty who's in front (that's why `pane`'s macOS/Linux
//! foreground queries have no Windows counterpart). What it *can* do is walk the
//! process table from the shell's own pid. Two pane operations need that:
//!
//!   - **titling** a pane by the command running under the shell
//!     ([`foreground_name`]), so Windows tabs show `git` / `node` / … instead of
//!     staying blank; and
//!   - **hangup** ([`descendants`]), because `portable-pty`'s Windows `kill`
//!     terminates only the shell process — its children would otherwise be
//!     reparented and linger, some still attached to the ConPTY, which keeps the
//!     pane reader's blocking read from ever hitting EOF.
//!
//! The Win32 surface is a thin [`snapshot`]/[`terminate`] pair; all the tree
//! logic is pure over a plain [`Proc`] list and unit-tested without a live
//! process. Note that reading another process's *cwd* is deliberately not here:
//! it needs PEB traversal via `ReadProcessMemory`, which is undocumented and
//! fragile across bitness/elevation — so cwd on Windows stays sourced from OSC 7
//! (see `pane::foreground_cwd`).

use std::collections::{HashSet, VecDeque};

/// One process-table row: a pid, its parent's pid, and the executable basename.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Proc {
    pub pid: u32,
    pub parent: u32,
    pub name: String,
}

/// BFS the table from `root` by parent link, returning `(depth, pid, name)` for
/// every reachable descendant (root excluded), shallowest-first. A `seen` set
/// makes the walk robust to Windows pid reuse: a stale parent link that points
/// back into the tree (or a process that lists itself as its own parent) can't
/// create a cycle, because each pid is expanded at most once.
fn walk(procs: &[Proc], root: u32) -> Vec<(u32, u32, &str)> {
    let mut seen = HashSet::new();
    seen.insert(root);
    let mut frontier = VecDeque::new();
    frontier.push_back((root, 0u32));
    let mut out = Vec::new();
    while let Some((parent, depth)) = frontier.pop_front() {
        for p in procs {
            if p.parent == parent && p.pid != parent && seen.insert(p.pid) {
                out.push((depth + 1, p.pid, p.name.as_str()));
                frontier.push_back((p.pid, depth + 1));
            }
        }
    }
    out
}

/// Descendants of `root` (children, grandchildren, …), each listed once and
/// ordered deepest-first — so a caller terminating them tears down leaf commands
/// before the shells that spawned them. `root` itself is never included.
pub(crate) fn descendants(procs: &[Proc], root: u32) -> Vec<u32> {
    let mut walked = walk(procs, root);
    // Deepest depth first; stable within a depth, so ordering is deterministic.
    walked.sort_by_key(|&(depth, ..)| std::cmp::Reverse(depth));
    walked.into_iter().map(|(_, pid, _)| pid).collect()
}

/// The foreground command's exe name for a shell rooted at `shell_pid`: the
/// deepest descendant (the thing actually running under the shell), or `None`
/// when the shell has no descendants at all — i.e. it's idle at its prompt, in
/// which case the caller keeps the pane's existing title. Ties at equal depth
/// break toward the largest pid (roughly the most recently created) so the pick
/// is stable frame to frame.
pub(crate) fn foreground_name(procs: &[Proc], shell_pid: u32) -> Option<String> {
    walk(procs, shell_pid)
        .into_iter()
        .max_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)))
        .map(|(_, _, name)| name.to_string())
}

/// Snapshot every process on the system as a [`Proc`] list, via a Toolhelp
/// snapshot. Best effort: any failure yields an empty list (the callers then
/// simply do nothing — no title, no extra kills).
pub(crate) fn snapshot() -> Vec<Proc> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let mut out = Vec::new();
    // SAFETY: a textbook Toolhelp enumeration. The snapshot handle is closed on
    // every exit path; `PROCESSENTRY32W` is zeroed and its `dwSize` set before the
    // first call, exactly as the API requires; each `szExeFile` is a NUL-terminated
    // UTF-16 buffer we read within its fixed length.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                out.push(Proc {
                    pid: entry.th32ProcessID,
                    parent: entry.th32ParentProcessID,
                    name: exe_name(&entry.szExeFile),
                });
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

/// Force-terminate `pid`. Best effort: a process we can't open (already gone, or
/// access denied) is simply skipped.
pub(crate) fn terminate(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_TERMINATE, TerminateProcess,
    };
    // SAFETY: open → terminate → close on a single pid. A null handle (the process
    // exited or we lack rights) is checked before use; the handle is always closed.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

/// The executable basename from a NUL-terminated UTF-16 `szExeFile` field.
fn exe_name(raw: &[u16]) -> String {
    let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..len])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, parent: u32, name: &str) -> Proc {
        Proc {
            pid,
            parent,
            name: name.to_string(),
        }
    }

    /// A realistic tree: only the shell's own descendants come back, unrelated
    /// processes (and the shell's ancestors) are excluded.
    #[test]
    fn descendants_collects_only_the_shell_subtree() {
        let procs = vec![
            p(1, 0, "System"),
            p(100, 1, "powershell.exe"), // the shell
            p(200, 100, "git.exe"),      // child
            p(300, 200, "less.exe"),     // grandchild
            p(201, 100, "node.exe"),     // another child
            p(999, 1, "explorer.exe"),   // unrelated
        ];
        let mut got = descendants(&procs, 100);
        got.sort();
        assert_eq!(got, vec![200, 201, 300]);
    }

    /// Descendants come back deepest-first, so a terminator hits leaves before
    /// the parents that spawned them.
    #[test]
    fn descendants_are_ordered_deepest_first() {
        let procs = vec![p(100, 1, "sh"), p(200, 100, "a"), p(300, 200, "b")];
        assert_eq!(descendants(&procs, 100), vec![300, 200]);
    }

    /// Pid reuse can make a parent link point back into the tree; the walk must
    /// not loop forever on that.
    #[test]
    fn descendants_survive_a_pid_reuse_cycle() {
        // 200's parent is 100 (real child); 100 *also* claims 200 as its parent
        // (a reused pid). 100 is the root, so it's never re-expanded.
        let procs = vec![p(100, 200, "a"), p(200, 100, "b")];
        assert_eq!(descendants(&procs, 100), vec![200]);
    }

    /// A self-parenting row (pid == parent, as some system pids report) can't
    /// wedge the walk either.
    #[test]
    fn descendants_survive_self_parenting() {
        let procs = vec![p(100, 1, "sh"), p(100, 100, "self")];
        // The only row whose parent is 100 is the self-referential one, which is
        // rejected (pid == parent), so nothing descends.
        assert!(descendants(&procs, 100).is_empty());
    }

    /// A shell sitting idle at its prompt (no children) has no descendants.
    #[test]
    fn descendants_empty_without_children() {
        let procs = vec![p(100, 1, "sh"), p(999, 1, "other")];
        assert!(descendants(&procs, 100).is_empty());
    }

    /// The pane title is the deepest running command, not the shell.
    #[test]
    fn foreground_name_is_the_deepest_command() {
        let procs = vec![
            p(100, 1, "powershell.exe"),
            p(200, 100, "git.exe"),
            p(300, 200, "less.exe"),
        ];
        assert_eq!(foreground_name(&procs, 100).as_deref(), Some("less.exe"));
    }

    /// Idle at the prompt → no foreground command, so the caller keeps the
    /// existing title rather than blanking it.
    #[test]
    fn foreground_name_is_none_at_idle_prompt() {
        let procs = vec![p(100, 1, "powershell.exe"), p(999, 1, "explorer.exe")];
        assert_eq!(foreground_name(&procs, 100), None);
    }

    /// Two equally-deep children resolve deterministically (largest pid wins) so
    /// the title doesn't flicker between them.
    #[test]
    fn foreground_name_breaks_depth_ties_by_pid() {
        let procs = vec![p(100, 1, "sh"), p(200, 100, "a"), p(201, 100, "b")];
        assert_eq!(foreground_name(&procs, 100).as_deref(), Some("b"));
    }

    /// UTF-16 `szExeFile` decoding stops at the NUL terminator.
    #[test]
    fn exe_name_reads_up_to_the_nul() {
        let mut raw = [0u16; 260];
        for (i, c) in "cmd.exe".encode_utf16().enumerate() {
            raw[i] = c;
        }
        assert_eq!(exe_name(&raw), "cmd.exe");
    }
}

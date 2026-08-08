use std::collections::{HashSet, VecDeque};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Proc {
    pub pid: u32,
    pub parent: u32,
    pub name: String,
}

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

pub(crate) fn descendants(procs: &[Proc], root: u32) -> Vec<u32> {
    let mut walked = walk(procs, root);
    walked.sort_by_key(|&(depth, ..)| std::cmp::Reverse(depth));
    walked.into_iter().map(|(_, pid, _)| pid).collect()
}

pub(crate) fn foreground_name(procs: &[Proc], shell_pid: u32) -> Option<String> {
    walk(procs, shell_pid)
        .into_iter()
        .max_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)))
        .map(|(_, _, name)| name.to_string())
}

pub(crate) fn snapshot() -> Vec<Proc> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let mut out = Vec::new();
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

pub(crate) fn terminate(pid: u32) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !handle.is_null() {
            TerminateProcess(handle, 1);
            CloseHandle(handle);
        }
    }
}

fn exe_name(raw: &[u16]) -> String {
    let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..len])
}

/// The full image path of a running process, or `None` for one this user
/// cannot query (system processes, another session's).
pub(crate) fn image_path(pid: u32) -> Option<std::path::PathBuf> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
        CloseHandle(handle);
        if ok == 0 {
            return None;
        }
        Some(std::path::PathBuf::from(String::from_utf16_lossy(
            &buf[..len as usize],
        )))
    }
}

/// Waits until the process is gone, up to `timeout`. Returns whether it exited
/// in time. A pid that cannot be opened is reported as exited: the handle is
/// what names the process, and no handle means there is nothing left to wait
/// on that this user could ever observe.
pub(crate) fn wait_for_exit(pid: u32, timeout: std::time::Duration) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return true;
        }
        let millis = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
        let result = WaitForSingleObject(handle, millis);
        CloseHandle(handle);
        result == WAIT_OBJECT_0
    }
}

/// Terminates every descendant of `root` and waits for them to release their
/// images, bounded by `timeout` overall. Deepest-first, like the per-pane
/// kill, so a parent never respawns a child we already visited.
pub(crate) fn reap_descendants_of(root: u32, timeout: std::time::Duration) {
    let deadline = std::time::Instant::now() + timeout;
    let targets = descendants(&snapshot(), root);
    for &pid in &targets {
        terminate(pid);
    }
    for pid in targets {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if !wait_for_exit(pid, left) {
            log::warn!("descendant process {pid} did not exit in time");
        }
    }
}

/// Pids (other than the caller's own) whose executable image lives under
/// `dir`. These are the processes that keep Windows from replacing the files
/// there: a stale daemon, or ConPTY hosts orphaned by a daemon that never got
/// to shut down.
pub(crate) fn processes_running_from(dir: &std::path::Path) -> Vec<u32> {
    let own = std::process::id();
    snapshot()
        .iter()
        .filter(|p| p.pid != own && p.pid > 4)
        .filter(|p| image_path(p.pid).is_some_and(|image| path_is_under(&image, dir)))
        .map(|p| p.pid)
        .collect()
}

/// Whether `path` names a file inside `dir`, the way the filesystem sees it:
/// case-insensitive, and only at a component boundary so `...\tty7-two` is not
/// inside `...\tty7`.
pub(crate) fn path_is_under(path: &std::path::Path, dir: &std::path::Path) -> bool {
    let (Some(path), Some(dir)) = (path.to_str(), dir.to_str()) else {
        return false;
    };
    let path = path.to_lowercase().replace('/', "\\");
    let mut dir = dir
        .trim_end_matches(['\\', '/'])
        .to_lowercase()
        .replace('/', "\\");
    if dir.is_empty() {
        return false;
    }
    dir.push('\\');
    path.starts_with(&dir)
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

    #[test]
    fn descendants_collects_only_the_shell_subtree() {
        let procs = vec![
            p(1, 0, "System"),
            p(100, 1, "powershell.exe"),
            p(200, 100, "git.exe"),
            p(300, 200, "less.exe"),
            p(201, 100, "node.exe"),
            p(999, 1, "explorer.exe"),
        ];
        let mut got = descendants(&procs, 100);
        got.sort();
        assert_eq!(got, vec![200, 201, 300]);
    }

    #[test]
    fn descendants_are_ordered_deepest_first() {
        let procs = vec![p(100, 1, "sh"), p(200, 100, "a"), p(300, 200, "b")];
        assert_eq!(descendants(&procs, 100), vec![300, 200]);
    }

    #[test]
    fn descendants_survive_a_pid_reuse_cycle() {
        let procs = vec![p(100, 200, "a"), p(200, 100, "b")];
        assert_eq!(descendants(&procs, 100), vec![200]);
    }

    #[test]
    fn descendants_survive_self_parenting() {
        let procs = vec![p(100, 1, "sh"), p(100, 100, "self")];
        assert!(descendants(&procs, 100).is_empty());
    }

    #[test]
    fn descendants_empty_without_children() {
        let procs = vec![p(100, 1, "sh"), p(999, 1, "other")];
        assert!(descendants(&procs, 100).is_empty());
    }

    #[test]
    fn foreground_name_is_the_deepest_command() {
        let procs = vec![
            p(100, 1, "powershell.exe"),
            p(200, 100, "git.exe"),
            p(300, 200, "less.exe"),
        ];
        assert_eq!(foreground_name(&procs, 100).as_deref(), Some("less.exe"));
    }

    #[test]
    fn foreground_name_is_none_at_idle_prompt() {
        let procs = vec![p(100, 1, "powershell.exe"), p(999, 1, "explorer.exe")];
        assert_eq!(foreground_name(&procs, 100), None);
    }

    #[test]
    fn foreground_name_breaks_depth_ties_by_pid() {
        let procs = vec![p(100, 1, "sh"), p(200, 100, "a"), p(201, 100, "b")];
        assert_eq!(foreground_name(&procs, 100).as_deref(), Some("b"));
    }

    #[test]
    fn path_is_under_respects_component_boundaries_and_case() {
        use std::path::Path;
        let dir = Path::new(r"C:\Users\me\Apps\tty7");
        assert!(path_is_under(
            Path::new(r"C:\Users\me\Apps\tty7\OpenConsole.exe"),
            dir
        ));
        assert!(path_is_under(
            Path::new(r"c:\users\me\apps\TTY7\server\tty7-server"),
            dir
        ));
        assert!(!path_is_under(
            Path::new(r"C:\Users\me\Apps\tty7-two\a.exe"),
            dir
        ));
        assert!(!path_is_under(Path::new(r"C:\Users\me\Apps\tty7"), dir));
        assert!(path_is_under(
            Path::new(r"C:\Users\me\Apps\tty7\x.exe"),
            Path::new(r"C:\Users\me\Apps\tty7\"),
        ));
        assert!(!path_is_under(Path::new(r"C:\anything"), Path::new("")));
    }

    #[test]
    fn exe_name_reads_up_to_the_nul() {
        let mut raw = [0u16; 260];
        for (i, c) in "cmd.exe".encode_utf16().enumerate() {
            raw[i] = c;
        }
        assert_eq!(exe_name(&raw), "cmd.exe");
    }
}

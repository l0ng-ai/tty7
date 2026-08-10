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

/// When the process started, or `None` for one this user cannot query. What
/// makes a pid an identity: a process claiming to be the writer of a file
/// must have started *before* that file was written, or it merely inherited
/// the writer's number.
pub(crate) fn creation_time(pid: u32) -> Option<std::time::SystemTime> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut creation: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        CloseHandle(handle);
        if ok == 0 {
            return None;
        }
        let ticks = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
        // FILETIME counts 100 ns ticks from 1601-01-01; Unix time starts
        // 11 644 473 600 seconds later.
        let unix_ticks = ticks.checked_sub(11_644_473_600 * 10_000_000)?;
        Some(
            std::time::UNIX_EPOCH
                + std::time::Duration::new(
                    unix_ticks / 10_000_000,
                    (unix_ticks % 10_000_000) as u32 * 100,
                ),
        )
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

/// Terminates every pid in order and waits for each to release its image,
/// all bounded by the one `deadline` — never a fresh timeout per process.
/// Callers pass parents after children when a parent could respawn one.
pub(crate) fn terminate_and_wait_all(pids: &[u32], deadline: std::time::Instant) {
    for &pid in pids {
        terminate(pid);
    }
    for &pid in pids {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if !wait_for_exit(pid, left) {
            log::warn!("process {pid} did not exit in time");
        }
    }
}

/// Terminates every descendant of `root` and waits for them to release their
/// images, bounded by `timeout` overall. Deepest-first, like the per-pane
/// kill, so a parent never respawns a child we already visited.
pub(crate) fn reap_descendants_of(root: u32, timeout: std::time::Duration) {
    terminate_and_wait_all(
        &descendants(&snapshot(), root),
        std::time::Instant::now() + timeout,
    );
}

/// Pids (other than the caller's own) whose executable image lives under
/// `dir`. These are the processes that keep Windows from replacing the files
/// there: a stale daemon, or ConPTY hosts orphaned by a daemon that never got
/// to shut down.
pub(crate) fn processes_running_from(dir: &std::path::Path) -> Vec<u32> {
    let own = std::process::id();
    // Image paths come back in long, resolved form, so a `dir` spelled
    // through a junction, a subst drive, or an 8.3 short name would never
    // prefix-match them. Trying the canonical spelling as well closes that —
    // one syscall for the directory, not one per process. (A miss here still
    // cannot let an installer proceed over a lock: `wait_until_images_unlocked`
    // probes the files themselves and its callers abort on timeout.)
    let canonical = std::fs::canonicalize(dir).ok().and_then(|real| {
        let text = real.to_str()?;
        Some(std::path::PathBuf::from(
            text.strip_prefix(r"\\?\").unwrap_or(text),
        ))
    });
    snapshot()
        .iter()
        .filter(|p| p.pid != own && p.pid > 4)
        .filter(|p| {
            image_path(p.pid).is_some_and(|image| {
                path_is_under(&image, dir)
                    || canonical
                        .as_deref()
                        .is_some_and(|canonical| path_is_under(&image, canonical))
            })
        })
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

/// Makes Ctrl+C reach the processes this one goes on to spawn.
///
/// Windows keeps an "ignore Ctrl+C" bit per process, and a child inherits it at
/// creation. Anything that sets it on the daemon — being created with
/// `CREATE_NEW_PROCESS_GROUP`, or simply being launched from a shell that
/// already had it — silently disarms Ctrl+C in every pane, because the pane's
/// shell and everything it runs are all descendants. The pane still writes 0x03
/// into the pty and conhost still turns it into a keypress; what never happens
/// is the CTRL_C_EVENT, so `go run` and `npm install` keep going (#451, #314).
///
/// A `NULL` routine with `FALSE` clears the bit, and it is what descendants
/// created afterwards inherit. Cheap and idempotent, so the pane spawn path
/// calls it rather than relying on a single well-placed call at startup.
pub(crate) fn allow_ctrl_c_in_children() {
    // SAFETY: A NULL handler routine is the documented spelling of "stop
    // ignoring Ctrl+C"; there is no handler list entry to keep alive.
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(None, 0);
    }
}

#[cfg(test)]
mod ctrl_c_tests {
    use std::io::{Read as _, Write as _};
    use std::os::windows::process::CommandExt as _;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    const RESULT_ENV: &str = "TTY7_CTRL_C_INHERITED_IGNORE_RESULT";
    const TEST_NAME: &str =
        "daemon::winproc::ctrl_c_tests::a_pane_survives_an_inherited_ignore_ctrl_c";

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// The reported bug (#451, #314): nothing in a tty7 pane could be
    /// interrupted, so `go run` and `npm install` ran on after Ctrl+C.
    ///
    /// The keystroke path was never at fault — the pane does write 0x03 into the
    /// pty, and conhost does turn it into a keypress. What never happened was
    /// the CTRL_C_EVENT, because the daemon carried Windows' "ignore Ctrl+C"
    /// process state and every ConPTY child inherits it.
    ///
    /// So the state has to be *inherited* here rather than switched on in
    /// place — that is the shape the daemon was in, and it is the only version a
    /// single `SetConsoleCtrlHandler(NULL, FALSE)` has to be able to undo. An
    /// intermediate process created exactly as the daemon used to be supplies
    /// it, and then runs both arms: a pane spawned without the fix must hang,
    /// and a pane spawned after it must not. Testing only the second arm would
    /// pass on its own on a machine whose runner never had the bit set.
    #[test]
    fn a_pane_survives_an_inherited_ignore_ctrl_c() {
        if let Some(result) = std::env::var_os(RESULT_ENV) {
            let unfixed = interruptible();
            super::allow_ctrl_c_in_children();
            let fixed = interruptible();
            std::fs::write(result, format!("{unfixed} {fixed}")).expect("write the arms' verdict");
            return;
        }

        let result =
            std::env::temp_dir().join(format!("tty7-pane-ctrl-c-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&result);
        let status = Command::new(std::env::current_exe().expect("locate the test executable"))
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(RESULT_ENV, &result)
            // Exactly how the daemon used to be created. CREATE_NEW_PROCESS_GROUP
            // is what leaves the inherited ignore behind.
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("run the arms in a process that inherits the ignore");
        assert!(status.success(), "the inner test process must finish");

        let verdict = std::fs::read_to_string(&result).unwrap_or_else(|_| "MISSING".into());
        let _ = std::fs::remove_file(&result);
        assert_eq!(
            verdict.trim(),
            "false true",
            "a pane spawned under an inherited ignore must be interruptible only after \
             allow_ctrl_c_in_children (got `{}`); `true true` means the ignore never took \
             hold and the test proves nothing, `false false` is the bug in #451 and #314",
            verdict.trim()
        );
    }

    /// Runs a shell in a pane the way `DaemonPane::spawn` does, interrupts a
    /// command that would otherwise never end, and reports whether the shell
    /// came back.
    ///
    /// The observable is the shell, not the interrupted command: after the ^C,
    /// `cmd` gets its prompt back and acts on the `exit` typed behind it. If the
    /// ^C is swallowed, `ping` still owns the console, the `exit` is never read,
    /// and the shell outlives the deadline. Nothing here reads a message, so it
    /// holds on a non-English Windows too.
    fn interruptible() -> bool {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open a pty the way a pane does");
        let mut child = pty
            .slave
            .spawn_command(CommandBuilder::new("cmd.exe"))
            .expect("spawn the shell");
        drop(pty.slave);

        // ConPTY stalls its client once the output pipe fills, so this has to
        // keep running whatever else happens. The byte count it keeps is how the
        // test paces itself instead of guessing at sleeps.
        let mut reader = pty.master.try_clone_reader().expect("clone the reader");
        let written = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&written);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                counter.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            }
        });
        let bytes = || written.load(std::sync::atomic::Ordering::Relaxed);
        // Waits for `rounds` separate batches of output. Two of them means the
        // running command is producing its own lines on its own schedule, which
        // is the only reliable sign that it — not the shell — holds the console.
        // Interrupting before that reaches nobody, and would look like this bug
        // on a slow machine for a reason that has nothing to do with the fix.
        let settle = |rounds: usize| {
            let give_up = Instant::now() + Duration::from_secs(20);
            for _ in 0..rounds {
                let before = bytes();
                while bytes() == before && Instant::now() < give_up {
                    std::thread::sleep(Duration::from_millis(50));
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        };

        let mut writer = pty.master.take_writer().expect("take the writer");
        let mut type_in = |keys: &[u8]| {
            writer.write_all(keys).expect("write to the pty");
            writer.flush().expect("flush the pty");
        };
        settle(1); // the shell's banner and first prompt
        // Runs until interrupted, and holds the console while it does.
        type_in(b"ping -t 127.0.0.1\r");
        settle(3); // the echoed line, then replies on ping's own clock
        type_in(&[0x03]);
        std::thread::sleep(Duration::from_millis(500));
        type_in(b"exit\r");

        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match child.try_wait().expect("poll the shell") {
                Some(_) => return true,
                None if Instant::now() >= deadline => {
                    // Killing the shell leaves the `ping` it never interrupted
                    // running forever, so take the subtree with it. Read the
                    // subtree while the shell is still alive: once it is gone
                    // its pid can be handed to somebody else, and a snapshot
                    // taken after that names a stranger's children.
                    let doomed = child
                        .process_id()
                        .map(|shell| super::descendants(&super::snapshot(), shell))
                        .unwrap_or_default();
                    let _ = child.kill();
                    let _ = child.wait();
                    super::terminate_and_wait_all(&doomed, Instant::now() + Duration::from_secs(2));
                    return false;
                }
                None => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
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
    fn creation_time_of_this_process_is_in_the_past() {
        let started = creation_time(std::process::id()).expect("own process is queryable");
        assert!(started <= std::time::SystemTime::now());
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

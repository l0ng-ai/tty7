//! Making the daemon its own *responsible process* on macOS (#909).
//!
//! macOS attributes privacy decisions — Local Network, Camera, Microphone,
//! Location, Apple Events — to a process's responsible process, and that is
//! fixed when the process is spawned: it is inherited from whoever started it.
//! The daemon is started by the GUI, so it and every shell it forks answer to
//! that GUI. That holds only while the GUI lives. The daemon is built to outlive
//! it — an update relaunches the GUI and keeps the shells — and once the GUI
//! that launched it is gone, new panes are no longer attributed to tty59.app.
//! Local Network then falls back to judging each binary on its own, and a
//! Homebrew `node` or `python` gets `EHOSTUNREACH` on the LAN while `/usr/bin`
//! tools, which are exempt, keep working.
//!
//! The fix is to disclaim the inherited responsibility at startup. There is no
//! way to change it in a running process, so the daemon re-executes itself —
//! `posix_spawn` with `POSIX_SPAWN_SETEXEC`, which is `execve` with spawn
//! attributes: same pid, same descriptors, same session — with
//! `responsibility_spawnattrs_setdisclaim` set. The new image is then
//! responsible for itself, and it is tty59.app's own signed executable, so its
//! shells are attributed to tty59 however long it outlives any window.
//!
//! Doing it at the top of `run_daemon` covers every way a daemon image starts:
//! a fresh spawn from the GUI or `tty59-cli`, and the far side of a handoff,
//! which is how a daemon started by an older build gets repaired in place
//! without losing its panes. Shells forked *before* that keep the attribution
//! they were born with; only panes opened afterwards pick up the new one.
//!
//! `responsibility_*` is private SPI in libsystem (Chromium, LLDB and others
//! use it the same way), so both symbols are looked up at runtime. If either is
//! missing, or anything fails, the daemon carries on exactly as before.

use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt as _;

/// Appended to the re-executed image's arguments, so it does not re-execute
/// again. An argument rather than an environment variable so it is not
/// inherited by every shell the daemon starts.
///
/// This is the only guard, on purpose. Asking whether the daemon already
/// answers for itself cannot tell the two cases apart:
/// `responsibility_get_pid_responsible_for_pid` reports a process as its own
/// responsible process both when it truly is and when the one it inherited has
/// exited — which is exactly the state a daemon outliving its GUI is in, and
/// exactly the one a handoff is meant to repair. So every image start re-execs
/// once; after a handoff that is one more `exec`, and it is cheap.
pub const DISCLAIMED_FLAG: &str = "--responsibility-disclaimed";

#[cfg(test)]
type ResponsibleFor = unsafe extern "C" fn(libc::pid_t) -> libc::pid_t;
type SetDisclaim = unsafe extern "C" fn(*mut libc::posix_spawnattr_t, libc::c_int) -> libc::c_int;

/// Re-execute this process with its inherited responsibility disclaimed,
/// unless this image is already the result of that. Returns only when no
/// re-exec happened; on success this process is already the new image.
pub fn disclaim_inherited() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.iter().any(|arg| arg == DISCLAIMED_FLAG) {
        log::info!("the daemon disclaimed its launcher's responsibility");
        return;
    }
    let Some(set_disclaim) =
        (unsafe { lookup::<SetDisclaim>(c"responsibility_spawnattrs_setdisclaim") })
    else {
        log::debug!("responsibility SPI unavailable; the daemon keeps its launcher's attribution");
        return;
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            log::warn!("could not locate own executable to disclaim responsibility: {e}");
            return;
        }
    };

    let error = reexec(&exe, &args, set_disclaim);
    log::warn!(
        "could not re-exec {} to disclaim responsibility: {error}",
        exe.display()
    );
}

/// `posix_spawn` with `POSIX_SPAWN_SETEXEC` and the disclaim attribute. Returns
/// only on failure.
fn reexec(
    exe: &std::path::Path,
    args: &[std::ffi::OsString],
    set_disclaim: SetDisclaim,
) -> std::io::Error {
    let Some(path) = c_string(exe.as_os_str()) else {
        return std::io::Error::other("executable path contains a NUL");
    };
    let mut argv: Vec<CString> = Vec::with_capacity(args.len() + 1);
    for arg in args {
        match c_string(arg) {
            Some(arg) => argv.push(arg),
            None => return std::io::Error::other("an argument contains a NUL"),
        }
    }
    argv.push(CString::new(DISCLAIMED_FLAG).expect("no NUL in a literal"));
    // `vars_os` skips entries that are not `KEY=VALUE`; nothing the daemon
    // reads depends on those.
    let envp: Vec<CString> = std::env::vars_os()
        .filter_map(|(key, value)| {
            let mut entry = key.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(value.as_bytes());
            CString::new(entry).ok()
        })
        .collect();
    let argv_ptrs = null_terminated(&argv);
    let envp_ptrs = null_terminated(&envp);

    unsafe {
        let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
        let rc = libc::posix_spawnattr_init(&mut attr);
        if rc != 0 {
            return std::io::Error::from_raw_os_error(rc);
        }
        // No other flags on purpose: the signal mask, signal dispositions and
        // every descriptor without FD_CLOEXEC cross exactly as they would an
        // `execve`, which is what a handoff's ptys and blob rely on.
        let rc =
            libc::posix_spawnattr_setflags(&mut attr, libc::POSIX_SPAWN_SETEXEC as libc::c_short);
        if rc != 0 {
            libc::posix_spawnattr_destroy(&mut attr);
            return std::io::Error::from_raw_os_error(rc);
        }
        let rc = set_disclaim(&mut attr, 1);
        if rc != 0 {
            libc::posix_spawnattr_destroy(&mut attr);
            return std::io::Error::from_raw_os_error(rc);
        }
        let mut child: libc::pid_t = 0;
        let rc = libc::posix_spawn(
            &mut child,
            path.as_ptr(),
            std::ptr::null(),
            &attr,
            argv_ptrs.as_ptr(),
            envp_ptrs.as_ptr(),
        );
        libc::posix_spawnattr_destroy(&mut attr);
        std::io::Error::from_raw_os_error(rc)
    }
}

unsafe fn lookup<F: Copy>(name: &CStr) -> Option<F> {
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    if symbol.is_null() {
        return None;
    }
    debug_assert_eq!(
        std::mem::size_of::<F>(),
        std::mem::size_of::<*mut libc::c_void>()
    );
    Some(unsafe { std::mem::transmute_copy::<*mut libc::c_void, F>(&symbol) })
}

fn c_string(s: &OsStr) -> Option<CString> {
    CString::new(s.as_bytes()).ok()
}

fn null_terminated(strings: &[CString]) -> Vec<*mut libc::c_char> {
    strings
        .iter()
        .map(|s| s.as_ptr() as *mut libc::c_char)
        .chain(std::iter::once(std::ptr::null_mut()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shell spawned with the attribute disclaimed becomes the responsible
    /// process for what *it* starts — which is the daemon's situation exactly:
    /// the daemon disclaims, its shells inherit from it. Without the attribute
    /// the grandchild answers to whatever this test binary answers to, never to
    /// the shell. Checked through a grandchild because a process whose chain
    /// never had a responsible process reports itself, which would make a
    /// direct check pass whether or not the attribute did anything.
    #[test]
    fn a_disclaimed_spawn_is_responsible_for_its_children() {
        let (Some(responsible_for), Some(set_disclaim)) = (unsafe {
            (
                lookup::<ResponsibleFor>(c"responsibility_get_pid_responsible_for_pid"),
                lookup::<SetDisclaim>(c"responsibility_spawnattrs_setdisclaim"),
            )
        }) else {
            eprintln!("responsibility SPI unavailable on this macOS; nothing to check");
            return;
        };

        let dir = std::env::temp_dir().join(format!("tty59-responsibility-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // (shell pid, pid of the `sleep` the shell started, its responsible pid)
        let run = |disclaim: bool| -> (libc::pid_t, libc::pid_t, libc::pid_t) {
            let pidfile = dir.join(if disclaim { "disclaimed" } else { "plain" });
            let script = CString::new(format!(
                "/bin/sleep 30 & echo $! > '{}'; wait",
                pidfile.display()
            ))
            .unwrap();
            let sh = c"/bin/sh";
            let argv = [
                sh.as_ptr() as *mut libc::c_char,
                c"-c".as_ptr() as *mut _,
                script.as_ptr() as *mut _,
                std::ptr::null_mut(),
            ];
            let envp = [std::ptr::null_mut()];
            let shell = unsafe {
                let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
                assert_eq!(libc::posix_spawnattr_init(&mut attr), 0);
                if disclaim {
                    assert_eq!(set_disclaim(&mut attr, 1), 0);
                }
                let mut pid = 0;
                let rc = libc::posix_spawn(
                    &mut pid,
                    sh.as_ptr(),
                    std::ptr::null(),
                    &attr,
                    argv.as_ptr(),
                    envp.as_ptr(),
                );
                libc::posix_spawnattr_destroy(&mut attr);
                assert_eq!(rc, 0, "posix_spawn failed");
                pid
            };
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let grandchild = loop {
                if let Some(pid) = std::fs::read_to_string(&pidfile)
                    .ok()
                    .and_then(|text| text.trim().parse::<libc::pid_t>().ok())
                {
                    break pid;
                }
                if std::time::Instant::now() > deadline {
                    unsafe {
                        libc::kill(shell, libc::SIGKILL);
                        libc::waitpid(shell, std::ptr::null_mut(), 0);
                    }
                    panic!("the shell never reported its child");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            let responsible = unsafe { responsible_for(grandchild) };
            unsafe {
                libc::kill(grandchild, libc::SIGKILL);
                libc::kill(shell, libc::SIGKILL);
                libc::waitpid(shell, std::ptr::null_mut(), 0);
            }
            (shell, grandchild, responsible)
        };

        let (plain_shell, _, plain_responsible) = run(false);
        let (disclaimed_shell, _, disclaimed_responsible) = run(true);
        let _ = std::fs::remove_dir_all(&dir);

        assert_ne!(
            plain_responsible, plain_shell,
            "without the attribute the shell is not responsible for its children"
        );
        assert_eq!(
            disclaimed_responsible, disclaimed_shell,
            "a disclaimed shell is responsible for its children"
        );
    }
}

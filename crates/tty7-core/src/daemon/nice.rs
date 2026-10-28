//! Lowering a pane's shell priority (`nice` in the config), so agents and
//! builds typed into it yield the CPU to the GUI and to each other.

/// Applies `n` (already clamped by `Config::sanitize`) to a freshly spawned
/// shell, off the spawning thread since macOS has to wait out the exec first.
/// A failure is logged, never fatal: a pane at normal priority beats no pane.
pub(crate) fn apply(pid: u32, n: i32) {
    if n == 0 {
        return;
    }
    if let Err(e) = std::thread::Builder::new()
        .name("tty7-daemon-pane-nice".to_string())
        .spawn(move || apply_n(pid, n))
    {
        log::warn!("could not nice pane shell {pid}: {e}");
    }
}

/// The shell leads its own session (portable_pty calls `setsid`), so its pid
/// is its process group: renicing the group reaches anything already forked
/// into it. Later children inherit the value.
fn apply_n(pid: u32, n: i32) {
    #[cfg(target_os = "macos")]
    wait_for_exec(pid);
    // SAFETY: setpriority takes plain integers and touches no memory of ours.
    if unsafe { libc::setpriority(libc::PRIO_PGRP, pid as libc::id_t, n) } != 0 {
        log::warn!(
            "could not nice pane shell {pid} to {n}: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// portable_pty's pre-exec closes std's exec-status pipe, so spawn returns
/// before the shell's `execve` is done, and macOS drops a priority set while
/// an exec is in flight: the shell would come out at the daemon's own nice.
#[cfg(target_os = "macos")]
fn wait_for_exec(pid: u32) {
    // <sys/proc_info.h>; not in the libc crate.
    const PROC_FLAG_EXEC: u32 = 0x4000;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while std::time::Instant::now() < deadline {
        // SAFETY: a zeroed `proc_bsdinfo` is a valid value of plain integers.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` is a `proc_bsdinfo` of exactly `size` bytes.
        let got = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if got != size || info.pbi_flags & PROC_FLAG_EXEC != 0 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    use super::apply_n;

    fn priority(pid: u32) -> i32 {
        // SAFETY: plain integers in, a plain integer out.
        unsafe { libc::getpriority(libc::PRIO_PROCESS, pid as libc::id_t) }
    }

    /// Like portable_pty, close the exec-status pipe early so spawn returns
    /// before the exec. Without the wait about one renice in six lands inside
    /// `execve` and is lost, so fifty spawns all but always catch a regression.
    #[cfg(target_os = "macos")]
    #[test]
    fn apply_n_survives_the_exec_portable_pty_does_not_wait_for() {
        let want = (priority(0) + 1).min(19);
        for _ in 0..50 {
            let mut cmd = Command::new("/bin/sleep");
            cmd.arg("5").process_group(0);
            // SAFETY: close is async-signal-safe.
            unsafe {
                cmd.pre_exec(|| {
                    for fd in 3..256 {
                        libc::close(fd);
                    }
                    Ok(())
                });
            }
            let mut child = cmd.spawn().unwrap();
            apply_n(child.id(), want);
            // Read back after the exec, which is what drops a renice.
            std::thread::sleep(std::time::Duration::from_millis(30));
            let got = priority(child.id());
            child.kill().ok();
            child.wait().ok();
            assert_eq!(got, want);
        }
    }

    #[test]
    fn apply_n_reaches_a_child_the_shell_already_forked() {
        let mut shell = Command::new("sh")
            .args(["-c", "sleep 5 & echo $!; wait"])
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(shell.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        let sleep: u32 = line.trim().parse().unwrap();
        let want = (priority(0) + 2).min(19);
        apply_n(shell.id(), want);
        let got = priority(sleep);
        // SAFETY: kills the shell's process group, the sleep with it.
        unsafe { libc::kill(-(shell.id() as libc::pid_t), libc::SIGKILL) };
        shell.wait().ok();
        assert_eq!(got, want);
    }
}

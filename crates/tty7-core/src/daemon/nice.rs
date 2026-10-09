//! Lowering a pane's shell priority (`nice` in the config), so agents and
//! builds typed into it yield the CPU to the GUI and to each other.

use std::sync::Once;

use crate::core::config::Config;

/// The value to hand `setpriority`, or `None` when there is nothing to do.
/// Negative values would need root to raise priority, so they turn it off.
fn niceness(n: i32) -> Option<i32> {
    if !(0..=19).contains(&n) {
        static WARNED: Once = Once::new();
        WARNED.call_once(|| log::warn!("nice = {n} is outside 0..=19; using {}", n.clamp(0, 19)));
    }
    match n.clamp(0, 19) {
        0 => None,
        n => Some(n),
    }
}

/// Applies the configured nice to a freshly spawned shell. A failure is
/// logged, never fatal: a pane at normal priority beats no pane.
pub(crate) fn apply(pid: u32) {
    if let Some(n) = niceness(Config::load().nice) {
        apply_n(pid, n);
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

    use super::*;

    fn priority(pid: u32) -> i32 {
        // SAFETY: plain integers in, a plain integer out.
        unsafe { libc::getpriority(libc::PRIO_PROCESS, pid as libc::id_t) }
    }

    #[test]
    fn niceness_turns_zero_and_negatives_off_and_caps_at_nineteen() {
        for (input, want) in [(0, None), (5, Some(5)), (-3, None), (40, Some(19))] {
            assert_eq!(niceness(input), want, "niceness({input})");
        }
    }

    /// Like portable_pty, close the exec-status pipe early so spawn returns
    /// before the exec; the exec then waits a little longer to come.
    #[cfg(target_os = "macos")]
    #[test]
    fn apply_n_waits_for_the_exec_portable_pty_does_not() {
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("5");
        // SAFETY: close is async-signal-safe and usleep is a nanosleep.
        unsafe {
            cmd.pre_exec(|| {
                for fd in 3..256 {
                    libc::close(fd);
                }
                libc::usleep(200_000);
                Ok(())
            });
        }
        let mut child = cmd.spawn().unwrap();
        let start = std::time::Instant::now();
        apply_n(child.id(), (priority(0) + 1).min(19));
        let waited = start.elapsed();
        child.kill().ok();
        child.wait().ok();
        assert!(
            waited >= std::time::Duration::from_millis(150),
            "{waited:?}"
        );
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

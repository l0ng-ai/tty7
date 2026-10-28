//! Resident memory and CPU time for the Info panel's process rows.
//!
//! Read per walked pid rather than in `process_table`'s pass over every pid on
//! the machine: the panel shows at most `MAX_PROCS` rows, and a failed read on
//! another user's process must not drop it from the tree.

use crate::daemon::protocol::ProcEntry;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Usage {
    pub rss: Option<u64>,
    pub cpu_ns: Option<u64>,
    pub started: Option<u64>,
}

pub(crate) fn fill(procs: &mut [ProcEntry]) {
    for p in procs {
        let u = read(p.pid);
        p.rss = u.rss;
        p.cpu_ns = u.cpu_ns;
        p.started = u.started;
    }
}

/// `512 KB`, `38 MB`, `1.2 GB`: binary units under the short names `ps` and
/// Activity Monitor print.
pub fn compact_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    match v < 10.0 {
        true => format!("{v:.1} {}", UNITS[unit]),
        false => format!("{v:.0} {}", UNITS[unit]),
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn read(pid: u32) -> Usage {
    let mut info: libc::proc_taskallinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskallinfo>() as libc::c_int;
    // SAFETY: `info` is a correctly sized, writable `proc_taskallinfo`.
    let ret = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTASKALLINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if ret != size {
        return Usage::default();
    }
    let t = &info.ptinfo;
    let b = &info.pbsd;
    Usage {
        rss: Some(t.pti_resident_size),
        // Mach ticks, not nanoseconds: 1:1 on Intel, 125:3 on Apple silicon.
        cpu_ns: Some(mach_to_ns(t.pti_total_user + t.pti_total_system)),
        started: Some(b.pbi_start_tvsec * 1_000_000 + b.pbi_start_tvusec),
    }
}

#[cfg(target_os = "macos")]
// libc deprecates these in favour of `mach2`; not worth a dependency.
#[allow(deprecated)]
fn mach_to_ns(ticks: u64) -> u64 {
    static TIMEBASE: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    let (numer, denom) = *TIMEBASE.get_or_init(|| {
        let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: writes the two fields of `tb` and nothing else.
        let ok = unsafe { libc::mach_timebase_info(&mut tb) } == 0 && tb.denom != 0;
        match ok {
            true => (tb.numer as u64, tb.denom as u64),
            false => (1, 1),
        }
    });
    (ticks as u128 * numer as u128 / denom as u128) as u64
}

#[cfg(target_os = "linux")]
pub(crate) fn read(pid: u32) -> Usage {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => parse_stat(
            &stat,
            sysconf(libc::_SC_CLK_TCK),
            sysconf(libc::_SC_PAGESIZE),
        ),
        Err(_) => Usage::default(),
    }
}

#[cfg(target_os = "linux")]
fn sysconf(name: libc::c_int) -> u64 {
    // SAFETY: `sysconf` only reads a configuration value.
    let v = unsafe { libc::sysconf(name) };
    if v > 0 { v as u64 } else { 0 }
}

/// utime, stime, starttime and rss out of a `/proc/<pid>/stat` line — fields
/// 14, 15, 22 and 24, counted from the `)` that closes the command name, which
/// may itself hold spaces.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_stat(stat: &str, ticks_per_sec: u64, page: u64) -> Usage {
    let Some(close) = stat.rfind(')') else {
        return Usage::default();
    };
    let f: Vec<u64> = stat[close + 1..]
        .split_whitespace()
        .map(|s| s.parse().unwrap_or(0))
        .collect();
    if f.len() < 22 || ticks_per_sec == 0 {
        return Usage::default();
    }
    Usage {
        rss: (page > 0).then(|| f[21] * page),
        cpu_ns: Some((f[11] + f[12]) * (1_000_000_000 / ticks_per_sec)),
        started: Some(f[19]),
    }
}

#[cfg(windows)]
pub(crate) fn read(pid: u32) -> Usage {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let ft = |t: FILETIME| (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64;
    let mut u = Usage::default();
    // SAFETY: every out-pointer is a zeroed local of the type the call expects,
    // and the handle is closed on the only path that opened it.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return u;
        }
        let (mut created, mut exited, mut kernel, mut user): (
            FILETIME,
            FILETIME,
            FILETIME,
            FILETIME,
        ) = std::mem::zeroed();
        if GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user) != 0 {
            // FILETIME is in 100 ns units.
            u.cpu_ns = Some((ft(kernel) + ft(user)) * 100);
            u.started = Some(ft(created));
        }
        let mut mem: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        let cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if K32GetProcessMemoryInfo(h, &mut mem, cb) != 0 {
            u.rss = Some(mem.WorkingSetSize as u64);
        }
        CloseHandle(h);
    }
    u
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(crate) fn read(_pid: u32) -> Usage {
    Usage::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_read_compactly() {
        assert_eq!(compact_bytes(512), "512 B");
        assert_eq!(compact_bytes(512 * 1024), "512 KB");
        assert_eq!(compact_bytes(38 * 1024 * 1024), "38 MB");
        assert_eq!(compact_bytes(1_288_490_189), "1.2 GB");
        assert_eq!(compact_bytes(5 * 1024 * 1024 + 300 * 1024), "5.3 MB");
    }

    #[test]
    fn an_old_daemons_entry_has_no_usage() {
        let old = r#"{"pid":1,"name":"zsh","depth":0,"foreground":true}"#;
        let p: ProcEntry = serde_json::from_str(old).unwrap();
        assert_eq!((p.rss, p.cpu_ns, p.started), (None, None, None));
        assert_eq!(
            serde_json::to_string(&p).unwrap(),
            old,
            "no nulls sent back"
        );
    }

    #[test]
    fn a_stat_line_with_spaces_in_the_name_parses() {
        let stat = "42 (tmux: server) S 1 42 42 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 1 0 \
                    9000 12345678 2048 18446744073709551615";
        let u = parse_stat(stat, 100, 4096);
        assert_eq!(u.rss, Some(2048 * 4096));
        assert_eq!(u.cpu_ns, Some(3_000_000_000));
        assert_eq!(u.started, Some(9000));
        assert_eq!(parse_stat("garbage", 100, 4096), Usage::default());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn this_process_reports_plausible_usage() {
        let u = read(std::process::id());
        let rss = u.rss.expect("own rss");
        assert!((1 << 20..1 << 40).contains(&rss), "rss {rss}");
        assert!(u.cpu_ns.is_some(), "{u:?}");
        assert!(u.started.is_some_and(|s| s > 0), "{u:?}");
    }

    /// The process's CPU time is at least this thread's, in the same unit.
    /// Unconverted mach ticks read ~42x low on Apple silicon; a bare `> 0`
    /// would pass them, and would fail on Linux before the first 10 ms tick.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn cpu_time_is_in_nanoseconds() {
        fn thread_ns() -> u64 {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: writes `ts` and nothing else.
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
            ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
        }
        let before = read(std::process::id()).cpu_ns.expect("cpu time");
        let t0 = thread_ns();
        let mut x = 0u64;
        while thread_ns() - t0 < 200_000_000 {
            x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(1));
        }
        let burned = thread_ns() - t0;
        let after = read(std::process::id()).cpu_ns.expect("cpu time");
        // Slack for Linux's 10 ms clock tick on each end.
        assert!(
            after - before + 30_000_000 >= burned,
            "process cpu went up {} ns while this thread alone burned {burned}",
            after - before
        );
    }
}

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::core::config;
use crate::daemon::protocol::{ClientMsg, DaemonMsg, DaemonVersion, PROTOCOL_VERSION};
use crate::daemon::{pidfile, transport};

#[cfg(windows)]
mod windows;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(6);
#[cfg(any(target_os = "macos", target_os = "linux"))]
const REAP_TERM_TIMEOUT: Duration = Duration::from_secs(6);
#[cfg(any(target_os = "macos", target_os = "linux"))]
const REAP_KILL_TIMEOUT: Duration = Duration::from_secs(2);

pub struct MismatchedDaemon {
    pub version: Option<DaemonVersion>,
}

static MISMATCHED_DAEMON: std::sync::Mutex<Option<MismatchedDaemon>> = std::sync::Mutex::new(None);

pub fn take_mismatched_daemon() -> Option<MismatchedDaemon> {
    MISMATCHED_DAEMON.lock().ok()?.take()
}

static LOCAL_DAEMON: std::sync::Mutex<Option<DaemonVersion>> = std::sync::Mutex::new(None);

pub fn local_daemon_supports(feature: &str) -> bool {
    LOCAL_DAEMON
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|v| v.has_feature(feature)))
        .unwrap_or(false)
}

fn note_local_daemon(version: Option<DaemonVersion>) {
    if let Ok(mut slot) = LOCAL_DAEMON.lock() {
        *slot = version;
    }
}

#[derive(Debug, PartialEq, Eq)]
enum VersionProbe {
    Speaks(DaemonVersion),
    Legacy,
    Unresponsive,
}

const DAEMON_EXE_STEMS: [&str; 3] = ["tty7-app", "tty7-server", "tty7"];

fn strip_exe_suffix(name: &str) -> &str {
    match name.len().checked_sub(4) {
        Some(i) if name.is_char_boundary(i) && name[i..].eq_ignore_ascii_case(".exe") => &name[..i],
        _ => name,
    }
}

fn exe_names_equal(a: &str, b: &str) -> bool {
    let a = strip_exe_suffix(a);
    let b = strip_exe_suffix(b);
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn is_reapable_daemon_name(name: &str) -> bool {
    let own = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    own.as_deref().is_some_and(|own| exe_names_equal(own, name))
        || DAEMON_EXE_STEMS
            .iter()
            .any(|stem| exe_names_equal(stem, name))
}

pub fn ensure_running() -> anyhow::Result<()> {
    if let Ok(mut stream) = transport::connect() {
        match query_daemon_version(&mut stream) {
            VersionProbe::Speaks(v) if v.protocol == PROTOCOL_VERSION => {
                note_local_daemon(Some(v));
                return Ok(());
            }
            VersionProbe::Speaks(v) => {
                log::warn!(
                    "daemon (build {}) speaks protocol {}, this build needs {}; \
                     keeping it and deferring to the user",
                    v.build,
                    v.protocol,
                    PROTOCOL_VERSION
                );
                note_local_daemon(Some(v.clone()));
                if let Ok(mut slot) = MISMATCHED_DAEMON.lock() {
                    *slot = Some(MismatchedDaemon { version: Some(v) });
                }
                return Ok(());
            }
            VersionProbe::Legacy => {
                log::warn!(
                    "daemon predates protocol versioning; keeping it and deferring to the user"
                );
                note_local_daemon(None);
                if let Ok(mut slot) = MISMATCHED_DAEMON.lock() {
                    *slot = Some(MismatchedDaemon { version: None });
                }
                return Ok(());
            }
            VersionProbe::Unresponsive => {
                log::info!("daemon did not answer the version handshake; restarting it");
                note_local_daemon(None);
                drop(stream);
                stop();
            }
        }
    } else {
        reap_recorded_daemon();

        if transport::endpoint_exists() {
            transport::remove_stale_endpoint();
        }
    }

    spawn_detached()?;

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Ok(mut stream) = transport::connect() {
            match query_daemon_version(&mut stream) {
                VersionProbe::Speaks(v) => note_local_daemon(Some(v)),
                _ => note_local_daemon(None),
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "daemon did not start listening at {} within {:?}",
                transport::endpoint_display(),
                STARTUP_TIMEOUT
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn query_daemon_version(stream: &mut transport::Stream) -> VersionProbe {
    use std::io::Write as _;

    let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
    if ClientMsg::Version
        .encode(stream)
        .and_then(|()| stream.flush())
        .is_err()
    {
        return VersionProbe::Unresponsive;
    }
    match DaemonMsg::read(stream) {
        Ok(DaemonMsg::Version(v)) => VersionProbe::Speaks(v),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) =>
        {
            VersionProbe::Unresponsive
        }
        _ => VersionProbe::Legacy,
    }
}

pub fn restart() -> anyhow::Result<()> {
    stop();
    ensure_running()
}

pub fn stop() {
    use std::io::Write as _;

    if let Ok(mut stream) = transport::connect() {
        if ClientMsg::Shutdown.encode(&mut stream).is_ok() {
            let _ = stream.flush();
            let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
            while Instant::now() < deadline && transport::connect().is_ok() {
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }

    reap_recorded_daemon();

    if transport::endpoint_exists() {
        transport::remove_stale_endpoint();
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn reap_recorded_daemon() {
    let Some(pid) = pidfile::read() else { return };
    if pid <= 1 || pid == std::process::id() {
        pidfile::remove();
        return;
    }
    if process_matches_daemon_exe(pid as libc::pid_t) {
        log::warn!("reaping unreachable daemon (pid {pid}); its sessions will be hung up");
        reap_process(pid as libc::pid_t);
    }
    pidfile::remove();
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_matches_daemon_exe(pid: libc::pid_t) -> bool {
    process_path(pid)
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .is_some_and(|name| is_reapable_daemon_name(&name))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn reap_process(pid: libc::pid_t) {
    if signal_and_await_exit(pid, libc::SIGTERM, REAP_TERM_TIMEOUT) {
        return;
    }
    if !signal_and_await_exit(pid, libc::SIGKILL, REAP_KILL_TIMEOUT) {
        log::error!("daemon pid {pid} survived SIGKILL; leaving it behind");
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn signal_and_await_exit(pid: libc::pid_t, sig: libc::c_int, timeout: Duration) -> bool {
    unsafe { libc::kill(pid, sig) };
    let deadline = Instant::now() + timeout;
    while process_alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    true
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_alive(pid: libc::pid_t) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(windows)]
fn reap_recorded_daemon() {
    use crate::daemon::winproc;

    let Some(pid) = pidfile::read() else { return };
    if pid <= 4 || pid == std::process::id() {
        pidfile::remove();
        return;
    }
    let procs = winproc::snapshot();
    let matches = procs
        .iter()
        .find(|p| p.pid == pid)
        .is_some_and(|entry| is_reapable_daemon_name(&entry.name));
    if matches {
        log::warn!("reaping unreachable daemon (pid {pid}); its sessions will be hung up");
        for descendant in winproc::descendants(&procs, pid) {
            winproc::terminate(descendant);
        }
        winproc::terminate(pid);
    }
    pidfile::remove();
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn reap_recorded_daemon() {}

fn spawn_detached() -> anyhow::Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("could not locate own executable: {e}"))?;

    let config_dir = config::config_dir_path();

    // Some Windows shell brokers enforce Redirection Trust on processes they
    // launch. An ordinary child inherits it and cannot traverse Scoop's
    // user-created `current` junctions. The policy cannot be relaxed in place,
    // so create the daemon through the clean interactive desktop shell only
    // when the enforcing bit is actually present.
    #[cfg(windows)]
    if windows::redirection_trust_enforced() {
        let mut args = vec![std::ffi::OsString::from("--daemon")];
        if let Some(dir) = &config_dir {
            args.push(std::ffi::OsString::from("--config-dir"));
            args.push(dir.as_os_str().to_owned());
        }
        match windows::spawn_detached_with_clean_parent(&exe, &args) {
            Ok(()) => return Ok(()),
            // The clean parent is unavailable whenever there is no interactive
            // Explorer to borrow — it is restarting, the shell was replaced, or
            // the session has no desktop at all. Losing it only costs junction
            // traversal inside the shells, so degrade to the ordinary path
            // instead of refusing to start tty7 at all.
            Err(error) => log::warn!(
                "could not spawn the daemon through the Windows desktop shell while \
                 Redirection Trust is enforced ({error}); falling back to the ordinary \
                 path, where Scoop-style junctions may be unreachable"
            ),
        }
    }

    let mut cmd = Command::new(exe);
    cmd.arg("--daemon");

    if let Some(dir) = config_dir {
        cmd.arg("--config-dir").arg(dir);
    }

    if let Some(shell) = detect_parent_shell() {
        cmd.env(crate::daemon::DETECTED_SHELL_ENV, shell);
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    detach(&mut cmd);

    match cmd.spawn() {
        Ok(_child) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("failed to spawn daemon process: {e}")),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn detect_parent_shell() -> Option<PathBuf> {
    process_path(unsafe { libc::getppid() }).filter(|path| is_supported_shell(path))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn detect_parent_shell() -> Option<PathBuf> {
    None
}

fn is_supported_shell(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "zsh"
            | "bash"
            | "fish"
            | "nu"
            | "nu.exe"
            | "pwsh"
            | "powershell"
            | "powershell.exe"
            | "pwsh.exe"
    )
}

#[cfg(target_os = "macos")]
fn process_path(pid: libc::pid_t) -> Option<PathBuf> {
    if pid <= 0 {
        return None;
    }
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let len =
        unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if len <= 0 {
        return None;
    }
    Some(PathBuf::from(
        String::from_utf8_lossy(&buf[..len as usize]).into_owned(),
    ))
}

#[cfg(target_os = "linux")]
fn process_path(pid: libc::pid_t) -> Option<PathBuf> {
    if pid <= 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(all(test, windows))]
mod windows_spawn_tests {
    use super::*;
    use std::mem::size_of;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::System::Threading::{
        ProcessRedirectionTrustPolicy, SetProcessMitigationPolicy,
    };

    const INNER_ENV: &str = "TTY7_REDIRECTION_TRUST_INNER";
    const CLEAN_PARENT_ENV: &str = "TTY7_REDIRECTION_TRUST_CLEAN_PARENT";
    const PROBE_RESULT_ENV: &str = "TTY7_REDIRECTION_TRUST_PROBE_RESULT";
    const TEST_NAME: &str =
        "daemon::spawn::windows_spawn_tests::daemon_spawn_does_not_inherit_redirection_trust";

    /// Redirection Trust cannot be disabled after it is enforced, so the outer
    /// test delegates the destructive policy change to a short-lived copy of
    /// the test executable. This keeps the remaining test process clean.
    #[test]
    fn daemon_spawn_does_not_inherit_redirection_trust() {
        // A probe process reports both its inherited policy and whether it can
        // traverse the fixture. Checking the policy directly keeps this test
        // deterministic on elevated CI runners, whose own junctions may remain
        // trusted even while Redirection Trust is enforced.
        if let Some(result) = std::env::var_os(PROBE_RESULT_ENV) {
            let result = PathBuf::from(result);
            let junction_probe = result
                .parent()
                .expect("probe result has a fixture directory")
                .join("current")
                .join("probe.txt");
            let verdict = if windows::redirection_trust_enforced() {
                "ENFORCED"
            } else if junction_probe.exists() {
                "CLEAN_OK"
            } else {
                "CLEAN_BLOCKED"
            };
            std::fs::write(result, verdict).expect("write mitigation probe verdict");
            return;
        }

        if std::env::var_os(INNER_ENV).is_none() {
            let output = Command::new(std::env::current_exe().expect("locate test executable"))
                .args(["--exact", TEST_NAME, "--nocapture"])
                .env(INNER_ENV, "1")
                .env(CLEAN_PARENT_ENV, std::process::id().to_string())
                .output()
                .expect("spawn isolated mitigation test process");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "isolated mitigation test failed:\nstdout:\n{stdout}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            // libtest exits 0 when `--exact` matches nothing, so a stale
            // TEST_NAME would turn this whole regression into a silent pass.
            assert!(
                stdout.contains("1 passed"),
                "the isolated mitigation test must actually run; TEST_NAME is probably stale:\n{stdout}"
            );
            return;
        }

        let clean_parent_pid: u32 = std::env::var(CLEAN_PARENT_ENV)
            .expect("clean parent pid")
            .parse()
            .expect("clean parent pid is numeric");

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tty7-redirection-trust-{}-{unique}",
            std::process::id()
        ));
        let target = root.join("version");
        let junction = root.join("current");
        let inherited_result = root.join("inherited-result.txt");
        let result = root.join("result.txt");
        let batch = root.join("junction probe.cmd");
        std::fs::create_dir_all(&target).expect("create junction target");
        std::fs::write(target.join("probe.txt"), b"ok").expect("write junction probe");

        let comspec = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());
        let linked = Command::new(&comspec)
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .status()
            .expect("create junction fixture");
        assert!(linked.success(), "mklink must create the junction fixture");

        let mut policy = 1u32;
        // SAFETY: The DWORD buffer exactly matches the Windows policy layout,
        // and only this disposable inner test process receives the policy.
        let enabled = unsafe {
            SetProcessMitigationPolicy(
                ProcessRedirectionTrustPolicy,
                (&raw mut policy).cast(),
                size_of::<u32>(),
            )
        };
        assert!(
            enabled != 0,
            "enable Redirection Trust: {}",
            io::Error::last_os_error()
        );
        assert!(
            windows::redirection_trust_enforced(),
            "tty7 must detect the enforcing policy before selecting the alternate spawn path"
        );

        // First prove that an ordinary child inherits the enforced policy.
        // This is the red-capable half of the regression and does not depend on
        // how Windows classifies the junction created by the current account.
        let inherited = Command::new(std::env::current_exe().expect("locate test executable"))
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(PROBE_RESULT_ENV, &inherited_result)
            .status()
            .expect("spawn ordinary mitigation probe");
        assert!(inherited.success(), "ordinary mitigation probe must run");
        assert_eq!(
            std::fs::read_to_string(&inherited_result)
                .expect("read inherited mitigation verdict")
                .trim(),
            "ENFORCED",
            "an ordinary child must demonstrate the policy inheritance that the alternate spawn path removes"
        );

        // The clean helper inherits INNER_ENV from this disposable process. A
        // batch wrapper adds the result path before launching another copy of
        // the test executable, whose first branch records its actual policy.
        let test_exe = std::env::current_exe().expect("locate test executable");
        let script = format!(
            "@echo off\r\nset \"{PROBE_RESULT_ENV}={}\"\r\n\"{}\" --exact \"{TEST_NAME}\" --nocapture\r\n",
            result.display(),
            test_exe.display(),
        );
        std::fs::write(&batch, script).expect("write mitigation probe batch");

        windows::spawn_detached_with_parent(
            Path::new(&comspec),
            &[
                std::ffi::OsString::from("/d"),
                std::ffi::OsString::from("/c"),
                batch.as_os_str().to_owned(),
            ],
            clean_parent_pid,
        )
        .expect("spawn mitigation probe through clean logical parent");

        let deadline = Instant::now() + Duration::from_secs(5);
        while !result.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }

        let verdict = std::fs::read_to_string(&result).unwrap_or_else(|_| "MISSING".into());
        let _ = std::fs::remove_dir(&junction);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            verdict.trim(),
            "CLEAN_OK",
            "a tty7 daemon child must drop the inherited policy and retain access to user-created junctions"
        );
    }
}

#[cfg(test)]
mod exe_name_tests {
    use super::*;

    #[test]
    fn every_legitimate_daemon_name_is_reapable_with_and_without_exe() {
        for name in [
            "tty7-app",
            "tty7-server",
            "tty7",
            "tty7-app.exe",
            "tty7-server.exe",
            "tty7.exe",
        ] {
            assert!(is_reapable_daemon_name(name), "{name} is a daemon of ours");
        }
    }

    #[test]
    fn the_current_executable_name_remains_reapable() {
        let own = std::env::current_exe()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            is_reapable_daemon_name(&own),
            "{own} launched this process and must stay in the set"
        );
    }

    #[test]
    fn foreign_process_names_are_never_reapable() {
        for name in [
            "explorer.exe",
            "sleep",
            "tty7d",
            "nottty7",
            "tty7-app2",
            "tty7.",
            "",
        ] {
            assert!(
                !is_reapable_daemon_name(name),
                "{name:?} must be protected from the reap"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_matches_daemon_names_case_insensitively() {
        assert!(is_reapable_daemon_name("TTY7-APP.EXE"));
        assert!(is_reapable_daemon_name("Tty7-Server"));
        assert!(is_reapable_daemon_name("TTY7"));
    }

    #[test]
    fn strip_exe_suffix_only_strips_a_trailing_exe() {
        assert_eq!(strip_exe_suffix("tty7-app.exe"), "tty7-app");
        assert_eq!(strip_exe_suffix("tty7-app.EXE"), "tty7-app");
        assert_eq!(strip_exe_suffix("tty7-app"), "tty7-app");
        assert_eq!(strip_exe_suffix(".exe"), "");
        assert_eq!(strip_exe_suffix("exe"), "exe");
    }

    #[test]
    fn supported_shell_detection_matches_shell_basenames_only() {
        assert!(is_supported_shell(Path::new("/opt/homebrew/bin/fish")));
        assert!(is_supported_shell(Path::new("/bin/zsh")));
        assert!(is_supported_shell(Path::new("/usr/bin/bash")));
        assert!(is_supported_shell(Path::new("/opt/homebrew/bin/nu")));
        assert!(is_supported_shell(Path::new("/portable/Nu.EXE")));
        assert!(!is_supported_shell(Path::new(
            "/Applications/kitty.app/kitty"
        )));
        assert!(!is_supported_shell(Path::new("/usr/bin/omp")));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use std::os::unix::net::UnixStream;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn reap_guard_rejects_a_live_process_of_another_executable() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as libc::pid_t;

        assert!(process_alive(pid), "the sleep child is alive and ours");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let basename = process_path(pid).and_then(|p| p.file_name().map(|n| n.to_os_string()));
            if basename == Some("sleep".into()) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "process_path resolves an arbitrary pid, not just our parent \
                 (still {basename:?} after 5s)"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !process_matches_daemon_exe(pid),
            "sleep must not match any daemon name; matching here would mean the reap could kill it"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn signal_and_await_exit_observes_the_death_it_caused() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as libc::pid_t;
        let reaper = std::thread::spawn(move || {
            let _ = child.wait();
        });

        assert!(
            signal_and_await_exit(pid, libc::SIGTERM, std::time::Duration::from_secs(5)),
            "the child must be seen exiting within the grace window"
        );
        assert!(!process_alive(pid), "and be gone afterwards");
        reaper.join().unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn process_alive_is_false_once_the_process_is_gone() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as libc::pid_t;
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!process_alive(pid));
    }

    #[test]
    fn version_handshake_reads_a_matching_reply() {
        use crate::daemon::protocol::{ClientMsg, DaemonMsg, DaemonVersion, PROTOCOL_VERSION};

        let (mut client, mut daemon) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let msg = ClientMsg::read(&mut daemon).unwrap();
            assert_eq!(msg, ClientMsg::Version);
            DaemonMsg::Version(DaemonVersion {
                protocol: PROTOCOL_VERSION,
                build: "test".into(),
                features: Vec::new(),
                instance: "inst-test".into(),
            })
            .encode(&mut daemon)
            .unwrap();
        });

        match query_daemon_version(&mut client) {
            VersionProbe::Speaks(got) => {
                assert_eq!(got.protocol, PROTOCOL_VERSION);
                assert_eq!(got.build, "test");
            }
            other => panic!("a live daemon must answer, got {other:?}"),
        }
        server.join().unwrap();
    }

    #[test]
    fn version_handshake_treats_a_hangup_as_legacy() {
        use crate::daemon::protocol::ClientMsg;

        let (mut client, mut daemon) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let _ = ClientMsg::read(&mut daemon);
            drop(daemon);
        });

        assert_eq!(query_daemon_version(&mut client), VersionProbe::Legacy);
        server.join().unwrap();
    }

    #[test]
    fn version_handshake_treats_silence_as_unresponsive() {
        let (mut client, daemon) = UnixStream::pair().unwrap();
        let start = Instant::now();
        assert_eq!(
            query_daemon_version(&mut client),
            VersionProbe::Unresponsive
        );
        assert!(start.elapsed() >= HANDSHAKE_TIMEOUT);
        drop(daemon);
    }

    #[test]
    fn connect_to_stale_socket_path_fails() {
        let dir = std::env::temp_dir().join(format!("tty7-spawn-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.sock");
        let err = UnixStream::connect(&path).unwrap_err();
        assert!(matches!(
            err.kind(),
            ErrorKind::NotFound | ErrorKind::ConnectionRefused
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Endpoint-scoped maintenance. Never scan processes, send OS signals, start
//! a daemon implicitly, or interpret EOF as permission to terminate one.

use super::deadline::DeadlineIo;
use super::protocol::{
    ClientMsg, DaemonMsg, DaemonVersion, FEATURE_IDLE_SHUTDOWN, PROTOCOL_VERSION, PaneAccess,
};
use super::transport::{self, Stream};
use serde::{Deserialize, Serialize};
use std::io;
use std::time::{Duration, Instant};

pub const PREPARE_FLAG: &str = "--prepare-idle-restart";
pub const HEALTH_FLAG: &str = "--check-running";
pub const SERVING_FLAG: &str = "--check-serving";
pub const STOP_RECORDED_FLAG: &str = "--stop-recorded-server";
const IO_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Reply {
    Stopped,
    Healthy {
        control: u32,
        protocol: u32,
        build: String,
        instance: String,
    },
    Deferred {
        kind: DeferredKind,
        message: String,
    },
}

/// Why a maintenance step stood down, in a form the far end of an SSH
/// command can branch on without matching English prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferredKind {
    /// The running server is too old to answer the safe-idle question. This
    /// is the only kind a user-confirmed legacy stop may follow — a busy or
    /// unresponsive server is never a reason to signal one.
    Unsupported,
    Timeout,
    Refused,
    Other,
}

impl Reply {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("maintenance reply contains only JSON values")
    }
    pub fn parse(output: &str) -> Option<Self> {
        serde_json::from_str(output.lines().rev().find(|line| !line.trim().is_empty())?).ok()
    }
    pub fn deferred(error: &io::Error) -> Self {
        let kind = match error.kind() {
            io::ErrorKind::Unsupported => DeferredKind::Unsupported,
            io::ErrorKind::TimedOut => DeferredKind::Timeout,
            io::ErrorKind::PermissionDenied => DeferredKind::Refused,
            _ => DeferredKind::Other,
        };
        Reply::Deferred {
            kind,
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_legacy_server_is_refused_before_any_mutating_frame() {
        use std::io::Read;
        let (mut client, mut server) = crate::client::stream_pair();
        let mut old = DaemonVersion::current();
        old.features
            .retain(|feature| feature != FEATURE_IDLE_SHUTDOWN);
        assert_eq!(
            request_idle_stop(&mut client, &old).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        drop(client);
        assert_eq!(server.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn idle_shutdown_requires_the_matching_ack_not_eof_or_another_instance() {
        for reply in [
            None,
            Some(DaemonMsg::ShutdownAck {
                instance: "another".into(),
            }),
            Some(DaemonMsg::Error("busy".into())),
        ] {
            let (mut client, mut server) = crate::client::stream_pair();
            let peer = std::thread::spawn(move || {
                assert!(matches!(
                    ClientMsg::read(&mut server).unwrap(),
                    ClientMsg::Access(PaneAccess::Manage)
                ));
                assert!(matches!(
                    ClientMsg::read(&mut server).unwrap(),
                    ClientMsg::ShutdownIfIdle { .. }
                ));
                if let Some(reply) = reply {
                    reply.encode(&mut server).unwrap();
                }
            });
            assert!(request_idle_stop(&mut client, &DaemonVersion::current()).is_err());
            peer.join().unwrap();
        }
    }

    #[test]
    fn maintenance_output_requires_structured_confirmation() {
        assert_eq!(
            Reply::parse("login banner\n{\"status\":\"stopped\"}\n"),
            Some(Reply::Stopped)
        );
        for invalid in [
            "",
            "success",
            "{}",
            "{\"status\":\"stopped\"}\nunrelated output",
        ] {
            assert!(Reply::parse(invalid).is_none());
        }
        assert_eq!(
            super::super::control::server_instance(),
            super::super::protocol::process_instance()
        );
    }

    #[test]
    fn a_deferral_is_typed_so_the_far_end_never_matches_prose() {
        let deferred = Reply::deferred(&io::Error::new(io::ErrorKind::Unsupported, "too old"));
        assert_eq!(
            Reply::parse(&deferred.to_line()),
            Some(Reply::Deferred {
                kind: DeferredKind::Unsupported,
                message: "too old".into(),
            })
        );
        for (kind, expected) in [
            (io::ErrorKind::Unsupported, DeferredKind::Unsupported),
            (io::ErrorKind::TimedOut, DeferredKind::Timeout),
            (io::ErrorKind::PermissionDenied, DeferredKind::Refused),
            (io::ErrorKind::Other, DeferredKind::Other),
        ] {
            assert!(matches!(
                Reply::deferred(&io::Error::new(kind, "x")),
                Reply::Deferred { kind: got, .. } if got == expected
            ));
        }
    }

    #[test]
    fn a_server_exe_is_anchored_to_the_name_not_a_suffix() {
        for exe in [
            "/home/me/.local/share/tty7/bin/tty7-server-c7p6",
            "/home/me/.local/share/tty7/bin/tty7-server-26.8.4-nightly.202609030942",
            "/home/me/.local/share/tty7/bin/tty7-server-c7p6 (deleted)",
            "/usr/local/bin/tty7-server",
        ] {
            assert!(is_tty7_server_exe(exe), "{exe} is a server image");
        }
        for exe in [
            "/usr/bin/vim",
            "/home/me/bin/my-tty7-server-c7p6",
            "/home/me/bin/tty7-serverless",
            "tty7-server-c7p6",
            "",
        ] {
            assert!(!is_tty7_server_exe(exe), "{exe} must not be signalled");
        }
    }

    mod stop_recorded {
        use super::*;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        const SERVER_EXE: &str = "/home/me/.local/share/tty7/bin/tty7-server-c7p6";

        struct Rig {
            signalled: AtomicUsize,
            dead: AtomicBool,
        }

        impl Rig {
            fn run(
                &self,
                recorded: Option<u32>,
                exe: Option<&str>,
                endpoint_answered: bool,
                wait: Duration,
            ) -> io::Result<()> {
                stop_recorded_with(
                    recorded,
                    |_| exe.map(str::to_string),
                    |_| !self.dead.load(Ordering::SeqCst),
                    |_| {
                        self.signalled.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                    || endpoint_answered,
                    wait,
                )
            }
            fn signalled(&self) -> usize {
                self.signalled.load(Ordering::SeqCst)
            }
        }

        #[test]
        fn no_recorded_pid_and_a_silent_endpoint_is_already_stopped() {
            let rig = Rig {
                signalled: AtomicUsize::new(0),
                dead: AtomicBool::new(true),
            };
            assert!(
                rig.run(None, None, false, Duration::from_millis(50))
                    .is_ok()
            );
            assert_eq!(rig.signalled(), 0);
        }

        #[test]
        fn an_answering_endpoint_without_a_recorded_pid_is_refused() {
            let rig = Rig {
                signalled: AtomicUsize::new(0),
                dead: AtomicBool::new(true),
            };
            assert!(
                rig.run(None, None, true, Duration::from_millis(50))
                    .is_err()
            );
            assert_eq!(rig.signalled(), 0, "nothing is signalled on doubt");
        }

        #[test]
        fn a_dead_recorded_pid_needs_no_signal() {
            let rig = Rig {
                signalled: AtomicUsize::new(0),
                dead: AtomicBool::new(true),
            };
            assert!(
                rig.run(Some(4242), None, false, Duration::from_millis(50))
                    .is_ok()
            );
            assert_eq!(rig.signalled(), 0);
        }

        #[test]
        fn a_dead_pid_with_a_live_endpoint_means_someone_unknown_is_serving() {
            let rig = Rig {
                signalled: AtomicUsize::new(0),
                dead: AtomicBool::new(true),
            };
            assert!(
                rig.run(Some(4242), None, true, Duration::from_millis(50))
                    .is_err()
            );
            assert_eq!(rig.signalled(), 0);
        }

        #[test]
        fn a_reused_pid_is_never_signalled() {
            let rig = Rig {
                signalled: AtomicUsize::new(0),
                dead: AtomicBool::new(false),
            };
            let err = rig
                .run(
                    Some(4242),
                    Some("/usr/bin/vim"),
                    false,
                    Duration::from_millis(50),
                )
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(rig.signalled(), 0);
        }

        #[test]
        fn an_unverifiable_pid_is_never_signalled() {
            let rig = Rig {
                signalled: AtomicUsize::new(0),
                dead: AtomicBool::new(false),
            };
            let err = rig
                .run(Some(4242), None, false, Duration::from_millis(50))
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(rig.signalled(), 0);
        }

        #[test]
        fn the_probe_never_signals_itself() {
            let rig = Rig {
                signalled: AtomicUsize::new(0),
                dead: AtomicBool::new(false),
            };
            assert!(
                rig.run(
                    Some(std::process::id()),
                    Some(SERVER_EXE),
                    false,
                    Duration::from_millis(50)
                )
                .is_err()
            );
            assert_eq!(rig.signalled(), 0);
        }

        #[test]
        fn a_verified_server_gets_sigterm_and_its_exit_is_confirmed() {
            // The SIGTERM stands in for the real thing: once sent, the server exits.
            let dead = AtomicBool::new(false);
            let signalled = AtomicUsize::new(0);
            let result = stop_recorded_with(
                Some(4242),
                |_| Some(SERVER_EXE.to_string()),
                |_| !dead.load(Ordering::SeqCst),
                |_| {
                    signalled.fetch_add(1, Ordering::SeqCst);
                    dead.store(true, Ordering::SeqCst);
                    Ok(())
                },
                || false,
                Duration::from_secs(5),
            );
            assert!(result.is_ok());
            assert_eq!(signalled.load(Ordering::SeqCst), 1, "exactly one signal");
        }

        #[test]
        fn a_deleted_server_image_still_counts_as_ours() {
            let dead = AtomicBool::new(false);
            let result = stop_recorded_with(
                Some(4242),
                |_| Some(format!("{SERVER_EXE} (deleted)")),
                |_| !dead.load(Ordering::SeqCst),
                |_| {
                    dead.store(true, Ordering::SeqCst);
                    Ok(())
                },
                || false,
                Duration::from_secs(5),
            );
            assert!(result.is_ok());
        }

        #[test]
        fn a_server_that_will_not_exit_is_left_running() {
            let signalled = AtomicUsize::new(0);
            let err = stop_recorded_with(
                Some(4242),
                |_| Some(SERVER_EXE.to_string()),
                |_| true, // never exits
                |_| {
                    signalled.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
                || false,
                Duration::from_millis(100),
            )
            .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::TimedOut);
            assert_eq!(
                signalled.load(Ordering::SeqCst),
                1,
                "SIGTERM once, never SIGKILL"
            );
        }

        #[test]
        fn an_endpoint_still_answering_after_the_exit_is_an_error() {
            let dead = AtomicBool::new(false);
            let err = stop_recorded_with(
                Some(4242),
                |_| Some(SERVER_EXE.to_string()),
                |_| !dead.load(Ordering::SeqCst),
                |_| {
                    dead.store(true, Ordering::SeqCst);
                    Ok(())
                },
                || true, // someone else holds the endpoint
                Duration::from_secs(5),
            )
            .unwrap_err();
            assert!(err.to_string().contains("still answers"), "{err}");
        }
    }
}

fn exchange(stream: &mut Stream, message: ClientMsg) -> io::Result<DaemonMsg> {
    let mut io = DeadlineIo::new(stream, IO_WAIT)?;
    message.encode(&mut io)?;
    DaemonMsg::read(&mut io)
}

fn version() -> io::Result<Option<DaemonVersion>> {
    let mut stream = match transport::connect() {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    match exchange(&mut stream, ClientMsg::Version)? {
        DaemonMsg::Version(version) if !version.instance.is_empty() => Ok(Some(version)),
        _ => Err(io::Error::other(
            "the pane endpoint did not identify a daemon instance",
        )),
    }
}

fn request_idle_stop(stream: &mut Stream, version: &DaemonVersion) -> io::Result<()> {
    if !version.has_feature(FEATURE_IDLE_SHUTDOWN) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the running server cannot safely check idle shutdown; keep it running and defer the update until its sessions have been closed explicitly",
        ));
    }
    let mut io = DeadlineIo::new(stream, IO_WAIT)?;
    ClientMsg::Access(PaneAccess::Manage).encode(&mut io)?;
    ClientMsg::ShutdownIfIdle {
        expected_instance: version.instance.clone(),
    }
    .encode(&mut io)?;
    match DaemonMsg::read(&mut io)? {
        DaemonMsg::ShutdownAck { instance } if instance == version.instance => Ok(()),
        DaemonMsg::Error(message) => Err(io::Error::other(message)),
        _ => Err(io::Error::other(
            "idle shutdown was not acknowledged by the expected daemon",
        )),
    }
}

pub fn prepare_idle_restart(wait: Duration) -> io::Result<Reply> {
    let Some(before) = version()? else {
        return Ok(Reply::Stopped);
    };
    // Check before opening a second connection, and again in the exchange.
    if !before.has_feature(FEATURE_IDLE_SHUTDOWN) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the running server does not support safe idle restart; no processes were stopped; close its sessions and stop that server explicitly before updating",
        ));
    }
    request_idle_stop(&mut transport::connect()?, &before)?;
    let deadline = Instant::now() + wait;
    loop {
        match version()? {
            None => return Ok(Reply::Stopped),
            Some(after) if after.instance != before.instance => {
                return Err(io::Error::other(
                    "another daemon replaced the acknowledged instance; reconnect before retrying maintenance",
                ));
            }
            Some(_) => {}
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the acknowledged daemon has not exited; no forced shutdown was attempted",
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub fn check_running() -> io::Result<Reply> {
    check_health(true)
}

/// Stop the recorded daemon with SIGTERM, having proven the recorded pid is
/// a tty7 server. This is the legacy-migration path: a server too old to
/// answer `ShutdownIfIdle` can still be asked to leave by the OS, and its
/// SIGTERM handler has stored scrollback and drained panes since long before
/// the feature gate existed.
///
/// The guards are the point. Nothing here scans process lists or matches
/// names; the pid comes from this config dir's own pidfile, the identity from
/// the kernel's answer to "what is this pid executing". Any doubt — no
/// pidfile, a pid the OS has reused for something else, an endpoint that
/// still answers afterwards — refuses the operation with nothing signalled,
/// and a server that will not exit within `wait` is left running rather than
/// followed up with SIGKILL.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn stop_recorded(wait: Duration) -> io::Result<Reply> {
    stop_recorded_with(
        super::pidfile::read(),
        exe_path_of,
        |pid| super::spawn::process_alive(pid as libc::pid_t),
        send_sigterm,
        endpoint_answered,
        wait,
    )?;
    Ok(Reply::Stopped)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn stop_recorded(_wait: Duration) -> io::Result<Reply> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "stopping a recorded server by signal is a Linux/macOS path; this platform stops daemons through its own control plane",
    ))
}

#[cfg(target_os = "linux")]
fn exe_path_of(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.display().to_string())
}

#[cfg(target_os = "macos")]
fn exe_path_of(pid: u32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe {
        libc::proc_pidpath(
            pid as libc::pid_t,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    String::from_utf8(buf).ok()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn send_sigterm(pid: u32) -> io::Result<()> {
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Doubt answers "someone is there": a probe that errors proves nothing about
/// who holds the endpoint, and nothing here signals on an unanswered question.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn endpoint_answered() -> bool {
    match version() {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(_) => true,
    }
}

/// The rule alone, so the decision table can be tested without a pidfile, a
/// signal, or a process — the same split as `recorded_daemon_is_dead_with`.
/// `Ok(())` certifies nothing is serving this config dir's endpoint anymore.
#[cfg(any(test, target_os = "linux", target_os = "macos"))]
fn stop_recorded_with(
    recorded: Option<u32>,
    exe_of: impl Fn(u32) -> Option<String>,
    alive: impl Fn(u32) -> bool,
    send_term: impl Fn(u32) -> io::Result<()>,
    endpoint_answered: impl Fn() -> bool,
    wait: Duration,
) -> io::Result<()> {
    let Some(pid) = recorded else {
        if endpoint_answered() {
            return Err(io::Error::other(
                "a server answers the endpoint but no pid is recorded; identify and stop it \
                 explicitly — nothing was signalled",
            ));
        }
        return Ok(());
    };
    if pid == std::process::id() {
        return Err(io::Error::other(
            "the recorded pid is this very probe; refusing to signal it",
        ));
    }
    if !alive(pid) {
        if endpoint_answered() {
            return Err(io::Error::other(
                "the recorded server is gone but the endpoint still answers; identify the \
                 answering process explicitly — nothing was signalled",
            ));
        }
        return Ok(());
    }
    match exe_of(pid) {
        Some(exe) if is_tty7_server_exe(&exe) => {}
        Some(exe) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "the recorded server pid {pid} now runs {exe}; refusing to signal a reused pid"
                ),
            ));
        }
        None => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("could not verify what pid {pid} runs; refusing to signal it"),
            ));
        }
    }
    send_term(pid)?;
    let deadline = Instant::now() + wait;
    while alive(pid) {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "the recorded server (pid {pid}) was asked to exit and has not within \
                     {wait:?}; no forced kill was attempted"
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if endpoint_answered() {
        return Err(io::Error::other(
            "the recorded server exited but the endpoint still answers; another server may hold \
             it — nothing else was signalled",
        ));
    }
    Ok(())
}

/// A kernel-reported executable path is ours to signal only when it names a
/// tty7 server binary — the same anchoring as the installer's `/proc` sweep,
/// where a name that merely ends in ours does not count. The kernel's answers
/// are absolute, so a bare name with no path separator is not evidence at all.
#[cfg(any(test, target_os = "linux", target_os = "macos"))]
fn is_tty7_server_exe(exe: &str) -> bool {
    let trimmed = exe.trim_end_matches(" (deleted)");
    let Some((_, name)) = trimmed.rsplit_once('/') else {
        return false;
    };
    name == "tty7-server" || name.starts_with("tty7-server-")
}

/// Normal connections accept a protocol-compatible build; maintenance verifies
/// the exact candidate build. Both require real replies from the same instance.
pub fn check_serving() -> io::Result<Reply> {
    check_health(false)
}

fn check_health(exact_build: bool) -> io::Result<Reply> {
    use crate::daemon::control::{CONTROL_VERSION, ControlHello, ControlRequest, ReplyOk};
    let before = version()?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotConnected, "no pane server is answering")
    })?;
    if before.protocol != PROTOCOL_VERSION
        || (exact_build && before.build != env!("CARGO_PKG_VERSION"))
    {
        return Err(io::Error::other(
            "the running pane server does not match this candidate build/dialect",
        ));
    }
    let control = crate::client::ControlClient::connect(&ControlHello::host_rpc(
        "maintenance-probe",
        "maintenance-probe",
    ))?;
    if control.hello().instance != before.instance
        || control.hello().protocol_version != before.protocol
    {
        return Err(io::Error::other(
            "control and pane endpoints do not identify the same daemon",
        ));
    }
    if !matches!(control.request(ControlRequest::Ping)?, ReplyOk::Pong) {
        return Err(io::Error::other("the control endpoint did not answer Ping"));
    }
    let after = version()?
        .ok_or_else(|| io::Error::other("the daemon disappeared during its health check"))?;
    if after.instance != before.instance {
        return Err(io::Error::other(
            "the daemon changed during its health check",
        ));
    }
    Ok(Reply::Healthy {
        control: CONTROL_VERSION,
        protocol: before.protocol,
        build: before.build,
        instance: before.instance,
    })
}

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::core::config;
use crate::daemon::control::DialectRefusal;
use crate::daemon::protocol::{ClientMsg, DaemonMsg, DaemonVersion, PROTOCOL_VERSION};
use crate::daemon::{pidfile, transport};

#[cfg(windows)]
mod windows;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
// A version echo takes microseconds on a healthy daemon; a second is already
// generous. The old two-second budget only made an unresponsive daemon's
// stall that much longer before the bounded reap below takes over.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(6);
/// How long to wait for a refusal before assuming a handoff went through.
///
/// The daemon either execs — in which case this socket dies and the read fails
/// at once — or writes back why it did not. Neither takes long; the timeout is
/// only here so a daemon that hangs mid-handoff does not hang the window with
/// it.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(5);
/// How long after the endpoint disappears the daemon process itself gets to
/// finish exiting. Under a graceful shutdown this is milliseconds; the margin
/// covers the Windows descendant reap that runs before `exit`.
const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(any(target_os = "macos", target_os = "linux"))]
const REAP_TERM_TIMEOUT: Duration = Duration::from_secs(6);
#[cfg(any(target_os = "macos", target_os = "linux"))]
const REAP_KILL_TIMEOUT: Duration = Duration::from_secs(2);

/// Why the daemon that answered is not the one this build expects.
///
/// One process carries two version numbers — the pane protocol on `daemon.sock`
/// and the control dialect on the control socket — and a release can move one
/// without the other. They also fail differently, so a mismatch has to say
/// which it is: the first garbles pane traffic, the second leaves panes working
/// while every machine-tree call is refused, which costs the window its tabs.
pub enum DaemonMismatch {
    /// The pane protocol differs, or predates versioning entirely.
    Protocol(Option<DaemonVersion>),
    /// The pane protocol agrees but the control dialect does not.
    Dialect(DialectRefusal),
}

static MISMATCHED_DAEMON: std::sync::Mutex<Option<DaemonMismatch>> = std::sync::Mutex::new(None);

pub fn take_mismatched_daemon() -> Option<DaemonMismatch> {
    MISMATCHED_DAEMON.lock().ok()?.take()
}

/// Arms the launch prompt for the next window built.
///
/// Callers that meet the mismatch later than [`ensure_running`] — a control
/// link that comes back refused, say — record it here so the next window still
/// offers the restart rather than opening empty with no explanation.
pub fn note_daemon_mismatch(mismatch: DaemonMismatch) {
    settle_daemon_mismatch(Some(mismatch));
}

/// Record what a fresh probe found, overturning whatever the last one left.
///
/// The difference from [`note_daemon_mismatch`] is the `None`, and that is the
/// whole point of having it: a probe that finds the daemon ours is evidence,
/// and the record has to be able to *lose* a mismatch as much as gain one.
///
/// Until it could, the record only ever accumulated. The GUI arms the prompt
/// from [`ensure_running`], which is the first thing every control-link
/// reconnect attempt runs, and a mismatched daemon is one no connect succeeds
/// against — so the link backed off and retried, arming the prompt again each
/// time round, including in the seconds the user spent reading the dialog it
/// had already opened. Restarting the daemon then fixed the daemon and not the
/// record, and the next window built took that last arming and asked a second
/// time about a server that no longer existed.
fn settle_daemon_mismatch(found: Option<DaemonMismatch>) {
    if let Ok(mut slot) = MISMATCHED_DAEMON.lock() {
        *slot = found;
    }
}

/// Which daemon the records above are allowed to be about.
///
/// A probe is not one instant: it connects, asks two sockets, and only then
/// writes down what it found. In between, this build can stop the daemon, hand
/// it off, or spawn a new one — and the verdict landing afterwards describes a
/// process that no longer exists. Against a mismatched daemon that is not a
/// remote possibility but the normal case, because the control link retries on
/// a backoff and every one of those retries runs a probe: `land_handoff_return`
/// clears the record, and a probe that connected before the exec re-arms the
/// prompt about the daemon the user just replaced.
///
/// So the counter moves whenever this build deliberately changes which process
/// serves, a verdict carries the value it was taken under, and a landing whose
/// value is stale is dropped rather than written.
///
/// It does not order two probes against the *same* daemon — the last one wins,
/// which is what a record of "what is running now" should do.
static DAEMON_GENERATION: AtomicU64 = AtomicU64::new(0);

/// The stamp for a verdict about to be gathered. Read it before the probe's
/// first connect: reading it afterwards would defeat the point.
fn daemon_generation() -> u64 {
    DAEMON_GENERATION.load(Ordering::SeqCst)
}

/// This build is about to change which process is the daemon. Everything a
/// probe already in flight is going to report describes the outgoing one.
fn daemon_generation_moved() {
    DAEMON_GENERATION.fetch_add(1, Ordering::SeqCst);
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

/// The build string of a running daemon that speaks our protocol but was
/// compiled from different sources — the state an in-place upgrade leaves
/// behind, since the GUI is replaced while the daemon keeps serving every pane
/// from the old binary.
///
/// `PROTOCOL_VERSION` moves only when the wire format does, and it has not
/// moved since July, so this is the *common* case after an update, not a rare
/// one: the user sees a new version number while `pane.rs`,
/// `shell_integration.rs` and the whole ssh stack still run last release's
/// code.
///
/// Deliberately not a prompt and not an automatic restart. Restarting the
/// daemon ends every process it owns — the shells, the agents, the SSH
/// sessions — which is exactly what a user who just accepted an app update did
/// not ask for. Settings surfaces it and lets them pick the moment.
///
/// A protocol mismatch is a different problem with its own prompt, so it is
/// excluded here rather than reported twice.
pub fn local_daemon_stale_build() -> Option<String> {
    let guard = LOCAL_DAEMON.lock().ok()?;
    let version = guard.as_ref()?;
    let ours = env!("CARGO_PKG_VERSION");
    (version.protocol == PROTOCOL_VERSION && !version.build.is_empty() && version.build != ours)
        .then(|| version.build.clone())
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

/// Whether the recorded daemon process is gone — the cheap, certain signal
/// that `daemon.port` and `control.port` are stale. One process-liveness
/// query, no TCP: a dead recorded daemon means a connect to the endpoint
/// would only pay the OS's refusal delay (seconds on some machines) for
/// information the pidfile already has.
///
/// The GUI's GuiOpen handoff probe relies on the same signal: the probe
/// connects to the daemon's control listener, so a dead recorded daemon
/// guarantees no GUI is registered there — running the probe would only wait
/// out the refusal delay on the stale `control.port` that `ensure_running`
/// clears later. The probe skips itself instead.
pub fn recorded_daemon_is_dead() -> bool {
    recorded_daemon_is_dead_with(pidfile::read(), daemon_process_alive)
}

/// The rule alone, so it can be tested without a pidfile on disk — pinning a
/// config directory would mean an env var, and that is process-global state
/// every other test running beside this one would inherit.
///
/// Note what a missing pidfile answers: **not dead**, because nothing here
/// knows otherwise. Callers must not read that as "alive" — a refused connect
/// is still the authority, and `ensure_running` keeps its own stale branch for
/// exactly that.
fn recorded_daemon_is_dead_with(recorded: Option<u32>, alive: impl Fn(u32) -> bool) -> bool {
    let Some(pid) = recorded else {
        return false;
    };
    pid > 4 && pid != std::process::id() && !alive(pid)
}

#[cfg(windows)]
fn daemon_process_alive(pid: u32) -> bool {
    !crate::daemon::winproc::wait_for_exit(pid, Duration::ZERO)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn daemon_process_alive(pid: u32) -> bool {
    process_alive(pid as libc::pid_t)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn daemon_process_alive(_pid: u32) -> bool {
    // No cheap liveness query on this platform: assume alive, which keeps
    // the TCP probe as the authority.
    true
}

/// What one version probe means, apart from the sockets it took to get it.
struct ProbeVerdict {
    /// The daemon to remember for feature checks, where it named a version.
    version: Option<DaemonVersion>,
    /// What the restart prompt should be holding afterwards.
    mismatch: MismatchVerdict,
}

/// What a probe leaves the restart prompt holding.
///
/// Three outcomes rather than an `Option`, because "the daemon is ours" and
/// "the handshake never finished" are different answers that an `Option` spells
/// the same way. Only the first is evidence, and only evidence may wipe what an
/// earlier probe recorded.
enum MismatchVerdict {
    /// Ours on both handshakes. Wipe whatever an earlier probe left.
    Clear,
    /// Put this to the user.
    Found(DaemonMismatch),
    /// Nothing conclusive came back — leave the record exactly as it stands.
    ///
    /// The asymmetry is deliberate. Leaving a stale record standing costs one
    /// prompt about a daemon that turned out to be fine, and the next probe a
    /// backoff later takes it away; clearing a live one costs a window that
    /// opens with no tabs and nothing on screen to explain why, which is the
    /// whole reason the record exists.
    Unchanged,
}

/// Read a probe's answer.
///
/// `None` means the daemon never answered, which is not a mismatch to put to
/// anyone: there is nothing to describe and nothing to decide. The caller
/// treats the endpoint as stale and reaps it.
///
/// The dialect is asked for through a closure because asking costs a second
/// connect, and it is only worth spending once the pane protocol has agreed —
/// a daemon that already fails on protocol is going to be restarted whatever
/// the other socket says.
fn judge_probe(
    probe: VersionProbe,
    control_dialect: impl FnOnce() -> DialectAnswer,
) -> Option<ProbeVerdict> {
    match probe {
        // Agreeing here is only half the handshake: the control dialect is
        // versioned apart and moves on its own, so a daemon from before a
        // dialect bump passes this check and then refuses every machine-tree
        // call. Ask the other socket before calling the daemon ours, or the
        // window opens with no tabs and nothing to explain why.
        VersionProbe::Speaks(v) if v.protocol == PROTOCOL_VERSION => Some(ProbeVerdict {
            version: Some(v),
            mismatch: match control_dialect() {
                DialectAnswer::Agrees => MismatchVerdict::Clear,
                DialectAnswer::Refuses(refusal) => {
                    MismatchVerdict::Found(DaemonMismatch::Dialect(refusal))
                }
                // Half an answer decides nothing. The pane protocol agreeing
                // says nothing about the socket that stayed silent, and a
                // refusal recorded from a control link that already met this
                // daemon is worth more than our failure to ask it again.
                DialectAnswer::Silent => MismatchVerdict::Unchanged,
            },
        }),
        VersionProbe::Speaks(v) => Some(ProbeVerdict {
            version: Some(v.clone()),
            mismatch: MismatchVerdict::Found(DaemonMismatch::Protocol(Some(v))),
        }),
        VersionProbe::Legacy => Some(ProbeVerdict {
            version: None,
            mismatch: MismatchVerdict::Found(DaemonMismatch::Protocol(None)),
        }),
        VersionProbe::Unresponsive => None,
    }
}

/// Write a probe's verdict down: the daemon to remember for feature checks, and
/// what the restart prompt is left holding.
///
/// `generation` is the stamp taken before the probe started. A verdict whose
/// stamp is stale is about a daemon this build has since replaced, and writing
/// it would re-arm the prompt about a process that is already gone.
fn land_probe(generation: u64, verdict: ProbeVerdict) {
    if generation != daemon_generation() {
        log::debug!(
            "dropping a version verdict taken before the daemon was replaced underneath it"
        );
        return;
    }
    let ProbeVerdict { version, mismatch } = verdict;
    match &mismatch {
        MismatchVerdict::Found(DaemonMismatch::Protocol(Some(v))) => log::warn!(
            "daemon (build {}) speaks protocol {}, this build needs {}; \
             keeping it and deferring to the user",
            v.build,
            v.protocol,
            PROTOCOL_VERSION
        ),
        MismatchVerdict::Found(DaemonMismatch::Protocol(None)) => {
            log::warn!("daemon predates protocol versioning; keeping it and deferring to the user")
        }
        MismatchVerdict::Found(DaemonMismatch::Dialect(refusal)) => log::warn!(
            "daemon (build {}) speaks control v{}, this build speaks v{}; \
             its machine tree is out of reach until it restarts",
            refusal.peer_build,
            refusal.peer,
            refusal.ours
        ),
        MismatchVerdict::Clear | MismatchVerdict::Unchanged => {
            if let Some(v) = &version
                && !v.build.is_empty()
                && v.build != env!("CARGO_PKG_VERSION")
            {
                log::info!(
                    "daemon build {} differs from this build {}; keeping it so its panes \
                     survive the upgrade — Settings offers the restart",
                    v.build,
                    env!("CARGO_PKG_VERSION")
                );
            }
        }
    }
    note_local_daemon(version);
    // Settled, not merely noted: this probe is the current word on the daemon,
    // so a clean one has to clear what a mismatched one left behind. See
    // `settle_daemon_mismatch`.
    match mismatch {
        MismatchVerdict::Clear => settle_daemon_mismatch(None),
        MismatchVerdict::Found(found) => settle_daemon_mismatch(Some(found)),
        MismatchVerdict::Unchanged => {}
    }
}

pub fn ensure_running() -> anyhow::Result<()> {
    // A dead recorded daemon is the cheap, certain signal that the endpoint
    // files are stale: skip the connect entirely and let the cleanup below
    // clear them before the fresh daemon is spawned — the spawn poll would
    // otherwise keep paying the OS's refusal delay on the dead port.
    let mut stale = recorded_daemon_is_dead();
    if !stale {
        // Stamped before the connect, so a daemon replaced while the two
        // handshakes are in flight takes this verdict down with it.
        let generation = daemon_generation();
        if let Ok(mut stream) = transport::connect() {
            match judge_probe(query_daemon_version(&mut stream), control_dialect_answer) {
                Some(verdict) => {
                    land_probe(generation, verdict);
                    return Ok(());
                }
                None => {
                    log::info!("daemon did not answer the version handshake; restarting it");
                    note_local_daemon(None);
                    drop(stream);
                    // A graceful stop is wasted on a daemon that will not answer:
                    // `stop` polls the still-connectable endpoint for its whole
                    // SHUTDOWN_TIMEOUT before giving up. Reap the recorded process
                    // instead — bounded on every platform — and clear the stale
                    // endpoint below, exactly like a refused connect.
                    stale = true;
                }
            }
        } else {
            // A refused connect is the other stale signal, and it is the only
            // one left when the pidfile cannot answer: it is missing (the
            // daemon died between `bind` and `write_current`, or never managed
            // to write it), or it records a pid the OS has since handed to an
            // unrelated process. `recorded_daemon_is_dead` says "not dead" to
            // both, so without this the endpoint file survives and the spawn
            // poll below pays the refusal delay on it — the very cost this
            // function was rewritten to avoid.
            stale = true;
        }
    }

    if stale {
        reap_stranded();
    }

    // While an installer is replacing the installation, spawning a daemon
    // would relock the very images it is clearing — the update would fail
    // with "files in use" caused by us. Connecting to a live daemon above is
    // fine; only creating a new one waits. The short patience first is for
    // the guard's holder being a Setup that is exiting right now — the
    // post-install "Launch tty7" click — where the launch deserves its
    // daemon, not an error.
    #[cfg(windows)]
    {
        const UPDATE_GUARD_PATIENCE: Duration = Duration::from_secs(5);
        let deadline = Instant::now() + UPDATE_GUARD_PATIENCE;
        while crate::daemon::update_guard::held() {
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "a tty7 update is being installed right now; the daemon will return \
                     when the installer relaunches the app"
                );
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    spawn_detached()?;

    // After the spawn, because the spawn is itself a generation move.
    let generation = daemon_generation();
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Ok(mut stream) = transport::connect() {
            // Judged by the same rules as a daemon found already running, and
            // for the same reason. `restart()` is `stop()` + this function, so
            // on that path the branch above never runs and this is the only
            // place a verdict can land. Without one the mismatch the user just
            // clicked Restart about outlives the daemon it described, and a
            // window opened before the control link's next probe asks all over
            // again about a server that is already gone — the gap
            // `land_handoff_return` closes on the other path.
            //
            // The dialect really can be asked this early: `run_daemon` spawns
            // the control listener before it binds the pane endpoint, so a
            // daemon answering this connect is already answering that one.
            match judge_probe(query_daemon_version(&mut stream), control_dialect_answer) {
                Some(verdict) => land_probe(generation, verdict),
                // It listens but will not say what it is. Nothing to remember
                // and nothing to put to the user.
                None => note_local_daemon(None),
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "daemon did not start listening at {} within {:?}{}",
                transport::endpoint_display(),
                STARTUP_TIMEOUT,
                seat_holder_note()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Clears whatever is left of a server a connect probe already failed to
/// reach: reaps the recorded (or seat-holding, #667) process and removes the
/// endpoint files a fresh spawn would otherwise stand down against or pay a
/// refusal delay on.
///
/// A healthy server is the caller's to detect first — everything here acts on
/// the premise that nobody answered.
pub fn reap_stranded() {
    daemon_generation_moved();

    // A seat holder mid-handoff or mid-startup — claimed, not yet listening —
    // looks exactly like a stranded one from out here, and it may be carrying
    // every live session across an exec. Give it a moment to open its
    // endpoint before concluding it never will; a genuinely stranded holder
    // costs this wait once and then gets reaped.
    //
    // Health is an *answered handshake*, never a bare connect: a wedged
    // daemon's listener still completes connections out of the kernel's
    // backlog, and callers on the Unresponsive path have already proven that
    // connecting says nothing. A holder that connects but will not answer is
    // the reap's subject, not its exception — waiting out the rest of the
    // grace on it would only delay what its silence already decided.

    if crate::daemon::singleton::holder_pid().is_some() {
        let deadline = Instant::now() + STRANDED_GRACE;
        while Instant::now() < deadline {
            if let Ok(mut stream) = transport::connect() {
                match query_daemon_version(&mut stream) {
                    VersionProbe::Speaks(_) | VersionProbe::Legacy => return,
                    VersionProbe::Unresponsive => break,
                }
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    reap_recorded_daemon(None);

    if transport::endpoint_exists() {
        transport::remove_stale_endpoint();
    }
    // The daemon's control listener probes control.port for a live
    // predecessor before binding; a stale file would cost it the same
    // refused-connect delay the reap above just made unnecessary.
    #[cfg(windows)]
    crate::host::server::remove_control_endpoint();
}

/// How long a seat holder that is not answering yet gets to be a daemon
/// mid-handoff or mid-startup rather than a stranded one.
const STRANDED_GRACE: Duration = Duration::from_secs(1);

/// Names the process still holding the server seat, for the startup-timeout
/// errors: a spawned daemon that stood down against a survivor used to time
/// out with a message that pointed at nothing (#667). Empty when the seat is
/// free — the timeout is then genuinely about a slow or crashed start.
///
/// The kill advice is identity-gated: the recorded pid can outlive the
/// process it named (a pre-recording build holding the seat over an older
/// number), and an ungated message would be telling the user to kill
/// whatever process the OS reuses that pid for.
pub fn seat_holder_note() -> String {
    let Some(pid) = crate::daemon::singleton::holder_pid() else {
        return String::new();
    };
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        if !process_alive(pid as libc::pid_t) {
            return format!(
                "; the server seat is still held, but its recorded pid {pid} is no longer \
                 alive — find the holder of daemon.lock before killing anything"
            );
        }
        return match process_identity(pid as libc::pid_t) {
            ProcessIdentity::OurDaemon => format!(
                "; the server seat is still held by pid {pid}, which could not be reaped — \
                 `kill {pid}` and retry"
            ),
            ProcessIdentity::Foreign => format!(
                "; the server seat is still held, but its recorded pid {pid} now names an \
                 unrelated process — find the holder of daemon.lock before killing anything"
            ),
            // Alive, seat held, executable unreadable: most likely this *is*
            // the stranded server — saying otherwise would talk the user out
            // of the one action that frees the seat.
            ProcessIdentity::Unknown => format!(
                "; the server seat is still held by pid {pid}, whose executable can no \
                 longer be read — likely a stranded server; check it with `ps -p {pid}` \
                 before killing it"
            ),
        };
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    format!("; the server seat is still held (recorded pid {pid})")
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

/// What the control socket said when asked which dialect it speaks.
///
/// Three answers, not two. `Option<DialectRefusal>` spelled "it agrees" and "it
/// never answered" the same way, which was harmless while silence only meant
/// "record nothing" — and wrong the moment an agreeing probe started *clearing*
/// the record, because a control socket that timed out would then wipe a
/// refusal an earlier probe had recorded.
enum DialectAnswer {
    /// It answered, with our own dialect number.
    Agrees,
    /// It answered, with a number that is not ours.
    Refuses(DialectRefusal),
    /// It did not answer: still coming up, unreachable, or gone before the
    /// hello came back. Nothing was learned.
    Silent,
}

/// The control dialect the running daemon speaks.
///
/// A control link would find this out on its own, but only after the first
/// window is already on screen and already empty. Asking here — one handshake
/// on a socket the daemon answers before it touches any state — is what lets
/// launch put the question to the user instead of a log line nobody reads.
///
/// Silence is not agreement: a daemon that is still coming up, or one whose
/// control socket is unreachable, answers nothing and is left alone.
fn control_dialect_answer() -> DialectAnswer {
    #[cfg(unix)]
    let sock = match crate::host::server::control_socket_path()
        .ok()
        .and_then(|path| std::os::unix::net::UnixStream::connect(path).ok())
    {
        Some(sock) => sock,
        None => return DialectAnswer::Silent,
    };
    #[cfg(windows)]
    let sock = match crate::host::server::connect_control() {
        Ok(sock) => sock,
        Err(_) => return DialectAnswer::Silent,
    };

    if sock.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).is_err() {
        return DialectAnswer::Silent;
    }
    dialect_of(&sock)
}

/// The dialect half of a control handshake, over a link already open.
fn dialect_of<S: io::Read + io::Write>(mut link: S) -> DialectAnswer {
    use crate::daemon::control::{
        CONTROL_VERSION, ControlClientMsg, ControlHello, ControlServerMsg,
    };

    // Not `gui()`: this connection asks one question and hangs up, and a GUI
    // hello is a claim on a workspace.
    let hello =
        ControlHello::host_rpc(crate::daemon::protocol::process_instance(), "this computer");
    if ControlClientMsg::Hello(hello)
        .encode(&mut link)
        .and_then(|()| link.flush())
        .is_err()
    {
        return DialectAnswer::Silent;
    }
    match ControlServerMsg::read(&mut link) {
        Ok(ControlServerMsg::HelloOk(ok)) if ok.control_version != CONTROL_VERSION => {
            DialectAnswer::Refuses(DialectRefusal {
                peer_build: ok.build,
                peer: ok.control_version,
                ours: CONTROL_VERSION,
            })
        }
        Ok(ControlServerMsg::HelloOk(_)) => DialectAnswer::Agrees,
        // Anything else is a link that did not complete the handshake: a peer
        // that hung up, a reply we cannot parse, the read timing out.
        _ => DialectAnswer::Silent,
    }
}

pub fn restart() -> anyhow::Result<()> {
    stop();
    ensure_running()
}

/// What the daemon listening again after a handoff turned out to be.
enum HandoffReturn {
    /// This build: the exec took.
    Replaced(DaemonVersion),
    /// It answers, but it is still the image we asked to go away.
    CarriedOn(DaemonVersion),
    /// It does not answer the version handshake at all, so there is no telling
    /// what came back.
    Mute,
}

/// Land what the handoff returned: the daemon to remember for feature checks,
/// and what the restart prompt is left holding.
///
/// A replacement clears the record, and that is the step this function exists
/// to make testable. The daemon the user was asked about is gone, so the reason
/// to ask is gone with it — but the control link re-arms the prompt on every
/// failed reconnect, and against a mismatched daemon every reconnect fails. By
/// the time the dialog is answered the record has usually been set again behind
/// it. Left standing, that arming outlives the restart it asked for, and the
/// next window built asks all over again about a server that is already gone.
///
/// The reconnect that follows would eventually settle the record clean by
/// itself, through `ensure_running`. Doing it here closes the gap in between,
/// which is exactly wide enough for a window to open in.
fn land_handoff_return(generation: u64, outcome: HandoffReturn) -> anyhow::Result<()> {
    if generation != daemon_generation() {
        anyhow::bail!("the daemon was replaced again while this handoff was landing");
    }
    match outcome {
        HandoffReturn::Replaced(v) => {
            note_local_daemon(Some(v));
            settle_daemon_mismatch(None);
            Ok(())
        }
        HandoffReturn::CarriedOn(v) => {
            note_local_daemon(Some(v));
            anyhow::bail!("the daemon is still running its old build")
        }
        HandoffReturn::Mute => {
            note_local_daemon(None);
            anyhow::bail!("the daemon that came back does not answer the version handshake")
        }
    }
}

fn judge_handoff_return(probe: VersionProbe) -> HandoffReturn {
    match probe {
        VersionProbe::Speaks(v) if v.build == env!("CARGO_PKG_VERSION") => {
            HandoffReturn::Replaced(v)
        }
        VersionProbe::Speaks(v) => HandoffReturn::CarriedOn(v),
        VersionProbe::Legacy | VersionProbe::Unresponsive => HandoffReturn::Mute,
    }
}

/// Ask the running daemon to become this build without stopping.
///
/// It keeps its pid, its ptys and everything running on them; what it loses is
/// this connection, which its new image has never heard of. So the reply to a
/// handoff that worked is the socket closing, and an actual message back means
/// it did not happen.
///
/// Callers that only want the daemon to be on the current build should treat a
/// failure here as "fall back to [`restart`]": every reason this can fail — an
/// older daemon that does not know the message, a platform with no `execve`, an
/// exec that was refused — leaves the daemon exactly as it was, still serving.
pub fn hand_off() -> anyhow::Result<()> {
    use std::io::Write as _;

    if !local_daemon_supports(crate::daemon::protocol::FEATURE_HANDOFF) {
        anyhow::bail!("the running daemon cannot replace itself in place");
    }
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("could not locate own executable: {e}"))?;

    let mut stream = transport::connect()?;
    // Past this point the daemon may be replaced at any moment, so every probe
    // already in flight is describing the image being retired. Moving the
    // generation here rather than after the exec is what makes those verdicts
    // land before this one does harmless.
    if local_daemon_supports(crate::daemon::protocol::FEATURE_PANE_ACCESS) {
        ClientMsg::Access(crate::daemon::protocol::PaneAccess::Manage).encode(&mut stream)?;
    }
    daemon_generation_moved();
    let generation = daemon_generation();
    ClientMsg::Handoff { exe: exe.clone() }.encode(&mut stream)?;
    stream.flush()?;

    let _ = stream.set_read_timeout(Some(HANDOFF_TIMEOUT));
    match crate::daemon::protocol::DaemonMsg::read(&mut stream) {
        // The far end is the new image, which never had this socket.
        Err(_) => {}
        Ok(crate::daemon::protocol::DaemonMsg::Error(why)) => {
            anyhow::bail!("the daemon refused to hand over: {why}")
        }
        Ok(other) => anyhow::bail!("unexpected daemon reply to Handoff: {other:?}"),
    }
    drop(stream);

    // The socket is rebound by the new image a moment after the exec. Until it
    // is, connecting fails the same way it would against no daemon at all —
    // which is exactly what `ensure_running` would misread as "start another
    // one", so the wait belongs here rather than there.
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Ok(mut stream) = transport::connect() {
            return land_handoff_return(
                generation,
                judge_handoff_return(query_daemon_version(&mut stream)),
            );
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "the daemon did not start listening again at {} within {:?}{}",
                transport::endpoint_display(),
                STARTUP_TIMEOUT,
                seat_holder_note()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

pub fn stop() {
    use std::io::Write as _;

    daemon_generation_moved();

    // Read the pid before asking the daemon to die: the endpoint disappearing
    // is not the same event as the process releasing its image — the gap
    // between them is exactly where an installer starts replacing files that
    // are still locked — and an old build's shutdown still deletes the pidfile
    // before the process is gone, so this is the last moment the pid is
    // guaranteed readable. When the pidfile is already gone (#667), the pid
    // recorded in the singleton lock file is the remaining name for a
    // survivor that is still holding the seat.
    let recorded = pidfile::read()
        .or_else(crate::daemon::singleton::holder_pid)
        .filter(|&pid| pid > 4 && pid != std::process::id());

    let mut asked = false;
    // Explicit maintenance may still target a pre-v7 daemon. Probe on a
    // separate read-only socket, never downgrade workspace pane traffic.
    let scoped = transport::connect().ok().is_some_and(|mut stream| {
        matches!(query_daemon_version(&mut stream), VersionProbe::Speaks(version) if version.has_feature(crate::daemon::protocol::FEATURE_PANE_ACCESS))
    });
    if let Ok(mut stream) = transport::connect() {
        if scoped
            && ClientMsg::Access(crate::daemon::protocol::PaneAccess::Manage)
                .encode(&mut stream)
                .is_err()
        {
            return;
        }
        if ClientMsg::Shutdown.encode(&mut stream).is_ok() {
            let _ = stream.flush();
            asked = true;
            let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
            while Instant::now() < deadline && transport::connect().is_ok() {
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }

    // Only a shutdown that was actually delivered earns this wait: it is the
    // time a daemon that stopped listening gets to finish releasing its
    // image. A daemon nobody could even connect to was never asked to die —
    // waiting on it is five seconds spent watching a survivor not move
    // (#667); the reap below has its own graceful SIGTERM window.
    if asked
        && let Some(pid) = recorded
        && !wait_for_recorded_exit(pid, PROCESS_EXIT_TIMEOUT)
    {
        log::warn!("daemon pid {pid} released its endpoint but has not exited yet");
    }

    // With the pid read at the top, not from the pidfile: a shutdown that got
    // as far as its cleanup deleted nothing we rely on here, but a build that
    // still removes the pidfile mid-shutdown — or one whose exit stalled after
    // the cleanup — would otherwise leave the reap with no pid to act on, and
    // the survivor holding the singleton lock against every later launch
    // (#653).
    reap_recorded_daemon(recorded);

    if transport::endpoint_exists() {
        transport::remove_stale_endpoint();
    }
}

/// Whether the recorded daemon process actually exited within `timeout`. The
/// endpoint file only says the daemon stopped listening; this is what says its
/// executable image is no longer mapped.
#[cfg(windows)]
fn wait_for_recorded_exit(pid: u32, timeout: Duration) -> bool {
    crate::daemon::winproc::wait_for_exit(pid, timeout)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn wait_for_recorded_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while process_alive(pid as libc::pid_t) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    true
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn wait_for_recorded_exit(_pid: u32, _timeout: Duration) -> bool {
    true
}

/// `recorded` is a pid the caller captured before asking the daemon to die;
/// the pidfile is the first fallback, because a shutdown that stalled after
/// its cleanup may have already deleted it — and the pid in the singleton
/// lock file is the last one, for the #667 state where the pidfile is gone
/// entirely but a survivor still holds the seat.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn reap_recorded_daemon(recorded: Option<u32>) {
    let Some(pid) = recorded
        .or_else(pidfile::read)
        .or_else(crate::daemon::singleton::holder_pid)
    else {
        return;
    };
    if pid <= 1 || pid == std::process::id() {
        clear_daemon_records();
        return;
    }
    if !process_alive(pid as libc::pid_t) {
        clear_daemon_records();
        return;
    }
    match process_identity(pid as libc::pid_t) {
        ProcessIdentity::OurDaemon => {
            log::warn!("reaping unreachable daemon (pid {pid}); its sessions will be hung up");
            if !reap_process(pid as libc::pid_t) {
                // The pid is the only handle left on the survivor; keep the
                // file so the next attempt still has someone to reap.
                return;
            }
        }
        // The recorded pid now belongs to some unrelated program: the record
        // is stale, not the process.
        ProcessIdentity::Foreign => {}
        ProcessIdentity::Unknown => {
            // Alive but unnameable. Deleting the record here is what used to
            // strand the machine (#667): the pid is the only handle on
            // whatever this is, so it must outlive this attempt.
            log::warn!(
                "recorded daemon pid {pid} is alive but its executable cannot be identified; \
                 keeping its record and leaving it alone"
            );
            return;
        }
    }
    clear_daemon_records();
}

/// Clears both records of a daemon the reap has confirmed dealt with — the
/// pidfile, and the pid in the lock file (which is only touched if the seat
/// is actually free; see `clear_record_if_free`).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn clear_daemon_records() {
    pidfile::remove();
    // On macOS a dying process can stop answering proc_pidinfo before its
    // descriptors release the seat. A single nonblocking clear can therefore
    // leave the old pid behind even after a confirmed reap. Retry briefly,
    // always under the exclusive lock so a replacement's record stays intact.
    crate::daemon::singleton::clear_record_after_exit(Duration::from_secs(1));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
enum ProcessIdentity {
    /// Named like a daemon of ours: safe to reap.
    OurDaemon,
    /// Named like something else: the recorded pid has been reused.
    Foreign,
    /// Alive, but neither its executable path nor its comm name is readable.
    Unknown,
}

/// What the process behind `pid` is, judged by name — by executable path
/// first, and by the kernel's comm name when the path is unreadable.
///
/// The path is unreadable in exactly the case the reap exists for: on macOS
/// `proc_pidpath` fails outright for a live process whose binary has been
/// deleted, which is every daemon still running through an update that
/// replaced the installation. The comm name is recorded at `exec` time and
/// survives the deletion on both platforms.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_identity(pid: libc::pid_t) -> ProcessIdentity {
    let named = process_path(pid)
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .or_else(|| process_comm(pid));
    match named {
        Some(name) if is_reapable_daemon_name(&name) => ProcessIdentity::OurDaemon,
        Some(_) => ProcessIdentity::Foreign,
        None => ProcessIdentity::Unknown,
    }
}

#[cfg(target_os = "macos")]
fn process_comm(pid: libc::pid_t) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    // 2 * MAXCOMLEN, the buffer libproc's own callers use; proc_name refuses
    // anything smaller.
    let mut buf = [0u8; 64];
    let len =
        unsafe { libc::proc_name(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if len <= 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..len as usize]).into_owned())
}

#[cfg(target_os = "linux")]
fn process_comm(pid: libc::pid_t) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let comm = comm.trim();
    (!comm.is_empty()).then(|| comm.to_string())
}

/// Whether the process is gone by the end.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn reap_process(pid: libc::pid_t) -> bool {
    if signal_and_await_exit(pid, libc::SIGTERM, REAP_TERM_TIMEOUT) {
        return true;
    }
    if signal_and_await_exit(pid, libc::SIGKILL, REAP_KILL_TIMEOUT) {
        return true;
    }
    log::error!("daemon pid {pid} survived SIGKILL; leaving it behind");
    false
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn signal_and_await_exit(pid: libc::pid_t, sig: libc::c_int, timeout: Duration) -> bool {
    unsafe { libc::kill(pid, sig) };
    wait_for_recorded_exit(pid as u32, timeout)
}

/// Whether `pid` is a process that still exists — where a zombie does not
/// count. A zombie answers `kill(pid, 0)` like the living, but it holds no
/// lock, no endpoint and no image, and no signal can end it: counting it
/// alive made the reap SIGTERM-then-SIGKILL a corpse for the full eight
/// seconds of both timeouts before giving up on it. The GUI never waits on
/// the daemons it spawns, so a crashed daemon *is* a zombie of a long-lived
/// GUI, not a rare state.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn process_alive(pid: libc::pid_t) -> bool {
    let exists = unsafe { libc::kill(pid, 0) == 0 };
    exists && !is_zombie(pid)
}

#[cfg(target_os = "macos")]
fn is_zombie(pid: libc::pid_t) -> bool {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if got == size {
        return info.pbi_status == libc::SZOMB;
    }
    // Measured, not assumed: for a zombie this call fails outright while
    // `kill(pid, 0)` still succeeds — unlike a live process with a deleted
    // executable, whose state (though not its path) stays readable. A pid we
    // may signal but cannot introspect is a corpse.
    true
}

#[cfg(target_os = "linux")]
fn is_zombie(pid: libc::pid_t) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        // Readable state is the same boundary as on macOS: signalable but
        // not introspectable is a corpse, not a daemon.
        return true;
    };
    // The state field follows the comm's closing paren — the comm itself may
    // contain spaces and parens.
    stat.rsplit_once(')')
        .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
}

#[cfg(windows)]
fn reap_recorded_daemon(recorded: Option<u32>) {
    use crate::daemon::winproc;

    let Some(pid) = recorded.or_else(pidfile::read) else {
        return;
    };
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
        // One deadline across the whole tree: this runs synchronously before
        // the first window exists, and a crash that left many hosts behind
        // must not multiply the wait by their count.
        let mut targets = winproc::descendants(&procs, pid);
        targets.push(pid);
        winproc::terminate_and_wait_all(&targets, Instant::now() + REAP_WAIT_TIMEOUT);
    }
    pidfile::remove();
}

#[cfg(windows)]
const REAP_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

/// Stops the daemon and then makes the installation directory actually
/// replaceable, which is more than `stop` alone can promise: a daemon that
/// died without cleaning up leaves its ConPTY hosts (OpenConsole.exe) running,
/// orphaned, each holding the installed image open — invisible to the pidfile
/// and fatal to any installer's `DeleteFile`.
///
/// Terminates every process whose executable lives under `install_dir`
/// (except the caller), then waits until the replaceable images there can be
/// opened for writing. An error names what is still locked, so the update log
/// finally says *why* an upgrade could not replace its files.
#[cfg(windows)]
pub fn stop_for_update(install_dir: &Path) -> Result<(), String> {
    use crate::daemon::winproc;

    stop();

    let deadline = Instant::now() + UPDATE_CLEAR_TIMEOUT;
    let holdouts = winproc::processes_running_from(install_dir);
    for &pid in &holdouts {
        log::warn!(
            "terminating pid {pid} still running from {}",
            install_dir.display()
        );
    }
    winproc::terminate_and_wait_all(&holdouts, deadline);

    wait_until_images_unlocked(install_dir, deadline)
}

#[cfg(windows)]
const UPDATE_CLEAR_TIMEOUT: Duration = Duration::from_secs(10);

/// Waits until every .exe and .dll directly in `dir` can be opened for
/// writing — the same access an installer needs to replace it. Only the top
/// level: that is where the locked images (tty7-app.exe, OpenConsole.exe,
/// conpty.dll) live, and a recursive sweep would stall on unrelated content.
#[cfg(windows)]
fn wait_until_images_unlocked(dir: &Path, deadline: Instant) -> Result<(), String> {
    // Never probe our own image: the legitimate callers run from a staged
    // copy outside `dir`, but if someone invokes the *installed* binary with
    // this flag, its own image can never open for writing and the wait would
    // only ever time out.
    let own = std::env::current_exe().and_then(std::fs::canonicalize).ok();
    let images: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|error| format!("reading {}: {error}", dir.display()))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("exe") || ext.eq_ignore_ascii_case("dll")
                })
        })
        .filter(|path| {
            // Excluding takes a positive identification: a candidate that
            // cannot be canonicalized (delete-pending, held by a scanner) is
            // a lock to wait out, not ours to skip.
            !own.as_deref()
                .is_some_and(|own| std::fs::canonicalize(path).is_ok_and(|path| path == own))
        })
        .collect();

    let mut locked: Vec<&PathBuf> = images.iter().collect();
    loop {
        locked.retain(|path| std::fs::OpenOptions::new().write(true).open(path).is_err());
        if locked.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let names: Vec<String> = locked
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .collect();
            return Err(format!(
                "these files in {} are still in use by another process: {}",
                dir.display(),
                names.join(", ")
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn reap_recorded_daemon(_recorded: Option<u32>) {}

fn spawn_detached() -> anyhow::Result<()> {
    daemon_generation_moved();
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
    let path = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    Some(PathBuf::from(strip_deleted_marker(
        path.to_string_lossy().into_owned(),
    )))
}

/// A deleted executable's `/proc/<pid>/exe` reads as "/path/name (deleted)".
/// The marker is the kernel's, not part of the name — left in place it made a
/// replaced daemon read as foreign, which dropped its record without reaping
/// it (#667).
#[cfg(any(target_os = "linux", test))]
fn strip_deleted_marker(path: String) -> String {
    match path.strip_suffix(" (deleted)") {
        Some(stripped) => stripped.to_string(),
        None => path,
    }
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

/// How the daemon is created: no console of its own, and no window.
///
/// Deliberately without `CREATE_NEW_PROCESS_GROUP`. That flag disables Ctrl+C
/// for the whole new group, and Windows hands the resulting "ignore Ctrl+C"
/// process state down to every descendant — which for the daemon means every
/// ConPTY shell it spawns, and everything those shells run. It bought nothing
/// either: `DETACHED_PROCESS` already leaves the daemon with no console, so no
/// console control event could reach it in the first place. Keeping it cost
/// every pane its Ctrl+C (#451, #314).
///
/// Public because `tty7-cli` launches the headless `tty7-server` the same way,
/// and that server spawns panes too: the two detach paths agreeing is the whole
/// point of the constant.
#[cfg(windows)]
pub const DAEMON_CREATION_FLAGS: u32 = DETACHED_PROCESS | CREATE_NO_WINDOW;

#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// Named only so the tests can say which bit must stay out of the flags above.
#[cfg(all(windows, test))]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;

    cmd.creation_flags(DAEMON_CREATION_FLAGS);
}

#[cfg(all(test, windows))]
mod ctrl_c_flag_tests {
    use super::*;

    /// `CREATE_NEW_PROCESS_GROUP` disables Ctrl+C for every process in the new
    /// group, and the daemon's group is every pane shell it ever spawns. Both
    /// spawn paths — the ordinary one and the Redirection Trust detour — have to
    /// stay clear of it or #451 and #314 come straight back.
    #[test]
    fn the_daemon_is_never_created_into_a_ctrl_c_free_process_group() {
        assert_eq!(
            DAEMON_CREATION_FLAGS & CREATE_NEW_PROCESS_GROUP,
            0,
            "the daemon must not disable Ctrl+C for everything it spawns"
        );
        assert_eq!(
            DAEMON_CREATION_FLAGS,
            DETACHED_PROCESS | CREATE_NO_WINDOW,
            "the daemon still wants no console and no window"
        );

        assert_eq!(
            windows::SPAWN_FLAGS & CREATE_NEW_PROCESS_GROUP,
            0,
            "the clean-parent spawn path must not reintroduce the group either"
        );
    }
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

    /// The kernel's " (deleted)" marker on a replaced executable is not part
    /// of its name; treating it as one made an updated-under daemon read as
    /// foreign, which dropped its record without reaping it (#667).
    #[test]
    fn the_deleted_marker_is_not_part_of_a_process_name() {
        assert_eq!(
            strip_deleted_marker("/opt/tty7/tty7-server (deleted)".into()),
            "/opt/tty7/tty7-server"
        );
        assert_eq!(
            strip_deleted_marker("/opt/tty7/tty7-server".into()),
            "/opt/tty7/tty7-server"
        );
        // Only the trailing marker is the kernel's.
        assert_eq!(
            strip_deleted_marker("/tmp/x (deleted)/tty7-server".into()),
            "/tmp/x (deleted)/tty7-server"
        );
    }

    /// A zombie answers `kill(pid, 0)` like the living but holds nothing and
    /// cannot be signalled dead; counting it alive made the reap spend both
    /// kill timeouts on a corpse. The GUI never waits on the daemons it
    /// spawns, so this is the ordinary afterlife of a crashed daemon.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_zombie_is_not_an_alive_process() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id() as libc::pid_t;
        // It has exited; nobody has waited: a zombie, once the exit lands.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !is_zombie(pid) {
            assert!(
                Instant::now() < deadline,
                "the unwaited child must read as a zombie"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            unsafe { libc::kill(pid, 0) == 0 },
            "a zombie still answers signal 0 — that is the trap"
        );
        assert!(
            !process_alive(pid),
            "a corpse is not a process the reap can act on"
        );
        let _ = child.wait();
    }

    /// The comm fallback is what identifies a daemon whose executable path is
    /// unreadable — on macOS `proc_pidpath` fails outright once the binary is
    /// deleted. This process is alive and its own comm must resolve.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_live_process_resolves_its_own_comm_name() {
        let comm = process_comm(std::process::id() as libc::pid_t)
            .expect("this process is alive; its comm name must be readable");
        assert!(!comm.is_empty());
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
    use crate::daemon::control::CONTROL_VERSION;
    use std::io::ErrorKind;
    use std::os::unix::net::UnixStream;

    #[test]
    fn a_missing_pidfile_is_not_a_dead_daemon() {
        let dead = |_| false;
        let alive = |_| true;

        // The case the caller has to keep handling itself: no pidfile is no
        // evidence. The daemon may have died between `transport::bind` (which
        // writes daemon.port) and `pidfile::write_current`, or never managed
        // the write at all — a refused connect is then the only signal that
        // the endpoint file is stale, and skipping the cleanup on it leaves
        // the spawn poll paying the refusal delay this whole path exists to
        // avoid.
        assert!(!recorded_daemon_is_dead_with(None, dead));

        // A recorded pid the OS has handed to something else is likewise not
        // evidence of death — it is alive, just not ours. `reap_recorded_daemon`
        // is what checks the executable before killing anything.
        assert!(!recorded_daemon_is_dead_with(Some(4242), alive));

        // Our own pid: an in-process daemon, so certainly not dead.
        assert!(!recorded_daemon_is_dead_with(
            Some(std::process::id()),
            dead
        ));

        // Reserved low pids are never a daemon of ours; refuse to call them
        // dead so nothing downstream reaps them.
        for reserved in [0, 1, 4] {
            assert!(
                !recorded_daemon_is_dead_with(Some(reserved), dead),
                "pid {reserved} must never be treated as our dead daemon"
            );
        }

        // The one case that is evidence: a plausible pid that is gone.
        assert!(recorded_daemon_is_dead_with(Some(4242), dead));
    }

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
            matches!(process_identity(pid), ProcessIdentity::Foreign),
            "sleep must read as a foreign process — OurDaemon would mean the reap \
             could kill it, Unknown would mean its record is never cleared"
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

    /// A control server on one end of a pair, answering the handshake with
    /// `control_version` and nothing else.
    fn control_peer_speaking(
        version: u32,
        build: &'static str,
    ) -> (UnixStream, std::thread::JoinHandle<()>) {
        use crate::daemon::control::{ControlClientMsg, ControlHelloOk, ControlServerMsg};

        let (client, mut server) = UnixStream::pair().unwrap();
        let joined = std::thread::spawn(move || {
            let _ = ControlClientMsg::read(&mut server);
            let _ = ControlServerMsg::HelloOk(ControlHelloOk {
                control_version: version,
                protocol_version: PROTOCOL_VERSION,
                build: build.to_string(),
                separator: '/',
                home: "/home/test".into(),
                features: Vec::new(),
                instance: "test".into(),
            })
            .encode(&mut server);
        });
        (client, joined)
    }

    #[test]
    fn an_older_control_dialect_is_named_even_though_the_pane_protocol_agrees() {
        let (client, peer) = control_peer_speaking(CONTROL_VERSION - 1, "26.7.7-nightly");
        let DialectAnswer::Refuses(refusal) = dialect_of(&client) else {
            panic!("a dialect a version behind is a mismatch")
        };
        assert_eq!(refusal.peer, CONTROL_VERSION - 1);
        assert_eq!(refusal.ours, CONTROL_VERSION);
        assert_eq!(
            refusal.peer_build, "26.7.7-nightly",
            "the prompt names the build the user has to restart"
        );
        peer.join().unwrap();
    }

    /// A daemon left behind by a *newer* build is just as unreachable, and the
    /// refusal has to carry which side is ahead — the prompt reads the two
    /// numbers to decide whether to tell the user to restart or to update.
    #[test]
    fn a_newer_control_dialect_is_a_mismatch_too_and_says_which_side_is_ahead() {
        let (client, peer) = control_peer_speaking(CONTROL_VERSION + 1, "26.9.0-nightly");
        let DialectAnswer::Refuses(refusal) = dialect_of(&client) else {
            panic!("a dialect a version ahead is a mismatch")
        };
        assert!(
            refusal.peer > refusal.ours,
            "the peer is ahead and the refusal must show it: {refusal:?}"
        );
        peer.join().unwrap();
    }

    #[test]
    fn a_server_of_our_own_dialect_is_not_a_mismatch() {
        let (client, peer) = control_peer_speaking(CONTROL_VERSION, "26.8.2-nightly");
        assert!(
            matches!(dialect_of(&client), DialectAnswer::Agrees),
            "a server that answered with our own number is the one answer that clears the record"
        );
        peer.join().unwrap();
    }

    #[test]
    fn a_control_socket_that_says_nothing_is_left_alone() {
        let (client, peer) = UnixStream::pair().unwrap();
        client.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).unwrap();
        assert!(
            matches!(dialect_of(&client), DialectAnswer::Silent),
            "a server still coming up must be reported as neither the wrong version nor \
             the right one — reading it as agreement is what wipes a live refusal"
        );
        drop(peer);
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

#[cfg(test)]
mod mismatch_record_tests {
    use super::*;
    use crate::daemon::control::DialectRefusal;

    fn answering(protocol: u32, build: &str) -> VersionProbe {
        VersionProbe::Speaks(DaemonVersion {
            protocol,
            build: build.to_string(),
            features: Vec::new(),
            instance: "test-instance".to_string(),
        })
    }

    fn agrees() -> DialectAnswer {
        DialectAnswer::Agrees
    }

    fn refuses() -> DialectAnswer {
        DialectAnswer::Refuses(DialectRefusal {
            peer_build: "26.8.4-nightly.202608251825".to_string(),
            peer: 7,
            ours: 8,
        })
    }

    fn says_nothing() -> DialectAnswer {
        DialectAnswer::Silent
    }

    /// The record says what the daemon running *now* is, so a clean probe has
    /// to be able to overturn it — not merely decline to add to it.
    ///
    /// The prompt is armed from `ensure_running`, and `ensure_running` is the
    /// first thing every control-link reconnect attempt does. A mismatched
    /// daemon fails that connect, so the link backs off and retries, arming the
    /// prompt again each time round — including in the seconds the user spends
    /// reading the dialog it already opened. Restart the daemon and it is fine,
    /// but that last arming outlived the restart, and the next window built
    /// took it and asked a second time about a server that no longer exists.
    #[test]
    fn a_probe_that_finds_the_daemon_ours_overturns_an_earlier_mismatch() {
        let verdict = judge_probe(answering(PROTOCOL_VERSION, "1.0.0"), agrees)
            .expect("a daemon that answers the handshake is not a stale endpoint");
        assert!(
            matches!(verdict.mismatch, MismatchVerdict::Clear),
            "a daemon that agrees on both versions must wipe the record, not leave \
             an earlier probe's verdict standing"
        );
    }

    /// The half of the handshake that lives on the other socket. Panes work,
    /// every machine-tree call is refused, and the window opens with no tabs.
    #[test]
    fn a_refused_control_dialect_is_a_mismatch_even_when_the_protocol_agrees() {
        let verdict = judge_probe(answering(PROTOCOL_VERSION, "old"), refuses)
            .expect("the daemon answered; it is not stale");
        assert!(
            matches!(verdict.mismatch, MismatchVerdict::Found(DaemonMismatch::Dialect(r)) if r.peer == 7),
            "the dialect refusal has to reach the prompt, carrying the peer's number"
        );
    }

    /// The other half of the same socket: it did not answer at all.
    ///
    /// Not a mismatch and not a clean bill either. Reading silence as agreement
    /// is how a dialect refusal that a control link had already met gets wiped
    /// by a probe that merely failed to ask, leaving the next window to open
    /// empty with nothing on screen to explain it.
    #[test]
    fn a_silent_control_socket_leaves_an_earlier_verdict_standing() {
        let verdict = judge_probe(answering(PROTOCOL_VERSION, "old"), says_nothing)
            .expect("the pane endpoint answered; it is not stale");
        assert!(
            matches!(verdict.mismatch, MismatchVerdict::Unchanged),
            "a control socket that never answered has agreed to nothing"
        );
        assert!(
            verdict.version.is_some(),
            "the pane protocol still answered, so the daemon is still the one to \
             remember for feature checks"
        );
    }

    /// A build that differs is not by itself a mismatch: its panes still speak
    /// this protocol, and killing them to even up a version string is the
    /// upgrade that costs the user their session for nothing.
    #[test]
    fn a_different_build_on_the_agreed_protocol_is_not_a_mismatch() {
        let verdict = judge_probe(
            answering(PROTOCOL_VERSION, "26.8.4-nightly.202608251825"),
            agrees,
        )
        .expect("the daemon answered; it is not stale");
        assert!(
            matches!(verdict.mismatch, MismatchVerdict::Clear),
            "only the version numbers decide; a build string that differs is left alone"
        );
    }

    #[test]
    fn a_disagreeing_pane_protocol_is_a_mismatch() {
        let verdict = judge_probe(answering(PROTOCOL_VERSION + 1, "newer"), agrees)
            .expect("the daemon answered; it is not stale");
        assert!(
            matches!(&verdict.mismatch, MismatchVerdict::Found(DaemonMismatch::Protocol(Some(v))) if v.protocol == PROTOCOL_VERSION + 1),
            "the prompt needs the peer's protocol number to say what it found"
        );
        assert!(
            verdict.version.is_some(),
            "a mismatched daemon is still the daemon to remember for feature checks"
        );
    }

    #[test]
    fn a_daemon_from_before_versioning_is_a_mismatch_with_nothing_to_report() {
        let verdict = judge_probe(VersionProbe::Legacy, agrees)
            .expect("it answered, just not with a version");
        assert!(matches!(
            verdict.mismatch,
            MismatchVerdict::Found(DaemonMismatch::Protocol(None))
        ));
        assert!(
            verdict.version.is_none(),
            "there is no version to remember from a daemon that reports none"
        );
    }

    /// Not a mismatch — nothing to put to the user. The endpoint is stale and
    /// the caller reaps it.
    #[test]
    fn a_daemon_that_does_not_answer_is_stale_rather_than_mismatched() {
        assert!(judge_probe(VersionProbe::Unresponsive, agrees).is_none());
    }

    /// The dialect costs a second connect, so it is only worth asking once the
    /// pane protocol has agreed — on a protocol mismatch the restart is already
    /// decided.
    #[test]
    fn the_control_dialect_is_only_asked_once_the_pane_protocol_agrees() {
        let asked = std::cell::Cell::new(false);
        let _ = judge_probe(answering(PROTOCOL_VERSION + 1, "newer"), || {
            asked.set(true);
            DialectAnswer::Agrees
        });
        assert!(
            !asked.get(),
            "a daemon already known to be mismatched must not be probed a second time"
        );
    }

    #[test]
    fn a_handoff_that_returns_this_build_is_a_replacement() {
        let outcome = judge_handoff_return(answering(PROTOCOL_VERSION, env!("CARGO_PKG_VERSION")));
        assert!(matches!(outcome, HandoffReturn::Replaced(_)));
    }

    /// The exec did not take. Same process, same old image — and the caller
    /// reports the failure rather than pretending the upgrade happened.
    #[test]
    fn a_handoff_that_returns_the_old_build_is_not_a_replacement() {
        let outcome =
            judge_handoff_return(answering(PROTOCOL_VERSION, "26.8.4-nightly.202608251825"));
        assert!(matches!(outcome, HandoffReturn::CarriedOn(_)));
    }

    #[test]
    fn a_handoff_that_returns_something_mute_is_neither() {
        assert!(matches!(
            judge_handoff_return(VersionProbe::Legacy),
            HandoffReturn::Mute
        ));
        assert!(matches!(
            judge_handoff_return(VersionProbe::Unresponsive),
            HandoffReturn::Mute
        ));
    }

    /// The verdict actually reaching the global record, which is the step the
    /// pure judgement above cannot cover.
    ///
    /// One test for the whole sequence on purpose: the record is process-global
    /// and Rust runs tests in this binary side by side, so splitting these into
    /// separate `#[test]`s would let them overwrite each other's state.
    #[test]
    fn a_clean_verdict_wipes_what_an_earlier_one_recorded() {
        settle_daemon_mismatch(Some(DaemonMismatch::Protocol(None)));
        assert!(
            take_mismatched_daemon().is_some(),
            "a recorded mismatch is there for the next window to take"
        );

        settle_daemon_mismatch(Some(DaemonMismatch::Protocol(None)));
        settle_daemon_mismatch(None);
        assert!(
            take_mismatched_daemon().is_none(),
            "a clean probe after a mismatched one must leave nothing for a window to \
             prompt about — this is the second, bogus dialog"
        );

        // The restart the user actually asked for. The record here is the one
        // the reconnect loop set again while the dialog was on screen; landing
        // the handoff has to take it away, or it outlives the daemon it
        // describes and the next window opened prompts about a dead server.
        note_daemon_mismatch(DaemonMismatch::Protocol(None));
        land_handoff_return(
            daemon_generation(),
            HandoffReturn::Replaced(DaemonVersion {
                protocol: PROTOCOL_VERSION,
                build: env!("CARGO_PKG_VERSION").to_string(),
                features: Vec::new(),
                instance: "replacement".to_string(),
            }),
        )
        .expect("a daemon that came back as this build is a successful handoff");
        assert!(
            take_mismatched_daemon().is_none(),
            "the daemon the prompt described has been replaced; nothing is left to ask"
        );

        // The other restart, the one for a daemon too old to hand off: `stop()`
        // and a fresh spawn, landed through the same verdict the
        // already-running branch uses. Nothing else on that path touches the
        // record, so without this the prompt the user just answered comes back
        // about the daemon they killed.
        note_daemon_mismatch(DaemonMismatch::Protocol(None));
        land_probe(
            daemon_generation(),
            judge_probe(
                answering(PROTOCOL_VERSION, env!("CARGO_PKG_VERSION")),
                agrees,
            )
            .expect("the freshly spawned daemon answered its own handshake"),
        );
        assert!(
            take_mismatched_daemon().is_none(),
            "the daemon the prompt described was stopped and replaced; nothing is left to ask"
        );

        // A verdict about a daemon that has since been replaced is not written
        // at all. This is the reconnect that connected before the handoff and
        // came back after it: without the stamp it re-arms the prompt about a
        // process the user has already replaced, and the fix above closes only
        // the gap it can see.
        let taken_before = daemon_generation();
        daemon_generation_moved();
        land_probe(
            taken_before,
            judge_probe(
                answering(PROTOCOL_VERSION + 1, "the daemon that was"),
                agrees,
            )
            .expect("the outgoing daemon answered, late"),
        );
        assert!(
            take_mismatched_daemon().is_none(),
            "a verdict about a replaced daemon must not reach the prompt"
        );

        // And the same both ways round: a late *clean* verdict about a daemon
        // that has since been replaced must not wipe what the current one left.
        note_daemon_mismatch(DaemonMismatch::Protocol(None));
        let taken_before = daemon_generation();
        daemon_generation_moved();
        land_probe(
            taken_before,
            judge_probe(
                answering(PROTOCOL_VERSION, env!("CARGO_PKG_VERSION")),
                agrees,
            )
            .expect("the outgoing daemon answered, late"),
        );
        assert!(
            take_mismatched_daemon().is_some(),
            "a stale clean verdict says nothing about the daemon running now"
        );

        // A handoff that did not take leaves the daemon exactly as it was, so
        // the record describing it stays — the user still needs the offer.
        note_daemon_mismatch(DaemonMismatch::Protocol(None));
        assert!(
            land_handoff_return(
                daemon_generation(),
                HandoffReturn::CarriedOn(DaemonVersion {
                    protocol: PROTOCOL_VERSION,
                    build: "26.8.4-nightly.202608251825".to_string(),
                    features: Vec::new(),
                    instance: "survivor".to_string(),
                })
            )
            .is_err(),
            "a daemon still on its old build is a handoff that failed"
        );
        assert!(
            take_mismatched_daemon().is_some(),
            "the mismatched daemon is still running, so the prompt still has something to say"
        );
    }
}

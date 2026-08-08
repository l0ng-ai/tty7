use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::core::kitty_graphics::{GraphicsSniffer, Segment, Sniffed};
use crate::core::osc::OscTokenizer;
use crate::daemon::protocol::{
    AuthResponse, DaemonMsg, MAX_FRAME, NativeSshSpec, PaneInfo, RemoteContext, RemoteKind,
    ShellSpec, WinSize,
};
use crate::daemon::shell_integration;

#[cfg(windows)]
fn default_prog() -> CommandBuilder {
    CommandBuilder::new(crate::core::shells::windows_default_shell())
}

#[cfg(not(windows))]
fn default_prog() -> CommandBuilder {
    default_prog_with_override(detected_shell_override())
}

#[cfg(not(windows))]
fn default_prog_with_override(shell_override: Option<String>) -> CommandBuilder {
    let cmd = CommandBuilder::new_default_prog();
    let portable_shell = cmd.get_shell();
    if let Some(shell) = shell_override.filter(|shell| shell != &portable_shell) {
        return CommandBuilder::new(&shell);
    }
    cmd
}

#[cfg(not(windows))]
fn detected_shell_override() -> Option<String> {
    let path = std::env::var_os(crate::daemon::DETECTED_SHELL_ENV)?;
    usable_shell_path(path)
}

#[cfg(not(windows))]
fn usable_shell_path(path: std::ffi::OsString) -> Option<String> {
    let path = PathBuf::from(path);
    if path.as_os_str().is_empty() || !path.is_file() {
        return None;
    }
    path.into_os_string().into_string().ok()
}

#[cfg(windows)]
fn default_shell_name(_cmd: &CommandBuilder) -> String {
    crate::core::shells::windows_default_shell().to_string()
}

#[cfg(not(windows))]
fn default_shell_name(cmd: &CommandBuilder) -> String {
    cmd.get_shell()
}

#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
struct ChosenShell {
    program: String,
    args: Vec<String>,
    args_are_tty7_defaults: bool,
}

fn has_custom_args(chosen: Option<&ChosenShell>) -> bool {
    chosen.is_some_and(|c| !c.args.is_empty() && !c.args_are_tty7_defaults)
}

fn choose_shell(
    spawn_override: Option<ShellSpec>,
    configured: Option<(String, Vec<String>)>,
) -> Option<ChosenShell> {
    spawn_override
        .map(|s| ChosenShell {
            program: s.program,
            args: s.args,
            args_are_tty7_defaults: s.args_are_tty7_defaults,
        })
        .or_else(|| {
            configured.map(|(program, args)| ChosenShell {
                program,
                args,
                args_are_tty7_defaults: false,
            })
        })
}

fn apply_shell_integration(
    cmd: &mut CommandBuilder,
    resolved_program: &str,
    integration: &shell_integration::Injection,
) {
    if integration.replaces_argv || (cmd.is_default_prog() && !integration.args.is_empty()) {
        *cmd = CommandBuilder::new(resolved_program);
    }
    cmd.args(&integration.args);
    for (k, v) in &integration.env {
        cmd.env(k, v);
    }
}

struct SpawnConfig {
    cmd: CommandBuilder,
    initial_cwd: Option<PathBuf>,
    integration_dir: Option<PathBuf>,
    remote: Option<RemoteContext>,
}

fn build_spawn_config(
    pane: u64,
    cwd: Option<PathBuf>,
    shell: Option<ShellSpec>,
    workspace: Option<&str>,
) -> anyhow::Result<SpawnConfig> {
    let initial_cwd = initial_working_directory(cwd);
    let configured = choose_shell(shell, crate::core::config::shell_command());
    let remote = wsl_remote_context(configured.as_ref());
    let (cmd, integration_dir) = build_shell_command(configured, &initial_cwd, pane, workspace)?;
    Ok(SpawnConfig {
        cmd,
        initial_cwd,
        integration_dir,
        remote,
    })
}

fn wsl_remote_context(shell: Option<&ChosenShell>) -> Option<RemoteContext> {
    if !cfg!(windows) {
        return None;
    }
    let chosen = shell?;
    let base = std::path::Path::new(&chosen.program)
        .file_name()?
        .to_str()?
        .to_ascii_lowercase();
    if base.strip_suffix(".exe").unwrap_or(&base) != "wsl" {
        return None;
    }
    Some(RemoteContext {
        kind: RemoteKind::Wsl,
        argv: Vec::new(),
        target: shell_integration::wsl_distro(&chosen.args).unwrap_or_default(),
    })
}

fn build_shell_command(
    configured: Option<ChosenShell>,
    initial_cwd: &Option<PathBuf>,
    pane: u64,
    workspace: Option<&str>,
) -> anyhow::Result<(CommandBuilder, Option<PathBuf>)> {
    let mut cmd = match &configured {
        Some(chosen) => {
            let mut c = CommandBuilder::new(&chosen.program);
            c.args(&chosen.args);
            c
        }
        None => default_prog(),
    };
    let resolved_program = match &configured {
        Some(chosen) => chosen.program.clone(),
        None => default_shell_name(&cmd),
    };

    let integration = shell_integration::setup(
        Some(&resolved_program),
        configured.as_ref().map_or(&[][..], |c| c.args.as_slice()),
        has_custom_args(configured.as_ref()),
    );
    if let Some(integration) = &integration {
        apply_shell_integration(&mut cmd, &resolved_program, integration);
    }
    let integration_dir = integration.as_ref().and_then(|i| i.dir.clone());
    let launched = launched_shell_program(&cmd, &resolved_program);
    apply_common_command_setup(&mut cmd, initial_cwd, pane, workspace, &launched);
    Ok((cmd, integration_dir))
}

/// The program the pane is really about to exec.
///
/// `resolved_program` is what tty7 *decided* to run before shell integration
/// had its say, and for a default-prog builder that decision is
/// `CommandBuilder::get_shell()` — the passwd entry — even when
/// `default_prog()` swapped the builder for the shell that launched the daemon.
/// argv survives every rewrite on the way here, including the one an
/// argv-replacing injection performs, so it is the honest answer. An empty argv
/// means the builder is still the untouched default prog, which is precisely
/// the case `resolved_program` already describes.
fn launched_shell_program(cmd: &CommandBuilder, resolved_program: &str) -> String {
    cmd.get_argv()
        .first()
        .map(|argv0| argv0.to_string_lossy().into_owned())
        .unwrap_or_else(|| resolved_program.to_string())
}

fn initial_working_directory(cwd: Option<PathBuf>) -> Option<PathBuf> {
    let fallback = std::env::current_dir()
        .ok()
        .filter(|d| d != std::path::Path::new("/"))
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from));
    let forced = crate::core::config::working_directory_base();
    [cwd, forced, fallback]
        .into_iter()
        .flatten()
        .find(|d| d.is_dir())
}

#[cfg(any(target_os = "macos", test))]
fn locale_fallback_is_needed(
    extra_env: &std::collections::HashMap<String, String>,
    mut inherited: impl FnMut(&str) -> Option<String>,
) -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"].iter().all(|key| {
        !extra_env.contains_key(*key) && inherited(key).as_deref().is_none_or(str::is_empty)
    })
}

#[cfg(target_os = "macos")]
const LOCALE_DEFINITION_DIR: &str = "/usr/share/locale";

#[cfg(any(target_os = "macos", test))]
const FALLBACK_CHARACTER_LOCALES: [&str; 2] = ["C.UTF-8", "en_US.UTF-8"];

/// The variable the locale fallback sets.
///
/// It has to be one a shell consults for *every* category. `LC_CTYPE` alone
/// fixes character handling and leaves collation, time and numbers at `C`; a
/// shell that then asks `setlocale(LC_COLLATE, "")` finds no variable to read
/// and bash warns `setlocale: LC_COLLATE: cannot change locale ()` once per
/// category. (zsh and fish swallow the failure, so they look fine while being
/// just as half-configured.) `LC_ALL` would also cover everything, but it wins
/// over every `LC_*` the user's own rc files set afterwards — `LANG` loses to
/// them, which is what a fallback should do.
#[cfg(any(target_os = "macos", test))]
const LOCALE_FALLBACK_KEY: &str = "LANG";

#[cfg(any(target_os = "macos", test))]
fn character_locale(identifier: Option<&str>, exists: impl Fn(&str) -> bool) -> Option<String> {
    identifier
        .and_then(posix_locale_stem)
        .map(|stem| format!("{stem}.UTF-8"))
        .into_iter()
        .chain(FALLBACK_CHARACTER_LOCALES.iter().map(|s| (*s).to_string()))
        .find(|candidate| exists(candidate))
}

#[cfg(any(target_os = "macos", test))]
fn posix_locale_stem(identifier: &str) -> Option<String> {
    let base = identifier.split('@').next()?;
    let mut parts = base.split(['_', '-']).filter(|s| !s.is_empty());
    let language = parts
        .next()
        .filter(|l| (2..=3).contains(&l.len()) && l.chars().all(|c| c.is_ascii_alphabetic()))?;
    let region = parts.next_back().filter(|r| {
        (r.len() == 2 && r.chars().all(|c| c.is_ascii_alphabetic()))
            || (r.len() == 3 && r.chars().all(|c| c.is_ascii_digit()))
    })?;
    Some(format!(
        "{}_{}",
        language.to_ascii_lowercase(),
        region.to_ascii_uppercase()
    ))
}

#[cfg(target_os = "macos")]
fn system_locale_identifier() -> Option<String> {
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use std::os::raw::c_void;

    type CFLocaleRef = *const c_void;
    unsafe extern "C" {
        fn CFLocaleCopyCurrent() -> CFLocaleRef;
        fn CFLocaleGetIdentifier(locale: CFLocaleRef) -> CFStringRef;
        fn CFRelease(cf: *const c_void);
    }

    unsafe {
        let locale = CFLocaleCopyCurrent();
        if locale.is_null() {
            return None;
        }
        let identifier = CFLocaleGetIdentifier(locale);
        let out =
            (!identifier.is_null()).then(|| CFString::wrap_under_get_rule(identifier).to_string());
        CFRelease(locale);
        out
    }
}

const TERM_PROGRAM_NAME: &str = "tty7";

/// The config dir this server runs on, not a socket path.
///
/// A client needs *two* endpoints — control and pane — and they are derived
/// from the config dir by rules that already live in this crate
/// (`control_socket_path`, `transport::socket_path_for`), including a hashed
/// fallback when the directory is too long for `sun_path`. Publishing one
/// socket path and letting the client reconstruct the other from it means a
/// second, independent derivation that can and did disagree with the first.
/// Publishing the directory instead means a CLI in this shell resolves both
/// endpoints with the very same functions the server used to open them.
const TTY7_CONFIG_DIR_ENV: &str = "TTY7_CONFIG_DIR";
const TTY7_PANE_ENV: &str = "TTY7_PANE";
const TTY7_WS_ENV: &str = "TTY7_WS";

fn config_dir_env() -> Option<String> {
    crate::core::config::config_dir_path().map(|p| p.display().to_string())
}

/// The shell the pane is running, not the one the passwd entry names.
///
/// Everything that spawns "the user's shell" — tmux's `default-shell`, `sudo
/// -s`, an editor's `!` escape, a coding agent picking a quoting dialect —
/// reads `$SHELL`. A GUI launch inherits the login session's snapshot of it,
/// so a pane told to run fish still advertised `/bin/zsh` and every one of
/// those consumers walked off into the wrong shell (#342). Inside a pane, the
/// shell tty7 was told to run *is* the user's shell, so that is what we name.
///
/// Note this is the opposite direction from `shells::login_shell()`, which
/// deliberately ignores `$SHELL` in favour of passwd: that answers "which
/// shell did the user log in with", which is still the login shell. This
/// answers "which shell is this pane", which is not the same question.
const SHELL_ENV: &str = "SHELL";

/// The absolute path to name in `$SHELL`, or `None` when there is none to
/// promise.
///
/// `$SHELL` is a POSIX contract, so the path rules here are POSIX ones rather
/// than the host's — this only ever describes a pane on a unix daemon (see the
/// caller), and hard-coding them keeps the resolution testable on any host.
///
/// A configured command may be bare (`fish`), because a bare command is what
/// the shell inventory keeps when it wants PATH to decide which install wins.
/// Consumers of `$SHELL` exec it directly, and many of them do so with a PATH
/// of their own, so a bare name there is not the same binary the pane runs.
/// Resolve it the way exec would, against the PATH the pane will inherit, and
/// leave `$SHELL` untouched when that finds nothing — a stale but honest login
/// shell beats a name that resolves somewhere else.
fn shell_env_path(
    program: &str,
    path_var: impl FnOnce() -> Option<String>,
    is_file: impl Fn(&str) -> bool,
) -> Option<String> {
    let program = program.trim();
    if program.is_empty() {
        return None;
    }
    if program.starts_with('/') {
        return Some(program.to_string());
    }
    // Anything else with a separator is relative to a working directory, and
    // the reader's is not necessarily the pane's. Only a bare name is worth
    // resolving.
    if program.contains('/') {
        return None;
    }
    path_var()?
        .split(':')
        .filter(|dir| dir.starts_with('/'))
        .map(|dir| format!("{}/{program}", dir.trim_end_matches('/')))
        .find(|candidate| is_file(candidate))
}

const CAPABILITY_ENV: [&str; 2] = ["TERM", "COLORTERM"];

fn names_capability_env(key: &str) -> bool {
    CAPABILITY_ENV.iter().any(|cap| {
        if cfg!(windows) {
            key.eq_ignore_ascii_case(cap)
        } else {
            key == *cap
        }
    })
}

fn pane_environment(
    extra_env: &std::collections::HashMap<String, String>,
    // Only Windows has a use for it — see the `COLORFGBG` block below.
    #[cfg_attr(not(windows), allow(unused_variables))] dark: bool,
    pane: u64,
    workspace: Option<&str>,
    shell: &str,
) -> Vec<(String, String)> {
    let version = env!("CARGO_PKG_VERSION");
    let mut env = vec![
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
        (
            crate::core::agent_hooks::TTY7_ENV_MARKER.to_string(),
            version.to_string(),
        ),
        ("TERM_PROGRAM".to_string(), TERM_PROGRAM_NAME.to_string()),
        ("TERM_PROGRAM_VERSION".to_string(), version.to_string()),
        (TTY7_PANE_ENV.to_string(), pane.to_string()),
    ];
    #[cfg(windows)]
    if !extra_env
        .keys()
        .any(|key| key.eq_ignore_ascii_case("COLORFGBG"))
    {
        // The bundled ConPTY forwards OSC 11 queries, so this is no longer the
        // only answer a Windows pane gets — but the in-box conhost still
        // swallows them, and that is what a build without `conpty.dll` beside
        // it runs on. Keep the conventional fallback hint for that case, and
        // for applications that never learned to ask. An explicit user override
        // still wins, below.
        let colorfgbg = if dark { "15;0" } else { "0;15" };
        env.push(("COLORFGBG".to_string(), colorfgbg.to_string()));
    }
    if let Some(ws) = workspace {
        env.push((TTY7_WS_ENV.to_string(), ws.to_string()));
    }
    if let Some(dir) = config_dir_env() {
        env.push((TTY7_CONFIG_DIR_ENV.to_string(), dir));
    }
    // Windows is deliberately left out. `SHELL` is not a native concept there —
    // neither cmd nor PowerShell reads it — and the tools that do read it are
    // the POSIX emulations: MSYS/Git Bash, Cygwin, and WSL, which take `WSLENV`
    // exports as Linux paths. All of them want a POSIX path, and every shell
    // this branch could name is a Windows one, so setting it would point them
    // at something they cannot exec. That also covers the WSL pane, whose
    // program is `wsl.exe` rather than a shell at all: the distro's own login
    // shell is the right answer inside it, and leaving `SHELL` alone is how
    // that answer survives.
    //
    // Resolution runs against the PATH the pane will actually inherit, so a
    // user who redirects PATH in their `env` block gets the binary that PATH
    // resolves rather than the daemon's.
    if !cfg!(windows)
        && let Some(path) = shell_env_path(
            shell,
            || {
                extra_env
                    .get("PATH")
                    .cloned()
                    .or_else(|| std::env::var("PATH").ok())
            },
            |candidate| std::path::Path::new(candidate).is_file(),
        )
    {
        env.push((SHELL_ENV.to_string(), path));
    }
    env.extend(
        extra_env
            .iter()
            .filter(|(k, _)| !names_capability_env(k))
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    env
}

fn apply_common_command_setup(
    cmd: &mut CommandBuilder,
    initial_cwd: &Option<PathBuf>,
    pane: u64,
    workspace: Option<&str>,
    shell: &str,
) {
    if let Some(dir) = initial_cwd {
        cmd.cwd(dir);
    }
    let extra_env = crate::core::config::extra_env();

    // Windows hands every process a private copy of the environment at spawn
    // time and never updates it, so a daemon that has been running since before
    // an installer touched `HKCU\Environment` would give a brand-new pane its
    // startup `PATH` (#333). Re-read both hives here and pin the merge onto the
    // command; the pane-specific variables below (and the configured overrides
    // they carry) are applied afterwards and still win.
    #[cfg(windows)]
    for (k, v) in crate::daemon::windows_env::refreshed_pane_environment(&extra_env) {
        cmd.env(k, v);
    }

    let dark = crate::core::machine::appearance().dark;
    for (k, v) in pane_environment(&extra_env, dark, pane, workspace, shell) {
        cmd.env(k, v);
    }

    #[cfg(target_os = "macos")]
    if locale_fallback_is_needed(&extra_env, |key| std::env::var(key).ok())
        && let Some(locale) = character_locale(system_locale_identifier().as_deref(), |name| {
            std::path::Path::new(LOCALE_DEFINITION_DIR)
                .join(name)
                .is_dir()
        })
    {
        cmd.env(LOCALE_FALLBACK_KEY, locale);
    }
}

const RING_CAP: usize = 8 * 1024 * 1024;

const MAX_RING_SEGMENTS: usize = 64;
const REMOTE_CONTEXT_POLL_INTERVAL: Duration = Duration::from_millis(500);

pub struct OutputGate {
    queued: AtomicI64,
    park: Mutex<()>,
    drained: Condvar,
}

impl OutputGate {
    const HIGH_WATER: i64 = 16 * 1024 * 1024;
    const MAX_WAIT: Duration = Duration::from_secs(2);

    pub(crate) fn new() -> Self {
        Self {
            queued: AtomicI64::new(0),
            park: Mutex::new(()),
            drained: Condvar::new(),
        }
    }

    fn add(&self, n: usize) {
        self.queued.fetch_add(n as i64, Ordering::Relaxed);
    }

    pub fn sub(&self, n: usize) {
        let prev = self.queued.fetch_sub(n as i64, Ordering::Relaxed);
        if prev >= Self::HIGH_WATER && prev - (n as i64) < Self::HIGH_WATER {
            let _park = self.park.lock().unwrap();
            self.drained.notify_all();
        }
    }

    fn reset(&self) {
        self.queued.store(0, Ordering::Relaxed);
        let _park = self.park.lock().unwrap();
        self.drained.notify_all();
    }

    pub(crate) fn queued_bytes(&self) -> i64 {
        self.queued.load(Ordering::Relaxed)
    }

    fn wait_below_high_water(&self) {
        if self.queued.load(Ordering::Relaxed) < Self::HIGH_WATER {
            return;
        }
        let deadline = std::time::Instant::now() + Self::MAX_WAIT;
        let mut park = self.park.lock().unwrap();
        while self.queued.load(Ordering::Relaxed) >= Self::HIGH_WATER {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return;
            }
            let (guard, _) = self.drained.wait_timeout(park, left).unwrap();
            park = guard;
        }
    }
}

pub(crate) const OBSERVER_BUDGET: i64 = 8 * 1024 * 1024;

/// How long the death path will chase a child's exit status before giving up
/// and reporting `Exited { code: None }`. The pty hits EOF when the last slave
/// fd closes, which can land a few milliseconds ahead of the child becoming
/// reapable — that race is all this window exists to cover. Everything past it
/// is a pane that looks frozen to every attached client, so it stays short.
const EXIT_CODE_PROBE_WINDOW: Duration = Duration::from_millis(500);

const EXIT_CODE_PROBE_INTERVAL: Duration = Duration::from_millis(10);

struct Observer {
    id: u64,
    tx: Sender<DaemonMsg>,
    gate: Arc<OutputGate>,
}

struct PaneState {
    id: u64,
    ring: ReplayRing,
    subscriber: Option<Sender<DaemonMsg>>,
    subscriber_epoch: u64,
    observers: Vec<Observer>,
    observer_seq: u64,
    cwd: Option<PathBuf>,
    shell: ShellState,
    remote: Option<RemoteContext>,
    agent: Option<crate::core::cli_agent::CLIAgent>,
    agent_argv: Option<Vec<String>>,
    agent_session: Option<crate::core::cli_agent::AgentSessionState>,
    alive: bool,
    exit_code: Option<i32>,
}

fn notify(st: &mut PaneState, msg: DaemonMsg) {
    if let Some(sub) = &st.subscriber {
        let _ = sub.send(msg.clone());
    }
    // Status traffic is charged the same budget as output. These messages are
    // small, but an agent pane emits AgentStatus often enough that an observer
    // which stopped draining would still queue without bound. Being over the
    // line already disqualifies it; the message is not sized individually.
    st.observers.retain(|obs| {
        obs.gate.queued_bytes() < OBSERVER_BUDGET && obs.tx.send(msg.clone()).is_ok()
    });
}

/// One gated message to the controller and to every observer.
///
/// A send error just means that client is gone; ignore it and let the next
/// attach install a new sender. Successful sends are counted against the
/// pane gate, and the connection's writer thread credits them back. Observers
/// each carry their own gate and their own budget: one that has stopped
/// draining is dropped rather than allowed to hold the pane's output forever.
fn fan_out_one(st: &mut PaneState, msg: DaemonMsg, len: usize, gate: &OutputGate) {
    if let Some(sub) = &st.subscriber {
        if sub.send(msg.clone()).is_ok() {
            gate.add(len);
        }
    }
    st.observers.retain(|obs| {
        if obs.gate.queued_bytes() + len as i64 > OBSERVER_BUDGET {
            return false;
        }
        if obs.tx.send(msg.clone()).is_err() {
            return false;
        }
        obs.gate.add(len);
        true
    });
}

/// Fan one PTY read out to the controller and every observer.
///
/// On the no-graphics fast path `frames` is empty and the whole passthrough
/// goes as a single `Output`. When a chunk carried graphics the frames are
/// forwarded in stream order instead, so an image lands at the same cursor cell
/// the sender drew it at. Each `Output` is gated on its own length; an image
/// frame is gated too (it rode the same PTY read); a delete is tiny and
/// ungated, matching the drain accounting in `server.rs`.
fn fan_out_output(st: &mut PaneState, bytes: &[u8], frames: Vec<GraphicsFrame>, gate: &OutputGate) {
    if frames.is_empty() {
        if !bytes.is_empty() {
            fan_out_one(st, DaemonMsg::Output(bytes.to_vec()), bytes.len(), gate);
        }
        return;
    }
    for frame in frames {
        match frame {
            GraphicsFrame::Output(b) => {
                if !b.is_empty() {
                    let len = b.len();
                    fan_out_one(st, DaemonMsg::Output(b), len, gate);
                }
            }
            GraphicsFrame::Image(frame) => {
                let len = frame.len();
                fan_out_one(st, DaemonMsg::Image(frame), len, gate);
            }
            // Ungated, and `notify` already holds observers to their budget.
            GraphicsFrame::Delete(sel) => notify(st, DaemonMsg::DeleteImage(sel)),
        }
    }
}

enum PaneBackend {
    Pty(PtyBackend),
    NativeSsh(NativeSshBackend),
}

struct ForegroundProbes {
    remote: Box<dyn Fn() -> Option<RemoteContext> + Send>,
    agent: Box<dyn Fn() -> Option<Option<(crate::core::cli_agent::CLIAgent, Vec<String>)>> + Send>,
    cwd: Box<dyn Fn() -> Option<PathBuf> + Send>,
}

struct PtyBackend {
    /// Owns the PTY master until the pane is closed.
    ///
    /// Windows takes this value when the shell process exits. Dropping the
    /// ConPTY master calls `ClosePseudoConsole`, which closes the output side
    /// only after its pending bytes can be consumed by the dedicated reader.
    /// Keeping the slot optional lets that monitor end the stream without
    /// racing the reader for ownership of the exit notification.
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    #[cfg_attr(windows, allow(dead_code))]
    shell_pid: Option<u32>,
    integration_dir: Option<PathBuf>,
}

struct NativeSshBackend {
    handle: Arc<crate::daemon::ssh::SshSessionHandle>,
    connection: crate::daemon::ssh::SharedConnection,
}

pub struct DaemonPane {
    pub id: u64,
    owner: Option<String>,
    backend: PaneBackend,
    /// The input side (keyboard input / pasted text): the PTY writer, or the
    /// native-SSH channel writer. Behind a `Mutex` because writes can arrive from
    /// different connection threads. `Arc`-shared so the reader thread can write
    /// kitty-graphics query replies (`\x1b_G…;OK\x1b\\`) back to the PTY inline
    /// with the sniff, without routing through `write_input`.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Set during teardown so the reader doesn't emit a spurious exit.
    shutting_down: Arc<AtomicBool>,
    gate: Arc<OutputGate>,
    state: Arc<Mutex<PaneState>>,
    reader: Mutex<Option<JoinHandle<()>>>,
    broker: Option<Arc<crate::daemon::ssh::PromptBroker>>,
}

struct DeathReporter {
    reported: AtomicBool,
    on_dead: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    exit_code: Mutex<Option<Box<dyn FnMut() -> Option<i32> + Send>>>,
}

impl DeathReporter {
    fn new(on_dead: impl FnOnce() + Send + 'static) -> Self {
        Self {
            reported: AtomicBool::new(false),
            on_dead: Mutex::new(Some(Box::new(on_dead))),
            exit_code: Mutex::new(None),
        }
    }

    fn probe_exit_code(&self, probe: impl FnMut() -> Option<i32> + Send + 'static) {
        *self.exit_code.lock().unwrap() = Some(Box::new(probe));
    }

    #[cfg(windows)]
    fn has_reported(&self) -> bool {
        self.reported.load(Ordering::SeqCst)
    }

    fn report(&self, state: &Mutex<PaneState>, shutting_down: &AtomicBool) {
        if self.reported.swap(true, Ordering::SeqCst) {
            return;
        }
        let code = self
            .exit_code
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|probe| probe());
        let mut st = state.lock().unwrap();
        st.alive = false;
        st.exit_code = code;
        let pane = st.id;
        if shutting_down.load(Ordering::SeqCst) {
            drop(st);
            crate::core::machine::observe_pane(pane, |p| p.live = false);
            return;
        }
        let subscribed = st.subscriber.is_some();
        notify(&mut st, DaemonMsg::Exited { code });
        drop(st);
        crate::core::machine::observe_pane(pane, |p| p.live = false);
        if subscribed {
            return;
        }
        if let Some(on_dead) = self.on_dead.lock().unwrap().take() {
            on_dead();
        }
    }
}

/// Releases the PTY master without holding its slot mutex during destruction.
///
/// `ClosePseudoConsole` may wait while the output pipe is drained, and the same
/// slot is what `resize` locks. Dropping the master only after releasing the
/// mutex keeps a concurrent resize from parking behind a close that is itself
/// waiting on the reader.
#[cfg(windows)]
fn close_pty_master(master: &Mutex<Option<Box<dyn MasterPty + Send>>>) {
    let owned = master.lock().ok().and_then(|mut slot| slot.take());
    drop(owned);
}

/// How long the Windows exit monitor lets the reader announce the death on its
/// own before doing it itself.
///
/// The reader is the better reporter: it publishes `Exited` only after it has
/// forwarded every byte that preceded EOF, which is what keeps a short-lived
/// command's final frame ahead of its exit. But EOF is not guaranteed. A
/// grandchild that inherited the ConPTY output pipe holds it open after the
/// shell is gone — `cmd /c start …` is enough — and `ClosePseudoConsole` then
/// never completes. Without this window such a pane would read as alive
/// forever to every attached client.
#[cfg(windows)]
const EXIT_DRAIN_WINDOW: Duration = Duration::from_secs(2);

#[cfg(windows)]
const EXIT_DRAIN_POLL: Duration = Duration::from_millis(10);

/// Releases the pseudoconsole, then reports the pane's death — preferring the
/// reader's EOF-ordered report and falling back to its own after `window`.
///
/// Must run on a background thread: both halves block.
#[cfg(windows)]
fn drain_then_report(
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    state: Arc<Mutex<PaneState>>,
    shutting_down: Arc<AtomicBool>,
    death: Arc<DeathReporter>,
    window: Duration,
) {
    // `ClosePseudoConsole` can block until the reader drains the pipe, so it
    // cannot run on the thread that owns the deadline below.
    std::thread::Builder::new()
        .name("tty7-daemon-pane-pty-close".to_string())
        .spawn(move || close_pty_master(&master))
        .expect("spawn daemon pane pty close thread");

    let deadline = std::time::Instant::now() + window;
    while !death.has_reported() && std::time::Instant::now() < deadline {
        std::thread::sleep(EXIT_DRAIN_POLL);
    }
    // `DeathReporter` is idempotent, so this is a no-op whenever the reader
    // already got there — which is the ordinary case.
    death.report(&state, &shutting_down);
}

/// One out-of-band frame the reader forwards to the subscriber, kept in stream
/// order so a kitty image lands at the cursor cell the sender drew it at: a chunk
/// carrying graphics splits into `Output` runs interleaved with `Image`/`Delete`
/// frames, and they must reach the client in that same order. `Output` becomes a
/// `DaemonMsg::Output`, `Image` an encoded image frame, `Delete` a compact
/// selector — see the reader loop's `sniff` handling.
enum GraphicsFrame {
    Output(Vec<u8>),
    Image(Vec<u8>),
    Delete(Vec<u8>),
}

/// Queue an encoded image frame for the subscriber, dropping any frame larger
/// than [`MAX_FRAME`]. The writer's `write_frame` rejects an oversize payload
/// with an error the writer loop treats as *fatal* — it would tear the client
/// off an otherwise-healthy pane. A single image that won't fit is not worth
/// that: drop it here so the rest of the stream keeps flowing. The inline
/// `t=d` path is bounded by `MAX_TRANSMISSION_BASE64` but that still admits
/// ~72 MiB of raw pixels, and the local shm/file path has no cap of its own,
/// so both can land here over the limit.
fn push_image_frame(frames: &mut Vec<GraphicsFrame>, frame: Vec<u8>) {
    if frame.len() <= MAX_FRAME {
        frames.push(GraphicsFrame::Image(frame));
    }
}

impl DaemonPane {
    pub fn spawn(
        id: u64,
        cwd: Option<PathBuf>,
        size: WinSize,
        shell: Option<ShellSpec>,
        owner: Option<String>,
        workspace: Option<String>,
        on_dead: impl FnOnce() + Send + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        let pty_size = pty_size(size);

        let pair = native_pty_system().openpty(pty_size)?;
        let spawn = build_spawn_config(id, cwd, shell, workspace.as_deref())?;

        let child = pair.slave.spawn_command(spawn.cmd)?;
        let shell_pid = child.process_id();
        let child = Arc::new(Mutex::new(child));

        drop(pair.slave);

        let reader_handle = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));

        let state = Arc::new(Mutex::new(PaneState {
            id,
            ring: ReplayRing::new(size),
            subscriber: None,
            subscriber_epoch: 0,
            observers: Vec::new(),
            observer_seq: 0,
            cwd: spawn.initial_cwd,
            shell: ShellState::default(),
            remote: spawn.remote.clone(),
            agent: None,
            agent_session: None,
            agent_argv: None,
            alive: true,
            exit_code: None,
        }));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(OutputGate::new());

        let master = Arc::new(Mutex::new(Some(pair.master)));

        let pane = Arc::new(Self {
            id,
            owner,
            backend: PaneBackend::Pty(PtyBackend {
                master: master.clone(),
                child: child.clone(),
                shell_pid,
                integration_dir: spawn.integration_dir,
            }),
            writer: writer.clone(),
            shutting_down: shutting_down.clone(),
            gate: gate.clone(),
            state: state.clone(),
            reader: Mutex::new(None),
            broker: None,
        });

        let death = Arc::new(DeathReporter::new(on_dead));
        death.probe_exit_code({
            let child = child.clone();
            move || {
                let deadline = std::time::Instant::now() + EXIT_CODE_PROBE_WINDOW;
                loop {
                    // try_lock, never lock: DaemonPane::drop holds this very
                    // mutex across a blocking child.wait(), and this probe runs
                    // on the reader thread on its way to announcing the death.
                    // Blocking for the lock would park the Exited notification
                    // behind a wait() with no bound of its own; the deadline
                    // below is the only thing clients should ever wait on.
                    if let Ok(mut child) = child.try_lock() {
                        match child.try_wait() {
                            Ok(Some(status)) => return Some(status.exit_code() as i32),
                            Ok(None) => {}
                            Err(_) => return None,
                        }
                    }
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(EXIT_CODE_PROBE_INTERVAL);
                }
            }
        });

        #[cfg(windows)]
        Self::spawn_exit_monitor(
            shell_pid,
            master.clone(),
            state.clone(),
            pane.shutting_down.clone(),
            death.clone(),
        );

        let fg_master = master.clone();
        let remote_master = master.clone();
        let agent_master = master.clone();
        let cwd_master = master.clone();
        let reader = Self::spawn_reader(
            state,
            shutting_down,
            gate,
            reader_handle,
            writer.clone(),
            move || foreground_command_running(&fg_master, shell_pid),
            ForegroundProbes {
                remote: Box::new(move || foreground_remote_context(&remote_master)),
                agent: Box::new(move || foreground_agent(&agent_master)),
                cwd: Box::new(move || foreground_cwd(&cwd_master, shell_pid)),
            },
            death,
        );
        *pane.reader.lock().unwrap() = Some(reader);

        Ok(pane)
    }

    pub fn spawn_native_ssh(
        id: u64,
        size: WinSize,
        spec: Box<NativeSshSpec>,
        on_dead: impl FnOnce() + Send + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        let bridge = crate::daemon::ssh::session::make_bridge();
        let reader_handle: Box<dyn Read + Send> = Box::new(bridge.reader);
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(Box::new(bridge.writer)));
        // The connect task fills this once authenticated; the pane exposes it to
        // WS4/WS5 via `ssh_connection()`.
        let connection: crate::daemon::ssh::SharedConnection = Arc::new(Mutex::new(Weak::new()));

        let target = spec
            .display_name
            .clone()
            .unwrap_or_else(|| format!("{}@{}", spec.user, spec.host));
        let remote = RemoteContext {
            kind: RemoteKind::NativeSsh,
            argv: Vec::new(),
            target,
        };

        let state = Arc::new(Mutex::new(PaneState {
            id,
            ring: ReplayRing::new(size),
            subscriber: None,
            subscriber_epoch: 0,
            observers: Vec::new(),
            observer_seq: 0,
            cwd: None,
            shell: ShellState::default(),
            remote: Some(remote),
            agent: None,
            agent_session: None,
            agent_argv: None,
            alive: true,
            exit_code: None,
        }));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(OutputGate::new());

        let broker = {
            let state = state.clone();
            crate::daemon::ssh::PromptBroker::new(Box::new(move |msg: DaemonMsg| {
                match &state.lock().unwrap().subscriber {
                    Some(sub) => sub.send(msg).is_ok(),
                    None => false,
                }
            }))
        };

        let pane = Arc::new(Self {
            id,
            owner: None,
            backend: PaneBackend::NativeSsh(NativeSshBackend {
                handle: bridge.handle,
                connection: connection.clone(),
            }),
            writer: writer.clone(),
            shutting_down: shutting_down.clone(),
            gate: gate.clone(),
            state: state.clone(),
            reader: Mutex::new(None),
            broker: Some(broker.clone()),
        });

        let death = Arc::new(DeathReporter::new(on_dead));

        let reader = Self::spawn_reader(
            state,
            shutting_down,
            gate,
            reader_handle,
            writer.clone(),
            || false,
            ForegroundProbes {
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            death,
        );
        *pane.reader.lock().unwrap() = Some(reader);

        crate::daemon::ssh::SshManager::global().spawn_native_session(
            id,
            spec,
            size,
            broker,
            bridge.data_tx,
            bridge.cmd_rx,
            connection,
        );

        Ok(pane)
    }

    pub fn deliver_auth_response(&self, request_id: u64, response: AuthResponse) {
        if let Some(broker) = &self.broker {
            broker.deliver(request_id, response);
        }
    }

    #[allow(dead_code)]
    pub fn ssh_connection(&self) -> Option<Arc<crate::daemon::ssh::SshConnection>> {
        match &self.backend {
            PaneBackend::NativeSsh(b) => b.connection.lock().unwrap().upgrade(),
            PaneBackend::Pty(_) => None,
        }
    }

    fn spawn_reader(
        state: Arc<Mutex<PaneState>>,
        shutting_down: Arc<AtomicBool>,
        gate: Arc<OutputGate>,
        mut reader: Box<dyn Read + Send>,
        // The pane's input side, shared so the reader can write kitty-graphics
        // query replies (`\x1b_G…;OK\x1b\\`) straight back to the PTY — see the
        // graphics sniff in the loop below.
        writer: Arc<Mutex<Box<dyn Write + Send>>>,
        foreground_running: impl Fn() -> bool + Send + 'static,
        probes: ForegroundProbes,
        death: Arc<DeathReporter>,
    ) -> JoinHandle<()> {
        let ForegroundProbes {
            remote: foreground_remote,
            agent: foreground_agent_fn,
            cwd: foreground_cwd_fn,
        } = probes;
        std::thread::Builder::new()
            .name("tty7-daemon-pane-reader".to_string())
            .spawn(move || {
                crate::core::threads::promote_to_user_interactive();
                let mut sniffer = OscSniffer::new();
                // Kitty graphics interception (issue #213): lifts image
                // sequences out of the stream *before* the ring/subscriber see
                // them, so the base64 pixels never enter replay and the client's
                // VT parser never chews through them. Zero-copy on the common
                // no-graphics chunk — see [`GraphicsSniffer::sniff`].
                //
                // File/shm transfer (`t=s`/`t=f`/`t=t`) is honored only while the
                // pane is *local*: the object/path names resolve on this host, and
                // reading them can't leak across an SSH tunnel. A pane that starts
                // local can `ssh` out mid-session, so the flag is refreshed from
                // the same remote-context poll below. Seed it from the pane's
                // current context so the first probe answers correctly.
                let starts_local = state.lock().unwrap().remote.is_none();
                let mut graphics = GraphicsSniffer::new_local(starts_local);
                let mut buf = [0u8; 65536];

                let trace = std::env::var("TTY7_TRACE").is_ok_and(|v| !v.is_empty() && v != "0");
                let mut tr_last = std::time::Instant::now();
                let mut tr_bytes: u64 = 0;
                let mut tr_reads: u32 = 0;
                let mut tr_read_t = std::time::Duration::ZERO;
                let mut tr_disp_t = std::time::Duration::ZERO;
                let mut next_remote_check = std::time::Instant::now();

                loop {
                    if trace && tr_last.elapsed() >= std::time::Duration::from_secs(1) {
                        // Not `eprintln!`: a daemon can be running without any
                        // standard error at all (see `daemon::server`), and a
                        // failed write there would panic the reader thread.
                        let _ = writeln!(
                            std::io::stderr(),
                            "[trace daemon] {:.1} MB/s | {} reads ({} B/read) | pty wait {:?} dispatch {:?}",
                            tr_bytes as f64 / tr_last.elapsed().as_secs_f64() / 1e6,
                            tr_reads,
                            if tr_reads > 0 { tr_bytes / tr_reads as u64 } else { 0 },
                            tr_read_t,
                            tr_disp_t,
                        );
                        tr_last = std::time::Instant::now();
                        tr_bytes = 0;
                        tr_reads = 0;
                        tr_read_t = std::time::Duration::ZERO;
                        tr_disp_t = std::time::Duration::ZERO;
                    }
                    gate.wait_below_high_water();
                    let tr0 = trace.then(std::time::Instant::now);
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Some(tr0) = tr0 {
                                tr_read_t += tr0.elapsed();
                                tr_reads += 1;
                                tr_bytes += n as u64;
                            }
                            let raw = &buf[..n];
                            // Kitty graphics: strip any image sequences out of the
                            // stream before anything else sees them. On the common
                            // no-graphics chunk this borrows `raw` unchanged; only a
                            // chunk that actually carries `\x1b_G…` allocates. Query
                            // replies are written straight back to the PTY here, and
                            // any images/deletes are forwarded to the subscriber
                            // out-of-band below — none of it enters the replay ring.
                            //
                            // `frames` is the ordered list of out-of-band frames to
                            // forward *in stream position*: a kitty image anchors to
                            // the cursor cell as it stood when its command appeared,
                            // so the client must apply the text before an image, then
                            // the image, then the text after it, in that order. On
                            // the no-graphics fast path `frames` stays empty and the
                            // whole chunk is sent as one `Output`; only a chunk with
                            // graphics splits into interleaved frames.
                            let mut frames: Vec<GraphicsFrame> = Vec::new();
                            let passthrough: std::borrow::Cow<[u8]> = match graphics.sniff(raw) {
                                Sniffed::Plain(b) => std::borrow::Cow::Borrowed(b),
                                Sniffed::Segments(segs) => {
                                    let mut pass = Vec::new();
                                    for seg in segs {
                                        match seg {
                                            Segment::Output(b) => {
                                                pass.extend_from_slice(&b);
                                                frames.push(GraphicsFrame::Output(b));
                                            }
                                            Segment::Query(reply) => {
                                                if let Ok(mut w) = writer.lock() {
                                                    let _ = w.write_all(&reply);
                                                    let _ = w.flush();
                                                }
                                            }
                                            Segment::Image(img) => {
                                                push_image_frame(&mut frames, img.encode_frame());
                                            }
                                            Segment::ImageFromMedium(transfer) => {
                                                // File/shm handoff on a local pane:
                                                // read (and unlink) the object here,
                                                // then forward the raw pixels exactly
                                                // like an inline image. This is the
                                                // fast path that skips the client-side
                                                // inflate the compressed-inline `t=d`
                                                // fallback would force. A failed read
                                                // just drops the frame — the sender
                                                // reclaims its own object.
                                                if let Some(img) = transfer.resolve() {
                                                    push_image_frame(
                                                        &mut frames,
                                                        img.encode_frame(),
                                                    );
                                                }
                                            }
                                            Segment::Delete(d) => {
                                                frames.push(GraphicsFrame::Delete(d.encode()));
                                            }
                                        }
                                    }
                                    std::borrow::Cow::Owned(pass)
                                }
                            };
                            let bytes: &[u8] = &passthrough;
                            // Sniff first (cheap, over the same bytes); collect any
                            // cwd/prompt change to emit while we hold the lock.
                            let mut signals = sniffer.feed(bytes);

                            if signals.shell.iter().any(|s| s.at_prompt) && foreground_running() {
                                for s in signals.shell.iter_mut() {
                                    s.at_prompt = false;
                                }
                                signals.shell.dedup();
                            }

                            let poll_now = std::time::Instant::now() >= next_remote_check;
                            if poll_now {
                                next_remote_check =
                                    std::time::Instant::now() + REMOTE_CONTEXT_POLL_INTERVAL;
                            }
                            let remote = if poll_now {
                                let managed = {
                                    let st = state.lock().unwrap();
                                    st.remote
                                        .as_ref()
                                        .is_some_and(|remote| remote.kind != RemoteKind::Ssh)
                                };
                                (!managed).then(&foreground_remote)
                            } else {
                                None
                            };
                            let agent = poll_now.then(&foreground_agent_fn).flatten();
                            let probed_cwd = poll_now.then(&foreground_cwd_fn).flatten();

                            let tr1 = trace.then(std::time::Instant::now);
                            let may_change_facts = signals.cwd.is_some()
                                || !signals.agent_events.is_empty()
                                || signals.notification.is_some()
                                || !signals.shell.is_empty()
                                || remote.is_some()
                                || agent.is_some()
                                || probed_cwd.is_some();
                            let mut st = state.lock().unwrap();
                            let facts_before = may_change_facts.then(|| observed_facts(&st));
                            st.ring.append(bytes);
                            fan_out_output(&mut st, bytes, frames, &gate);
                            apply_signals(&mut st, signals);
                            if let Some(remote) = remote {
                                apply_remote_context(&mut st, remote);
                            }
                            // Keep kitty file/shm transfer gated on the pane's
                            // *current* locality: an `ssh` that just took the PTY
                            // must stop us honoring host-local object names. Cheap
                            // and only meaningful when a probe follows.
                            graphics.set_local(st.remote.is_none());
                            if let Some(agent) = agent {
                                apply_agent(&mut st, agent);
                            }
                            apply_probed_cwd(&mut st, probed_cwd);
                            if let Some(tr1) = tr1 {
                                tr_disp_t += tr1.elapsed();
                            }
                            let pane = st.id;
                            let alive = st.alive;
                            let facts_after = may_change_facts.then(|| observed_facts(&st));
                            drop(st);
                            if !shutting_down.load(Ordering::SeqCst)
                                && let (Some(before), Some(after)) = (facts_before, facts_after)
                                && facts_changed(&before, &after)
                            {
                                let (cwd, agent) = after;
                                crate::core::machine::observe_pane(pane, |p| {
                                    if cwd.is_some() {
                                        p.cwd = cwd;
                                    }
                                    p.agent = agent;
                                    if alive {
                                        p.live = true;
                                    }
                                });
                            }
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }

                death.report(&state, &shutting_down);
            })
            .expect("spawn daemon pane reader thread")
    }

    pub fn attach(&self, subscriber: Sender<DaemonMsg>) -> u64 {
        let mut st = self.state.lock().unwrap();
        let epoch = attach_subscriber(&mut st, subscriber);
        self.gate.reset();
        epoch
    }

    pub fn detach(&self, epoch: u64) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.subscriber_epoch == epoch {
            st.subscriber = None;
            self.gate.reset();
        }
        !st.alive && st.subscriber.is_none()
    }

    pub fn observe(&self, observer: Sender<DaemonMsg>, gate: Arc<OutputGate>) -> u64 {
        let mut st = self.state.lock().unwrap();
        observe_subscriber(&mut st, observer, gate)
    }

    pub fn unobserve(&self, observer_id: u64) {
        let mut st = self.state.lock().unwrap();
        st.observers.retain(|obs| obs.id != observer_id);
    }

    pub fn controls(&self, epoch: u64) -> bool {
        self.state.lock().unwrap().subscriber_epoch == epoch
    }

    pub fn agent_state(&self) -> Option<crate::daemon::control::PaneAgentState> {
        agent_state_snapshot(&self.state.lock().unwrap())
    }

    pub fn gate(&self) -> Arc<OutputGate> {
        self.gate.clone()
    }

    pub fn write_input(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    pub fn procs(&self) -> crate::daemon::protocol::PaneProcs {
        let Some(pty) = self.pty() else {
            return Default::default();
        };
        let Some(shell_pid) = pty.shell_pid else {
            return Default::default();
        };
        crate::daemon::procinfo::snapshot(shell_pid, pty_foreground_pgid(&pty.master))
    }

    fn pty(&self) -> Option<&PtyBackend> {
        match &self.backend {
            PaneBackend::Pty(p) => Some(p),
            PaneBackend::NativeSsh(_) => None,
        }
    }

    pub fn resize(&self, size: WinSize) {
        {
            let mut st = self.state.lock().unwrap();
            st.ring.resize(size);
            st.observers
                .retain(|obs| obs.tx.send(DaemonMsg::Size(size)).is_ok());
        }
        match &self.backend {
            PaneBackend::Pty(p) => {
                if let Ok(master) = p.master.lock() {
                    if let Some(master) = master.as_ref() {
                        let _ = master.resize(pty_size(size));
                    }
                }
            }
            PaneBackend::NativeSsh(b) => b.handle.resize(size),
        }
    }

    pub fn alive(&self) -> bool {
        self.state.lock().unwrap().alive
    }

    pub fn info(&self) -> PaneInfo {
        let (cwd, alive) = {
            let st = self.state.lock().unwrap();
            (st.cwd.clone(), st.alive)
        };
        PaneInfo {
            pane_id: self.id,
            cwd: cwd.or_else(|| self.foreground_cwd()),
            title: self.foreground_title(),
            alive,
            owner: self.owner.clone(),
        }
    }

    pub(crate) fn remote_context(&self) -> Option<RemoteContext> {
        let cached = self.state.lock().unwrap().remote.clone();
        cached.or_else(|| self.foreground_remote_context())
    }

    pub fn kill(&self) {
        self.hangup();
    }

    fn hangup(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        match &self.backend {
            PaneBackend::Pty(p) => {
                #[cfg(unix)]
                Self::signal_group(p, libc::SIGHUP);
                #[cfg(windows)]
                Self::kill_descendants(p);
                if let Ok(mut child) = p.child.lock() {
                    let _ = child.kill();
                }
                #[cfg(unix)]
                Self::signal_group(p, libc::SIGKILL);
            }
            PaneBackend::NativeSsh(b) => b.handle.close(),
        }
    }

    #[cfg(windows)]
    fn kill_descendants(pty: &PtyBackend) {
        if let Some(pid) = pty.shell_pid {
            let procs = crate::daemon::winproc::snapshot();
            for target in crate::daemon::winproc::descendants(&procs, pid) {
                crate::daemon::winproc::terminate(target);
            }
        }
    }

    #[cfg(windows)]
    fn spawn_exit_monitor(
        shell_pid: Option<u32>,
        master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
        state: Arc<Mutex<PaneState>>,
        shutting_down: Arc<AtomicBool>,
        death: Arc<DeathReporter>,
    ) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };

        let Some(pid) = shell_pid else { return };
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            // The process may have exited before OpenProcess ran, so go
            // straight to the close-and-report path — off this thread, since
            // pane construction is what calls us.
            std::thread::Builder::new()
                .name("tty7-daemon-pane-exit-fallback".to_string())
                .spawn(move || {
                    drain_then_report(master, state, shutting_down, death, EXIT_DRAIN_WINDOW);
                })
                .expect("spawn daemon pane exit fallback thread");
            return;
        }
        let handle = handle as isize;
        std::thread::Builder::new()
            .name("tty7-daemon-pane-exit-monitor".to_string())
            .spawn(move || {
                let handle = handle as windows_sys::Win32::Foundation::HANDLE;
                unsafe {
                    WaitForSingleObject(handle, INFINITE);
                    CloseHandle(handle);
                }
                drain_then_report(master, state, shutting_down, death, EXIT_DRAIN_WINDOW);
            })
            .expect("spawn daemon pane exit monitor thread");
    }

    #[cfg(unix)]
    fn signal_group(pty: &PtyBackend, sig: libc::c_int) {
        if let Some(pid) = pty.shell_pid {
            unsafe {
                libc::killpg(pid as libc::pid_t, sig);
            }
        }
        let fg = pty
            .master
            .lock()
            .ok()
            .and_then(|m| m.as_ref().and_then(|m| m.process_group_leader()));
        if let Some(fg) = fg {
            if Some(fg as u32) != pty.shell_pid {
                unsafe {
                    libc::killpg(fg, sig);
                }
            }
        }
    }

    fn foreground_cwd(&self) -> Option<PathBuf> {
        let pty = self.pty()?;
        foreground_cwd(&pty.master, pty.shell_pid)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn foreground_title(&self) -> String {
        let Some(pty) = self.pty() else {
            return String::new();
        };
        pty.master
            .lock()
            .ok()
            .and_then(|m| m.as_ref().and_then(|m| m.process_group_leader()))
            .and_then(proc_name)
            .unwrap_or_default()
    }

    #[cfg(windows)]
    fn foreground_title(&self) -> String {
        let Some(pty) = self.pty() else {
            return String::new();
        };
        let Some(pid) = pty.shell_pid else {
            return String::new();
        };
        let procs = crate::daemon::winproc::snapshot();
        crate::daemon::winproc::foreground_name(&procs, pid).unwrap_or_default()
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    fn foreground_title(&self) -> String {
        String::new()
    }

    fn foreground_remote_context(&self) -> Option<RemoteContext> {
        match &self.backend {
            PaneBackend::Pty(p) => foreground_remote_context(&p.master),
            PaneBackend::NativeSsh(_) => None,
        }
    }
}

impl Drop for DaemonPane {
    fn drop(&mut self) {
        if matches!(self.backend, PaneBackend::NativeSsh(_)) {
            crate::daemon::ssh::SshManager::global().teardown_pane_forwards(self.id);
        }
        self.hangup();
        if let PaneBackend::Pty(p) = &self.backend {
            if let Ok(mut child) = p.child.lock() {
                let _ = child.wait();
            }
        }
        if let Some(handle) = self.reader.lock().unwrap().take() {
            join_bounded(handle, Duration::from_secs(2));
        }
        if let PaneBackend::Pty(p) = &mut self.backend {
            if let Some(dir) = p.integration_dir.take() {
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
}

fn join_bounded(handle: JoinHandle<()>, timeout: Duration) -> bool {
    let (tx, rx) = mpsc::channel();
    if std::thread::Builder::new()
        .name("tty7-daemon-pane-join".to_string())
        .spawn(move || {
            let _ = handle.join();
            let _ = tx.send(());
        })
        .is_err()
    {
        return false;
    }
    rx.recv_timeout(timeout).is_ok()
}

fn pty_size(size: WinSize) -> PtySize {
    PtySize {
        rows: size.rows.max(1),
        cols: size.cols.max(1),
        pixel_width: size.cols.saturating_mul(size.cell_w),
        pixel_height: size.rows.saturating_mul(size.cell_h),
    }
}

struct ReplayRing {
    segments: VecDeque<RingSegment>,
    len: usize,
}

struct RingSegment {
    size: WinSize,
    bytes: VecDeque<u8>,
}

impl RingSegment {
    fn empty(size: WinSize) -> Self {
        Self {
            size,
            bytes: VecDeque::new(),
        }
    }

    fn to_vec(&self) -> Vec<u8> {
        let (a, b) = self.bytes.as_slices();
        let mut out = Vec::with_capacity(self.bytes.len());
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        out
    }
}

impl ReplayRing {
    fn new(size: WinSize) -> Self {
        Self {
            segments: VecDeque::from([RingSegment::empty(size)]),
            len: 0,
        }
    }

    fn tail(&mut self) -> &mut RingSegment {
        self.segments.back_mut().expect("ring always has a tail")
    }

    fn resize(&mut self, size: WinSize) {
        let tail = self.tail();
        if tail.size == size {
            return;
        }
        if tail.bytes.is_empty() {
            tail.size = size;
            return;
        }
        if self.segments.len() >= MAX_RING_SEGMENTS {
            let old = self.segments.pop_front().expect("len >= cap");
            let head = self.segments.front_mut().expect("cap >= 2");
            let mut merged = old.bytes;
            merged.extend(head.bytes.drain(..));
            head.bytes = merged;
        }
        self.segments.push_back(RingSegment::empty(size));
    }

    fn append(&mut self, bytes: &[u8]) {
        if bytes.len() >= RING_CAP {
            let size = self.tail().size;
            self.segments.clear();
            let mut tail = RingSegment::empty(size);
            tail.bytes.extend(&bytes[bytes.len() - RING_CAP..]);
            self.segments.push_back(tail);
            self.len = RING_CAP;
            return;
        }
        self.tail().bytes.extend(bytes);
        self.len += bytes.len();
        let mut overflow = self.len.saturating_sub(RING_CAP);
        while overflow > 0 {
            let head = self
                .segments
                .front_mut()
                .expect("len > 0 implies a segment");
            let drop = overflow.min(head.bytes.len());
            head.bytes.drain(..drop);
            self.len -= drop;
            overflow -= drop;
            if head.bytes.is_empty() && self.segments.len() > 1 {
                self.segments.pop_front();
            }
        }
    }

    fn replay(&self, subscriber: &Sender<DaemonMsg>) {
        for seg in &self.segments {
            let _ = subscriber.send(DaemonMsg::Size(seg.size));
            let _ = subscriber.send(DaemonMsg::Snapshot(seg.to_vec()));
        }
    }

    #[cfg(test)]
    fn flatten(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len);
        for seg in &self.segments {
            out.extend(seg.to_vec());
        }
        out
    }
}

fn replay_state(st: &PaneState, subscriber: &Sender<DaemonMsg>) {
    st.ring.replay(subscriber);
    if let Some(cwd) = &st.cwd {
        let _ = subscriber.send(DaemonMsg::Cwd(cwd.clone()));
    }
    if st.shell.active {
        let _ = subscriber.send(DaemonMsg::Prompt {
            active: st.shell.active,
            at_prompt: st.shell.at_prompt,
            last_exit: st.shell.last_exit_code,
        });
    }
    if st.remote.is_some() {
        let _ = subscriber.send(DaemonMsg::RemoteContext(st.remote.clone()));
    }
    if st.agent.is_some() {
        let _ = subscriber.send(DaemonMsg::Agent(st.agent));
    }
    if st.agent_session.is_some() {
        let _ = subscriber.send(DaemonMsg::AgentStatus(st.agent_session.clone()));
    }
    if !st.alive {
        let _ = subscriber.send(DaemonMsg::Exited { code: st.exit_code });
    }
}

fn attach_subscriber(st: &mut PaneState, subscriber: Sender<DaemonMsg>) -> u64 {
    st.subscriber_epoch += 1;
    replay_state(st, &subscriber);
    st.subscriber = Some(subscriber);
    st.subscriber_epoch
}

fn observe_subscriber(
    st: &mut PaneState,
    observer: Sender<DaemonMsg>,
    gate: Arc<OutputGate>,
) -> u64 {
    st.observer_seq += 1;
    replay_state(st, &observer);
    // The replay just queued the whole ring into this channel. Charge it, or
    // the first budget check would read zero while a full scrollback is already
    // sitting there unread — an observer that never drains would be allowed a
    // ring plus a full budget before anyone noticed.
    gate.add(st.ring.len);
    st.observers.push(Observer {
        id: st.observer_seq,
        tx: observer,
        gate,
    });
    st.observer_seq
}

fn agent_state_snapshot(st: &PaneState) -> Option<crate::daemon::control::PaneAgentState> {
    st.agent_session
        .clone()
        .map(|state| crate::daemon::control::PaneAgentState {
            pane_id: st.id,
            agent: st.agent,
            state,
        })
}

fn observed_facts(st: &PaneState) -> (Option<String>, Option<crate::core::machine::AgentFacts>) {
    let cwd = st.cwd.as_ref().map(|p| p.to_string_lossy().into_owned());
    let agent = st.agent.map(|agent| crate::core::machine::AgentFacts {
        agent,
        session_id: st.agent_session.as_ref().and_then(|s| s.session_id.clone()),
        launch_argv: st
            .agent_session
            .as_ref()
            .and_then(|s| s.launch_argv.clone())
            .or_else(|| st.agent_argv.clone()),
        status: st.agent_session.as_ref().map(|s| s.status),
    });
    (cwd, agent)
}

fn facts_changed(
    before: &(Option<String>, Option<crate::core::machine::AgentFacts>),
    after: &(Option<String>, Option<crate::core::machine::AgentFacts>),
) -> bool {
    before.0 != after.0 || agent_facts_changed(before.1.as_ref(), after.1.as_ref())
}

fn agent_facts_changed(
    before: Option<&crate::core::machine::AgentFacts>,
    after: Option<&crate::core::machine::AgentFacts>,
) -> bool {
    match (before, after) {
        (None, None) => false,
        (Some(a), Some(b)) => {
            a.agent != b.agent
                || a.session_id != b.session_id
                || a.launch_argv != b.launch_argv
                || a.status != b.status
        }
        _ => true,
    }
}

fn apply_signals(st: &mut PaneState, signals: SniffSignals) {
    if let Some(cwd) = signals.cwd {
        if st.cwd.as_ref() != Some(&cwd) {
            notify(st, DaemonMsg::Cwd(cwd.clone()));
            st.cwd = Some(cwd);
        }
    }
    for shell in signals.shell {
        #[cfg(windows)]
        if shell_mark_capture_changed(&st.shell, &shell) {
            apply_agent(
                st,
                agent_from_shell_mark(&shell, crate::core::config::agent_commands_cached()),
            );
        }
        st.shell = shell.clone();
        notify(
            st,
            DaemonMsg::Prompt {
                active: shell.active,
                at_prompt: shell.at_prompt,
                last_exit: shell.last_exit_code,
            },
        );
    }
    apply_agent_signals(st, signals.agent_events, signals.notification);
}

#[cfg_attr(not(windows), allow(dead_code))]
fn shell_mark_capture_changed(prev: &ShellState, next: &ShellState) -> bool {
    prev.command != next.command
}

#[cfg_attr(not(windows), allow(dead_code))]
fn agent_from_shell_mark(
    shell: &ShellState,
    custom: &std::collections::HashMap<String, String>,
) -> Option<(crate::core::cli_agent::CLIAgent, Vec<String>)> {
    let cmd = shell.command.as_deref()?;
    let agent = crate::core::cli_agent::CLIAgent::detect_from_command_with(cmd, custom)?;
    Some((agent, crate::core::cli_agent::command_argv(cmd)))
}

fn apply_agent_signals(
    st: &mut PaneState,
    events: Vec<crate::core::cli_agent::AgentEvent>,
    notification: Option<String>,
) {
    use crate::core::cli_agent::{AgentSessionState, AgentStatus};

    if events.is_empty() && notification.is_none() {
        return;
    }
    let before = st.agent_session.clone();

    for event in &events {
        if st.agent.is_none() && event.agent.is_some() {
            st.agent = event.agent;
            notify(st, DaemonMsg::Agent(st.agent));
        }
        st.agent_session
            .get_or_insert_with(AgentSessionState::default)
            .apply_event(event);
    }

    if let Some(body) = notification
        && st.agent.is_some()
        && !st.agent_session.as_ref().is_some_and(|s| s.rich)
    {
        let sess = st
            .agent_session
            .get_or_insert_with(AgentSessionState::default);
        sess.status = AgentStatus::Waiting;
        sess.message = Some(body);
    }

    if let (Some(sess), Some(argv)) = (&mut st.agent_session, &st.agent_argv)
        && sess.launch_argv.is_none()
    {
        sess.launch_argv = Some(argv.clone());
    }

    if st.agent_session != before {
        notify(st, DaemonMsg::AgentStatus(st.agent_session.clone()));
    }
}

fn apply_probed_cwd(st: &mut PaneState, probed: Option<PathBuf>) {
    let Some(probed) = probed else {
        return;
    };
    if st.remote.is_some() {
        return;
    }
    if st.cwd.as_deref().is_some_and(|cur| same_dir(cur, &probed)) {
        return;
    }
    notify(st, DaemonMsg::Cwd(probed.clone()));
    st.cwd = Some(probed);
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || match (a.canonicalize(), b.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
}

fn apply_remote_context(st: &mut PaneState, remote: Option<RemoteContext>) {
    if st.remote == remote {
        return;
    }
    st.cwd = None;
    notify(st, DaemonMsg::RemoteContext(remote.clone()));
    st.remote = remote;
}

fn apply_agent(
    st: &mut PaneState,
    detected: Option<(crate::core::cli_agent::CLIAgent, Vec<String>)>,
) {
    let (agent, argv) = match detected {
        Some((agent, argv)) => (Some(agent), Some(argv)),
        None => (None, None),
    };
    if st.agent == agent {
        stamp_launch_argv(st, argv);
        return;
    }
    if agent.is_none() && st.agent_session.is_some() {
        st.agent_session = None;
        notify(st, DaemonMsg::AgentStatus(None));
    }
    if agent.is_none() {
        st.agent_argv = None;
    }
    notify(st, DaemonMsg::Agent(agent));
    st.agent = agent;
    stamp_launch_argv(st, argv);
}

fn stamp_launch_argv(st: &mut PaneState, argv: Option<Vec<String>>) {
    let Some(argv) = argv else { return };
    if argv.is_empty() {
        return;
    }
    if st.agent_argv.as_ref() == Some(&argv) {
        return;
    }
    st.agent_argv = Some(argv.clone());
    if let Some(sess) = &mut st.agent_session
        && sess.launch_argv.as_ref() != Some(&argv)
    {
        sess.launch_argv = Some(argv);
        notify(st, DaemonMsg::AgentStatus(st.agent_session.clone()));
    }
}

fn foreground_command_running(
    master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
    shell_pid: Option<u32>,
) -> bool {
    is_foreground_command(pty_foreground_pgid(master), shell_pid)
}

#[cfg(unix)]
fn pty_foreground_pgid(master: &Mutex<Option<Box<dyn MasterPty + Send>>>) -> Option<i32> {
    master
        .lock()
        .ok()
        .and_then(|m| m.as_ref().and_then(|m| m.process_group_leader()))
}

#[cfg(not(unix))]
fn pty_foreground_pgid(_master: &Mutex<Option<Box<dyn MasterPty + Send>>>) -> Option<i32> {
    None
}

fn is_foreground_command(fg_pgid: Option<i32>, shell_pid: Option<u32>) -> bool {
    match (fg_pgid, shell_pid) {
        (Some(pg), Some(shell)) if pg > 0 => pg as u32 != shell,
        _ => false,
    }
}

#[cfg(target_os = "macos")]
fn foreground_cwd(
    master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
    shell_pid: Option<u32>,
) -> Option<PathBuf> {
    use std::ffi::CStr;

    let read_cwd = |pid: i32| -> Option<PathBuf> {
        if pid <= 0 {
            return None;
        }
        let mut vinfo: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let ret = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                &mut vinfo as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if ret != size {
            return None;
        }
        let s = unsafe { CStr::from_ptr(vinfo.pvi_cdir.vip_path.as_ptr() as *const libc::c_char) }
            .to_str()
            .ok()?;
        if s.is_empty() {
            None
        } else {
            Some(PathBuf::from(s))
        }
    };

    pty_foreground_pgid(master)
        .and_then(read_cwd)
        .or_else(|| read_cwd(shell_pid.map(|p| p as i32).unwrap_or(0)))
}

#[cfg(target_os = "linux")]
fn foreground_cwd(
    master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
    shell_pid: Option<u32>,
) -> Option<PathBuf> {
    let read_cwd = |pid: i32| -> Option<PathBuf> {
        if pid <= 0 {
            return None;
        }
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
        cwd.is_dir().then_some(cwd)
    };
    pty_foreground_pgid(master)
        .and_then(read_cwd)
        .or_else(|| read_cwd(shell_pid.map(|p| p as i32).unwrap_or(0)))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn foreground_cwd(
    _master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
    _shell_pid: Option<u32>,
) -> Option<PathBuf> {
    None
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn foreground_remote_context(
    master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
) -> Option<RemoteContext> {
    let pid = master
        .lock()
        .ok()
        .and_then(|m| m.as_ref().and_then(|m| m.process_group_leader()))?;
    let argv = crate::daemon::remote::foreground_argv(pid)?;
    crate::daemon::remote::parse_ssh_invocation(&argv).map(|inv| inv.context)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn foreground_remote_context(
    _master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
) -> Option<RemoteContext> {
    None
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn foreground_agent(
    master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
) -> Option<Option<(crate::core::cli_agent::CLIAgent, Vec<String>)>> {
    let detect = || {
        let pid = master
            .lock()
            .ok()
            .and_then(|m| m.as_ref().and_then(|m| m.process_group_leader()))?;
        let argv = crate::daemon::remote::foreground_argv(pid)?;
        let agent = crate::core::cli_agent::CLIAgent::detect_from_argv_with(
            &argv,
            crate::core::config::agent_commands_cached(),
        )?;
        Some((agent, argv))
    };
    Some(detect())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn foreground_agent(
    _master: &Mutex<Option<Box<dyn MasterPty + Send>>>,
) -> Option<Option<(crate::core::cli_agent::CLIAgent, Vec<String>)>> {
    None
}

#[derive(Default, Clone, PartialEq, Eq)]
struct ShellState {
    active: bool,
    at_prompt: bool,
    last_exit_code: Option<i32>,
    command: Option<String>,
}

#[derive(Default)]
struct SniffSignals {
    cwd: Option<PathBuf>,
    shell: Vec<ShellState>,
    agent_events: Vec<crate::core::cli_agent::AgentEvent>,
    notification: Option<String>,
}

struct OscSniffer {
    tok: OscTokenizer,
    shell: ShellState,
}

impl OscSniffer {
    fn new() -> Self {
        Self {
            tok: OscTokenizer::new(&[b"7", b"133", b"9", b"777"]),
            shell: ShellState::default(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> SniffSignals {
        let mut signals = SniffSignals::default();
        let shell = &mut self.shell;
        self.tok.feed(bytes, |payload| {
            if let Some(path) = parse_osc7(payload) {
                signals.cwd = Some(path);
            } else if let Some(rest) = payload.strip_prefix(b"133;") {
                if handle_osc133(shell, rest) {
                    match signals.shell.last_mut() {
                        Some(last) if last.at_prompt == shell.at_prompt => {
                            *last = shell.clone();
                        }
                        _ => signals.shell.push(shell.clone()),
                    }
                }
            } else if let Some(event) = crate::core::cli_agent::parse_agent_event(payload) {
                signals.agent_events.push(event);
            } else if let Some((title, body)) = crate::core::osc::parse_notification(payload) {
                if title.as_deref() != Some(crate::core::cli_agent::AGENT_EVENT_SENTINEL) {
                    signals.notification = Some(body);
                }
            }
        });
        signals
    }
}

fn handle_osc133(shell: &mut ShellState, rest: &[u8]) -> bool {
    shell.active = true;
    match rest.first() {
        Some(b'A') | Some(b'B') => shell.at_prompt = true,
        Some(b'C') => {
            shell.at_prompt = false;
            shell.command = rest
                .strip_prefix(b"C;")
                .map(|c| String::from_utf8_lossy(&percent_decode(c)).into_owned())
                .filter(|s| !s.trim().is_empty());
        }
        Some(b'D') => {
            shell.at_prompt = true;
            shell.command = None;
            shell.last_exit_code = rest
                .strip_prefix(b"D;")
                .and_then(|c| std::str::from_utf8(c).ok())
                .and_then(|s| s.trim().parse::<i32>().ok());
        }
        _ => return false,
    }
    true
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    let s = String::from_utf8_lossy(bytes);
    PathBuf::from(strip_uri_drive_slash(s.as_ref()))
}

#[cfg_attr(unix, allow(dead_code))]
fn strip_uri_drive_slash(path: &str) -> &str {
    let b = path.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        &path[1..]
    } else {
        path
    }
}

pub(crate) fn parse_osc7(payload: &[u8]) -> Option<PathBuf> {
    let rest = payload.strip_prefix(b"7;")?;
    let path_bytes: &[u8] = if let Some(after) = rest.strip_prefix(b"file://") {
        let idx = after.iter().position(|&c| c == b'/')?;
        &after[idx..]
    } else if rest.first() == Some(&b'/') {
        rest
    } else {
        return None;
    };
    let decoded = percent_decode(path_bytes);
    if decoded.is_empty() {
        return None;
    }
    Some(path_from_bytes(&decoded))
}

fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len() {
            if let (Some(h), Some(l)) = (hex_val(input[i + 1]), hex_val(input[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn proc_name(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let ret =
        unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if ret <= 0 {
        return None;
    }
    let path = std::str::from_utf8(&buf[..ret as usize]).ok()?;
    Some(path.rsplit('/').next().unwrap_or(path).to_string())
}

#[cfg(target_os = "linux")]
fn proc_name(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    if let Ok(path) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let name = name.strip_suffix(" (deleted)").unwrap_or(name);
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let comm = comm.trim();
    (!comm.is_empty()).then(|| comm.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::kitty_graphics::ImageDelete;
    use std::path::Path;

    #[test]
    fn initial_working_directory_skips_paths_that_are_not_directories() {
        let real = std::env::temp_dir();
        assert!(real.is_dir(), "temp dir should exist");

        assert_eq!(
            initial_working_directory(Some(real.clone())),
            Some(real.clone())
        );

        for bogus in [
            "/home/someone/definitely-not-here",
            "/c/Users/definitely-not-here",
        ] {
            let got = initial_working_directory(Some(PathBuf::from(bogus)));
            assert_ne!(
                got.as_deref(),
                Some(Path::new(bogus)),
                "{bogus} is not a directory here and must not be handed to spawn"
            );
            if let Some(d) = got {
                assert!(d.is_dir(), "fallback {d:?} must be a real directory");
            }
        }

        let file = real.join("tty7-iwd-probe");
        std::fs::write(&file, b"x").expect("write probe file");
        let got = initial_working_directory(Some(file.clone()));
        assert_ne!(got.as_deref(), Some(file.as_path()));
        let _ = std::fs::remove_file(&file);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn live_pane_reports_an_uninstrumented_shells_cwd() {
        let target = std::path::Path::new("/usr");

        let (tx, rx) = mpsc::channel();
        let pane = DaemonPane::spawn(
            1,
            Some(PathBuf::from("/")),
            ws(80, 24),
            Some(ShellSpec {
                program: "sh".into(),
                args: vec!["-c".into(), "cd /usr && exec cat".into()],
                args_are_tty7_defaults: false,
            }),
            None,
            None,
            || {},
        )
        .expect("spawn pane");
        pane.attach(tx);

        let mut reported = None;
        for _ in 0..200 {
            pane.write_input(b"\n");
            while let Ok(msg) = rx.try_recv() {
                if let DaemonMsg::Cwd(p) = msg {
                    reported = Some(p);
                }
            }
            if reported.as_deref() == Some(target) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        pane.kill();

        assert_eq!(
            reported.as_deref(),
            Some(target),
            "a pane whose shell emits no OSC 7 must still report where it \
             actually is — this is what a new tab inherits (issue #187)"
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn live_pty_child_argv_detects_the_agent() {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};

        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new("bash");
        cmd.args(["-c", "exec -a codex cat"]);
        let mut child = pty.slave.spawn_command(cmd).expect("spawn child");
        let master = Mutex::new(Some(pty.master));

        let mut detected = None;
        for _ in 0..200 {
            if let Some(agent) = foreground_agent(&master).flatten() {
                detected = Some(agent);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = child.kill();
        let _ = child.wait();

        let (agent, argv) = detected.expect("agent detected from live PTY child");
        assert_eq!(
            agent,
            crate::core::cli_agent::CLIAgent::Codex,
            "a live PTY child with argv[0]=codex must be detected as Codex"
        );
        assert_eq!(
            argv.first().map(String::as_str),
            Some("codex"),
            "the observed argv rides along with the detection"
        );
    }

    #[test]
    fn choose_shell_prefers_override_then_config_then_default() {
        let over = ShellSpec {
            program: "fish".into(),
            args: vec!["-l".into()],
            args_are_tty7_defaults: true,
        };
        let cfg = ("zsh".to_string(), vec!["-i".to_string()]);

        assert_eq!(
            choose_shell(Some(over.clone()), Some(cfg.clone())),
            Some(ChosenShell {
                program: "fish".to_string(),
                args: vec!["-l".to_string()],
                args_are_tty7_defaults: true,
            })
        );
        assert_eq!(
            choose_shell(None, Some(cfg.clone())),
            Some(ChosenShell {
                program: "zsh".to_string(),
                args: vec!["-i".to_string()],
                args_are_tty7_defaults: false,
            })
        );
        assert_eq!(choose_shell(None, None), None);
    }

    #[test]
    fn only_user_authored_args_block_shell_integration() {
        let chosen = |args: Vec<&str>, tty7: bool| ChosenShell {
            program: r"C:\Program Files\Git\bin\bash.exe".to_string(),
            args: args.into_iter().map(str::to_string).collect(),
            args_are_tty7_defaults: tty7,
        };

        assert!(!has_custom_args(Some(&chosen(vec!["-i", "-l"], true))));
        assert!(has_custom_args(Some(&chosen(vec!["-i", "-l"], false))));
        assert!(!has_custom_args(Some(&chosen(vec![], false))));
        assert!(!has_custom_args(Some(&chosen(vec![], true))));
        assert!(!has_custom_args(None));
    }

    #[cfg(windows)]
    #[test]
    fn wsl_panes_are_tagged_as_a_foreign_filesystem() {
        let spec = |program: &str, args: Vec<&str>| ChosenShell {
            program: program.to_string(),
            args: args.into_iter().map(str::to_string).collect(),
            args_are_tty7_defaults: true,
        };

        let ctx = wsl_remote_context(Some(&spec(
            "wsl.exe",
            vec!["--distribution", "Ubuntu-24.04", "--cd", "~"],
        )))
        .expect("wsl.exe must be tagged");
        assert_eq!(ctx.kind, RemoteKind::Wsl);
        assert_eq!(ctx.target, "Ubuntu-24.04");
        assert!(ctx.argv.is_empty());

        assert_eq!(
            wsl_remote_context(Some(&spec("wsl.exe", vec!["-d", "Debian"])))
                .expect("short flag")
                .target,
            "Debian"
        );
        assert_eq!(
            wsl_remote_context(Some(&spec("wsl.exe", vec![])))
                .expect("default distro is still WSL")
                .target,
            ""
        );
        assert_eq!(
            wsl_remote_context(Some(&spec("wsl.exe", vec!["--distribution=Arch"])))
                .expect("joined flag")
                .target,
            "Arch"
        );
        assert!(wsl_remote_context(Some(&spec(r"C:\Windows\System32\WSL.EXE", vec![]))).is_some());

        let from_config = choose_shell(None, Some(("wsl.exe".to_string(), Vec::new())));
        assert_eq!(
            wsl_remote_context(from_config.as_ref()).map(|c| c.kind),
            Some(RemoteKind::Wsl),
            "a configured wsl.exe is as much a WSL pane as a dropdown one"
        );

        assert!(wsl_remote_context(Some(&spec("powershell.exe", vec![]))).is_none());
        assert!(
            wsl_remote_context(Some(&spec(r"C:\Program Files\Git\bin\bash.exe", vec![]))).is_none()
        );
        assert!(wsl_remote_context(None).is_none());
    }

    #[test]
    fn arg_based_integration_rebuilds_default_shell_builder() {
        let mut cmd = CommandBuilder::new_default_prog();
        let injection = shell_integration::Injection {
            env: std::collections::HashMap::new(),
            args: vec!["-C".to_string(), "echo ready".to_string()],
            replaces_argv: false,
            dir: None,
        };

        apply_shell_integration(&mut cmd, "/bin/fish", &injection);

        assert!(!cmd.is_default_prog(), "argv can now be appended safely");
        let argv: Vec<_> = cmd
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv, vec!["/bin/fish", "-C", "echo ready"]);
    }

    #[test]
    fn env_only_integration_keeps_default_login_shell_builder() {
        let mut cmd = CommandBuilder::new_default_prog();
        let mut env = std::collections::HashMap::new();
        env.insert("ZDOTDIR".to_string(), "/tmp/tty7-zdotdir-test".to_string());
        let injection = shell_integration::Injection {
            env,
            args: Vec::new(),
            replaces_argv: false,
            dir: None,
        };

        apply_shell_integration(&mut cmd, "/bin/zsh", &injection);

        assert!(
            cmd.is_default_prog(),
            "zsh still launches as the login shell"
        );
        assert_eq!(
            cmd.get_env("ZDOTDIR").and_then(|value| value.to_str()),
            Some("/tmp/tty7-zdotdir-test")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn detected_shell_override_uses_explicit_command_builder() {
        let portable_shell = CommandBuilder::new_default_prog().get_shell();
        let detected_shell = format!("{portable_shell}-detected");
        let cmd = default_prog_with_override(Some(detected_shell.clone()));

        assert!(!cmd.is_default_prog());
        let argv: Vec<_> = cmd
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv, vec![detected_shell]);
    }

    #[cfg(not(windows))]
    #[test]
    fn no_detected_shell_keeps_portable_login_default() {
        let cmd = default_prog_with_override(None);

        assert!(cmd.is_default_prog());
        assert!(!default_shell_name(&cmd).is_empty());
    }

    #[test]
    fn join_bounded_returns_true_when_the_thread_finishes() {
        let handle = std::thread::spawn(|| {});
        assert!(join_bounded(handle, Duration::from_secs(5)));
    }

    #[test]
    fn join_bounded_times_out_on_a_stuck_thread() {
        let (unblock, blocked) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let _ = blocked.recv();
        });
        assert!(!join_bounded(handle, Duration::from_millis(50)));
        drop(unblock);
    }

    #[test]
    fn gate_passes_below_high_water() {
        let gate = OutputGate::new();
        gate.add((OutputGate::HIGH_WATER - 1) as usize);
        let t0 = std::time::Instant::now();
        gate.wait_below_high_water();
        assert!(
            t0.elapsed() < Duration::from_millis(100),
            "no wait expected"
        );
    }

    #[test]
    fn gate_parks_at_high_water_until_drained() {
        let gate = Arc::new(OutputGate::new());
        gate.add(OutputGate::HIGH_WATER as usize);

        let drainer = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                gate.sub(OutputGate::HIGH_WATER as usize);
            })
        };
        let t0 = std::time::Instant::now();
        gate.wait_below_high_water();
        let waited = t0.elapsed();
        drainer.join().unwrap();
        assert!(waited >= Duration::from_millis(40), "must park until sub()");
        assert!(
            waited < OutputGate::MAX_WAIT,
            "the drain, not the escape timeout, must unpark"
        );
    }

    #[test]
    fn gate_reset_unparks_a_throttled_reader() {
        let gate = Arc::new(OutputGate::new());
        gate.add(OutputGate::HIGH_WATER as usize * 2);

        let resetter = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                gate.reset();
            })
        };
        let t0 = std::time::Instant::now();
        gate.wait_below_high_water();
        resetter.join().unwrap();
        assert!(t0.elapsed() < OutputGate::MAX_WAIT);
    }

    #[test]
    fn gate_tolerates_negative_drift() {
        let gate = OutputGate::new();
        gate.sub(1024);
        gate.add(512);
        let t0 = std::time::Instant::now();
        gate.wait_below_high_water();
        assert!(t0.elapsed() < Duration::from_millis(100));
    }

    fn ws(cols: u16, rows: u16) -> WinSize {
        WinSize {
            cols,
            rows,
            cell_w: 8,
            cell_h: 16,
        }
    }

    #[test]
    fn ring_under_cap_keeps_all() {
        let mut ring = ReplayRing::new(ws(80, 24));
        ring.append(b"hello ");
        ring.append(b"world");
        assert_eq!(ring.flatten(), b"hello world");
    }

    #[test]
    fn ring_over_cap_drops_oldest() {
        let mut ring = ReplayRing::new(ws(80, 24));
        ring.append(&vec![b'a'; RING_CAP]);
        assert_eq!(ring.len, RING_CAP);
        ring.append(&vec![b'b'; 100]);
        assert_eq!(ring.len, RING_CAP);
        let flat = ring.flatten();
        assert_eq!(&flat[..RING_CAP - 100], &vec![b'a'; RING_CAP - 100][..]);
        assert_eq!(&flat[RING_CAP - 100..], &vec![b'b'; 100][..]);
    }

    #[test]
    fn ring_giant_chunk_keeps_tail() {
        let mut ring = ReplayRing::new(ws(100, 24));
        ring.append(b"seed");
        ring.resize(ws(80, 24));
        let mut big = vec![b'x'; RING_CAP];
        big.extend_from_slice(b"TAIL");
        ring.append(&big);
        assert_eq!(ring.len, RING_CAP);
        assert_eq!(ring.segments.len(), 1);
        assert_eq!(&ring.flatten()[RING_CAP - 4..], b"TAIL");
    }

    #[test]
    fn ring_resize_splits_replay_into_geometry_segments() {
        let mut ring = ReplayRing::new(ws(100, 24));
        ring.append(b"wide bytes");
        ring.resize(ws(80, 24));
        ring.append(b"narrow bytes");

        let (tx, rx) = mpsc::channel();
        ring.replay(&tx);
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Size(s)) if s == ws(100, 24)));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Snapshot(b)) if b == b"wide bytes"));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Size(s)) if s == ws(80, 24)));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Snapshot(b)) if b == b"narrow bytes"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ring_idle_resizes_collapse_and_replay_ends_at_current_size() {
        let mut ring = ReplayRing::new(ws(100, 24));
        ring.append(b"bytes");
        ring.resize(ws(100, 24));
        assert_eq!(ring.segments.len(), 1);
        ring.resize(ws(90, 24));
        ring.resize(ws(80, 30));
        assert_eq!(ring.segments.len(), 2);

        let (tx, rx) = mpsc::channel();
        ring.replay(&tx);
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Size(s)) if s == ws(100, 24)));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Snapshot(b)) if b == b"bytes"));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Size(s)) if s == ws(80, 30)));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Snapshot(b)) if b.is_empty()));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ring_eviction_drops_emptied_segments() {
        let mut ring = ReplayRing::new(ws(100, 24));
        ring.append(b"old");
        ring.resize(ws(80, 24));
        ring.append(&vec![b'n'; RING_CAP - 2]);
        assert_eq!(ring.segments.len(), 2, "two bytes of the old segment left");
        ring.append(b"nn");
        assert_eq!(ring.segments.len(), 1, "the emptied old segment is gone");
        assert_eq!(ring.len, RING_CAP);

        let (tx, rx) = mpsc::channel();
        ring.replay(&tx);
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Size(s)) if s == ws(80, 24)));
    }

    #[test]
    fn ring_caps_segment_count_by_merging_oldest() {
        let mut ring = ReplayRing::new(ws(100, 24));
        let rounds = MAX_RING_SEGMENTS + 10;
        for i in 0..rounds {
            ring.append(format!("seg{i:02} ").as_bytes());
            ring.resize(ws(101 + i as u16, 24));
        }
        assert_eq!(ring.segments.len(), MAX_RING_SEGMENTS);

        let flat = String::from_utf8(ring.flatten()).unwrap();
        let expect: String = (0..rounds).map(|i| format!("seg{i:02} ")).collect();
        assert_eq!(flat, expect);

        let head = ring.segments.front().unwrap();
        assert_eq!(head.size, ws(111, 24));
        assert!(
            String::from_utf8(head.to_vec())
                .unwrap()
                .ends_with("seg11 ")
        );
    }

    #[test]
    fn sniff_osc7_cwd() {
        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]7;file://host/Users/me/dev\x07");
        assert_eq!(sig.cwd, Some(PathBuf::from("/Users/me/dev")));
    }

    #[test]
    fn sniff_osc133_prompt() {
        let mut s = OscSniffer::new();
        let b = s.feed(b"\x1b]133;B\x07");
        assert!(b.shell.last().unwrap().active);
        assert!(b.shell.last().unwrap().at_prompt);

        let c = s.feed(b"\x1b]133;C\x07");
        assert!(!c.shell.last().unwrap().at_prompt);

        let d = s.feed(b"\x1b]133;D;130\x07");
        assert!(d.shell.last().unwrap().at_prompt);
        assert_eq!(d.shell.last().unwrap().last_exit_code, Some(130));
    }

    #[test]
    fn a_full_command_cycle_in_one_chunk_still_reports_leaving_the_prompt() {
        let mut s = OscSniffer::new();
        s.feed(b"\x1b]133;A\x07\x1b]133;B\x07");

        let sig =
            s.feed(b"\x1b]133;C;echo%20hi\x07hi\r\n\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;B\x07");
        let states: Vec<bool> = sig.shell.iter().map(|s| s.at_prompt).collect();
        assert_eq!(
            states,
            vec![false, true],
            "the chunk must report leaving the prompt and coming back, not just the end state"
        );
        assert_eq!(sig.shell.last().unwrap().last_exit_code, Some(0));
        assert_eq!(sig.shell.last().unwrap().command, None);
    }

    #[test]
    fn marks_on_the_same_side_of_the_prompt_boundary_fold_into_one_state() {
        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]133;D;3\x07\x1b]133;A\x07\x1b]133;B\x07");
        assert_eq!(sig.shell.len(), 1, "D/A/B is one at-prompt state");
        assert!(sig.shell[0].at_prompt);
        assert_eq!(sig.shell[0].last_exit_code, Some(3));
    }

    #[test]
    fn sniff_osc133_command_capture_drives_agent_detection() {
        let custom = std::collections::HashMap::new();
        let mut s = OscSniffer::new();

        let c = s.feed(b"\x1b]133;C;claude%20--help\x07");
        let shell = c.shell.last().unwrap();
        assert!(!shell.at_prompt);
        assert_eq!(shell.command.as_deref(), Some("claude --help"));
        assert_eq!(
            agent_from_shell_mark(shell, &custom),
            Some((
                crate::core::cli_agent::CLIAgent::Claude,
                vec!["claude".to_string(), "--help".to_string()],
            ))
        );

        let d = s.feed(b"\x1b]133;D;0\x07");
        let shell = d.shell.last().unwrap();
        assert_eq!(shell.command, None);
        assert_eq!(agent_from_shell_mark(shell, &custom), None);

        let c = s.feed(b"\x1b]133;C;git%20status\x07");
        let shell = c.shell.last().unwrap();
        assert_eq!(shell.command.as_deref(), Some("git status"));
        assert_eq!(agent_from_shell_mark(shell, &custom), None);

        let c = s.feed(b"\x1b]133;C\x07");
        assert_eq!(c.shell.last().unwrap().command, None);

        let _ = s.feed(b"\x1b]133;C;codex\x07");
        let a = s.feed(b"\x1b]133;A\x1b]133;B\x07");
        assert_eq!(a.shell.last().unwrap().command.as_deref(), Some("codex"));
        let d = s.feed(b"\x1b]133;D;0\x07");
        assert_eq!(d.shell.last().unwrap().command, None);

        let c = s.feed(b"\x1b]133;C;echo%20a%0Aecho%20b\x07");
        assert_eq!(
            c.shell.last().unwrap().command.as_deref(),
            Some("echo a\necho b")
        );

        let c = s.feed(b"\x1b]133;C;%20%20\x07");
        assert_eq!(c.shell.last().unwrap().command, None);
    }

    #[test]
    fn shell_mark_agent_detection_keeps_launch_argv_for_resume() {
        let custom = std::collections::HashMap::new();
        let mut s = OscSniffer::new();

        let c = s.feed(b"\x1b]133;C;claude%20--dangerously-skip-permissions\x07");
        let (agent, argv) = agent_from_shell_mark(c.shell.last().unwrap(), &custom).unwrap();
        assert_eq!(agent, crate::core::cli_agent::CLIAgent::Claude);
        assert_eq!(
            agent.resume_command("abc-123", Some(&argv)).as_deref(),
            Some("claude --dangerously-skip-permissions --resume abc-123")
        );

        // Case must survive capture: a lowercased path or model name would
        // replay the wrong flags.
        let c = s.feed(b"\x1b]133;C;claude%20--model%20Opus\x07");
        let (agent, argv) = agent_from_shell_mark(c.shell.last().unwrap(), &custom).unwrap();
        assert_eq!(argv, ["claude", "--model", "Opus"]);
        assert_eq!(
            agent.resume_command("abc", Some(&argv)).as_deref(),
            Some("claude --model Opus --resume abc")
        );

        // PowerShell call-operator launches keep working end to end.
        let c = s.feed(b"\x1b]133;C;%26%20%22C%3A%5Ctools%5Cclaude.exe%22%20--continue\x07");
        let (agent, argv) = agent_from_shell_mark(c.shell.last().unwrap(), &custom).unwrap();
        assert_eq!(agent, crate::core::cli_agent::CLIAgent::Claude);
        assert_eq!(argv, [r"C:\tools\claude.exe", "--continue"]);
        assert_eq!(
            agent.resume_command("abc", Some(&argv)).as_deref(),
            Some("claude --resume abc"),
            "stale session flags are dropped, not replayed"
        );
    }

    #[test]
    fn sniff_osc133_stray_marks_do_not_reapply_mark_detection() {
        let mut s = OscSniffer::new();

        let mut prev = ShellState::default();
        let c = s.feed(b"\x1b]133;C;.%5Cdev.ps1\x07").shell.pop().unwrap();
        assert!(shell_mark_capture_changed(&prev, &c));
        assert_eq!(
            agent_from_shell_mark(&c, &std::collections::HashMap::new()),
            None
        );
        prev = c;

        let ab = s.feed(b"\x1b]133;A\x1b]133;B\x07").shell.pop().unwrap();
        assert!(!shell_mark_capture_changed(&prev, &ab));
        prev = ab;

        let d = s.feed(b"\x1b]133;D;0\x07").shell.pop().unwrap();
        assert!(shell_mark_capture_changed(&prev, &d));
    }

    #[test]
    fn sniff_osc133_edit_mode_does_not_emit_prompt_state() {
        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]133;V;1\x07");
        assert!(
            sig.shell.is_empty(),
            "edit-mode metadata must not bump prompt state or prompt sequence"
        );

        let b = s.feed(b"\x1b]133;B\x07");
        assert!(b.shell.last().unwrap().active);
        assert!(b.shell.last().unwrap().at_prompt);
    }

    #[test]
    fn foreground_command_distinguishes_the_shell_from_a_command() {
        assert!(!is_foreground_command(Some(1000), Some(1000)));
        assert!(is_foreground_command(Some(2000), Some(1000)));
        assert!(!is_foreground_command(None, Some(1000)));
        assert!(!is_foreground_command(Some(2000), None));
        assert!(!is_foreground_command(Some(0), Some(1000)));
    }

    #[test]
    fn foreground_program_prompt_marks_do_not_claim_the_prompt() {
        let mut s = OscSniffer::new();
        let mut signals = s.feed(b"\x1b]133;A\x1b]133;B\x07");
        assert!(
            signals.shell.last().unwrap().at_prompt,
            "the raw marks read as at-prompt"
        );

        let ssh_running = is_foreground_command(Some(2000), Some(1000));
        if signals.shell.iter().any(|st| st.at_prompt) && ssh_running {
            for st in signals.shell.iter_mut() {
                st.at_prompt = false;
            }
        }
        assert!(
            !signals.shell.last().unwrap().at_prompt,
            "a foreground program's prompt marks must not engage the local editor"
        );

        let mut local = s.feed(b"\x1b]133;A\x1b]133;B\x07");
        let shell_idle = is_foreground_command(Some(1000), Some(1000));
        if local.shell.iter().any(|st| st.at_prompt) && shell_idle {
            for st in local.shell.iter_mut() {
                st.at_prompt = false;
            }
        }
        assert!(local.shell.last().unwrap().at_prompt);
    }

    #[test]
    fn sniff_resyncs_on_new_osc_after_an_unterminated_one() {
        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]133;A\x1b]133;B\x07");
        assert!(
            sig.shell.last().is_some_and(|sh| sh.at_prompt),
            "OSC 133;B after an unterminated 133;A was dropped (no resync on `]`)"
        );

        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]7;file://host/dropped\x1b]7;file://host/kept\x07");
        assert_eq!(sig.cwd, Some(PathBuf::from("/kept")));
    }

    #[test]
    fn at_prompt_covers_prompt_draw_gap_across_chunks() {
        let mut s = OscSniffer::new();

        assert!(!s.feed(b"\x1b]133;C\x07").shell.last().unwrap().at_prompt);

        let d = s.feed(b"\x1b]133;D;0\x07");
        assert!(
            d.shell.last().unwrap().at_prompt,
            "D should mark us back at the prompt before the prompt text is drawn"
        );

        let chunk = s.feed(
            b"\x1b]133;A\x07\x1b]7;file://host/repo/tty7\x07\r\ntty7 git:(main) \xe2\x9e\x9c ",
        );
        assert!(
            chunk.shell.last().unwrap().at_prompt,
            "prompt visible but at_prompt=false — the mis-routing window is still open"
        );

        assert!(s.feed(b"\x1b]133;B\x07").shell.last().unwrap().at_prompt);
    }

    #[test]
    fn pty_size_clamps_and_computes_pixels() {
        let ps = pty_size(WinSize {
            cols: 80,
            rows: 24,
            cell_w: 8,
            cell_h: 17,
        });
        assert_eq!(ps.rows, 24);
        assert_eq!(ps.cols, 80);
        assert_eq!(ps.pixel_width, 80 * 8);
        assert_eq!(ps.pixel_height, 24 * 17);

        let z = pty_size(WinSize {
            cols: 0,
            rows: 0,
            cell_w: 0,
            cell_h: 0,
        });
        assert_eq!(z.rows, 1);
        assert_eq!(z.cols, 1);
        assert_eq!(z.pixel_width, 0);
        assert_eq!(z.pixel_height, 0);

        let big = pty_size(WinSize {
            cols: u16::MAX,
            rows: u16::MAX,
            cell_w: u16::MAX,
            cell_h: u16::MAX,
        });
        assert_eq!(big.pixel_width, u16::MAX);
        assert_eq!(big.pixel_height, u16::MAX);
    }

    #[test]
    fn parse_osc7_forms_and_rejections() {
        assert_eq!(
            parse_osc7(b"7;file://host/Users/me/dev"),
            Some(PathBuf::from("/Users/me/dev"))
        );
        assert_eq!(parse_osc7(b"7;file:///etc"), Some(PathBuf::from("/etc")));
        assert_eq!(parse_osc7(b"7;/var/log"), Some(PathBuf::from("/var/log")));
        assert_eq!(
            parse_osc7(b"7;file://host/a%20b"),
            Some(PathBuf::from("/a b"))
        );
        assert_eq!(
            parse_osc7(b"7;file://host/%E4%B8%AD%E6%96%87"),
            Some(PathBuf::from("/中文"))
        );
        assert_eq!(
            parse_osc7(b"7;file://host/tmp/a%2520b"),
            Some(PathBuf::from("/tmp/a%20b"))
        );
        assert!(parse_osc7(b"8;file://host/x").is_none());
        assert!(parse_osc7(b"7;file://host").is_none());
        assert!(parse_osc7(b"7;relative/path").is_none());
        assert!(parse_osc7(b"7;file://host").is_none());
    }

    #[test]
    fn strip_uri_drive_slash_only_unwraps_drive_paths() {
        assert_eq!(strip_uri_drive_slash("/C:/Users/foo"), "C:/Users/foo");
        assert_eq!(strip_uri_drive_slash("/d:/x"), "d:/x");
        assert_eq!(strip_uri_drive_slash("/home/me/dev"), "/home/me/dev");
        assert_eq!(strip_uri_drive_slash("//host/share"), "//host/share");
        assert_eq!(strip_uri_drive_slash("C:/already"), "C:/already");
        assert_eq!(strip_uri_drive_slash("/"), "/");
    }

    #[test]
    fn percent_decode_handles_escapes_and_garbage() {
        assert_eq!(percent_decode(b"a%20b"), b"a b");
        assert_eq!(percent_decode(b"%2F"), b"/");
        assert_eq!(percent_decode(b"%2f"), b"/");
        assert_eq!(percent_decode(b"%GG"), b"%GG");
        assert_eq!(percent_decode(b"x%2"), b"x%2");
        assert_eq!(percent_decode(b"plain"), b"plain");
    }

    #[test]
    fn hex_val_ranges() {
        assert_eq!(hex_val(b'0'), Some(0));
        assert_eq!(hex_val(b'9'), Some(9));
        assert_eq!(hex_val(b'a'), Some(10));
        assert_eq!(hex_val(b'f'), Some(15));
        assert_eq!(hex_val(b'A'), Some(10));
        assert_eq!(hex_val(b'F'), Some(15));
        assert!(hex_val(b'g').is_none());
        assert!(hex_val(b' ').is_none());
        assert!(hex_val(b'/').is_none());
    }

    #[test]
    fn osc133_exit_code_parsing() {
        let mut s = OscSniffer::new();
        let d = s.feed(b"\x1b]133;D\x07");
        assert!(d.shell.last().unwrap().at_prompt);
        assert_eq!(d.shell.last().unwrap().last_exit_code, None);

        let d = s.feed(b"\x1b]133;D;oops\x07");
        assert_eq!(d.shell.last().unwrap().last_exit_code, None);

        let d = s.feed(b"\x1b]133;D;-1\x07");
        assert_eq!(d.shell.last().unwrap().last_exit_code, Some(-1));
    }

    /// A discard writer for `spawn_reader` tests that don't inspect the PTY
    /// write-back path (graphics query replies).
    fn null_writer() -> Arc<Mutex<Box<dyn Write + Send>>> {
        Arc::new(Mutex::new(Box::new(std::io::sink())))
    }

    fn test_state(alive: bool) -> PaneState {
        PaneState {
            id: 0,
            ring: ReplayRing::new(ws(80, 24)),
            subscriber: None,
            subscriber_epoch: 0,
            observers: Vec::new(),
            observer_seq: 0,
            cwd: None,
            shell: ShellState::default(),
            remote: None,
            agent: None,
            agent_session: None,
            agent_argv: None,
            alive,
            exit_code: None,
        }
    }

    #[test]
    fn observed_facts_prefer_the_sessions_argv_and_carry_its_status() {
        use crate::core::cli_agent::{AgentSessionState, AgentStatus, CLIAgent};

        let mut st = test_state(true);
        assert_eq!(observed_facts(&st), (None, None));

        st.cwd = Some(PathBuf::from("/work/api"));
        st.agent = Some(CLIAgent::Claude);
        st.agent_argv = Some(vec!["claude".into()]);
        st.agent_session = Some(AgentSessionState {
            status: AgentStatus::Working,
            session_id: Some("sess-1".into()),
            launch_argv: Some(vec!["claude".into(), "--model".into(), "opus".into()]),
            ..Default::default()
        });

        let (cwd, agent) = observed_facts(&st);
        assert_eq!(cwd.as_deref(), Some("/work/api"));
        let agent = agent.expect("an agent in the foreground is a fact");
        assert_eq!(agent.agent, CLIAgent::Claude);
        assert_eq!(agent.session_id.as_deref(), Some("sess-1"));
        assert_eq!(
            agent.launch_argv.as_deref(),
            Some(&["claude".to_string(), "--model".into(), "opus".into()][..]),
            "the session's own argv outranks the identity poll's capture"
        );
        assert_eq!(agent.status, Some(AgentStatus::Working));

        st.agent_session = None;
        let (_, agent) = observed_facts(&st);
        assert_eq!(
            agent.unwrap().launch_argv.as_deref(),
            Some(&["claude".to_string()][..])
        );
    }

    #[test]
    fn a_pane_killed_with_its_agent_keeps_the_facts_a_resume_needs() {
        use crate::core::cli_agent::{AgentSessionState, CLIAgent};
        use crate::core::machine::{
            AgentFacts, MACHINE_FILE, MachineStore, OBSERVE_SLOT, PaneSeed, publish_observations,
            withdraw_observations,
        };

        const PANE: u64 = 77;
        let _slot = OBSERVE_SLOT.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::TempDir::new().unwrap();
        let store = MachineStore::open(dir.path().join(MACHINE_FILE));
        let ws = store.workspace_create(None, None, None).unwrap();
        store
            .tab_create(
                ws.id,
                None,
                PaneSeed {
                    pane: PANE,
                    cwd: Some("/work/api".to_string()),
                    ssh_spec: None,
                    agent: Some(AgentFacts {
                        agent: CLIAgent::Claude,
                        session_id: Some("sess-1".to_string()),
                        launch_argv: Some(vec!["claude".to_string()]),
                        status: None,
                    }),
                },
                None,
                None,
            )
            .unwrap();
        publish_observations(&store);

        let run = |shutting_down: bool| {
            let mut state = test_state(true);
            state.id = PANE;
            state.agent = Some(CLIAgent::Claude);
            state.agent_session = Some(AgentSessionState {
                session_id: Some("sess-1".to_string()),
                ..Default::default()
            });
            DaemonPane::spawn_reader(
                Arc::new(Mutex::new(state)),
                Arc::new(AtomicBool::new(shutting_down)),
                Arc::new(OutputGate::new()),
                Box::new(std::io::Cursor::new(b"\x1b]133;D;0\x07".to_vec())),
                null_writer(),
                || false,
                ForegroundProbes {
                    remote: Box::new(|| None),
                    agent: Box::new(|| Some(None)),
                    cwd: Box::new(|| None),
                },
                Arc::new(DeathReporter::new(|| {})),
            )
            .join()
            .unwrap();
        };

        run(true);
        let kept = store
            .pane(PANE)
            .expect("the record outlives the pane")
            .agent
            .expect("a teardown must not report the agent away");
        assert_eq!(kept.session_id.as_deref(), Some("sess-1"));

        run(false);
        assert!(
            store.pane(PANE).unwrap().agent.is_none(),
            "an agent that left a pane still in use is a fact, and clears"
        );

        withdraw_observations();
    }

    #[test]
    fn sentinel_events_drive_agent_session_state() {
        use crate::core::cli_agent::{AgentStatus, CLIAgent};

        let mut st = test_state(true);
        let (tx, rx) = mpsc::channel();
        st.subscriber = Some(tx);

        let mut sniffer = OscSniffer::new();
        let stream = concat!(
            "\x1b]777;notify;tty7://cli-agent;",
            r#"{"v":1,"agent":"claude","event":"session-start","session_id":"sid-9"}"#,
            "\x07",
            "\x1b]777;notify;tty7://cli-agent;",
            r#"{"v":1,"agent":"claude","event":"prompt-submit"}"#,
            "\x07",
        );
        apply_signals(&mut st, sniffer.feed(stream.as_bytes()));

        assert_eq!(st.agent, Some(CLIAgent::Claude));
        let sess = st.agent_session.clone().expect("session state exists");
        assert_eq!(sess.status, AgentStatus::Working);
        assert_eq!(sess.session_id.as_deref(), Some("sid-9"));
        assert!(sess.rich);

        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonMsg::Agent(Some(CLIAgent::Claude)))
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonMsg::AgentStatus(Some(s))) if s.status == AgentStatus::Working
        ));

        let waiting = concat!(
            "\x1b]777;notify;tty7://cli-agent;",
            r#"{"event":"notification","message":"Claude needs your permission to use Bash"}"#,
            "\x07",
        );
        apply_signals(&mut st, sniffer.feed(waiting.as_bytes()));
        assert_eq!(
            st.agent_session.as_ref().unwrap().status,
            AgentStatus::Waiting
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonMsg::AgentStatus(Some(s))) if s.message.as_deref().unwrap().contains("permission")
        ));

        apply_agent(&mut st, None);
        assert!(st.agent_session.is_none());
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::AgentStatus(None))));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Agent(None))));
    }

    #[test]
    fn opaque_notifications_only_fall_back_when_no_rich_state() {
        use crate::core::cli_agent::{AgentSessionState, AgentStatus, CLIAgent};

        let mut st = test_state(true);
        let mut sniffer = OscSniffer::new();
        apply_signals(&mut st, sniffer.feed(b"\x1b]9;Build finished\x07"));
        assert!(st.agent_session.is_none());

        st.agent = Some(CLIAgent::Codex);
        apply_signals(
            &mut st,
            sniffer.feed(b"\x1b]9;Codex wants to run tests\x07"),
        );
        let sess = st.agent_session.clone().unwrap();
        assert_eq!(sess.status, AgentStatus::Waiting);
        assert!(!sess.rich);

        st.agent_session = Some(AgentSessionState {
            status: AgentStatus::Working,
            message: None,
            session_id: Some("sid".into()),
            launch_argv: None,
            rich: true,
            cwd: None,
            activity: 0,
        });
        apply_signals(&mut st, sniffer.feed(b"\x1b]9;noise\x07"));
        assert_eq!(
            st.agent_session.as_ref().unwrap().status,
            AgentStatus::Working
        );
    }

    #[test]
    fn probed_cwd_corrects_a_pane_whose_shell_never_reports_osc7() {
        let mut st = test_state(true);
        st.cwd = Some(PathBuf::from("/Users/alice"));
        let (tx, rx) = mpsc::channel();
        st.subscriber = Some(tx);

        apply_probed_cwd(&mut st, Some(PathBuf::from("/Users/alice/dev/tty7")));

        assert_eq!(st.cwd.as_deref(), Some(Path::new("/Users/alice/dev/tty7")));
        assert!(
            matches!(rx.try_recv(), Ok(DaemonMsg::Cwd(p)) if p == PathBuf::from("/Users/alice/dev/tty7"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn probed_cwd_keeps_the_shells_spelling_for_a_symlinked_path() {
        let tmp = std::env::temp_dir().join(format!("tty7-cwd-{}", std::process::id()));
        let real = tmp.join("real");
        let link = tmp.join("link");
        std::fs::create_dir_all(&real).unwrap();
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let mut st = test_state(true);
        st.cwd = Some(link.clone());
        let (tx, rx) = mpsc::channel();
        st.subscriber = Some(tx);

        apply_probed_cwd(&mut st, Some(real.canonicalize().unwrap()));

        assert_eq!(st.cwd.as_deref(), Some(link.as_path()));
        assert!(rx.try_recv().is_err(), "same directory → nothing to tell");

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn probed_cwd_matching_the_reported_one_says_nothing() {
        let mut st = test_state(true);
        st.cwd = Some(PathBuf::from("/Users/alice/dev/tty7"));
        let (tx, rx) = mpsc::channel();
        st.subscriber = Some(tx);

        apply_probed_cwd(&mut st, Some(PathBuf::from("/Users/alice/dev/tty7")));

        assert_eq!(st.cwd.as_deref(), Some(Path::new("/Users/alice/dev/tty7")));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn probed_cwd_declines_to_speak_for_a_remote_pane() {
        let mut st = test_state(true);
        st.remote = Some(RemoteContext {
            kind: RemoteKind::Ssh,
            argv: vec!["ssh".into(), "build-box".into()],
            target: "build-box".into(),
        });
        st.cwd = None;
        let (tx, rx) = mpsc::channel();
        st.subscriber = Some(tx);

        apply_probed_cwd(&mut st, Some(PathBuf::from("/Users/alice/dev/tty7")));

        assert_eq!(st.cwd, None);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn probed_cwd_absent_leaves_the_reported_cwd_alone() {
        let mut st = test_state(true);
        st.cwd = Some(PathBuf::from("/work"));
        let (tx, rx) = mpsc::channel();
        st.subscriber = Some(tx);

        apply_probed_cwd(&mut st, None);

        assert_eq!(st.cwd.as_deref(), Some(Path::new("/work")));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn attach_replays_state_in_order_and_installs_subscriber() {
        let mut st = test_state(true);
        st.ring.append(b"screen");
        st.cwd = Some(PathBuf::from("/work"));

        let (tx, rx) = mpsc::channel();
        let epoch = attach_subscriber(&mut st, tx);
        assert_eq!(epoch, 1);
        assert!(st.subscriber.is_some());
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Size(_))));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Snapshot(b)) if b == b"screen"));
        assert!(
            matches!(rx.try_recv(), Ok(DaemonMsg::Cwd(p)) if p.as_path() == std::path::Path::new("/work"))
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn attach_replays_initial_cwd_even_before_shell_reports_osc7() {
        let mut st = test_state(true);
        st.cwd = Some(PathBuf::from("/Users/alice/clone/tty7"));

        let (tx, rx) = mpsc::channel();
        attach_subscriber(&mut st, tx);

        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Size(_))));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Snapshot(_))));
        assert!(
            matches!(rx.try_recv(), Ok(DaemonMsg::Cwd(p)) if p == PathBuf::from("/Users/alice/clone/tty7"))
        );
    }

    #[test]
    fn attach_to_a_dead_pane_replays_exited() {
        let mut st = test_state(false);
        st.ring.append(b"final screen");

        let (tx, rx) = mpsc::channel();
        attach_subscriber(&mut st, tx);
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Size(_))));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Snapshot(_))));
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonMsg::Exited { code: None })
        ));
    }

    fn drain(rx: &mpsc::Receiver<DaemonMsg>) -> Vec<DaemonMsg> {
        let mut got = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            got.push(msg);
        }
        got
    }

    #[test]
    fn observe_replays_state_without_displacing_the_controller() {
        let mut st = test_state(true);
        st.ring.append(b"screen");
        st.cwd = Some(PathBuf::from("/work"));

        let (controller_tx, controller_rx) = mpsc::channel();
        let epoch = attach_subscriber(&mut st, controller_tx);
        drain(&controller_rx);

        let (observer_tx, observer_rx) = mpsc::channel();
        let id = observe_subscriber(&mut st, observer_tx, Arc::new(OutputGate::new()));
        assert_eq!(
            st.subscriber_epoch, epoch,
            "observing must not bump the controller epoch"
        );
        assert!(st.subscriber.is_some(), "the controller keeps its seat");

        assert!(matches!(observer_rx.try_recv(), Ok(DaemonMsg::Size(_))));
        assert!(matches!(observer_rx.try_recv(), Ok(DaemonMsg::Snapshot(b)) if b == b"screen"));
        assert!(
            matches!(observer_rx.try_recv(), Ok(DaemonMsg::Cwd(p)) if p == PathBuf::from("/work"))
        );
        assert!(
            controller_rx.try_recv().is_err(),
            "an observer joining must be invisible to the controller"
        );

        notify(&mut st, DaemonMsg::Output(b"tick".to_vec()));
        assert!(matches!(controller_rx.try_recv(), Ok(DaemonMsg::Output(b)) if b == b"tick"));
        assert!(matches!(observer_rx.try_recv(), Ok(DaemonMsg::Output(b)) if b == b"tick"));

        st.observers.retain(|obs| obs.id != id);
        notify(&mut st, DaemonMsg::Output(b"tock".to_vec()));
        assert!(matches!(controller_rx.try_recv(), Ok(DaemonMsg::Output(_))));
        assert!(
            observer_rx.try_recv().is_err(),
            "a departed observer hears nothing"
        );
    }

    #[test]
    fn a_new_controller_preempts_the_old_while_observers_survive() {
        let mut st = test_state(true);

        let (first_tx, first_rx) = mpsc::channel();
        let first_epoch = attach_subscriber(&mut st, first_tx);
        drain(&first_rx);

        let (observer_tx, observer_rx) = mpsc::channel();
        observe_subscriber(&mut st, observer_tx, Arc::new(OutputGate::new()));
        drain(&observer_rx);

        let (second_tx, second_rx) = mpsc::channel();
        let second_epoch = attach_subscriber(&mut st, second_tx);
        assert!(second_epoch > first_epoch);
        drain(&second_rx);

        notify(&mut st, DaemonMsg::Output(b"live".to_vec()));
        assert!(
            matches!(first_rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)),
            "the preempted controller's channel must be gone, exactly as before"
        );
        assert!(matches!(second_rx.try_recv(), Ok(DaemonMsg::Output(b)) if b == b"live"));
        assert!(
            matches!(observer_rx.try_recv(), Ok(DaemonMsg::Output(b)) if b == b"live"),
            "a controller handover must not evict read-only observers"
        );

        if st.subscriber_epoch == first_epoch {
            st.subscriber = None;
        }
        assert!(
            st.subscriber.is_some(),
            "a stale epoch's detach must not unseat the new controller"
        );
    }

    #[test]
    fn a_gone_observer_is_pruned_on_the_next_broadcast() {
        let mut st = test_state(true);
        let (observer_tx, observer_rx) = mpsc::channel();
        observe_subscriber(&mut st, observer_tx, Arc::new(OutputGate::new()));
        drop(observer_rx);

        notify(&mut st, DaemonMsg::Output(b"x".to_vec()));
        assert!(
            st.observers.is_empty(),
            "a dead observer must not accumulate"
        );
    }

    #[test]
    fn a_stalled_observer_is_dropped_at_its_budget_while_the_controller_streams_on() {
        let mut st = test_state(true);
        let (controller_tx, controller_rx) = mpsc::channel();
        attach_subscriber(&mut st, controller_tx);
        drain(&controller_rx);

        let (observer_tx, observer_rx) = mpsc::channel();
        observe_subscriber(&mut st, observer_tx, Arc::new(OutputGate::new()));
        drain(&observer_rx);

        let pane_gate = OutputGate::new();
        let chunk = vec![b'x'; 1024 * 1024];
        let sends = (OBSERVER_BUDGET / chunk.len() as i64) as usize + 2;
        for _ in 0..sends {
            fan_out_output(&mut st, &chunk, Vec::new(), &pane_gate);
            pane_gate.sub(chunk.len());
        }

        assert!(
            st.observers.is_empty(),
            "an observer past its budget must be pruned"
        );
        let mut controller_bytes = 0usize;
        while let Ok(DaemonMsg::Output(b)) = controller_rx.try_recv() {
            controller_bytes += b.len();
        }
        assert_eq!(
            controller_bytes,
            sends * chunk.len(),
            "the controller stream must stay complete"
        );
        let mut observer_bytes = 0i64;
        while let Ok(DaemonMsg::Output(b)) = observer_rx.try_recv() {
            observer_bytes += b.len() as i64;
        }
        assert!(
            observer_bytes <= OBSERVER_BUDGET,
            "a stalled observer must never hold more than its budget, held {observer_bytes}"
        );
    }

    #[test]
    fn a_draining_observer_under_the_cap_stays_subscribed() {
        let mut st = test_state(true);
        let (observer_tx, observer_rx) = mpsc::channel();
        let observer_gate = Arc::new(OutputGate::new());
        observe_subscriber(&mut st, observer_tx, observer_gate.clone());
        drain(&observer_rx);

        let pane_gate = OutputGate::new();
        let chunk = vec![b'y'; 1024 * 1024];
        let sends = (OBSERVER_BUDGET / chunk.len() as i64) as usize * 3;
        let mut got = 0usize;
        for _ in 0..sends {
            fan_out_output(&mut st, &chunk, Vec::new(), &pane_gate);
            pane_gate.sub(chunk.len());
            while let Ok(DaemonMsg::Output(b)) = observer_rx.try_recv() {
                observer_gate.sub(b.len());
                got += b.len();
            }
        }
        assert_eq!(
            st.observers.len(),
            1,
            "an observer that keeps draining must stay subscribed"
        );
        assert_eq!(got, sends * chunk.len(), "and must miss no bytes");
    }

    #[test]
    fn death_notifies_observers_but_only_controllers_defer_the_reap() {
        let with_observer_only = Arc::new(Mutex::new(test_state(true)));
        let (observer_tx, observer_rx) = mpsc::channel();
        observe_subscriber(
            &mut with_observer_only.lock().unwrap(),
            observer_tx,
            Arc::new(OutputGate::new()),
        );
        drain(&observer_rx);
        let (dead_tx, dead_rx) = mpsc::channel();
        DeathReporter::new(move || dead_tx.send(()).unwrap())
            .report(&with_observer_only, &AtomicBool::new(false));
        assert!(matches!(
            observer_rx.try_recv(),
            Ok(DaemonMsg::Exited { code: None })
        ));
        assert!(
            dead_rx.try_recv().is_ok(),
            "read-only observers must not keep a dead pane in the registry"
        );

        let with_both = Arc::new(Mutex::new(test_state(true)));
        let (controller_tx, controller_rx) = mpsc::channel();
        let (observer_tx, observer_rx) = mpsc::channel();
        {
            let mut st = with_both.lock().unwrap();
            attach_subscriber(&mut st, controller_tx);
            observe_subscriber(&mut st, observer_tx, Arc::new(OutputGate::new()));
        }
        drain(&controller_rx);
        drain(&observer_rx);
        let (dead_tx, dead_rx) = mpsc::channel();
        DeathReporter::new(move || dead_tx.send(()).unwrap())
            .report(&with_both, &AtomicBool::new(false));
        assert!(matches!(
            controller_rx.try_recv(),
            Ok(DaemonMsg::Exited { code: None })
        ));
        assert!(matches!(
            observer_rx.try_recv(),
            Ok(DaemonMsg::Exited { code: None })
        ));
        assert!(
            dead_rx.try_recv().is_err(),
            "an attached death is still the detach path's to reclaim"
        );
    }

    #[test]
    fn agent_state_snapshot_reports_only_panes_with_a_session() {
        use crate::core::cli_agent::{AgentSessionState, AgentStatus, CLIAgent};

        let mut st = test_state(true);
        st.id = 42;
        assert_eq!(agent_state_snapshot(&st), None);

        st.agent = Some(CLIAgent::Claude);
        assert_eq!(
            agent_state_snapshot(&st),
            None,
            "a detected agent without session state is not yet a fact worth listing"
        );

        st.agent_session = Some(AgentSessionState {
            status: AgentStatus::Waiting,
            session_id: Some("sess-1".into()),
            ..Default::default()
        });
        let snap = agent_state_snapshot(&st).expect("a session state is the fact");
        assert_eq!(snap.pane_id, 42);
        assert_eq!(snap.agent, Some(CLIAgent::Claude));
        assert_eq!(snap.state.status, AgentStatus::Waiting);
        assert_eq!(snap.state.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn reader_eof_with_subscriber_sends_exited_not_on_dead() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = mpsc::channel();
        state.lock().unwrap().subscriber = Some(sub_tx);
        let dead = Arc::new(AtomicBool::new(false));
        let dead_flag = dead.clone();

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(b"tail".to_vec())),
            null_writer(),
            || false,
            ForegroundProbes {
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(move || {
                dead_flag.store(true, Ordering::SeqCst)
            })),
        );
        handle.join().unwrap();

        assert!(!state.lock().unwrap().alive);
        assert_eq!(state.lock().unwrap().ring.flatten(), b"tail");
        assert!(matches!(sub_rx.try_recv(), Ok(DaemonMsg::Output(b)) if b == b"tail"));
        assert!(matches!(
            sub_rx.try_recv(),
            Ok(DaemonMsg::Exited { code: None })
        ));
        assert!(
            !dead.load(Ordering::SeqCst),
            "an attached death is the detach path's to reclaim, not on_dead's"
        );
    }

    #[cfg(windows)]
    #[test]
    fn closing_conpty_waits_for_delayed_reader_output_before_exit() {
        /// A master whose destruction opens the simulated ConPTY output pipe.
        /// The production `ConPtyMasterPty` performs the equivalent transition
        /// by calling `ClosePseudoConsole` from its destructor.
        struct SignallingMaster {
            released: Arc<(Mutex<bool>, Condvar)>,
        }

        impl Drop for SignallingMaster {
            fn drop(&mut self) {
                let (lock, ready) = &*self.released;
                *lock.lock().unwrap() = true;
                ready.notify_all();
            }
        }

        impl MasterPty for SignallingMaster {
            fn resize(&self, _size: PtySize) -> anyhow::Result<()> {
                Ok(())
            }

            fn get_size(&self) -> anyhow::Result<PtySize> {
                Ok(PtySize::default())
            }

            fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send>> {
                Ok(Box::new(std::io::empty()))
            }

            fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send>> {
                Ok(Box::new(std::io::sink()))
            }
        }

        /// Models a reader that stays delayed well past any plausible grace
        /// period, then receives a final output chunk followed by EOF.
        struct DelayedTailReader {
            released: Arc<(Mutex<bool>, Condvar)>,
            tail: std::io::Cursor<Vec<u8>>,
            delayed: bool,
        }

        impl Read for DelayedTailReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if !self.delayed {
                    let (lock, ready) = &*self.released;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = ready.wait(released).unwrap();
                    }
                    drop(released);
                    std::thread::sleep(Duration::from_millis(800));
                    self.delayed = true;
                }
                self.tail.read(buf)
            }
        }

        let released = Arc::new((Mutex::new(false), Condvar::new()));
        let master: Mutex<Option<Box<dyn MasterPty + Send>>> =
            Mutex::new(Some(Box::new(SignallingMaster {
                released: released.clone(),
            })));
        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = mpsc::channel();
        state.lock().unwrap().subscriber = Some(sub_tx);

        let reader = DaemonPane::spawn_reader(
            state,
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(DelayedTailReader {
                released,
                tail: std::io::Cursor::new(b"final output".to_vec()),
                delayed: false,
            }),
            null_writer(),
            || false,
            ForegroundProbes {
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(|| {})),
        );

        close_pty_master(&master);
        assert!(
            sub_rx.recv_timeout(Duration::from_millis(600)).is_err(),
            "closing the master must not publish Exited while the reader is still delayed"
        );
        assert!(matches!(
            sub_rx.recv_timeout(Duration::from_secs(1)),
            Ok(DaemonMsg::Output(bytes)) if bytes == b"final output"
        ));
        assert!(matches!(
            sub_rx.recv_timeout(Duration::from_secs(1)),
            Ok(DaemonMsg::Exited { code: None })
        ));
        assert!(sub_rx.try_recv().is_err(), "no output may follow Exited");
        reader.join().unwrap();
    }

    /// An inert stand-in for the ConPTY master: releasing it is instant, which
    /// is what `ClosePseudoConsole` does once the pipe has no pending bytes.
    #[cfg(windows)]
    struct InertMaster;

    #[cfg(windows)]
    impl MasterPty for InertMaster {
        fn resize(&self, _size: PtySize) -> anyhow::Result<()> {
            Ok(())
        }

        fn get_size(&self) -> anyhow::Result<PtySize> {
            Ok(PtySize::default())
        }

        fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send>> {
            Ok(Box::new(std::io::empty()))
        }

        fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send>> {
            Ok(Box::new(std::io::sink()))
        }
    }

    #[cfg(windows)]
    fn exit_drain_fixture() -> (
        Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
        Arc<Mutex<PaneState>>,
        Arc<AtomicBool>,
        Arc<DeathReporter>,
        mpsc::Receiver<DaemonMsg>,
    ) {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = mpsc::channel();
        state.lock().unwrap().subscriber = Some(sub_tx);
        (
            Arc::new(Mutex::new(Some(
                Box::new(InertMaster) as Box<dyn MasterPty + Send>
            ))),
            state,
            Arc::new(AtomicBool::new(false)),
            Arc::new(DeathReporter::new(|| {})),
            sub_rx,
        )
    }

    /// A grandchild that inherited the ConPTY output pipe keeps it open after
    /// the shell is gone, so the reader never sees EOF and can never publish
    /// the exit. The monitor's drain window is the only thing that stops such
    /// a pane from reading as alive forever.
    #[cfg(windows)]
    #[test]
    fn a_pty_that_never_reaches_eof_still_reports_the_exit() {
        struct NeverEofReader;

        impl Read for NeverEofReader {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                // Long enough to outlast the window below, bounded so the
                // thread cannot outlive the test binary.
                std::thread::sleep(Duration::from_secs(5));
                Ok(0)
            }
        }

        let (master, state, shutting_down, death, sub_rx) = exit_drain_fixture();
        let _reader = DaemonPane::spawn_reader(
            state.clone(),
            shutting_down.clone(),
            Arc::new(OutputGate::new()),
            Box::new(NeverEofReader),
            null_writer(),
            || false,
            ForegroundProbes {
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            death.clone(),
        );

        drain_then_report(
            master,
            state,
            shutting_down,
            death,
            Duration::from_millis(50),
        );
        assert!(
            matches!(
                sub_rx.recv_timeout(Duration::from_secs(1)),
                Ok(DaemonMsg::Exited { code: None })
            ),
            "the monitor must report the exit once its drain window elapses"
        );
    }

    /// The ordinary case: EOF arrives, the reader reports, and the monitor
    /// neither waits out its window nor publishes a second `Exited`.
    #[cfg(windows)]
    #[test]
    fn the_exit_monitor_defers_to_the_reader_that_saw_eof() {
        let (master, state, shutting_down, death, sub_rx) = exit_drain_fixture();
        let reader = DaemonPane::spawn_reader(
            state.clone(),
            shutting_down.clone(),
            Arc::new(OutputGate::new()),
            Box::new(std::io::empty()),
            null_writer(),
            || false,
            ForegroundProbes {
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            death.clone(),
        );

        let started = std::time::Instant::now();
        drain_then_report(master, state, shutting_down, death, Duration::from_secs(30));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the monitor must stop waiting as soon as the reader has reported"
        );
        assert!(matches!(
            sub_rx.recv_timeout(Duration::from_secs(1)),
            Ok(DaemonMsg::Exited { code: None })
        ));
        assert!(
            sub_rx.try_recv().is_err(),
            "the idempotent reporter must not publish a second Exited"
        );
        reader.join().unwrap();
    }

    /// Issue #213 end-to-end at the reader: a chunk carrying text plus a
    /// kitty graphics query and a transmit-and-display must (1) keep only the
    /// text in the replay ring and the `Output` frame, (2) write the `a=q` reply
    /// back to the PTY writer, and (3) forward the image out-of-band as an
    /// `Image` frame the client can decode.
    #[test]
    fn reader_strips_graphics_and_forwards_them_out_of_band() {
        use crate::core::kitty_graphics::Image;
        use base64::Engine as _;

        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = mpsc::channel();
        state.lock().unwrap().subscriber = Some(sub_tx);

        // A writer we can read back, to prove the query reply reached the PTY.
        #[derive(Clone)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl Write for SharedBuf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = Arc::new(Mutex::new(Vec::new()));
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(Box::new(SharedBuf(sink.clone()))));

        // One 1x1 opaque-red RGBA pixel, transmitted-and-displayed inline.
        let pixel = [0xffu8, 0x00, 0x00, 0xff];
        let b64 = base64::engine::general_purpose::STANDARD.encode(pixel);
        let stream = format!(
            "before\x1b_Gi=1,a=q,t=d,f=32,s=1,v=1;AAAA\x1b\\\
             mid\x1b_Ga=T,f=32,t=d,s=1,v=1,i=1;{b64}\x1b\\after"
        );

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(stream.into_bytes())),
            writer,
            || false,
            ForegroundProbes {
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(|| {})),
        );
        handle.join().unwrap();

        // (1) The ring holds the text, none of the graphics bytes.
        assert_eq!(state.lock().unwrap().ring.flatten(), b"beforemidafter");
        // (2) The query reply went to the PTY writer.
        assert_eq!(sink.lock().unwrap().as_slice(), b"\x1b_Gi=1;OK\x1b\\");
        // (3) The subscriber sees the passthrough and the image *in stream order*:
        // the text before the image, then the image at its cursor cell, then the
        // text after it. The `a=q` reply splits "before" from "mid" into two
        // Output frames; the image sits between "mid" and "after".
        assert!(matches!(sub_rx.try_recv(), Ok(DaemonMsg::Output(b)) if b == b"before"));
        assert!(matches!(sub_rx.try_recv(), Ok(DaemonMsg::Output(b)) if b == b"mid"));
        match sub_rx.try_recv() {
            Ok(DaemonMsg::Image(frame)) => {
                let img = Image::decode_frame(&frame).expect("decodable image frame");
                assert_eq!(img.id, 1);
                assert_eq!((img.width, img.height), (1, 1));
                assert_eq!(img.to_rgba8().unwrap(), pixel);
            }
            other => panic!("expected Image frame, got {other:?}"),
        }
        assert!(matches!(sub_rx.try_recv(), Ok(DaemonMsg::Output(b)) if b == b"after"));
    }

    /// A kitty delete (`a=d`) is lifted out and forwarded as a `DeleteImage`
    /// selector, leaving the surrounding text intact in the ring.
    #[test]
    fn reader_forwards_graphics_deletes() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = mpsc::channel();
        state.lock().unwrap().subscriber = Some(sub_tx);

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(b"x\x1b_Ga=d,d=A\x1b\\y".to_vec())),
            null_writer(),
            || false,
            ForegroundProbes {
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(|| {})),
        );
        handle.join().unwrap();

        assert_eq!(state.lock().unwrap().ring.flatten(), b"xy");
        // In stream order: text before the delete, the delete, then text after.
        assert!(matches!(sub_rx.try_recv(), Ok(DaemonMsg::Output(b)) if b == b"x"));
        match sub_rx.try_recv() {
            Ok(DaemonMsg::DeleteImage(sel)) => {
                let d = ImageDelete::decode(&sel).unwrap();
                assert_eq!(d.target, b'A');
            }
            other => panic!("expected DeleteImage, got {other:?}"),
        }
        assert!(matches!(sub_rx.try_recv(), Ok(DaemonMsg::Output(b)) if b == b"y"));
    }

    #[test]
    fn push_image_frame_drops_over_max_frame() {
        let mut frames = Vec::new();
        // At the limit: queued.
        push_image_frame(&mut frames, vec![0u8; MAX_FRAME]);
        // Over the limit: dropped, so the writer's fatal `write_frame` error
        // (which would disconnect the client) is never reached.
        push_image_frame(&mut frames, vec![0u8; MAX_FRAME + 1]);
        assert_eq!(frames.len(), 1, "only the in-budget frame is queued");
        assert!(matches!(&frames[0], GraphicsFrame::Image(f) if f.len() == MAX_FRAME));
    }

    #[test]
    fn reader_poll_applies_the_probed_cwd() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = mpsc::channel();
        {
            let mut st = state.lock().unwrap();
            st.cwd = Some(PathBuf::from("/Users/alice"));
            st.subscriber = Some(sub_tx);
        }

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(b"alice@host ~ % ".to_vec())),
            null_writer(),
            || false,
            ForegroundProbes {
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| Some(PathBuf::from("/Users/alice/dev/tty7"))),
            },
            Arc::new(DeathReporter::new(|| {})),
        );
        handle.join().unwrap();

        assert_eq!(
            state.lock().unwrap().cwd.as_deref(),
            Some(Path::new("/Users/alice/dev/tty7"))
        );
        assert!(matches!(sub_rx.try_recv(), Ok(DaemonMsg::Output(_))));
        assert!(
            matches!(sub_rx.try_recv(), Ok(DaemonMsg::Cwd(p)) if p == PathBuf::from("/Users/alice/dev/tty7"))
        );
    }

    #[test]
    fn reader_eof_without_subscriber_fires_on_dead() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (dead_tx, dead_rx) = mpsc::channel();

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(Vec::new())),
            null_writer(),
            || false,
            ForegroundProbes {
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(move || dead_tx.send(()).unwrap())),
        );
        handle.join().unwrap();

        assert!(!state.lock().unwrap().alive);
        assert!(dead_rx.try_recv().is_ok(), "unattached death → on_dead");
    }

    #[test]
    fn reader_eof_during_shutdown_is_silent() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let dead = Arc::new(AtomicBool::new(false));
        let dead_flag = dead.clone();

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(true)),
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(Vec::new())),
            null_writer(),
            || false,
            ForegroundProbes {
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(move || {
                dead_flag.store(true, Ordering::SeqCst)
            })),
        );
        handle.join().unwrap();

        assert!(!state.lock().unwrap().alive);
        assert!(!dead.load(Ordering::SeqCst));
    }

    #[test]
    fn death_reporter_notifies_once_across_racing_callers() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = mpsc::channel();
        state.lock().unwrap().subscriber = Some(sub_tx);
        let shutting_down = AtomicBool::new(false);
        let calls = Arc::new(AtomicBool::new(false));
        let calls_flag = calls.clone();
        let death = DeathReporter::new(move || calls_flag.store(true, Ordering::SeqCst));

        death.report(&state, &shutting_down);
        death.report(&state, &shutting_down);

        assert!(!state.lock().unwrap().alive);
        assert!(matches!(
            sub_rx.try_recv(),
            Ok(DaemonMsg::Exited { code: None })
        ));
        assert!(
            sub_rx.try_recv().is_err(),
            "a second report must not re-notify"
        );
    }

    #[test]
    fn death_reporter_fires_on_dead_at_most_once() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let shutting_down = AtomicBool::new(false);
        let (dead_tx, dead_rx) = mpsc::channel();
        let death = DeathReporter::new(move || dead_tx.send(()).unwrap());

        death.report(&state, &shutting_down);
        death.report(&state, &shutting_down);

        assert!(dead_rx.try_recv().is_ok(), "unattached death → on_dead");
        assert!(dead_rx.try_recv().is_err(), "on_dead must fire only once");
    }

    #[test]
    fn pane_environment_advertises_the_terminal_under_the_standard_names() {
        let env: std::collections::HashMap<_, _> = pane_environment(
            &std::collections::HashMap::new(),
            false,
            7,
            Some("ws-main"),
            "/bin/zsh",
        )
        .into_iter()
        .collect();
        let version = env!("CARGO_PKG_VERSION");

        assert_eq!(env.get("TERM_PROGRAM").map(String::as_str), Some("tty7"));
        assert_eq!(
            env.get("TERM_PROGRAM_VERSION").map(String::as_str),
            Some(version)
        );
        assert_eq!(
            env.get(crate::core::agent_hooks::TTY7_ENV_MARKER)
                .map(String::as_str),
            Some(version)
        );
        assert_eq!(
            env.get("TERM").map(String::as_str),
            Some("xterm-256color"),
            "terminfo name is what the pane's decoder actually implements"
        );
    }

    #[test]
    fn pane_environment_hands_the_shell_its_own_address() {
        let env: std::collections::HashMap<_, _> = pane_environment(
            &std::collections::HashMap::new(),
            false,
            42,
            Some("ws-main"),
            "/bin/zsh",
        )
        .into_iter()
        .collect();

        assert_eq!(
            env.get(TTY7_PANE_ENV).map(String::as_str),
            Some("42"),
            "a CLI inside the pane needs its own pane id for address-free verbs"
        );
        assert_eq!(
            env.get(TTY7_WS_ENV).map(String::as_str),
            Some("ws-main"),
            "the workspace the spawn was filed under rides into the shell"
        );
        match config_dir_env() {
            Some(dir) => assert_eq!(
                env.get(TTY7_CONFIG_DIR_ENV),
                Some(&dir),
                "the shell is told which config dir this server runs on, so a CLI \
                 there resolves both endpoints the same way the server opened them"
            ),
            None => assert!(
                !env.contains_key(TTY7_CONFIG_DIR_ENV),
                "no resolvable config dir must not inject an empty TTY7_CONFIG_DIR"
            ),
        }

        let unfiled: std::collections::HashMap<_, _> = pane_environment(
            &std::collections::HashMap::new(),
            false,
            42,
            None,
            "/bin/zsh",
        )
        .into_iter()
        .collect();
        assert!(
            !unfiled.contains_key(TTY7_WS_ENV),
            "a pane outside any workspace must not claim one"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pane_environment_names_the_shell_the_pane_actually_runs() {
        // The reported bug: a pane running fish still advertised the passwd
        // login shell, so tmux, `sudo -s` and agents inside it all picked zsh.
        let env: std::collections::HashMap<_, _> = pane_environment(
            &std::collections::HashMap::new(),
            false,
            1,
            None,
            "/opt/homebrew/bin/fish",
        )
        .into_iter()
        .collect();

        assert_eq!(
            env.get(SHELL_ENV).map(String::as_str),
            Some("/opt/homebrew/bin/fish"),
            "$SHELL must name the shell the pane is actually running"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unconfigured_pane_still_announces_the_shell_it_runs() {
        // No configured shell means the pane runs the login shell, and naming
        // it is both harmless and correct: it is what the pane is running.
        let cmd = build_shell_command(None, &Some(PathBuf::from("/tmp")), 1, None)
            .expect("build default shell command")
            .0;
        let shell = cmd
            .get_env(SHELL_ENV)
            .and_then(|v| v.to_str())
            .map(str::to_string)
            .expect("the default shell resolves to an absolute path");

        assert!(
            shell.starts_with('/'),
            "$SHELL must be an absolute path, got {shell}"
        );
        assert_eq!(
            shell,
            default_shell_name(&CommandBuilder::new_default_prog()),
            "an unconfigured pane announces the very shell it was about to run"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_configured_shell_variable_outranks_the_shell_tty7_launched() {
        // Same precedence contract as TERM_PROGRAM: tty7 describes the pane,
        // the user's own `env` block gets the last word.
        let configured = [("SHELL".to_string(), "/bin/ksh".to_string())]
            .into_iter()
            .collect();

        let applied: std::collections::HashMap<_, _> =
            pane_environment(&configured, false, 1, None, "/opt/homebrew/bin/fish")
                .into_iter()
                .collect();

        assert_eq!(applied.get(SHELL_ENV).map(String::as_str), Some("/bin/ksh"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_panes_are_left_without_a_shell_variable() {
        let applied = pane_environment(
            &std::collections::HashMap::new(),
            false,
            1,
            None,
            r"C:\Program Files\PowerShell\7\pwsh.exe",
        );

        assert!(
            !applied
                .iter()
                .any(|(key, _)| key.eq_ignore_ascii_case(SHELL_ENV)),
            "$SHELL is a POSIX contract: the MSYS, Cygwin and WSL tools that \
             read it on Windows want a POSIX path, and a Windows one would \
             point them at a shell they cannot exec"
        );
    }

    #[test]
    fn shell_env_path_only_ever_answers_an_absolute_path() {
        let never = |_: &str| false;
        let path = || Some("/usr/local/bin:/usr/bin:relative/bin".to_string());

        assert_eq!(
            shell_env_path("/opt/homebrew/bin/fish", path, never).as_deref(),
            Some("/opt/homebrew/bin/fish"),
            "an absolute program is already the answer, PATH untouched"
        );
        assert_eq!(
            shell_env_path("fish", path, |c| c == "/usr/local/bin/fish").as_deref(),
            Some("/usr/local/bin/fish"),
            "a bare command must be resolved the way exec would resolve it"
        );
        assert_eq!(
            shell_env_path("fish", path, never),
            None,
            "an unresolvable bare command must leave $SHELL alone rather than \
             hand a consumer a name it may resolve differently"
        );
        assert_eq!(
            shell_env_path("./fish", path, |_| true),
            None,
            "a cwd-relative program is meaningless to a process with another cwd"
        );
        assert_eq!(shell_env_path("  ", path, |_| true), None);
        assert_eq!(
            shell_env_path("fish", || None, |_| true),
            None,
            "no PATH to search means nothing to promise"
        );
    }

    #[test]
    fn the_shell_we_announce_is_the_argv_we_are_about_to_exec() {
        let mut configured = CommandBuilder::new("/opt/homebrew/bin/fish");
        configured.args(["-l"]);
        assert_eq!(
            launched_shell_program(&configured, "/bin/zsh"),
            "/opt/homebrew/bin/fish",
            "a builder with an argv has already overruled whatever tty7 resolved"
        );

        assert_eq!(
            launched_shell_program(&CommandBuilder::new_default_prog(), "/bin/zsh"),
            "/bin/zsh",
            "an untouched default prog is exactly what resolved_program describes"
        );
    }

    #[test]
    fn shell_env_path_ignores_relative_path_entries() {
        // A relative PATH entry resolves against the *pane's* cwd, which is not
        // ours, so it can never yield the absolute path $SHELL must hold.
        assert_eq!(
            shell_env_path("fish", || Some("bin:.".to_string()), |_| true),
            None
        );
    }

    #[test]
    fn pane_environment_lets_configured_env_override_identity_but_not_capability() {
        let configured = [
            ("TERM_PROGRAM", "iTerm.app"),
            ("TERM_PROGRAM_VERSION", "3.5.0"),
            ("TERM", "dumb"),
            ("COLORTERM", ""),
            ("EDITOR", "hx"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();

        let applied: std::collections::HashMap<_, _> =
            pane_environment(&configured, false, 1, None, "/bin/zsh")
                .into_iter()
                .collect();

        assert_eq!(
            applied.get("TERM_PROGRAM").map(String::as_str),
            Some("iTerm.app")
        );
        assert_eq!(
            applied.get("TERM_PROGRAM_VERSION").map(String::as_str),
            Some("3.5.0")
        );
        assert_eq!(applied.get("EDITOR").map(String::as_str), Some("hx"));
        assert_eq!(
            applied.get("TERM").map(String::as_str),
            Some("xterm-256color")
        );
        assert_eq!(
            applied.get("COLORTERM").map(String::as_str),
            Some("truecolor")
        );
    }

    #[cfg(windows)]
    #[test]
    fn pane_environment_advertises_light_and_dark_backgrounds() {
        let empty = std::collections::HashMap::new();
        let light: std::collections::HashMap<_, _> =
            pane_environment(&empty, false, 1, None, "pwsh.exe")
                .into_iter()
                .collect();
        let dark: std::collections::HashMap<_, _> =
            pane_environment(&empty, true, 1, None, "pwsh.exe")
                .into_iter()
                .collect();

        assert_eq!(light.get("COLORFGBG").map(String::as_str), Some("0;15"));
        assert_eq!(dark.get("COLORFGBG").map(String::as_str), Some("15;0"));
    }

    #[cfg(windows)]
    #[test]
    fn configured_colorfgbg_wins_case_insensitively() {
        let configured = [("ColorFgBg".to_string(), "3;4".to_string())]
            .into_iter()
            .collect();
        let applied = pane_environment(&configured, false, 1, None, "pwsh.exe");

        assert!(!applied.iter().any(|(key, _)| key == "COLORFGBG"));
        assert!(
            applied
                .iter()
                .any(|(key, value)| key == "ColorFgBg" && value == "3;4")
        );
    }

    #[cfg(windows)]
    #[test]
    fn pane_environment_capability_keys_cannot_be_overridden_by_recasing() {
        let configured = [("Term", "dumb"), ("ColorTerm", ""), ("term_program", "x")]
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();

        let applied = pane_environment(&configured, false, 1, None, "pwsh.exe");

        assert!(
            !applied.iter().any(|(k, _)| k == "Term" || k == "ColorTerm"),
            "a recased capability key must be filtered out, or it would land \
             in the same case-folded slot and win by coming later"
        );
        let get = |key: &str| {
            applied
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("TERM"), Some("xterm-256color"));
        assert_eq!(get("COLORTERM"), Some("truecolor"));
        assert_eq!(get("term_program"), Some("x"));
    }

    #[test]
    fn locale_fallback_respects_inherited_and_configured_environments() {
        let environment = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<std::collections::HashMap<_, _>>()
        };
        let fallback_is_needed = |extra: &[(&str, &str)], parent: &[(&str, &str)]| {
            let extra = environment(extra);
            let parent = environment(parent);
            locale_fallback_is_needed(&extra, |key| parent.get(key).cloned())
        };

        assert!(fallback_is_needed(&[], &[]));
        assert!(fallback_is_needed(&[], &[("LANG", "")]));
        assert!(!fallback_is_needed(&[], &[("LANG", "en_US.UTF-8")]));
        assert!(!fallback_is_needed(&[], &[("LC_ALL", "C")]));
        assert!(!fallback_is_needed(&[("LC_CTYPE", "UTF-8")], &[]));
        assert!(!fallback_is_needed(&[("LC_CTYPE", "")], &[]));
        assert!(!fallback_is_needed(
            &[("LANG", "")],
            &[("LANG", "en_US.UTF-8")]
        ));
    }

    #[test]
    fn locale_fallback_sets_a_variable_that_backs_every_category() {
        let injected =
            std::iter::once((LOCALE_FALLBACK_KEY.to_string(), "en_US.UTF-8".to_string()))
                .collect::<std::collections::HashMap<_, _>>();

        // Whatever we inject has to satisfy the check that gated it, or every
        // pane would keep re-deriving a fallback that is already in place.
        assert!(!locale_fallback_is_needed(&injected, |_| None));

        // And it has to back every category, not just character handling — see
        // LOCALE_FALLBACK_KEY. LC_CTYPE here would leave a shell warning about
        // LC_COLLATE and friends on every launch.
        assert_eq!(LOCALE_FALLBACK_KEY, "LANG");
    }

    #[test]
    fn posix_locale_stem_drops_script_and_keyword_subtags() {
        let stem = |id: &str| posix_locale_stem(id);

        assert_eq!(stem("en_US").as_deref(), Some("en_US"));
        assert_eq!(stem("en-US").as_deref(), Some("en_US"));
        assert_eq!(stem("zh_Hans_CN").as_deref(), Some("zh_CN"));
        assert_eq!(
            stem("zh_Hans_CN@calendar=gregorian").as_deref(),
            Some("zh_CN")
        );
        assert_eq!(stem("es_419").as_deref(), Some("es_419"));
        assert_eq!(stem("EN_us").as_deref(), Some("en_US"));

        assert_eq!(stem("zh"), None);
        assert_eq!(stem("zh_Hans"), None);
        assert_eq!(stem(""), None);
        assert_eq!(stem("@calendar=gregorian"), None);
    }

    #[test]
    fn character_locale_only_returns_installed_locales() {
        let installed = |names: &'static [&'static str]| move |n: &str| names.contains(&n);

        assert_eq!(
            character_locale(Some("zh_Hans_CN"), installed(&["zh_CN.UTF-8", "C.UTF-8"])),
            Some("zh_CN.UTF-8".to_string())
        );
        assert_eq!(
            character_locale(Some("en_CN"), installed(&["C.UTF-8", "en_US.UTF-8"])),
            Some("C.UTF-8".to_string())
        );
        assert_eq!(
            character_locale(Some("en_CN"), installed(&["en_US.UTF-8"])),
            Some("en_US.UTF-8".to_string())
        );
        assert_eq!(
            character_locale(None, installed(&["C.UTF-8", "en_US.UTF-8"])),
            Some("C.UTF-8".to_string())
        );
        assert_eq!(character_locale(Some("zh_Hans_CN"), installed(&[])), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn derived_character_locale_is_installed_on_this_machine() {
        let exists = |name: &str| {
            std::path::Path::new(LOCALE_DEFINITION_DIR)
                .join(name)
                .is_dir()
        };
        let locale = character_locale(system_locale_identifier().as_deref(), exists)
            .expect("macOS always ships en_US.UTF-8");
        assert!(exists(&locale), "derived {locale} is not installed");
        assert!(
            locale.ends_with(".UTF-8"),
            "derived {locale} is not a UTF-8 locale"
        );
    }

    #[test]
    fn spawned_shell_carries_the_tty7_marker() {
        let cmd = build_shell_command(None, &Some(PathBuf::from("/tmp")), 42, Some("ws-main"))
            .expect("build default shell command")
            .0;
        let tty7 = cmd
            .get_env(crate::core::agent_hooks::TTY7_ENV_MARKER)
            .and_then(|v| v.to_str());
        assert_eq!(
            tty7,
            Some(env!("CARGO_PKG_VERSION")),
            "the daemon must inject TTY7 into every spawned shell"
        );
        assert_eq!(
            cmd.get_env(TTY7_PANE_ENV).and_then(|v| v.to_str()),
            Some("42"),
            "the daemon must tell every spawned shell which pane it is"
        );
        assert_eq!(
            cmd.get_env(TTY7_WS_ENV).and_then(|v| v.to_str()),
            Some("ws-main"),
            "the daemon must tell every spawned shell which workspace filed it"
        );
        if let Some(dir) = config_dir_env() {
            assert_eq!(
                cmd.get_env(TTY7_CONFIG_DIR_ENV).and_then(|v| v.to_str()),
                Some(dir.as_str()),
                "the daemon must tell every spawned shell which config dir it serves"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_two_endpoints_are_siblings_derived_from_one_config_dir() {
        // The bug this pins: the control socket used to ignore the config dir
        // and sit in ~/.local/share/tty7 as `daemon.sock`, the same basename the
        // pane socket uses. A `--config-dir` server therefore published someone
        // else's control endpoint, and anything deriving one path from the other
        // by filename got the wrong socket. Both must come from the config dir,
        // and they must not collide.
        let dir = std::path::Path::new("/tmp/tty7-endpoint-test");
        let pane = crate::daemon::transport::socket_path_for(dir);
        let control = crate::host::server::socket_path_in(dir, &[]).expect("short enough path");

        assert_eq!(pane.parent(), control.parent(), "one directory, two files");
        assert_ne!(
            pane.file_name(),
            control.file_name(),
            "same basename is what made them indistinguishable"
        );
        assert_eq!(pane, dir.join("daemon.sock"));
        assert_eq!(control, dir.join("control.sock"));
    }

    #[test]
    fn apply_signals_updates_state() {
        let mut st = test_state(true);

        apply_signals(
            &mut st,
            SniffSignals {
                cwd: Some(PathBuf::from("/tmp/x")),
                ..SniffSignals::default()
            },
        );
        assert_eq!(st.cwd, Some(PathBuf::from("/tmp/x")));

        apply_signals(
            &mut st,
            SniffSignals {
                shell: vec![ShellState {
                    active: true,
                    at_prompt: true,
                    last_exit_code: Some(0),
                    command: None,
                }],
                ..SniffSignals::default()
            },
        );
        assert!(st.shell.active && st.shell.at_prompt);
        assert_eq!(st.shell.last_exit_code, Some(0));

        apply_signals(&mut st, SniffSignals::default());
        assert_eq!(st.cwd, Some(PathBuf::from("/tmp/x")));
    }
}

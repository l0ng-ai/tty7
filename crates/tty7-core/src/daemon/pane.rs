use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::core::osc::OscTokenizer;
use crate::daemon::protocol::{
    AuthResponse, DaemonMsg, NativeSshSpec, PaneInfo, RemoteContext, RemoteKind, ShellSpec, WinSize,
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
    cwd: Option<PathBuf>,
    shell: Option<ShellSpec>,
) -> anyhow::Result<SpawnConfig> {
    let initial_cwd = initial_working_directory(cwd);
    let configured = choose_shell(shell, crate::core::config::shell_command());
    let remote = wsl_remote_context(configured.as_ref());
    let (cmd, integration_dir) = build_shell_command(configured, &initial_cwd)?;
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
    apply_common_command_setup(&mut cmd, initial_cwd);
    Ok((cmd, integration_dir))
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
    ];
    env.extend(
        extra_env
            .iter()
            .filter(|(k, _)| !names_capability_env(k))
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    env
}

fn apply_common_command_setup(cmd: &mut CommandBuilder, initial_cwd: &Option<PathBuf>) {
    if let Some(dir) = initial_cwd {
        cmd.cwd(dir);
    }
    let extra_env = crate::core::config::extra_env();
    for (k, v) in pane_environment(&extra_env) {
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
        cmd.env("LC_CTYPE", locale);
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

struct PaneState {
    id: u64,
    ring: ReplayRing,
    subscriber: Option<Sender<DaemonMsg>>,
    subscriber_epoch: u64,
    cwd: Option<PathBuf>,
    shell: ShellState,
    remote: Option<RemoteContext>,
    agent: Option<crate::core::cli_agent::CLIAgent>,
    agent_argv: Option<Vec<String>>,
    agent_session: Option<crate::core::cli_agent::AgentSessionState>,
    alive: bool,
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
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
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
    writer: Mutex<Box<dyn Write + Send>>,
    shutting_down: Arc<AtomicBool>,
    gate: Arc<OutputGate>,
    state: Arc<Mutex<PaneState>>,
    reader: Mutex<Option<JoinHandle<()>>>,
    broker: Option<Arc<crate::daemon::ssh::PromptBroker>>,
}

struct DeathReporter {
    reported: AtomicBool,
    on_dead: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl DeathReporter {
    fn new(on_dead: impl FnOnce() + Send + 'static) -> Self {
        Self {
            reported: AtomicBool::new(false),
            on_dead: Mutex::new(Some(Box::new(on_dead))),
        }
    }

    fn report(&self, state: &Mutex<PaneState>, shutting_down: &AtomicBool) {
        if self.reported.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut st = state.lock().unwrap();
        st.alive = false;
        let pane = st.id;
        if shutting_down.load(Ordering::SeqCst) {
            drop(st);
            crate::core::machine::observe_pane(pane, |p| p.live = false);
            return;
        }
        let subscribed = st.subscriber.is_some();
        if let Some(sub) = &st.subscriber {
            let _ = sub.send(DaemonMsg::Exited { code: None });
        }
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

impl DaemonPane {
    pub fn spawn(
        id: u64,
        cwd: Option<PathBuf>,
        size: WinSize,
        shell: Option<ShellSpec>,
        owner: Option<String>,
        on_dead: impl FnOnce() + Send + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        let pty_size = pty_size(size);

        let pair = native_pty_system().openpty(pty_size)?;
        let spawn = build_spawn_config(cwd, shell)?;

        let child = pair.slave.spawn_command(spawn.cmd)?;
        let shell_pid = child.process_id();

        drop(pair.slave);

        let reader_handle = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let state = Arc::new(Mutex::new(PaneState {
            id,
            ring: ReplayRing::new(size),
            subscriber: None,
            subscriber_epoch: 0,
            cwd: spawn.initial_cwd,
            shell: ShellState::default(),
            remote: spawn.remote.clone(),
            agent: None,
            agent_session: None,
            agent_argv: None,
            alive: true,
        }));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(OutputGate::new());

        let master = Arc::new(Mutex::new(pair.master));

        let pane = Arc::new(Self {
            id,
            owner,
            backend: PaneBackend::Pty(PtyBackend {
                master: master.clone(),
                child: Mutex::new(child),
                shell_pid,
                integration_dir: spawn.integration_dir,
            }),
            writer: Mutex::new(writer),
            shutting_down: shutting_down.clone(),
            gate: gate.clone(),
            state: state.clone(),
            reader: Mutex::new(None),
            broker: None,
        });

        let death = Arc::new(DeathReporter::new(on_dead));

        #[cfg(windows)]
        Self::spawn_exit_monitor(
            shell_pid,
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
        let writer: Box<dyn Write + Send> = Box::new(bridge.writer);
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
            cwd: None,
            shell: ShellState::default(),
            remote: Some(remote),
            agent: None,
            agent_session: None,
            agent_argv: None,
            alive: true,
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
            writer: Mutex::new(writer),
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
                        eprintln!(
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
                            let bytes = &buf[..n];
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
                            if let Some(sub) = &st.subscriber {
                                if sub.send(DaemonMsg::Output(bytes.to_vec())).is_ok() {
                                    gate.add(n);
                                }
                            }
                            apply_signals(&mut st, signals);
                            if let Some(remote) = remote {
                                apply_remote_context(&mut st, remote);
                            }
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
        self.state.lock().unwrap().ring.resize(size);
        match &self.backend {
            PaneBackend::Pty(p) => {
                if let Ok(master) = p.master.lock() {
                    let _ = master.resize(pty_size(size));
                }
            }
            PaneBackend::NativeSsh(b) => b.handle.resize(size),
        }
    }

    #[allow(dead_code)]
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
            death.report(&state, &shutting_down);
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
                death.report(&state, &shutting_down);
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
            .and_then(|m| m.process_group_leader());
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
            .and_then(|m| m.process_group_leader())
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

fn attach_subscriber(st: &mut PaneState, subscriber: Sender<DaemonMsg>) -> u64 {
    st.subscriber_epoch += 1;

    st.ring.replay(&subscriber);
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
        let _ = subscriber.send(DaemonMsg::Exited { code: None });
    }
    st.subscriber = Some(subscriber);
    st.subscriber_epoch
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
            a.agent != b.agent || a.session_id != b.session_id || a.launch_argv != b.launch_argv
        }
        _ => true,
    }
}

fn apply_signals(st: &mut PaneState, signals: SniffSignals) {
    if let Some(cwd) = signals.cwd {
        if st.cwd.as_ref() != Some(&cwd) {
            if let Some(sub) = &st.subscriber {
                let _ = sub.send(DaemonMsg::Cwd(cwd.clone()));
            }
            st.cwd = Some(cwd);
        }
    }
    for shell in signals.shell {
        #[cfg(windows)]
        if shell_mark_capture_changed(&st.shell, &shell) {
            apply_agent(
                st,
                agent_from_shell_mark(&shell, crate::core::config::agent_commands_cached())
                    .map(|agent| (agent, Vec::new())),
            );
        }
        st.shell = shell.clone();
        if let Some(sub) = &st.subscriber {
            let _ = sub.send(DaemonMsg::Prompt {
                active: shell.active,
                at_prompt: shell.at_prompt,
                last_exit: shell.last_exit_code,
            });
        }
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
) -> Option<crate::core::cli_agent::CLIAgent> {
    shell
        .command
        .as_deref()
        .and_then(|cmd| crate::core::cli_agent::CLIAgent::detect_from_command_with(cmd, custom))
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
            if let Some(sub) = &st.subscriber {
                let _ = sub.send(DaemonMsg::Agent(st.agent));
            }
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

    if st.agent_session != before
        && let Some(sub) = &st.subscriber
    {
        let _ = sub.send(DaemonMsg::AgentStatus(st.agent_session.clone()));
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
    if let Some(sub) = &st.subscriber {
        let _ = sub.send(DaemonMsg::Cwd(probed.clone()));
    }
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
    if let Some(sub) = &st.subscriber {
        let _ = sub.send(DaemonMsg::RemoteContext(remote.clone()));
    }
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
        if let Some(sub) = &st.subscriber {
            let _ = sub.send(DaemonMsg::AgentStatus(None));
        }
    }
    if agent.is_none() {
        st.agent_argv = None;
    }
    if let Some(sub) = &st.subscriber {
        let _ = sub.send(DaemonMsg::Agent(agent));
    }
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
        if let Some(sub) = &st.subscriber {
            let _ = sub.send(DaemonMsg::AgentStatus(st.agent_session.clone()));
        }
    }
}

fn foreground_command_running(
    master: &Mutex<Box<dyn MasterPty + Send>>,
    shell_pid: Option<u32>,
) -> bool {
    is_foreground_command(pty_foreground_pgid(master), shell_pid)
}

#[cfg(unix)]
fn pty_foreground_pgid(master: &Mutex<Box<dyn MasterPty + Send>>) -> Option<i32> {
    master.lock().ok().and_then(|m| m.process_group_leader())
}

#[cfg(not(unix))]
fn pty_foreground_pgid(_master: &Mutex<Box<dyn MasterPty + Send>>) -> Option<i32> {
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
    master: &Mutex<Box<dyn MasterPty + Send>>,
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
    master: &Mutex<Box<dyn MasterPty + Send>>,
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
    _master: &Mutex<Box<dyn MasterPty + Send>>,
    _shell_pid: Option<u32>,
) -> Option<PathBuf> {
    None
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn foreground_remote_context(master: &Mutex<Box<dyn MasterPty + Send>>) -> Option<RemoteContext> {
    let pid = master.lock().ok().and_then(|m| m.process_group_leader())?;
    let argv = crate::daemon::remote::foreground_argv(pid)?;
    crate::daemon::remote::parse_ssh_invocation(&argv).map(|inv| inv.context)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn foreground_remote_context(_master: &Mutex<Box<dyn MasterPty + Send>>) -> Option<RemoteContext> {
    None
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn foreground_agent(
    master: &Mutex<Box<dyn MasterPty + Send>>,
) -> Option<Option<(crate::core::cli_agent::CLIAgent, Vec<String>)>> {
    let detect = || {
        let pid = master.lock().ok().and_then(|m| m.process_group_leader())?;
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
    _master: &Mutex<Box<dyn MasterPty + Send>>,
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
        let master = Mutex::new(pty.master);

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
            Some(crate::core::cli_agent::CLIAgent::Claude)
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

    fn test_state(alive: bool) -> PaneState {
        PaneState {
            id: 0,
            ring: ReplayRing::new(ws(80, 24)),
            subscriber: None,
            subscriber_epoch: 0,
            cwd: None,
            shell: ShellState::default(),
            remote: None,
            agent: None,
            agent_session: None,
            agent_argv: None,
            alive,
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
        let env: std::collections::HashMap<_, _> =
            pane_environment(&std::collections::HashMap::new())
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
            pane_environment(&configured).into_iter().collect();

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
    fn pane_environment_capability_keys_cannot_be_overridden_by_recasing() {
        let configured = [("Term", "dumb"), ("ColorTerm", ""), ("term_program", "x")]
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();

        let applied = pane_environment(&configured);

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
        let cmd = build_shell_command(None, &Some(PathBuf::from("/tmp")))
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

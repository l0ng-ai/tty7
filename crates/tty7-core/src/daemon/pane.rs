//! `DaemonPane`: the daemon-side owner of one PTY + child shell.
//!
//! This is the daemon analogue of the client's mirror terminal, but **headless**:
//! it runs no alacritty `Term` and does no rendering. The reader thread instead
//! (a) appends raw PTY bytes to a bounded *replay ring*, (b) forwards them to the
//! currently-attached client as `DaemonMsg::Output`, and (c) feeds an OSC sniffer
//! that learns the cwd (OSC 7) and prompt state (OSC 133) and pushes those to the
//! client. The client rebuilds the screen locally from the attach replay (the
//! ring's segments, a `Size` + `Snapshot` pair each — see [`ReplayRing`]) plus
//! the live `Output` tail.
//!
//! The PTY is driven by [`portable-pty`](portable_pty): a Unix pty on Unix and a
//! ConPTY on Windows, behind one blocking `Read`/`Write`/`resize` API. That keeps
//! this module single-path across platforms — no fd/ioctl/signal code. What stays
//! platform-specific is the foreground-process query behind the pane title / cwd
//! fallback (macOS/Linux proc APIs; a Windows process-table walk in
//! [`winproc`](crate::daemon::winproc)) and the hangup that tears the child's
//! whole process tree down.
//!
//! Shell integration (the hooks that make the shell emit OSC 7 / OSC 133) lives
//! in the sibling [`shell_integration`](crate::daemon::shell_integration) module:
//! the PTY owner is the one place that injects it, so there's a single source of
//! truth and no duplicated rc logic. It covers zsh, bash and fish; on
//! Windows (and any other shell) the pane simply launches bare and the
//! cwd/prompt sniffing stays dormant. A shell that ends up uninstrumented
//! anyway — an rc file that `exec`s into another shell, a nested one started by
//! hand — has its cwd tracked from the process table instead, on the reader's
//! foreground poll (see [`apply_probed_cwd`]).

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

/// The platform default shell command, used when the user hasn't set `shell` in
/// `config.json`. On Windows `portable-pty`'s own default is `%COMSPEC%`
/// (i.e. `cmd.exe`); we override it to PowerShell — PowerShell 7 (`pwsh`) when
/// installed, probed once by `core::shells`, else the `powershell.exe` that
/// ships with every supported Windows. Mirrors Windows Terminal's preference
/// for the modern shell.
#[cfg(windows)]
fn default_prog() -> CommandBuilder {
    CommandBuilder::new(crate::core::shells::windows_default_shell())
}

/// On Unix, start from `portable-pty`'s login-shell builder, but switch to an
/// explicit command when the GUI has already detected the shell that launched
/// tty7 and forwarded it to the detached daemon. LaunchServices may otherwise
/// give the daemon a stale config dir and stale `$SHELL`.
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

/// The program name used to detect which shell integration applies for the
/// *default* shell. On Unix this is the login shell `portable-pty` resolved
/// (`$SHELL` / passwd). On Windows we can't ask the builder: its `get_shell()`
/// reports `%ComSpec%` (cmd.exe) regardless of what we actually spawn, so it
/// would send integration detection chasing cmd.exe and never engage — return
/// the same PowerShell `default_prog()` resolved instead.
#[cfg(windows)]
fn default_shell_name(_cmd: &CommandBuilder) -> String {
    crate::core::shells::windows_default_shell().to_string()
}

#[cfg(not(windows))]
fn default_shell_name(cmd: &CommandBuilder) -> String {
    cmd.get_shell()
}

/// The shell a spawn resolved to, plus who authored its args.
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
struct ChosenShell {
    program: String,
    args: Vec<String>,
    /// True when `args` are tty7's own defaults from shell discovery rather
    /// than the user's, so shell integration may replace them. See
    /// [`ShellSpec::args_are_tty7_defaults`].
    args_are_tty7_defaults: bool,
}

/// Whether the chosen shell carries args tty7 must not second-guess, which is
/// what makes `shell_integration::setup` decline bash and PowerShell (their
/// injections replace argv rather than extend it). Two things have to hold for
/// the args to be off-limits:
///
///  - there are some — an empty `args: []` (just picking the program) leaves
///    nothing for bash's `--rcfile … -i` to conflict with; and
///  - the *user* wrote them. The new-tab dropdown's args are tty7's own
///    (`core::shells::detect_shells`), so integration is free to express the
///    same intent its own way: Git Bash's `-i -l` means "interactive login
///    shell", exactly what `setup_bash` rebuilds out of `--rcfile … -i` plus a
///    replayed login-file chain. See [`ShellSpec::args_are_tty7_defaults`].
fn has_custom_args(chosen: Option<&ChosenShell>) -> bool {
    chosen.is_some_and(|c| !c.args.is_empty() && !c.args_are_tty7_defaults)
}

/// Which shell a spawn launches, by precedence: the explicit per-spawn override
/// (the new-tab dropdown) > the configured `shell` in `config.json` > `None`,
/// meaning the platform default (`default_prog()`). Kept as a function so the
/// contract is stated (and tested) in one place.
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
            // Straight from `config.json` — the user wrote these.
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
    // `CommandBuilder::new_default_prog()` preserves the Unix login-shell argv0
    // shape, but portable-pty intentionally panics if argv is appended to that
    // sentinel builder. Integrations that need argv (fish `-C`, bash `--rcfile`,
    // PowerShell flags) must use an explicit command builder first. Env-only zsh
    // integration keeps the default login-shell path.
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
    // Resolved here rather than inside `build_shell_command` because the WSL tag
    // must be read off the shell we *actually* launch. `shell` is only the
    // per-spawn override; `config.json` supplies the program when it is `None`,
    // and a `wsl.exe` configured there is just as much a WSL pane as one picked
    // from the dropdown.
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

/// Tag a `wsl.exe` pane as living in another filesystem namespace, from the
/// resolved shell rather than the process table — `wsl.exe` is exactly what tty7
/// launched, so there is nothing to detect.
///
/// This is what makes `TerminalView::local_cwd` decline the distro's cwd, and
/// so what keeps the local git probe, path completion, link resolution and cwd
/// inheritance away from a path that means nothing on this side (and that
/// Windows would read as drive-relative). It is set whether or not shell
/// integration succeeded: an unintegrated WSL pane reports no cwd today, but if
/// it ever does the gate must already be in place.
///
/// Takes the post-[`choose_shell`] program, not the per-spawn override: a
/// `wsl.exe` written into `config.json` reaches the same integration and so must
/// reach the same tag.
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
        // The distro, when the args name one; otherwise `wsl.exe` picks the
        // default and we have no name for it without another probe. Shared with
        // the integration so the two can't disagree about which distro an argv
        // names — they are handed the very same args.
        target: shell_integration::wsl_distro(&chosen.args).unwrap_or_default(),
    })
}

/// Build the argv for a spawn from an already-resolved shell (see
/// [`choose_shell`]); `None` means the platform default (the login shell on
/// Unix, PowerShell on Windows).
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
    // The program tty7 is actually about to spawn, used (rather than `$SHELL`,
    // which can disagree) to detect which shell integration applies. For a
    // configured shell this is just its program string; for the platform default
    // it's whatever `default_prog()` resolved (passwd/`$SHELL` on Unix,
    // `powershell.exe` on Windows — see `default_shell_name`).
    let resolved_program = match &configured {
        Some(chosen) => chosen.program.clone(),
        None => default_shell_name(&cmd),
    };

    // Shell integration: inject OSC 7 / OSC 133 hooks (zsh/fish/bash/PowerShell,
    // and through `wsl.exe` into a distro — see `daemon::shell_integration`).
    // Best effort — `None` (an unsupported shell, or one with unpreservable
    // custom args) means we launch bare. The args go in because the WSL path
    // reads the distro out of them.
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
    // Working directory for the shell: an explicit `cwd` from the client wins
    // (new tab/split inheriting the active pane's dir, or session restore).
    // Otherwise fall back to the daemon's own cwd — but skip a bare "/", which
    // is what Launch Services hands a `.app` started from Finder/Dock/`open`
    // (there's no meaningful inherited dir there). In that case default to the
    // user's home, matching Terminal.app / iTerm. Launching from a shell
    // (`cargo dev`) still inherits that shell's dir, since it isn't "/".
    let fallback = std::env::current_dir()
        .ok()
        .filter(|d| d != std::path::Path::new("/"))
        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from));
    // A `working_directory` of Home/Custom forces a base dir, but only when the
    // client didn't pass an explicit cwd (tab-inherit / session restore still
    // win). Inherit -> `forced` is `None`, so we keep the fallback as before.
    let forced = crate::core::config::working_directory_base();
    // Whatever wins must actually be a directory *here*. A client cwd is only
    // as good as the OSC 7 that produced it, and a shell that reports a path
    // this machine cannot resolve — a remote namespace, or an msys path like
    // `/c/Users/x` that Windows reads as drive-relative — would otherwise turn
    // a new tab or split into a hard spawn failure ("The directory name is
    // invalid") instead of quietly falling back. Cheap to check, and it bounds
    // the whole class rather than one shell's spelling at a time.
    [cwd, forced, fallback]
        .into_iter()
        .flatten()
        .find(|d| d.is_dir())
}

/// Whether a GUI-launched child needs tty7 to supply a character locale.
///
/// tmux checks these variables in this exact order to decide whether its client
/// supports UTF-8; when all three are absent or empty it deliberately renders
/// each Unicode cell as `_`. Any configured locale key is authoritative, even
/// when its value is empty, so the generic `env` override remains capable of
/// opting out of this fallback.
#[cfg(any(target_os = "macos", test))]
fn locale_fallback_is_needed(
    extra_env: &std::collections::HashMap<String, String>,
    mut inherited: impl FnMut(&str) -> Option<String>,
) -> bool {
    ["LC_ALL", "LC_CTYPE", "LANG"].iter().all(|key| {
        !extra_env.contains_key(*key) && inherited(key).as_deref().is_none_or(str::is_empty)
    })
}

/// Where macOS keeps its locale definitions. A name is usable as a locale only
/// if it has a directory here — the check that keeps us from ever exporting a
/// locale the C library will fail to load.
#[cfg(target_os = "macos")]
const LOCALE_DEFINITION_DIR: &str = "/usr/share/locale";

/// Last-resort character locales, in preference order. `C.UTF-8` says exactly
/// what we mean — Unicode character handling with no regional bias — and is
/// built into modern glibc, so it also survives the trip to a Linux host over
/// ssh. It is a recent addition to macOS though, so `en_US.UTF-8` (present on
/// every macOS) backs it up.
#[cfg(any(target_os = "macos", test))]
const FALLBACK_CHARACTER_LOCALES: [&str; 2] = ["C.UTF-8", "en_US.UTF-8"];

/// The UTF-8 locale to hand a GUI-launched shell, derived from the user's
/// system locale the way Terminal.app and iTerm2 do it.
///
/// Every candidate is checked against the locale definitions actually installed
/// (`exists`) before it is used. That check is the whole point: a name the C
/// library cannot load is worse than none at all, because `LC_CTYPE` outranks
/// the `LANG` a remote host sets for itself, and ssh forwards `LC_*` by default
/// (`SendEnv LANG LC_*` ships in the stock `ssh_config`). Exporting an
/// unloadable name would therefore *break* non-ASCII output on every host we
/// ssh into — the bug this seeds a locale to avoid.
#[cfg(any(target_os = "macos", test))]
fn character_locale(identifier: Option<&str>, exists: impl Fn(&str) -> bool) -> Option<String> {
    identifier
        .and_then(posix_locale_stem)
        .map(|stem| format!("{stem}.UTF-8"))
        .into_iter()
        .chain(FALLBACK_CHARACTER_LOCALES.iter().map(|s| (*s).to_string()))
        .find(|candidate| exists(candidate))
}

/// Reduce a CFLocale/`AppleLocale` identifier to its POSIX `lang_REGION` stem:
/// `zh_Hans_CN@calendar=gregorian` -> `zh_CN`, `en-US` -> `en_US`.
///
/// POSIX locale names carry no script subtag, so `Hans` in the middle is
/// dropped. A region is required — a bare `zh` cannot name a locale, and
/// guessing a default region for a language would be inventing a user
/// preference rather than reading one.
#[cfg(any(target_os = "macos", test))]
fn posix_locale_stem(identifier: &str) -> Option<String> {
    let base = identifier.split('@').next()?;
    let mut parts = base.split(['_', '-']).filter(|s| !s.is_empty());
    let language = parts
        .next()
        .filter(|l| (2..=3).contains(&l.len()) && l.chars().all(|c| c.is_ascii_alphabetic()))?;
    // ISO 3166-1 alpha-2 ("CN") or UN M.49 numeric ("419"); anything else in the
    // trailing position is a script or variant subtag, which POSIX has no slot for.
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

/// The current system locale's identifier (e.g. `en_CN`, `zh_Hans_CN`).
///
/// Read from CoreFoundation rather than the `defaults` domain so it reflects
/// the same resolved preference the rest of the system sees.
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
        // `Get` rule: the identifier is owned by the locale, so it is wrapped
        // without taking ownership and only the locale itself is released.
        let identifier = CFLocaleGetIdentifier(locale);
        let out =
            (!identifier.is_null()).then(|| CFString::wrap_under_get_rule(identifier).to_string());
        CFRelease(locale);
        out
    }
}

/// What tty7 answers to in `TERM_PROGRAM`. Terminals name themselves in the
/// form they brand themselves in — `Apple_Terminal`, `iTerm.app`, `WezTerm`,
/// `ghostty`, `vscode` — so ours is the lowercase product name.
const TERM_PROGRAM_NAME: &str = "tty7";

/// Env keys that describe our emulator's real capabilities. A user's `env` map
/// must not override these: the answer isn't a preference, it's a fact about
/// what the pane on the other end can decode.
const CAPABILITY_ENV: [&str; 2] = ["TERM", "COLORTERM"];

/// Whether a configured `env` key names one of [`CAPABILITY_ENV`]. Windows
/// environment blocks are case-insensitive — `portable-pty` keeps one slot per
/// lowercased key, so a configured `Term` there would replace `TERM` just as
/// surely as the exact spelling — so the filter must use the platform's own
/// notion of "the same variable". On Unix a differently-cased key is a genuinely
/// distinct variable and stays the user's to set.
fn names_capability_env(key: &str) -> bool {
    CAPABILITY_ENV.iter().any(|cap| {
        if cfg!(windows) {
            key.eq_ignore_ascii_case(cap)
        } else {
            key == *cap
        }
    })
}

/// The environment every pane starts with, in application order — tty7's own
/// advertisements first, then the user's `env` map, which overrides all but
/// [`CAPABILITY_ENV`]. Returned as a list rather than applied in place so the
/// precedence is testable without a `CommandBuilder` or a real `config.json`.
fn pane_environment(
    extra_env: &std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    let version = env!("CARGO_PKG_VERSION");
    let mut env = vec![
        // A widely-available terminfo + truecolor.
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
        // Mark the session as tty7's, for tooling that adapts to its host
        // terminal — most importantly the `tty7 agent-hook` emitter, which
        // stays silent without it so globally-installed agent hooks can't leak
        // escape sequences into other terminals (see `core::agent_hooks`).
        (
            crate::core::agent_hooks::TTY7_ENV_MARKER.to_string(),
            version.to_string(),
        ),
        // The de-facto standard pair for "which terminal is this": Apple
        // Terminal introduced it, and iTerm2, WezTerm, Ghostty, VS Code and
        // tmux all set it. `TERM` describes terminfo capabilities and can't
        // answer this — but capability probes (`supports-color`,
        // `supports-hyperlinks` and the JS CLI ecosystem built on them),
        // editors applying terminal-specific workarounds, and shell prompts all
        // branch on the program name, falling back to their most conservative
        // behaviour when it's missing. `TTY7` doesn't help them: it's ours, and
        // nothing third-party knows to look for it.
        //
        // Deliberately overridable below, unlike the capability keys: this
        // names an identity, and posing as another terminal is a legitimate way
        // to get a tool that only recognises a fixed list to light up.
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

    // LaunchServices commonly starts a macOS app with no locale variables at
    // all, so a GUI-launched shell runs in the C locale: `ls` prints one `?` per
    // non-ASCII byte, and tmux substitutes one `_` per Unicode cell. Seed only
    // LC_CTYPE — character handling is the broken part, and leaving LANG alone
    // keeps message/date/number localization as the user set it. Respect any
    // inherited non-empty locale and every user-configured locale key, including
    // `C` or an empty value used to opt out of the fallback.
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

/// Default cap on the replay ring: 8 MiB. Enough to reconstruct a deep screen +
/// scrollback for a fresh attach, while bounding daemon memory per pane. When the
/// ring is full we drop the *oldest* bytes: a terminal stream is only meaningful
/// from some recent point onward, and a client's emulator tolerates a truncated
/// prefix far better than a hole punched in the middle.
const RING_CAP: usize = 8 * 1024 * 1024;

/// Cap on the ring's geometry segments. The client does a full grid reflow per
/// replayed `Size`, and drag-resizing a pane whose TUI redraws on every
/// SIGWINCH cuts a segment per column change — tiny segments that never fill
/// `RING_CAP`, so over a long-lived pane's life they would accumulate without
/// bound and attach would degrade linearly. Past the cap the two *oldest*
/// segments merge (the older one's bytes replay at the newer one's geometry):
/// like the byte cap, precision degrades from the oldest scrollback first.
const MAX_RING_SEGMENTS: usize = 64;
const REMOTE_CONTEXT_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Backpressure between a pane's PTY reader and its connection writer: counts
/// the `Output` bytes sitting in the (unbounded) channel, and parks the reader
/// while the backlog is at the high-water mark. Without it the daemon slurps
/// the PTY far faster than a client can parse (the ring append no longer
/// throttles reads), so a long-running flood (`yes` in a pane) would grow the
/// queue without bound. Pausing the *reader* is exactly PTY backpressure: the
/// kernel buffer fills and the child blocks on write, like a slow real tty.
pub struct OutputGate {
    /// Bytes handed to the writer channel but not yet written out. Atomic —
    /// `add` runs per PTY read (~100k/s at full drain) and `sub` per socket
    /// write, so the hot paths must not take a lock. Signed so a late
    /// decrement racing a `reset` only drifts permissive (negative) instead
    /// of underflowing.
    queued: AtomicI64,
    /// Guards no data — it exists so a `sub`/`reset` notify can't slip between
    /// a parked reader's re-check of `queued` and its condvar wait (the
    /// classic lost-wakeup race). Only touched on the slow paths: an actual
    /// park, and the wakeup that crosses back below the mark.
    park: Mutex<()>,
    drained: Condvar,
}

impl OutputGate {
    /// Max Output bytes in flight before the PTY reader pauses. Sized to
    /// swallow a big burst whole (a 10+ MB `cat`, a build log dump) so the
    /// PTY drains at device speed and the client parses in its own time —
    /// while still bounding what a nonstop flood (`yes`) can pin per pane.
    const HIGH_WATER: i64 = 16 * 1024 * 1024;
    /// Upper bound on one backpressure pause. Attach/detach reset the counter;
    /// if an accounting slip ever left it stuck high anyway, this degrades to
    /// slow-drain instead of a wedged PTY.
    const MAX_WAIT: Duration = Duration::from_secs(2);

    pub(crate) fn new() -> Self {
        Self {
            queued: AtomicI64::new(0),
            park: Mutex::new(()),
            drained: Condvar::new(),
        }
    }

    /// Record `n` Output bytes handed to the writer channel.
    fn add(&self, n: usize) {
        self.queued.fetch_add(n as i64, Ordering::Relaxed);
    }

    /// Record `n` Output bytes leaving the channel (written to the socket, or
    /// dropped with a failed one — either way they no longer occupy memory).
    pub fn sub(&self, n: usize) {
        let prev = self.queued.fetch_sub(n as i64, Ordering::Relaxed);
        // Wake the parked reader only when this decrement crosses back below
        // the mark — not on every frame written. The lock makes the notify
        // ordered against a parking reader's re-check (see `park`).
        if prev >= Self::HIGH_WATER && prev - (n as i64) < Self::HIGH_WATER {
            let _park = self.park.lock().unwrap();
            self.drained.notify_all();
        }
    }

    /// Forget all in-flight accounting: the subscriber changed and any queued
    /// frames died with the old channel.
    fn reset(&self) {
        self.queued.store(0, Ordering::Relaxed);
        let _park = self.park.lock().unwrap();
        self.drained.notify_all();
    }

    /// Park the caller (the PTY reader; it must hold no locks) while the
    /// backlog is at/over the high-water mark, up to [`Self::MAX_WAIT`].
    /// Lock-free when the backlog is below the mark — the common case, checked
    /// before every PTY read.
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

/// Shared, mutable inner state of a pane. Split from the immutable handles (the
/// PTY master, writer, child) so a single `Mutex` guards everything the reader
/// thread and the connection threads both touch.
struct PaneState {
    /// The registry id of the pane this state belongs to — [`DaemonPane::id`],
    /// duplicated here so the code paths that only ever see the state (the
    /// signal appliers, [`DeathReporter::report`]) can name the pane when
    /// publishing an observation to the machine tree
    /// ([`crate::core::machine::observe_pane`]).
    id: u64,
    /// The replay ring: raw PTY bytes bounded to `RING_CAP`, segmented by the
    /// geometry they were recorded under so `attach` can replay each stretch
    /// at the width it was written for. Also the owner of the pane's current
    /// size (the tail segment's geometry). See [`ReplayRing`].
    ring: ReplayRing,
    /// The currently-attached client's outbound channel, or `None` when detached.
    /// v1 is single-subscriber: a new attach replaces this, and the old
    /// connection's receiver then sees its sender dropped and ends.
    subscriber: Option<Sender<DaemonMsg>>,
    /// Monotonic generation bumped on every `attach`. A connection remembers the
    /// epoch it installed; `detach` only clears the subscriber if it still owns
    /// that epoch, so a *replaced* connection tearing down can't blank the live
    /// subscriber a newer attach just installed (e.g. session-restore reattach,
    /// where the old GUI's connection lingers while the new one takes over).
    subscriber_epoch: u64,
    /// The pane's current directory, so a fresh attach can be told immediately.
    /// Seeded with the spawn directory, refined by the shell's OSC 7 reports,
    /// and — for shells that emit none — corrected from the process table by the
    /// reader's poll (see [`apply_probed_cwd`]).
    cwd: Option<PathBuf>,
    /// Shell prompt/command state from OSC 133.
    shell: ShellState,
    /// Trusted foreground remote context from the local process table.
    remote: Option<RemoteContext>,
    /// The third-party CLI coding agent running in the foreground, detected from
    /// the foreground `argv` (same process-table poll as `remote`). `None` when
    /// no known agent runs — see [`crate::core::cli_agent`].
    agent: Option<crate::core::cli_agent::CLIAgent>,
    /// The argv the detected agent was launched with, held here until a rich
    /// session exists to stamp it into (the sentinel events that create the
    /// session can land before the first argv poll, and vice versa). Cleared
    /// with the chip. See [`stamp_launch_argv`].
    agent_argv: Option<Vec<String>>,
    /// The rich agent-session status (idle/working/waiting/done + native
    /// session id), folded from the sentinel OSC events the agent's hooks emit
    /// (with an opaque OSC 9/777 fallback). Cleared when the agent exits.
    /// See [`crate::core::cli_agent::AgentSessionState`].
    agent_session: Option<crate::core::cli_agent::AgentSessionState>,
    /// False once the child has exited; the pane lingers so its ring stays
    /// readable by a late attach.
    alive: bool,
}

/// The byte source behind a pane. The PTY path (`Pty`) is byte-for-byte the
/// original local-shell backend; `NativeSsh` is a russh shell channel bridged to
/// the same blocking reader/writer contract (see [`crate::daemon::ssh::session`]).
/// The reader thread, replay ring, `OutputGate`, and OSC sniffer are identical for
/// both — only the handle-owning bits (resize, kill/hangup, foreground queries,
/// reap) differ, and dispatch on this enum.
enum PaneBackend {
    Pty(PtyBackend),
    NativeSsh(NativeSshBackend),
}

/// The reader thread's two off-hot-path foreground probes, bundled so
/// [`DaemonPane::spawn_reader`] takes them as one argument. Both are
/// process-table reads (foreground process-group leader → `argv`) run together
/// on the reader's 0.5 s poll: `remote` classifies an SSH context, `agent`
/// classifies a third-party coding agent. Boxed rather than generic because
/// they're invoked at most twice a second — the indirection is free here and
/// keeps the reader's signature readable.
struct ForegroundProbes {
    remote: Box<dyn Fn() -> Option<RemoteContext> + Send>,
    /// Outer `None` means this backend has no process-table view of the PTY
    /// foreground at all (native SSH; Windows, where ConPTY has no foreground
    /// process group) — "no opinion", never applied, so it can't wipe an agent
    /// identified another way (sentinel events, the Windows `133;C;<cmd>`
    /// mark). `Some(answer)` is a real poll result; its inner `None` ("polled,
    /// no agent") clears the chip. A detected agent travels with the `argv` it
    /// was identified from, kept for flag carry-over on session resume.
    agent: Box<dyn Fn() -> Option<Option<(crate::core::cli_agent::CLIAgent, Vec<String>)>> + Send>,
    /// The PTY owner's cwd straight from the process table. `None` means "no
    /// reading" (native SSH, Windows, or a process we can't inspect), never
    /// "no cwd" — see [`apply_probed_cwd`] for how a reading is reconciled with
    /// the shell's own OSC 7 report.
    cwd: Box<dyn Fn() -> Option<PathBuf> + Send>,
}

/// The local-PTY backend: the same handles `DaemonPane` has always owned.
struct PtyBackend {
    /// The PTY master. Kept for the pane's lifetime to `resize` it and to query the
    /// foreground process group (macOS title / cwd fallback, and the reader
    /// thread's remote-prompt gate — see [`foreground_command_running`]). Behind a
    /// `Mutex` because the trait object is `Send` but not `Sync`; wrapped in an
    /// `Arc` so the reader thread can hold its own handle for that gate.
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    /// The child shell. Behind a `Mutex` so `kill` (and `Drop`'s reap) can take it
    /// `&mut`. `kill()` hangs the child up (SIGHUP on Unix); `Drop` then waits it.
    child: Mutex<Box<dyn Child + Send + Sync>>,
    /// Child shell pid, when the platform reports one. Used to signal the
    /// process group on Unix and as the proc-query fallback target on
    /// macOS/Linux (hence dead on Windows).
    #[cfg_attr(windows, allow(dead_code))]
    shell_pid: Option<u32>,
    /// Throwaway dir backing shell integration (zsh's `ZDOTDIR`, bash's
    /// `--rcfile`), removed on drop. `None` if bare, or if the shell (fish)
    /// needed no on-disk file at all.
    integration_dir: Option<PathBuf>,
}

/// The native-SSH backend: a handle to the async channel driver (for resize /
/// close) plus the resolved remote context reported to the GUI. Resize becomes a
/// `window-change`; kill/hangup closes the channel; foreground/pgid queries are
/// `None` (a remote session has no local process group — the OSC 133 gate is a
/// no-op, which is correct, per the pipeline brief §9).
struct NativeSshBackend {
    handle: Arc<crate::daemon::ssh::SshSessionHandle>,
    /// The pane's russh connection, published by the connect task once
    /// authenticated (a `Weak`, upgraded on demand). The seam WS4/WS5 reach
    /// through [`DaemonPane::ssh_connection`].
    connection: crate::daemon::ssh::SharedConnection,
}

/// One live pane: a byte-source backend plus the shared [`PaneState`]. Shared
/// across connection threads via `Arc`; all mutable stream state lives behind the
/// locks.
pub struct DaemonPane {
    pub id: u64,
    /// The workspace this pane was spawned for (a `WorkspaceId` uuid string),
    /// when the spawning client said — see `ClientMsg::Spawn`'s `owner`.
    /// Immutable for the pane's lifetime: ownership is decided at spawn, and a
    /// pane that could change hands would be exactly the ambiguity this field
    /// exists to close. Reported in `List` ([`PaneInfo::owner`]) so restore can
    /// refuse to attach a workspace to a pane another one owns.
    owner: Option<String>,
    /// The byte source (local PTY or native-SSH channel).
    backend: PaneBackend,
    /// The input side (keyboard input / pasted text): the PTY writer, or the
    /// native-SSH channel writer. Behind a `Mutex` because writes can arrive from
    /// different connection threads.
    writer: Mutex<Box<dyn Write + Send>>,
    /// Set during teardown so the reader doesn't emit a spurious exit.
    shutting_down: Arc<AtomicBool>,
    /// Output backpressure shared by the reader thread (adds + waits) and the
    /// connection's writer thread (drains). See [`OutputGate`].
    gate: Arc<OutputGate>,
    state: Arc<Mutex<PaneState>>,
    /// The reader `JoinHandle`, taken and joined in `Drop`.
    reader: Mutex<Option<JoinHandle<()>>>,
    /// Auth/host-key prompt broker for native-SSH panes; `None` for PTY panes.
    /// `run_stream` routes `ClientMsg::AuthResponse` here via
    /// [`DaemonPane::deliver_auth_response`].
    broker: Option<Arc<crate::daemon::ssh::PromptBroker>>,
}

/// Fires a pane's "child gone" notification exactly once, whichever thread
/// notices the death first. On Unix the reader thread sees it as a PTY `read()`
/// EOF and reports here. On Windows the ConPTY output pipe does *not* EOF when
/// the shell exits on its own — it only closes on `ClosePseudoConsole`, so a
/// natural `exit` / Ctrl-D would leave the reader blocked forever and the pane
/// wedged open (see [`DaemonPane::spawn_exit_monitor`]). There a separate thread
/// waits on the child handle and reports here instead. The `reported` latch keeps
/// whichever route fires second a no-op, so a subscriber never sees two `Exited`s
/// and `on_dead` runs at most once.
struct DeathReporter {
    reported: AtomicBool,
    /// The server's reclaim hook, consumed the first time the pane dies with
    /// nobody attached. Behind a `Mutex<Option<…>>` because it's a `FnOnce`
    /// shared between the reader and (on Windows) the monitor — whichever fires
    /// first takes it.
    on_dead: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl DeathReporter {
    fn new(on_dead: impl FnOnce() + Send + 'static) -> Self {
        Self {
            reported: AtomicBool::new(false),
            on_dead: Mutex::new(Some(Box::new(on_dead))),
        }
    }

    /// Mark the pane not-alive and, unless the owner already began teardown
    /// (`shutting_down` — the killer owns cleanup then), tell the attached
    /// subscriber it exited; with nobody attached, hand the pane to `on_dead` so
    /// the server drops it instead of leaking the zombie child + replay ring.
    /// Idempotent: only the first call has any effect.
    fn report(&self, state: &Mutex<PaneState>, shutting_down: &AtomicBool) {
        if self.reported.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut st = state.lock().unwrap();
        st.alive = false;
        let pane = st.id;
        if shutting_down.load(Ordering::SeqCst) {
            drop(st);
            // Even a teardown the owner initiated is a death the tree must
            // hear about: the record's `live == false` *is* the client-visible
            // "awaiting revival" state, and it must not depend on which thread
            // noticed the child go.
            crate::core::machine::observe_pane(pane, |p| p.live = false);
            return;
        }
        let subscribed = st.subscriber.is_some();
        if let Some(sub) = &st.subscriber {
            let _ = sub.send(DaemonMsg::Exited { code: None });
        }
        drop(st);
        crate::core::machine::observe_pane(pane, |p| p.live = false);
        // A subscriber's later detach reclaims the pane, so only an *unattached*
        // death needs `on_dead` — and it fires at most once.
        if subscribed {
            return;
        }
        if let Some(on_dead) = self.on_dead.lock().unwrap().take() {
            on_dead();
        }
    }
}

impl DaemonPane {
    /// Spawn the user's shell on a fresh PTY in `cwd`, sized to `size`, and start
    /// its reader thread. `id` is the registry id the server assigns. `shell` is
    /// an explicit per-spawn override (the new-tab dropdown) that outranks the
    /// configured default — see [`choose_shell`]. `on_dead` fires (from the
    /// reader thread, or on Windows the child-exit monitor) when the child exits
    /// while *nobody is attached* — the case where no connection's detach would
    /// ever reclaim the pane; the server uses
    /// it to drop the dead pane from its registry instead of leaking the zombie
    /// child + replay ring for the daemon's lifetime.
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

        // Drop the slave handle now: the child holds its own slave fds, and our
        // extra handle must close so the master read side reports EOF when the
        // child exits (otherwise the reader thread would never see the hangup).
        drop(pair.slave);

        // An independent, *blocking* reader handle for the reader thread; the
        // master itself stays for resize + fg-process queries, and the writer is
        // taken once for input. (This is what makes the daemon's threaded model
        // work identically on Unix and Windows.)
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

        // Both the reader's EOF and (on Windows) the child-exit monitor report a
        // death through this shared, run-once latch — see [`DeathReporter`].
        let death = Arc::new(DeathReporter::new(on_dead));

        // Windows: the ConPTY output pipe never EOFs on a *natural* child exit, so
        // the reader alone would never notice `exit` / Ctrl-D and the pane would
        // hang open. Watch the shell handle directly and report through the same
        // latch. No-op on Unix, where the reader's `read()` EOF already covers it.
        #[cfg(windows)]
        Self::spawn_exit_monitor(
            shell_pid,
            state.clone(),
            pane.shutting_down.clone(),
            death.clone(),
        );

        // The reader gates a foreground program's OSC 133 prompt marks (a remote
        // shell over ssh emitting its own) out of `at_prompt`, so tty7's local
        // line editor stays disengaged for whatever is really reading the
        // keyboard — see `foreground_command_running` / issue #26.
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

    /// Spawn a native-SSH pane: a russh shell channel bridged into the *same*
    /// reader/ring/gate/sniffer pipeline as a PTY pane. Returns immediately; the
    /// connect → auth → shell sequence runs on the SSH engine's runtime and drives
    /// this pane through the bridge. Auth/host-key prompts and progress ride this
    /// pane's own connection via the prompt broker; a failed connect surfaces as a
    /// normal `Exited` (the driver drops its data sender, EOFing the reader).
    pub fn spawn_native_ssh(
        id: u64,
        size: WinSize,
        spec: Box<NativeSshSpec>,
        on_dead: impl FnOnce() + Send + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        // The async↔blocking bridge: blocking reader/writer for the daemon threads,
        // plus the async ends the channel driver takes.
        let bridge = crate::daemon::ssh::session::make_bridge();
        let reader_handle: Box<dyn Read + Send> = Box::new(bridge.reader);
        let writer: Box<dyn Write + Send> = Box::new(bridge.writer);
        // The connect task fills this once authenticated; the pane exposes it to
        // WS4/WS5 via `ssh_connection()`.
        let connection: crate::daemon::ssh::SharedConnection = Arc::new(Mutex::new(Weak::new()));

        // The remote context the GUI reads to label this as a native-SSH pane.
        // Forwarding/SFTP reach the connection through the in-memory registry.
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
            // The remote cwd is unknown until the remote shell's OSC 7 arrives.
            cwd: None,
            shell: ShellState::default(),
            remote: Some(remote),
            // A native-SSH pane has no local process group, so foreground-argv
            // agent detection never runs for it.
            agent: None,
            agent_session: None,
            agent_argv: None,
            alive: true,
        }));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(OutputGate::new());

        // The prompt broker emits `AuthPrompt`/`SshStatus` frames to whatever
        // client is currently subscribed to this pane, and returns whether a
        // subscriber was present (so a prompt can wait for the attach to land).
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
            // Native-SSH spawns don't carry an owner yet: their leaves persist
            // an `ssh_spec` and reconnect from it rather than by pane id, so
            // the ownership check has nothing to protect there today.
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

        // A remote session has no local PTY foreground process group, so both
        // gate closures answer "nothing local": OSC 133 marks from the remote
        // shell are trusted verbatim (correct — the remote shell *is* the session),
        // and no process-table SSH detection runs (this pane already *is* SSH).
        // The agent probe's `None` is "no opinion" (never applied), so an agent
        // identified from its sentinel events keeps its chip — the poll used to
        // wipe it within half a second. The cwd probe likewise: this pane's
        // directory lives in the remote's namespace, and the only cwd the local
        // process table could offer is meaningless here.
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

        // Kick off the connection on the SSH engine's runtime.
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

    /// Deliver a GUI `AuthResponse` to a native-SSH pane's pending auth prompt.
    /// A no-op for PTY panes (no broker).
    pub fn deliver_auth_response(&self, request_id: u64, response: AuthResponse) {
        if let Some(broker) = &self.broker {
            broker.deliver(request_id, response);
        }
    }

    /// The shared russh connection behind a native-SSH pane, if any — the seam WS4
    /// (port-forwards) and WS5 (SFTP) use to open further channels on the pane's
    /// existing connection (`open_direct_tcpip` / `open_session_channel`). Returns
    /// `None` for a PTY pane, or for a native pane that hasn't finished
    /// authenticating (or whose connection has since dropped). Upgraded from a
    /// `Weak`, so holding the returned `Arc` keeps the connection alive only for as
    /// long as the caller needs it.
    #[allow(dead_code)] // seam consumed by WS4 (forwards) / WS5 (SFTP)
    pub fn ssh_connection(&self) -> Option<Arc<crate::daemon::ssh::SshConnection>> {
        match &self.backend {
            PaneBackend::NativeSsh(b) => b.connection.lock().unwrap().upgrade(),
            PaneBackend::Pty(_) => None,
        }
    }

    /// Reader thread: blocking-reads PTY bytes and, for each chunk, (a) appends to
    /// the ring (dropping the oldest bytes past `RING_CAP`), (b) forwards them to
    /// the subscriber as `Output`, (c) sniffs OSC 7 / OSC 133 and pushes `Cwd` /
    /// `Prompt` on change. On EOF it reports the death through `death` — marking
    /// the pane not-alive and sending `Exited`, keeping the ring for a later
    /// attach, or handing an unattached pane to `on_dead` (see [`DeathReporter`]).
    ///
    /// The off-hot-path foreground probes (remote context, coding agent, cwd)
    /// travel together in [`ForegroundProbes`]: all are process-table reads run
    /// on the same 0.5 s poll, and bundling them keeps the signature at arity.
    fn spawn_reader(
        state: Arc<Mutex<PaneState>>,
        shutting_down: Arc<AtomicBool>,
        gate: Arc<OutputGate>,
        mut reader: Box<dyn Read + Send>,
        // "Is a foreground command (not the shell) currently on the PTY?" Consulted
        // when a prompt mark arrives, to reject marks a foreground program emits —
        // see the call site and [`foreground_command_running`].
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
                // Every microsecond this thread spends off `read()` stalls the
                // child's writes (macOS PTY buffers are ~1 KiB deep), so don't
                // let the scheduler park it on an efficiency core.
                crate::core::threads::promote_to_user_interactive();
                let mut sniffer = OscSniffer::new();
                let mut buf = [0u8; 65536];

                // TTY7_TRACE=1: per-second PTY-drain accounting on stderr (the
                // daemon must run in the foreground to see it), to localize
                // throughput stalls (PTY wait vs lock+dispatch).
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
                    // Backpressure: let the writer drain before pulling more
                    // out of the PTY (no locks are held here).
                    gate.wait_below_high_water();
                    let tr0 = trace.then(std::time::Instant::now);
                    match reader.read(&mut buf) {
                        Ok(0) => break, // EOF: child exited / was hung up.
                        Ok(n) => {
                            if let Some(tr0) = tr0 {
                                tr_read_t += tr0.elapsed();
                                tr_reads += 1;
                                tr_bytes += n as u64;
                            }
                            let bytes = &buf[..n];
                            // Sniff first (cheap, over the same bytes); collect any
                            // cwd/prompt change to emit while we hold the lock.
                            let mut signals = sniffer.feed(bytes);

                            // Reject a prompt mark emitted by a *foreground program*
                            // rather than the shell tty7 spawned. The shell only
                            // emits its OSC 133 marks while it is the PTY's own
                            // foreground group (idle at its prompt); a mark arriving
                            // while a command owns the PTY therefore comes from that
                            // command — most visibly a remote shell over ssh drawing
                            // its own prompt. Trusting it would flip `at_prompt` true
                            // and engage tty7's *local* line editor, whose completion
                            // and history are local-only and wrong for the remote
                            // session (Tab completed local paths instead of the
                            // remote's). Drop the flag so keys pass raw to whatever is
                            // really reading them. The proc query runs only when a
                            // mark actually claims the prompt — about once per prompt.
                            // See issue #26.
                            if signals.shell.iter().any(|s| s.at_prompt) && foreground_running() {
                                for s in signals.shell.iter_mut() {
                                    s.at_prompt = false;
                                }
                                // Clearing the flag can leave neighbours identical;
                                // they no longer describe a crossing, so don't spend
                                // a frame on each.
                                signals.shell.dedup();
                            }

                            // SSH-context + coding-agent detection are process-table
                            // queries (sysctl/procfs). Keep them out of the state lock
                            // and off the per-chunk hot path; half-second freshness is
                            // enough for link hover/click state and the agent tab chip
                            // while keeping PTY drain latency predictable. Both ride the
                            // one poll gate so we read the foreground process at most
                            // twice per interval.
                            let poll_now = std::time::Instant::now() >= next_remote_check;
                            if poll_now {
                                next_remote_check =
                                    std::time::Instant::now() + REMOTE_CONTEXT_POLL_INTERVAL;
                            }
                            let remote = if poll_now {
                                // A pane tty7 itself spawned as remote (native SSH,
                                // or WSL) already carries its own context from the
                                // spawn spec; process-table detection must not
                                // clobber it. Only `Ssh` — the kind this very probe
                                // produces — may be replaced, so a pane that has
                                // since left a foreground `ssh` clears correctly.
                                //
                                // Testing `!= Ssh` rather than `== NativeSsh` is
                                // load-bearing for WSL: `wsl.exe` is not `ssh`, so
                                // the probe returns `None` and would blank the
                                // context on the very next poll — twice a second,
                                // each time also clearing the pane's cwd.
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
                            // Flattened: a fired poll whose probe has no
                            // process-table view (native SSH, Windows) folds to
                            // "no opinion" and is never applied — see
                            // [`ForegroundProbes::agent`].
                            let agent = poll_now.then(&foreground_agent_fn).flatten();
                            // Same gate, same flattening: `None` is "nothing to
                            // read", not "no cwd", so it never clears one.
                            let probed_cwd = poll_now.then(&foreground_cwd_fn).flatten();

                            let tr1 = trace.then(std::time::Instant::now);
                            // Whether this chunk carries anything that *could*
                            // move a fact the tree records. An ordinary output
                            // chunk carries none of them, and must not pay for
                            // two snapshots and a compare per read: a build's
                            // worth of stdout is thousands of chunks and no
                            // facts at all.
                            let may_change_facts = signals.cwd.is_some()
                                || !signals.agent_events.is_empty()
                                || signals.notification.is_some()
                                // A prompt boundary: on Windows the agent
                                // identity rides the `133;C` capture, and
                                // everywhere the OSC 7 cwd travels with it.
                                || !signals.shell.is_empty()
                                || remote.is_some()
                                || agent.is_some()
                                || probed_cwd.is_some();
                            let mut st = state.lock().unwrap();
                            let facts_before = may_change_facts.then(|| observed_facts(&st));
                            st.ring.append(bytes);
                            if let Some(sub) = &st.subscriber {
                                // A send error just means the client is gone; ignore
                                // it and let the next attach install a new sender.
                                // Successful sends are counted against the gate; the
                                // connection's writer thread credits them back.
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
                            // Last: a remote transition in this same chunk has
                            // already cleared the cwd, and `apply_probed_cwd`
                            // declines to speak for a remote pane.
                            apply_probed_cwd(&mut st, probed_cwd);
                            if let Some(tr1) = tr1 {
                                tr_disp_t += tr1.elapsed();
                            }
                            // Publish what this chunk changed to the machine
                            // tree — outside the state lock, because the store
                            // broadcasts to every client of this machine and
                            // this thread's stalls are the child's write
                            // stalls. Gated twice over: `may_change_facts`
                            // keeps plain output free, and the compare below
                            // keeps a re-reported cwd from becoming a store
                            // mutation.
                            let pane = st.id;
                            // Read *with* the facts, not assumed: on Windows
                            // the exit monitor can report the death (flipping
                            // `alive`) while this thread is still draining
                            // ConPTY's buffered output, and the death report
                            // is latched — a "proof of life" published here
                            // after it would mark a dead pane live forever.
                            let alive = st.alive;
                            let facts_after = may_change_facts.then(|| observed_facts(&st));
                            drop(st);
                            // …and a third time, on teardown. From `hangup` on,
                            // nothing this thread still reads describes a pane
                            // in use — while the facts in the record are what
                            // the *next* open builds a successor from. The kill
                            // takes the whole process group down, so a poll
                            // landing between the coding agent's death and the
                            // PTY's EOF reports "nothing recognizable in the
                            // foreground" and would publish that as "the agent
                            // left", wiping the session id `--resume` needs.
                            // That race is why ending a workspace's sessions
                            // sometimes came back to a bare shell instead of
                            // the conversation. The last steady-state answer is
                            // the one worth keeping; `live` is not ours to
                            // write here either — `DeathReporter` owns it.
                            if !shutting_down.load(Ordering::SeqCst)
                                && let (Some(before), Some(after)) = (facts_before, facts_after)
                                && facts_changed(&before, &after)
                            {
                                let (cwd, agent) = after;
                                crate::core::machine::observe_pane(pane, |p| {
                                    // An unknown cwd never clears a seeded one:
                                    // the spawn directory in the record is
                                    // better revival information than nothing.
                                    if cwd.is_some() {
                                        p.cwd = cwd;
                                    }
                                    // The agent fact applies wholesale — its
                                    // `None` means the agent left the
                                    // foreground, and a revival must not
                                    // resume a session that already ended.
                                    p.agent = agent;
                                    // Output is proof of life — but only while
                                    // the pane still is; see `alive` above.
                                    if alive {
                                        p.live = true;
                                    }
                                });
                            }
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break, // EIO after hangup, etc.
                    }
                }

                // Child gone (EOF): report the death — mark not-alive and notify
                // the subscriber, or hand an unattached pane to `on_dead` — unless
                // we initiated teardown. On Windows the monitor may have already
                // reported this same death; the latch makes the second call a
                // no-op. See [`DeathReporter::report`].
                death.report(&state, &shutting_down);
            })
            .expect("spawn daemon pane reader thread")
    }

    /// Become this pane's sole subscriber (replacing any prior one): replay
    /// the ring (a `Size` + `Snapshot` pair per geometry segment), then push
    /// the currently-known `Cwd` / `Prompt` so the fresh client is immediately
    /// in sync.
    ///
    /// The PTY is deliberately *not* resized here. A re-attaching client only
    /// knows a pre-layout placeholder size at this point; resizing to it would
    /// SIGWINCH the shell into redrawing its prompt at a bogus width — and
    /// those redraw bytes land in the ring, corrupting every later replay. The
    /// client instead sizes its grid from our `Size` frame for the replay, and
    /// sends a real `Resize` once it is laid out.
    pub fn attach(&self, subscriber: Sender<DaemonMsg>) -> u64 {
        let mut st = self.state.lock().unwrap();
        let epoch = attach_subscriber(&mut st, subscriber);
        // Frames queued to the *previous* subscriber died with its channel:
        // start this connection's accounting from zero so stale backlog can't
        // park the PTY reader against bytes nobody will ever drain. Ordered
        // with the reader's `add` by the state lock both run under.
        self.gate.reset();
        epoch
    }

    /// Clear the current subscriber (the pane keeps running), but only if `epoch`
    /// still names the current subscriber — a connection that was already replaced
    /// by a newer attach must not blank its successor. Idempotent.
    ///
    /// Returns `true` when, *after* detaching, the pane is reclaimable: the child
    /// has already exited (`!alive`) and no subscriber remains. The caller can then
    /// drop it from the registry instead of leaking it — a dead pane is never
    /// re-attached (clients spawn fresh for `!alive` panes), so removal is invisible.
    /// Computed under the one state lock with the detach, so a concurrent re-attach
    /// can't slip a subscriber in between the clear and the check.
    pub fn detach(&self, epoch: u64) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.subscriber_epoch == epoch {
            st.subscriber = None;
            // Whatever was still queued dies with the channel; clear its
            // accounting so the reader isn't left throttled against it.
            self.gate.reset();
        }
        !st.alive && st.subscriber.is_none()
    }

    /// The pane's Output backpressure gate, shared with the connection's writer
    /// thread (which credits bytes back as it drains them to the socket).
    pub fn gate(&self) -> Arc<OutputGate> {
        self.gate.clone()
    }

    /// Write raw bytes to the PTY (keyboard input / pasted text).
    pub fn write_input(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Ok(mut writer) = self.writer.lock() {
            // A failed write just means the child/pty is gone; the reader will see
            // the same EOF and tear the pane down, so swallow it here.
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    /// The pane's process tree and listening ports, for the GUI's details panel
    /// (`QueryProcs`). A native-SSH pane has no local process tree at all — its
    /// commands run on the far side — so it answers empty rather than reporting
    /// the daemon's own descendants.
    pub fn procs(&self) -> crate::daemon::protocol::PaneProcs {
        let Some(pty) = self.pty() else {
            return Default::default();
        };
        let Some(shell_pid) = pty.shell_pid else {
            return Default::default();
        };
        crate::daemon::procinfo::snapshot(shell_pid, pty_foreground_pgid(&pty.master))
    }

    /// The local-PTY backend, or `None` for a native-SSH pane. PTY-only
    /// operations (resize via master, signal groups, foreground proc queries)
    /// short-circuit when this is `None`.
    fn pty(&self) -> Option<&PtyBackend> {
        match &self.backend {
            PaneBackend::Pty(p) => Some(p),
            PaneBackend::NativeSsh(_) => None,
        }
    }

    /// Resize the byte source: a PTY gets `SIGWINCH` (Unix) / console resize
    /// (Windows); a native-SSH channel gets a `window-change` request. The daemon
    /// holds no grid to resize. Seals the ring's current segment so bytes from
    /// here on are recorded — and later replayed — under the new geometry.
    pub fn resize(&self, size: WinSize) {
        self.state.lock().unwrap().ring.resize(size);
        match &self.backend {
            // A failure just means the pty is gone, which the reader will observe
            // as EOF; `MasterPty::resize` itself takes `&self`.
            PaneBackend::Pty(p) => {
                if let Ok(master) = p.master.lock() {
                    let _ = master.resize(pty_size(size));
                }
            }
            PaneBackend::NativeSsh(b) => b.handle.resize(size),
        }
    }

    /// Whether the child is still running. Part of the pane's public surface for
    /// the integration phase (session restore / pickers); `info()` also carries it.
    #[allow(dead_code)]
    pub fn alive(&self) -> bool {
        self.state.lock().unwrap().alive
    }

    /// Metadata for `List`: cwd prefers the OSC 7 report (falling back to a proc
    /// query), `title` is the foreground process basename when readable (macOS).
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

    /// Hang up the child now; the pane's `Drop` then reaps it. Used by the `Kill`
    /// control message and on registry teardown.
    pub fn kill(&self) {
        self.hangup();
    }

    /// Terminate the child and its whole process group. Signals the group with
    /// SIGHUP (graceful), lets `portable-pty` escalate on the shell pid (SIGHUP →
    /// ~200ms grace → SIGKILL), then SIGKILLs any group survivors so *every* holder
    /// of the slave PTY dies and the reader's blocking `read()` can finally EOF.
    /// Sets `shutting_down` so that EOF is treated as teardown, not a spurious exit.
    /// Idempotent — safe to call from `kill()` and again from `Drop`.
    fn hangup(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        match &self.backend {
            PaneBackend::Pty(p) => {
                // Graceful hangup of the whole group first (lets a shell run EXIT traps).
                #[cfg(unix)]
                Self::signal_group(p, libc::SIGHUP);
                // Windows has no process group to signal: `portable-pty`'s `kill`
                // below terminates only the shell process, so capture and kill its
                // descendant tree *first*, while their parent links still point at
                // the (still-live) shell. Otherwise those children reparent and
                // linger — some still attached to the ConPTY, which would keep the
                // reader's blocking read from EOFing.
                #[cfg(windows)]
                Self::kill_descendants(p);
                if let Ok(mut child) = p.child.lock() {
                    let _ = child.kill();
                }
                // Force-kill anything in the group that ignored/outlived the hangup
                // (a foreground job in its own process group, a `trap '' HUP`
                // child): without this they keep the slave PTY open and the reader
                // thread never EOFs.
                #[cfg(unix)]
                Self::signal_group(p, libc::SIGKILL);
            }
            // A native-SSH pane has no local child/pgid: closing the channel ends
            // the driver, which drops its data sender and EOFs the reader — the
            // same teardown a PTY hangup produces. `shutting_down` (set above) makes
            // that EOF a silent teardown, not a spurious `Exited`.
            PaneBackend::NativeSsh(b) => b.handle.close(),
        }
    }

    /// Terminate every descendant of the shell (children, grandchildren, …). The
    /// shell itself is left to `child.kill()`; this reaches the process tree the
    /// ConPTY's own teardown doesn't. Best effort — a snapshot failure or an
    /// already-exited process just means nothing to do.
    #[cfg(windows)]
    fn kill_descendants(pty: &PtyBackend) {
        if let Some(pid) = pty.shell_pid {
            let procs = crate::daemon::winproc::snapshot();
            for target in crate::daemon::winproc::descendants(&procs, pid) {
                crate::daemon::winproc::terminate(target);
            }
        }
    }

    /// Windows-only: watch the shell for a *natural* exit (`exit`, Ctrl-D, a
    /// crash) the ConPTY reader can't observe. The ConPTY output pipe reports EOF
    /// only once the pseudoconsole is closed (`ClosePseudoConsole`), not when the
    /// child dies — and the reader itself holds a `master` handle (the fg-gate
    /// clone), so it can keep its own pipe open. Without this a shell that exits
    /// on its own leaves the reader blocked and the pane wedged open forever.
    ///
    /// We open a wait-only handle to the shell *now*, while it's alive, so pid
    /// reuse can't retarget the wait, then block a thread on it. When it signals,
    /// the death flows through the shared latch — the same `Exited` / `on_dead`
    /// the reader's EOF drives on Unix. The kill path is unaffected: it sets
    /// `shutting_down`, under which `report` is a silent no-op.
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
        // SAFETY: `OpenProcess` on a currently-live pid for wait-only access. A null
        // return (already gone, or access denied) is handled below; on success the
        // handle is closed by the monitor thread after its single wait.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            // Couldn't watch it — report at once rather than risk wedging the pane
            // open. Opening a just-spawned child essentially never fails.
            death.report(&state, &shutting_down);
            return;
        }
        // `HANDLE` is a raw pointer and thus `!Send`; move it across the thread
        // boundary as an integer and rebuild it inside. It names a kernel object we
        // own for the handle's lifetime, so this is just relocating ownership.
        let handle = handle as isize;
        std::thread::Builder::new()
            .name("tty7-daemon-pane-exit-monitor".to_string())
            .spawn(move || {
                let handle = handle as windows_sys::Win32::Foundation::HANDLE;
                // SAFETY: `handle` is our live process handle; waited once (any
                // return — signaled or failed — means the child is effectively
                // gone), then closed exactly once.
                unsafe {
                    WaitForSingleObject(handle, INFINITE);
                    CloseHandle(handle);
                }
                death.report(&state, &shutting_down);
            })
            .expect("spawn daemon pane exit monitor thread");
    }

    /// Post `sig` to the child's process group(s), not just the shell pid. The
    /// shell is a session/group leader (`portable-pty` `setsid`s it), so its pgid
    /// equals `shell_pid`; a job-control child (vim, less, a pager…) runs in the
    /// terminal's *foreground* process group instead, which that pgid doesn't
    /// cover — so signal both. Only the process group reaches the descendants that
    /// inherited the slave PTY; signalling the bare pid (what `child.kill()` does)
    /// leaves them holding it open and wedges the reader.
    #[cfg(unix)]
    fn signal_group(pty: &PtyBackend, sig: libc::c_int) {
        // SAFETY: `killpg` only posts a signal to a process group; a nonexistent or
        // already-dead group returns `ESRCH`, which we intentionally ignore.
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

    /// Best-effort foreground cwd read straight from the process table — what
    /// `List` reports for a pane whose shell has yet to emit an OSC 7, and (via
    /// the reader's 0.5 s poll) what keeps an *unintegrated* shell's cwd honest.
    /// `None` for a native-SSH pane: there is no local process to read.
    fn foreground_cwd(&self) -> Option<PathBuf> {
        let pty = self.pty()?;
        foreground_cwd(&pty.master, pty.shell_pid)
    }

    /// Executable basename of the PTY's foreground process-group leader, used as the
    /// pane title (macOS/Linux).
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

    /// Windows has no pty foreground-process-group concept, so derive the title
    /// from the process table instead: the deepest command running under the shell
    /// (see [`winproc::foreground_name`](crate::daemon::winproc::foreground_name)).
    /// Empty while the shell sits idle at its prompt, which leaves the pane's
    /// existing title in place.
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

    /// Process-table SSH detection over the local PTY's foreground command. A
    /// native-SSH pane has no local process to inspect (it already carries its own
    /// `RemoteContext`), so this is `None` there.
    fn foreground_remote_context(&self) -> Option<RemoteContext> {
        match &self.backend {
            PaneBackend::Pty(p) => foreground_remote_context(&p.master),
            PaneBackend::NativeSsh(_) => None,
        }
    }
}

impl Drop for DaemonPane {
    fn drop(&mut self) {
        // A native-SSH pane's managed forwards (WS4) are attributed to this pane;
        // tear them down as the pane dies so listeners close and remote bindings are
        // cancelled — the FR-C2 blast radius when a shared connection drops takes
        // every pane through here. Detached, so it never blocks this connection
        // thread.
        if matches!(self.backend, PaneBackend::NativeSsh(_)) {
            crate::daemon::ssh::SshManager::global().teardown_pane_forwards(self.id);
        }
        // Hang up the byte source: SIGHUP → SIGKILL for a PTY child + its group, or
        // channel close for a native-SSH session — so the reader's `read()` can EOF.
        self.hangup();
        // Reap the (now SIGKILLed) shell so it isn't left a zombie. It can't block
        // on a live process: SIGKILL can't be caught, so the shell is dead/dying.
        // A native-SSH pane has no local child to reap.
        if let PaneBackend::Pty(p) = &self.backend {
            if let Ok(mut child) = p.child.lock() {
                let _ = child.wait();
            }
        }
        // Join the reader, but *bounded*. Normally the group-kill above closed the
        // slave and the reader EOFed at once, so this returns immediately. But a
        // fully-detached descendant (its own session, still holding the slave) can
        // be beyond the reach of our signals; never let that wedge this thread —
        // `Drop` runs on a connection thread, and blocking it forever is the P0
        // hang this guards against. If the reader doesn't finish in time, leave it
        // detached (it ends on its own if the slave ever closes).
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

/// Join `handle`, waiting at most `timeout`. Returns `true` if the thread finished
/// (and was joined), `false` if it didn't finish in time — in which case it's left
/// running/detached. This is the backstop that keeps a stuck reader thread (blocked
/// on a `read()` that never EOFs because some descendant still holds the slave PTY)
/// from wedging the connection thread that `DaemonPane::drop` runs on. Uses a
/// throwaway joiner thread because `std::thread::JoinHandle` has no timed join.
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
        // Couldn't even spawn the joiner; don't block. The reader (if stuck) leaks,
        // but the connection thread is freed — the whole point.
        return false;
    }
    rx.recv_timeout(timeout).is_ok()
}

/// Map our `WinSize` (cell grid + per-cell pixel size) to `portable-pty`'s
/// `PtySize`. `pixel_width`/`pixel_height` are the *total* window pixel
/// dimensions (cols × cell_w), matching the `ws_xpixel`/`ws_ypixel` semantics the
/// PTY layer ultimately reports to the child; most programs ignore them.
fn pty_size(size: WinSize) -> PtySize {
    PtySize {
        rows: size.rows.max(1),
        cols: size.cols.max(1),
        pixel_width: size.cols.saturating_mul(size.cell_w),
        pixel_height: size.rows.saturating_mul(size.cell_h),
    }
}

/// The replay ring: raw PTY bytes, oldest-first, segmented by the geometry
/// they were recorded under.
///
/// Raw bytes are only replayable at the width the program wrote them for. A
/// TUI that redraws with cursor-up + erase (Claude Code's inline renderer is
/// the canonical case) computes its row counts from the then-current width;
/// replaying the whole ring at the *final* width re-wraps every older frame,
/// so those redraws land mid-frame and each one leaks stale rows into
/// scrollback — duplication that never existed live. Cutting a new segment at
/// every resize lets `attach` replay each stretch of history at its recorded
/// geometry (a `Size` → `Snapshot` pair per segment), re-wrapping between
/// segments exactly where the live client did.
struct ReplayRing {
    /// Oldest-first, never empty: the back segment is the live tail, and its
    /// geometry is the PTY's current size.
    segments: VecDeque<RingSegment>,
    /// Total payload bytes across all segments, kept ≤ `RING_CAP`.
    len: usize,
}

/// One stretch of PTY output recorded under a single geometry.
struct RingSegment {
    size: WinSize,
    /// A `VecDeque` so evicting the oldest bytes is O(evicted): with a `Vec`,
    /// every append to a full ring memmoved the whole 8 MiB to close the front
    /// gap — at the ~1 KiB-per-read cadence macOS PTYs deliver, that memmove
    /// dominated the daemon's read loop and capped drain throughput at ~5 MB/s.
    bytes: VecDeque<u8>,
}

impl RingSegment {
    fn empty(size: WinSize) -> Self {
        Self {
            size,
            bytes: VecDeque::new(),
        }
    }

    /// The segment's bytes, oldest-first, as one contiguous `Vec` (the
    /// `Snapshot` payload). One copy over the deque's two slices.
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

    /// Seal the tail at a new geometry: bytes appended from here on belong to
    /// a fresh segment. A same-size resize is a no-op, and an empty tail is
    /// retagged in place, so repeated resizes with no output in between (a
    /// window drag over an idle pane) collapse into one segment instead of
    /// piling up empty ones. At `MAX_RING_SEGMENTS` the two oldest segments
    /// merge to make room, mis-wrapping only the oldest scrollback.
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
            // The merged segment keeps `head`'s (newer) geometry; prepending
            // the older bytes shifts the wrap error onto history that was
            // already the least accurate.
            let mut merged = old.bytes;
            merged.extend(head.bytes.drain(..));
            head.bytes = merged;
        }
        self.segments.push_back(RingSegment::empty(size));
    }

    /// Append `bytes` to the live tail, dropping the oldest bytes — and any
    /// segments this empties — past `RING_CAP`. A single write larger than
    /// the cap keeps only its trailing `RING_CAP` bytes (the most recent
    /// screen state), all recorded under the tail's geometry.
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

    /// Replay the ring through `subscriber`: a `Size` + `Snapshot` pair per
    /// segment, oldest first. The client applies each `Size` to its grid
    /// right before advancing the paired `Snapshot` (see the client reader's
    /// `pending_size`), reflowing between segments exactly like the live
    /// resizes did. The tail's pair always goes out — even empty — so the
    /// replay ends at the PTY's current geometry.
    fn replay(&self, subscriber: &Sender<DaemonMsg>) {
        for seg in &self.segments {
            let _ = subscriber.send(DaemonMsg::Size(seg.size));
            let _ = subscriber.send(DaemonMsg::Snapshot(seg.to_vec()));
        }
    }

    /// All payload bytes, oldest-first, geometry boundaries elided. Test-only:
    /// production replay must keep the per-segment sizes.
    #[cfg(test)]
    fn flatten(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len);
        for seg in &self.segments {
            out.extend(seg.to_vec());
        }
        out
    }
}

/// Install `subscriber` as the pane's sole subscriber (replacing any prior
/// one) and replay the pane's known state through it. Called with the state
/// lock held (the pure core of [`DaemonPane::attach`], split out so it is
/// testable without a PTY).
///
/// Send the ring replay + known signals *through the new channel* before we
/// install it, so the client's first frames are the replay, ahead of any live
/// `Output` the reader enqueues next. The replay is a `Size` → `Snapshot`
/// pair per ring segment: each Size leads its segment so the client's grid is
/// at the recorded geometry before those bytes advance (see [`ReplayRing`]).
/// Installing drops the previous sender (its receiver then ends — v1
/// single-client takeover). Returns the new subscriber epoch.
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
    // A dead pane's reader thread — the one that reports the child's exit — is
    // long gone, so replay its exit too: without this an attach racing the
    // child's death (it exited between the client's `List` and its `Attach`)
    // renders the snapshot and then waits forever on a pane that will never
    // speak again, input silently swallowed.
    if !st.alive {
        let _ = subscriber.send(DaemonMsg::Exited { code: None });
    }
    st.subscriber = Some(subscriber);
    st.subscriber_epoch
}

/// The slice of a pane's state the machine tree records about it — the cwd a
/// successor would spawn in, and the agent facts a successor would resume.
/// Captured before and after a chunk's signal application so the (rare) change
/// is published outside the state lock; see the reader loop.
///
/// The cwd crosses as a `String` because the tree's records do (the dialect's
/// path rule); the loss, if any, happens here where it can be seen next to the
/// path that caused it.
fn observed_facts(st: &PaneState) -> (Option<String>, Option<crate::core::machine::AgentFacts>) {
    let cwd = st.cwd.as_ref().map(|p| p.to_string_lossy().into_owned());
    let agent = st.agent.map(|agent| crate::core::machine::AgentFacts {
        agent,
        session_id: st.agent_session.as_ref().and_then(|s| s.session_id.clone()),
        // The session's own argv record wins — it survives the chip clearing —
        // with the identity poll's capture as the fallback until it is stamped.
        launch_argv: st
            .agent_session
            .as_ref()
            .and_then(|s| s.launch_argv.clone())
            .or_else(|| st.agent_argv.clone()),
        status: st.agent_session.as_ref().map(|s| s.status),
    });
    (cwd, agent)
}

/// Whether a chunk's facts are worth a store mutation.
///
/// The coarse agent status is deliberately **outside** the gate: it flips on
/// every hook event (working ↔ waiting ↔ idle), each of which would otherwise
/// rewrite `machine.json` from the PTY reader thread, and it is documented
/// display-only. It still *rides along* — whenever a load-bearing fact
/// changes, the record published carries the current status too.
fn facts_changed(
    before: &(Option<String>, Option<crate::core::machine::AgentFacts>),
    after: &(Option<String>, Option<crate::core::machine::AgentFacts>),
) -> bool {
    before.0 != after.0 || agent_facts_changed(before.1.as_ref(), after.1.as_ref())
}

/// [`facts_changed`]'s agent half: equality over every field but the status.
/// Compared field by field rather than by cloning-and-blanking, because this
/// runs on the pane's reader thread and the argv it would clone is a `Vec` of
/// `String`s.
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

/// Apply sniffed signals to the shared state and notify the subscriber of any cwd
/// / prompt change. Called with the state lock held.
fn apply_signals(st: &mut PaneState, signals: SniffSignals) {
    if let Some(cwd) = signals.cwd {
        if st.cwd.as_ref() != Some(&cwd) {
            if let Some(sub) = &st.subscriber {
                let _ = sub.send(DaemonMsg::Cwd(cwd.clone()));
            }
            st.cwd = Some(cwd);
        }
    }
    // One frame per entry: the client needs every prompt-boundary crossing the
    // chunk carried, not just where it ended up (see [`SniffSignals::shell`]).
    for shell in signals.shell {
        // Windows: agent identity rides the C mark's command capture — ConPTY
        // has no foreground process group for the Unix 0.5 s poll to read an
        // argv from. `C;<cmd>` detects, `D` cleared `command` so it applies
        // `None` and clears the chip. Unix keeps the poll (it sees through
        // scripts and wrappers) and never consults the mark. Applied before
        // the sentinel events below so an event naming the agent can still
        // re-brand within the same chunk.
        //
        // Gated on the capture *changing*: a stray foreign mark mid-command
        // (nested/remote shell, issue #26) re-delivers the same unchanged
        // capture, and when that capture names no agent (a wrapper script the
        // matcher can't see through), re-applying its `None` would wipe an
        // identity the sentinel events established.
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

/// The coding agent named by the shell's last `133;C;<command>` capture — the
/// Windows detection input ([`apply_signals`] applies it there whenever the
/// capture changes). `None` both at the prompt (`D` cleared `command`) and for
/// an unrecognized command, so applying the answer verbatim also clears
/// the chip when the command ends. Compiled on every platform so the unit
/// tests cover it from Unix dev machines; only the Windows build calls it.
/// Whether a shell-mark change should (re-)run mark-derived agent detection
/// on Windows: only when the capture itself changed. `C` (new capture) and `D`
/// (capture cleared) qualify; a stray foreign A/B mark re-delivers the same
/// capture, and when that capture names no agent (a wrapper script the matcher
/// can't see through) re-applying its `None` would wipe an identity the
/// sentinel events established. Compiled on every platform for the unit tests;
/// only the Windows build calls it.
#[cfg_attr(not(windows), allow(dead_code))]
fn shell_mark_capture_changed(prev: &ShellState, next: &ShellState) -> bool {
    prev.command != next.command
}

#[cfg_attr(not(windows), allow(dead_code))]
fn agent_from_shell_mark(
    shell: &ShellState,
    custom: &std::collections::HashMap<String, String>,
) -> Option<crate::core::cli_agent::CLIAgent> {
    // Identity only — deliberately NOT a launch-argv source for resume flag
    // carry-over. The `133;C` capture is terminal *output* (any program can
    // print the OSC and forge it), and forged flags would ride an
    // auto-executed resume command on the next restore; the Unix argv comes
    // from the process table and has no such problem. Windows sessions
    // therefore resume bare.
    shell
        .command
        .as_deref()
        .and_then(|cmd| crate::core::cli_agent::CLIAgent::detect_from_command_with(cmd, custom))
}

/// Fold the chunk's agent signals into the pane's session state and push any
/// resulting change. Called with the state lock held.
///
/// Two tiers: sentinel events (hooks installed) drive the full
/// state machine and may even *identify* the agent where argv detection can't
/// see through a wrapper; a plain OSC 9/777 notification is the no-hooks
/// fallback — it only means "the agent pinged you", so it marks the session
/// `Waiting` (non-rich) and never overrides live rich state.
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
        // An event naming an agent brands the pane even when the process-table
        // poll can't (an unrecognized wrapper binary): identity via protocol.
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

    // Opaque fallback: only meaningful when we know an agent runs here, and
    // never on top of rich state (the hooks channel owns it then).
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

    // A session the events just created starts without the launch argv the
    // identity poll captured — seed it so resume gets the flags no matter
    // which side observed the pane first. Updates keep flowing through
    // [`stamp_launch_argv`].
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

/// Reconcile a process-table cwd reading with what the pane already reports.
/// Called with the state lock held, off the 0.5 s poll.
///
/// **Why this exists.** OSC 7 is the precise channel, but it only speaks for
/// shells tty7 managed to instrument. A shell that `exec`s into another one from
/// its rc file (`exec fish` in `.zshrc`), a nested shell started by hand, or any
/// shell tty7 has no integration for emits nothing — and since a pane's cwd is
/// seeded with its spawn directory, such a pane doesn't report *no* cwd, it
/// reports a permanently stale one. Every consumer then quietly misbehaves: new
/// tabs and splits open in the wrong place (issue #187), and so do the git probe
/// and path completion. The process table knows the truth in all those cases, so
/// consult it — twice a second, off the hot path, alongside the SSH and agent
/// probes that already run there.
///
/// **Why the shell's spelling still wins when both name the same directory.**
/// A shell's `$PWD` keeps the *logical* path the user walked in through; the
/// kernel only knows the physical one. With `~/dev` symlinked onto a volume,
/// OSC 7 says `/Users/me/dev` and the process table says `/Volumes/x/dev` —
/// both correct, but the logical one is what the user typed and expects a new
/// tab to land in. So a reading that resolves to the directory we already report
/// changes nothing; only a genuine disagreement does.
fn apply_probed_cwd(st: &mut PaneState, probed: Option<PathBuf>) {
    let Some(probed) = probed else {
        return;
    };
    // A remote pane's directory lives in the remote's namespace. The local
    // process table can only see the `ssh` client's own cwd, which would be
    // attributed to the remote shell — the very confusion `apply_remote_context`
    // clears the cwd to avoid.
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

/// Whether two paths name the same directory — equal as written, or resolving to
/// the same place through symlinks (see [`apply_probed_cwd`]). A path that can't
/// be resolved at all (deleted since, or a namespace this machine can't see)
/// answers `false`: it is not the directory we just read from the kernel.
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
    // The cwd belonged to whichever side we are leaving, and it does not
    // survive the crossing: a remote path is meaningless locally, and a local
    // one is meaningless on the remote. Drop it so the pane reports no cwd
    // until the new shell's OSC 7 lands, rather than attributing the old
    // namespace's directory to the new one — otherwise a local shell without
    // shell integration keeps serving the remote's last path to the local
    // `git` probe for the rest of its life.
    // `DaemonMsg::Cwd` carries a bare path with no "cleared" form, so the
    // client mirrors this on its own when it sees the `RemoteContext` below.
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
        // Same chip, but the observed argv can still be news: the sentinel
        // events may have branded the pane before the first argv poll, or the
        // user relaunched the agent with different flags.
        stamp_launch_argv(st, argv);
        return;
    }
    // The agent leaving the foreground ends its session: clear the rich state
    // (and tell the client) so a stale "waiting" dot can't outlive the process.
    // The poll can blip momentarily (an agent-spawned subcommand takes the
    // foreground group), but events re-establish state on the next signal.
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

/// Record the detected agent's launch argv and mirror it into the rich session
/// state (pushing the change to the client) when one exists — resume-after-
/// restart reads it from there. `None` (a poll that saw no argv) and an empty
/// argv (Windows mark detection, identity without a trustworthy argv) never
/// wipe a captured value; the chip clearing in [`apply_agent`] does that.
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

/// Whether a foreground command — not the shell itself — currently owns the
/// PTY. True while e.g. `ssh`, `vim`, or a nested shell runs; false when the
/// shell sits idle at its own prompt (it is then the terminal's foreground
/// process group). Unknown/missing data answers false, so a bad reading never
/// suppresses a real local prompt.
///
/// This is the signal that keeps a foreground program's OSC 133 marks — a fish
/// session over ssh emitting its own prompt marks, most visibly — from engaging
/// tty7's local line editor, whose completion and history are local-only and
/// wrong for whatever is really reading the keyboard. See issue #26.
fn foreground_command_running(
    master: &Mutex<Box<dyn MasterPty + Send>>,
    shell_pid: Option<u32>,
) -> bool {
    is_foreground_command(pty_foreground_pgid(master), shell_pid)
}

/// The PTY's foreground process-group id (`pid_t`, i.e. `i32`), read from the
/// terminal via `tcgetpgrp`, or `None` when it can't be read.
#[cfg(unix)]
fn pty_foreground_pgid(master: &Mutex<Box<dyn MasterPty + Send>>) -> Option<i32> {
    master.lock().ok().and_then(|m| m.process_group_leader())
}

/// Windows conpty has no foreground-process-group concept — portable-pty doesn't
/// implement `process_group_leader` there — so there is nothing to gate on: we
/// answer `None`, leaving prompt marks handled exactly as before. (ssh from a
/// Windows tty7 is rare and uses a different model anyway.)
#[cfg(not(unix))]
fn pty_foreground_pgid(_master: &Mutex<Box<dyn MasterPty + Send>>) -> Option<i32> {
    None
}

/// Pure core of [`foreground_command_running`]: given the PTY's foreground
/// process group and the shell's pid, is the foreground group some *other*
/// process (a running command) rather than the shell idling at its prompt?
fn is_foreground_command(fg_pgid: Option<i32>, shell_pid: Option<u32>) -> bool {
    match (fg_pgid, shell_pid) {
        (Some(pg), Some(shell)) if pg > 0 => pg as u32 != shell,
        _ => false,
    }
}

/// The cwd of whatever currently owns the PTY, via `proc_pidinfo` (macOS).
/// Prefers the foreground process group over the shell pid: when the user has
/// started a *nested* shell (`fish` typed into a zsh pane) the outer shell's own
/// cwd stops tracking their `cd`s, and the group leader is the one that moves.
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
        // SAFETY: zeroed buffer of the expected type; real size passed; read
        // back only on success.
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
        // SAFETY: on success the kernel NUL-terminates `vip_path`.
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

/// The cwd of whatever currently owns the PTY, via `/proc/<pid>/cwd` (Linux).
/// Same foreground-group-first preference as the macOS reader.
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
        // A process whose directory was removed under it (`rm -rf` from another
        // pane, `git clean`) reads back as `<path> (deleted)` — the kernel's own
        // annotation, not a path. Harmless while this was only a `List` fallback;
        // now that the reading is broadcast, it would send every new tab and
        // split to a directory that cannot be opened. An unstat-able reading is
        // therefore no reading, and the shell pid gets its turn.
        cwd.is_dir().then_some(cwd)
    };
    pty_foreground_pgid(master)
        .and_then(read_cwd)
        .or_else(|| read_cwd(shell_pid.map(|p| p as i32).unwrap_or(0)))
}

/// No cwd fallback on Windows (or other non-mac/Linux targets): reading another
/// process's working directory needs PEB traversal via `ReadProcessMemory`,
/// which is undocumented and brittle across bitness/elevation. cwd there comes
/// from OSC 7 (the PowerShell shell integration emits it); `None` here just
/// means "no out-of-band fallback", so a shell without integration reports no
/// cwd rather than a wrong one.
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

/// Identify the third-party CLI coding agent (Claude Code, Codex, …) owning the
/// PTY foreground, from its `argv`. Same process-table read as
/// [`foreground_remote_context`]; runs off the hot path on the 0.5 s poll.
/// Always `Some(answer)` — this platform *has* the process-table view, so even
/// "no agent" is a real answer that must apply (it clears the chip when the
/// agent exits). See [`ForegroundProbes::agent`] for the outer option's contract.
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
        // The argv rides along: resume-after-restart replays its flags.
        Some((agent, argv))
    };
    Some(detect())
}

/// Windows: ConPTY has no foreground process group, so there is no process
/// table to poll — "no opinion" (`None`), never applied. Agent identity comes
/// from the shell integration's `133;C;<command>` capture instead, applied in
/// [`apply_signals`] via [`agent_from_shell_mark`].
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn foreground_agent(
    _master: &Mutex<Box<dyn MasterPty + Send>>,
) -> Option<Option<(crate::core::cli_agent::CLIAgent, Vec<String>)>> {
    None
}

// ---------------------------------------------------------------------------
// OSC sniffer (cwd + prompt). The byte-level OSC framing lives in
// `core::osc::OscTokenizer` (shared with the client's notification scanner);
// this layer only routes completed OSC 7 / OSC 133 payloads. Instead of
// mutating shared `Arc<Mutex<..>>` it returns the changes from each `feed`, so
// the pane decides when to notify (it already holds the state lock).
// ---------------------------------------------------------------------------

/// Shell-reported prompt/command state (OSC 133).
#[derive(Default, Clone, PartialEq, Eq)]
struct ShellState {
    active: bool,
    at_prompt: bool,
    last_exit_code: Option<i32>,
    /// The command line the shell reported on its last `133;C;<cmd>` mark
    /// (percent-decoded), cleared when the command finishes (`D`). All of
    /// tty7's shell integrations carry the payload; it is the Windows
    /// coding-agent detection input (see [`agent_from_shell_mark`]).
    command: Option<String>,
}

/// Changes a `feed` call produced, if any.
#[derive(Default)]
struct SniffSignals {
    cwd: Option<PathBuf>,
    /// Shell states completed in this chunk, in stream order — one entry per
    /// `at_prompt` *transition*, not one per marker: a run of marks on the same
    /// side of the prompt boundary folds into its latest state, so the ordinary
    /// `D`/`A`/`B` chunk still yields the single entry it always did.
    ///
    /// Only the last state describes "what the shell is doing now", but the
    /// client keys its prompt *cycle* off the false→true edge
    /// (`terminal::remote::ShellState::cycle` — what releases a Tab handoff, see
    /// `TerminalView::editor_handoff`). Collapsing to the last state alone hides
    /// that edge whenever a whole command cycle (`C` … `D`) lands in one read —
    /// routine over SSH, where a fast command's output arrives in a single
    /// packet — and the handed-off prompt would then never come back to tty7's
    /// line editor.
    shell: Vec<ShellState>,
    /// Sentinel agent events completed in this chunk, in stream order — each
    /// one is a state-machine step, so unlike cwd/shell they must *all* apply
    /// (a `stop` directly after a `notification` still means "done").
    agent_events: Vec<crate::core::cli_agent::AgentEvent>,
    /// A plain (non-sentinel) OSC 9/777 desktop notification completed in this
    /// chunk — the opaque "the agent pinged you" fallback signal for panes
    /// whose agent has no hooks installed. Last body wins.
    notification: Option<String>,
}

struct OscSniffer {
    tok: OscTokenizer,
    /// Running shell state, updated in place as 133 markers arrive.
    shell: ShellState,
}

impl OscSniffer {
    fn new() -> Self {
        Self {
            // 9 / 777 are the notification channels the agent-status layer
            // rides (sentinel events + opaque fallback); the client sniffs the
            // same two independently for its desktop toasts.
            tok: OscTokenizer::new(&[b"7", b"133", b"9", b"777"]),
            shell: ShellState::default(),
        }
    }

    /// Feed a chunk; return any cwd / shell-state change completed within it. (If
    /// a chunk completes several cwd markers the last one wins; shell states keep
    /// every prompt-boundary crossing — see [`SniffSignals::shell`].)
    fn feed(&mut self, bytes: &[u8]) -> SniffSignals {
        let mut signals = SniffSignals::default();
        let shell = &mut self.shell;
        self.tok.feed(bytes, |payload| {
            if let Some(path) = parse_osc7(payload) {
                signals.cwd = Some(path);
            } else if let Some(rest) = payload.strip_prefix(b"133;") {
                if handle_osc133(shell, rest) {
                    match signals.shell.last_mut() {
                        // Still on the same side of the prompt boundary: fold in,
                        // latest wins (it carries the freshest exit code / command
                        // capture). Only a crossing earns its own entry.
                        Some(last) if last.at_prompt == shell.at_prompt => {
                            *last = shell.clone();
                        }
                        _ => signals.shell.push(shell.clone()),
                    }
                }
            } else if let Some(event) = crate::core::cli_agent::parse_agent_event(payload) {
                signals.agent_events.push(event);
            } else if let Some((title, body)) = crate::core::osc::parse_notification(payload) {
                // A sentinel-titled payload whose JSON failed to parse is
                // protocol traffic, not a user notification — drop it.
                if title.as_deref() != Some(crate::core::cli_agent::AGENT_EVENT_SENTINEL) {
                    signals.notification = Some(body);
                }
            }
        });
        signals
    }
}

/// Fold one OSC 133 marker into the running shell state.
fn handle_osc133(shell: &mut ShellState, rest: &[u8]) -> bool {
    shell.active = true;
    // `at_prompt` means "no foreground command is running" — i.e. the shell is
    // drawing or sitting at its prompt, so tty7's local line editor should own
    // the keyboard. Only `C` (command started) clears it; `A` (prompt start),
    // `B` (input begins) and `D` (command finished) all set it.
    //
    // Crucially `D` and `A` set it *before* the prompt text is printed (the
    // byte stream is always `…[D][A][prompt text][B]`), whereas `B` sits at the
    // very end of PS1. Keying `at_prompt` off `B` alone left a window: when the
    // visible prompt text arrived in an earlier PTY chunk than the trailing
    // `B`, `at_prompt` was still false while the prompt was on screen, so keys
    // typed in that gap were routed to the PTY (echoed by the shell into the
    // grid) instead of the editor — the "un-deletable char / doubled prompt"
    // glitch. Setting it as early as `D`/`A` closes that window.
    match rest.first() {
        // A/B deliberately leave `command` alone: every tty7 integration emits
        // D *before* A at a real prompt (so it's already cleared there), while
        // a stray A/B from a foreign integration mid-command (a nested or
        // remote shell drawing its own prompt — Windows has no pgid gate to
        // reject it with, cf. issue #26) must not wipe the agent chip.
        Some(b'A') | Some(b'B') => shell.at_prompt = true,
        Some(b'C') => {
            shell.at_prompt = false;
            // tty7 extension: our shell integrations append the submitted
            // command line, percent-encoded — the Windows agent-detection
            // input (see [`agent_from_shell_mark`]). A bare `C` overwrites the
            // capture with `None` — deliberately, even though a *foreign* bare
            // `C` mid-command then clears the chip: our own PowerShell body
            // falls back to a bare `C` when escaping throws (lone surrogate on
            // PS 5.1), and there a new command *has* started, so keeping the
            // previous capture would misattribute it.
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

/// Build a `PathBuf` from raw OSC-7 path bytes. On Unix paths are arbitrary bytes,
/// so we go through `OsStr` losslessly; elsewhere (Windows) we interpret them as
/// UTF-8 (OSC 7 paths are UTF-8 in practice) and drop the URI's leading slash
/// ahead of a drive letter (see [`strip_uri_drive_slash`]).
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

/// A `file://` URI carries an absolute path with a leading `/`, but a Windows
/// drive path must drop it to be valid: `parse_osc7` hands us `/C:/Users/foo`,
/// which has to become `C:/Users/foo`. Only strips when a drive letter (`X:`)
/// follows, leaving POSIX paths (`/home/x`) and UNC shares untouched. Compiled
/// on all platforms so it's testable off Windows; only used by the non-unix
/// `path_from_bytes` above.
#[cfg_attr(unix, allow(dead_code))]
fn strip_uri_drive_slash(path: &str) -> &str {
    let b = path.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        &path[1..]
    } else {
        path
    }
}

/// Parse an OSC 7 `file://HOST/PATH` (or bare absolute path) payload.
/// `pub(crate)` so the shell-integration tests can round-trip what the snippets
/// actually emit through the parser that consumes it.
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

/// Decode `%XX` percent-escapes.
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

/// Executable basename of `pid` via `proc_pidpath` (macOS).
#[cfg(target_os = "macos")]
fn proc_name(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: valid, correctly-sized buffer; `proc_pidpath` writes at most
    // `buf.len()` bytes and returns the count (<=0 on failure).
    let ret =
        unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if ret <= 0 {
        return None;
    }
    let path = std::str::from_utf8(&buf[..ret as usize]).ok()?;
    Some(path.rsplit('/').next().unwrap_or(path).to_string())
}

/// Executable basename of `pid` via `/proc/<pid>/exe`, falling back to
/// `/proc/<pid>/comm` (Linux).
#[cfg(target_os = "linux")]
fn proc_name(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    // `exe` is a symlink to the full binary path. If the binary was deleted the
    // link target reads "<path> (deleted)" — strip that so the name stays clean.
    if let Ok(path) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let name = name.strip_suffix(" (deleted)").unwrap_or(name);
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    // `exe` can be unreadable (e.g. a setuid foreground process); `comm` is
    // world-readable but kernel-truncated to 15 chars — good enough for a title.
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let comm = comm.trim();
    (!comm.is_empty()).then(|| comm.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A cwd the client reports is only as trustworthy as the OSC 7 behind it.
    /// Passing one this machine cannot resolve straight to `cmd.cwd()` turns a
    /// new tab or split into a hard spawn failure, so anything that isn't a
    /// real directory here must fall through to the next candidate instead.
    #[test]
    fn initial_working_directory_skips_paths_that_are_not_directories() {
        let real = std::env::temp_dir();
        assert!(real.is_dir(), "temp dir should exist");

        // A usable client cwd still wins outright.
        assert_eq!(
            initial_working_directory(Some(real.clone())),
            Some(real.clone())
        );

        // A remote-namespace path, and the msys shape Windows reads as
        // drive-relative: neither resolves here, so neither may be used.
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
            // Whatever we fall back to must itself be usable.
            if let Some(d) = got {
                assert!(d.is_dir(), "fallback {d:?} must be a real directory");
            }
        }

        // A file is not a directory either.
        let file = real.join("tty7-iwd-probe");
        std::fs::write(&file, b"x").expect("write probe file");
        let got = initial_working_directory(Some(file.clone()));
        assert_ne!(got.as_deref(), Some(file.as_path()));
        let _ = std::fs::remove_file(&file);
    }

    /// Issue #187, end-to-end on a *live* PTY: the process-table cwd reading
    /// must follow a child that changed directory without saying so. This is the
    /// shape of an uninstrumented shell — `.zshrc` ending in `exec fish`, a
    /// nested shell, a shell tty7 has no integration for — where OSC 7 never
    /// arrives and the pane would otherwise report its spawn directory forever.
    ///
    /// Deliberately runs the whole platform chain (`process_group_leader` →
    /// `proc_pidinfo` / `/proc/<pid>/cwd`) rather than the pure reconciliation
    /// in [`apply_probed_cwd`]: spawn → PTY → reader poll → process-table read →
    /// `Cwd` to the client is exactly the plumbing the unit tests can't see, so
    /// a break anywhere in it fails here rather than in a bug report.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn live_pane_reports_an_uninstrumented_shells_cwd() {
        // `/usr` is unambiguous, is not the test runner's cwd, and is not a
        // symlink on either platform (`/tmp` is one on macOS), so the reading
        // can be compared verbatim.
        let target = std::path::Path::new("/usr");

        let (tx, rx) = mpsc::channel();
        let pane = DaemonPane::spawn(
            1,
            Some(PathBuf::from("/")), // the spawn directory, and the seeded cwd
            ws(80, 24),
            Some(ShellSpec {
                // `sh` is the point: tty7 has no integration for it, so nothing
                // in this pane will ever emit an OSC 7. `cd` then `exec cat`
                // leaves the child parked in a directory it never announced,
                // holding the PTY foreground group — the `exec fish` case
                // reduced to its essentials.
                program: "sh".into(),
                args: vec!["-c".into(), "cd /usr && exec cat".into()],
                args_are_tty7_defaults: false,
            }),
            None,
            || {},
        )
        .expect("spawn pane");
        pane.attach(tx);

        // The poll rides the reader's chunks, so the pane has to say *something*
        // first; the PTY echoes whatever we send. Bounded, and re-poked each
        // round so a slow `exec` doesn't need the whole budget in one shot.
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

    /// End-to-end check of the *live* agent-detection chain this feature rides
    /// on macOS/Linux: spawn a real PTY child whose `argv[0]` names a coding
    /// agent (`exec -a codex …`), then follow the exact path `foreground_agent`
    /// uses — read the PTY's foreground process-group leader, read its `argv`
    /// from the process table, and run `detect_from_argv`. Guards against a
    /// regression in the platform `process_group_leader` / `foreground_argv`
    /// plumbing that the pure `detect_from_argv` unit tests can't see.
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
        // `exec -a codex` replaces the shell with `cat`, giving it argv[0]=codex
        // while it blocks on stdin — so it stays the PTY's foreground group long
        // enough to observe. `cat` (not `sleep`) keeps it alive until the master
        // is dropped and its stdin EOFs. Must be bash: `exec -a` is a bashism
        // that dash (Ubuntu's /bin/sh) rejects.
        let mut cmd = CommandBuilder::new("bash");
        cmd.args(["-c", "exec -a codex cat"]);
        let mut child = pty.slave.spawn_command(cmd).expect("spawn child");
        let master = Mutex::new(pty.master);

        // Poll for the foreground group to become the child (not the transient
        // `sh`), then detect. Bounded so a stuck spawn fails the test rather than
        // hanging CI.
        let mut detected = None;
        for _ in 0..200 {
            // Flatten: the outer Some is just "this platform has a process
            // table"; the poll keeps going until detection actually answers.
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

    /// Spawn shell precedence: explicit override > configured > platform
    /// default (`None`). Locks the contract stated on [`choose_shell`].
    #[test]
    fn choose_shell_prefers_override_then_config_then_default() {
        let over = ShellSpec {
            program: "fish".into(),
            args: vec!["-l".into()],
            args_are_tty7_defaults: true,
        };
        let cfg = ("zsh".to_string(), vec!["-i".to_string()]);

        // Override wins even when a shell is configured, carrying its
        // arg-ownership flag through.
        assert_eq!(
            choose_shell(Some(over.clone()), Some(cfg.clone())),
            Some(ChosenShell {
                program: "fish".to_string(),
                args: vec!["-l".to_string()],
                args_are_tty7_defaults: true,
            })
        );
        // No override → the configured shell, whose args are the user's and so
        // are never tty7 defaults.
        assert_eq!(
            choose_shell(None, Some(cfg.clone())),
            Some(ChosenShell {
                program: "zsh".to_string(),
                args: vec!["-i".to_string()],
                args_are_tty7_defaults: false,
            })
        );
        // Neither → platform default.
        assert_eq!(choose_shell(None, None), None);
    }

    /// Only *user*-authored args block integration. Locks the contract stated
    /// on [`has_custom_args`] — in particular that the Git Bash dropdown row's
    /// `-i -l` does not, which is what lets it get shell integration at all.
    #[test]
    fn only_user_authored_args_block_shell_integration() {
        let chosen = |args: Vec<&str>, tty7: bool| ChosenShell {
            program: r"C:\Program Files\Git\bin\bash.exe".to_string(),
            args: args.into_iter().map(str::to_string).collect(),
            args_are_tty7_defaults: tty7,
        };

        // The Git Bash dropdown row: tty7 wrote `-i -l`, so `setup_bash` may
        // replace them.
        assert!(!has_custom_args(Some(&chosen(vec!["-i", "-l"], true))));
        // The same args from the user's config.json are theirs to keep.
        assert!(has_custom_args(Some(&chosen(vec!["-i", "-l"], false))));
        // No args at all: nothing to preserve either way.
        assert!(!has_custom_args(Some(&chosen(vec![], false))));
        assert!(!has_custom_args(Some(&chosen(vec![], true))));
        // Platform default — no configured shell at all.
        assert!(!has_custom_args(None));
    }

    /// A WSL pane must be tagged as living in another filesystem namespace, so
    /// `TerminalView::local_cwd` declines the distro's cwd and the local git
    /// probe / completion / link resolution / cwd inheritance never see a path
    /// that means nothing here — and that Windows would read as drive-relative
    /// (`/home/me` -> `C:\home\me`) rather than reject.
    #[cfg(windows)]
    #[test]
    fn wsl_panes_are_tagged_as_a_foreign_filesystem() {
        let spec = |program: &str, args: Vec<&str>| ChosenShell {
            program: program.to_string(),
            args: args.into_iter().map(str::to_string).collect(),
            args_are_tty7_defaults: true,
        };

        // The dropdown's WSL row.
        let ctx = wsl_remote_context(Some(&spec(
            "wsl.exe",
            vec!["--distribution", "Ubuntu-24.04", "--cd", "~"],
        )))
        .expect("wsl.exe must be tagged");
        assert_eq!(ctx.kind, RemoteKind::Wsl);
        // The distro rides along as the target so the UI has a name for it.
        assert_eq!(ctx.target, "Ubuntu-24.04");
        // Nothing reads `argv` for this kind; it is not an ssh invocation.
        assert!(ctx.argv.is_empty());

        // Short flag, and no flag at all (wsl.exe then picks the default
        // distro — still a WSL pane, just one we have no name for).
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
        // The `--distribution=NAME` spelling too — the tag reads the distro with
        // the integration's own parser, so the two cannot disagree about an argv
        // they are both handed.
        assert_eq!(
            wsl_remote_context(Some(&spec("wsl.exe", vec!["--distribution=Arch"])))
                .expect("joined flag")
                .target,
            "Arch"
        );
        // Case- and suffix-insensitive, like every other Windows program name.
        assert!(wsl_remote_context(Some(&spec(r"C:\Windows\System32\WSL.EXE", vec![]))).is_some());

        // Regression: the tag is read off the *resolved* shell, not the
        // per-spawn override. A `wsl.exe` written into `config.json` reaches
        // `setup_wsl` with no override in play (empty args, so nothing custom to
        // preserve), so the distro starts reporting its own cwd — and an
        // untagged pane would hand `/home/me/proj` straight to the local git
        // probe, which Windows resolves drive-relative to `C:\home\me\proj`.
        let from_config = choose_shell(None, Some(("wsl.exe".to_string(), Vec::new())));
        assert_eq!(
            wsl_remote_context(from_config.as_ref()).map(|c| c.kind),
            Some(RemoteKind::Wsl),
            "a configured wsl.exe is as much a WSL pane as a dropdown one"
        );

        // Everything else is a local pane and must not be tagged — tagging it
        // would silently disable its git status, completion and cwd inheritance.
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

    /// A reader that finishes is joined and reported done — the common teardown
    /// path (group-kill closed the slave, the reader EOFed) returns cleanly.
    #[test]
    fn join_bounded_returns_true_when_the_thread_finishes() {
        let handle = std::thread::spawn(|| {});
        assert!(join_bounded(handle, Duration::from_secs(5)));
    }

    /// A reader stuck forever (models one blocked on a `read()` that never EOFs
    /// because a detached grandchild still holds the slave PTY) does *not* wedge the
    /// caller: `join_bounded` gives up after the timeout and returns `false`. This
    /// is the P0 guarantee — `DaemonPane::drop` can never block indefinitely.
    #[test]
    fn join_bounded_times_out_on_a_stuck_thread() {
        let (unblock, blocked) = mpsc::channel::<()>();
        // Blocks until `unblock` is dropped — i.e. "forever" for the test's purposes.
        let handle = std::thread::spawn(move || {
            let _ = blocked.recv();
        });
        assert!(!join_bounded(handle, Duration::from_millis(50)));
        // Let the stuck thread finish so it doesn't linger past the test.
        drop(unblock);
    }

    /// Below the high-water mark the gate never blocks the reader.
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

    /// At the high-water mark the reader parks until the writer credits bytes
    /// back — the backpressure that keeps a flood's backlog bounded.
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

    /// `reset` (attach/detach) unparks a reader throttled against frames that
    /// died with a replaced subscriber channel.
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

    /// A late `sub` racing a `reset` (old writer thread draining after a
    /// re-attach) drives the counter negative and must not panic or wedge.
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

    /// The ring keeps appending verbatim while under the cap.
    #[test]
    fn ring_under_cap_keeps_all() {
        let mut ring = ReplayRing::new(ws(80, 24));
        ring.append(b"hello ");
        ring.append(b"world");
        assert_eq!(ring.flatten(), b"hello world");
    }

    /// Once total exceeds the cap, the oldest bytes are dropped from the front and
    /// the ring holds exactly the most recent `RING_CAP` bytes.
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

    /// A single chunk larger than the cap keeps only its trailing `RING_CAP`
    /// bytes, and collapses any older geometry segments with it.
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

    /// Regression for the "Claude Code scrollback duplicated after reattach"
    /// bug: bytes recorded before and after a resize must replay as separate
    /// `Size` + `Snapshot` pairs, each at its recorded geometry — replaying
    /// everything at the final width re-wraps the older stretch, and a TUI's
    /// cursor-up redraws then leak stale frames into scrollback.
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

    /// Same-size resizes are no-ops and an idle (empty-tail) pane's resizes
    /// retag the tail in place — a window drag must not pile up segments. The
    /// replay still ends at the current geometry, empty tail included.
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

    /// Cap eviction that empties a leading segment drops the segment itself,
    /// so its geometry no longer appears in the replay.
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

    /// Tiny segments (a drag-resize over a redrawing TUI) must not accumulate
    /// without bound: past `MAX_RING_SEGMENTS` the oldest two merge — no bytes
    /// lost, and the merged head carries the *newer* of the two geometries.
    #[test]
    fn ring_caps_segment_count_by_merging_oldest() {
        let mut ring = ReplayRing::new(ws(100, 24));
        let rounds = MAX_RING_SEGMENTS + 10;
        for i in 0..rounds {
            ring.append(format!("seg{i:02} ").as_bytes());
            ring.resize(ws(101 + i as u16, 24));
        }
        assert_eq!(ring.segments.len(), MAX_RING_SEGMENTS);

        // Every byte survives the merges, in order.
        let flat = String::from_utf8(ring.flatten()).unwrap();
        let expect: String = (0..rounds).map(|i| format!("seg{i:02} ")).collect();
        assert_eq!(flat, expect);

        // 74 recorded segments squeezed into 64: the head absorbed the 11
        // oldest, and replays them at the geometry of the newest one merged
        // (seg 11 was recorded at 111 cols).
        let head = ring.segments.front().unwrap();
        assert_eq!(head.size, ws(111, 24));
        assert!(
            String::from_utf8(head.to_vec())
                .unwrap()
                .ends_with("seg11 ")
        );
    }

    /// OSC 7 cwd is sniffed and surfaced as a `cwd` signal.
    #[test]
    fn sniff_osc7_cwd() {
        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]7;file://host/Users/me/dev\x07");
        assert_eq!(sig.cwd, Some(PathBuf::from("/Users/me/dev")));
    }

    /// OSC 133 B/C/D drive the shell prompt state.
    #[test]
    fn sniff_osc133_prompt() {
        let mut s = OscSniffer::new();
        let b = s.feed(b"\x1b]133;B\x07");
        assert!(b.shell.last().unwrap().active);
        assert!(b.shell.last().unwrap().at_prompt);

        let c = s.feed(b"\x1b]133;C\x07");
        assert!(!c.shell.last().unwrap().at_prompt);

        // D (command finished) means no command is running, so we're back at the
        // prompt: at_prompt is true again (it also carries the exit code).
        let d = s.feed(b"\x1b]133;D;130\x07");
        assert!(d.shell.last().unwrap().at_prompt);
        assert_eq!(d.shell.last().unwrap().last_exit_code, Some(130));
    }

    /// A whole command cycle inside ONE chunk still reports the prompt-boundary
    /// crossing. The client counts `at_prompt` false→true edges to tell a fresh
    /// prompt from a same-prompt redraw, and a Tab handoff only returns the line
    /// to tty7's editor on that edge (`TerminalView::editor_handoff`). Reporting
    /// just the chunk's final state hid the edge whenever `C` … `D` arrived
    /// together — the norm over SSH, where a fast command's whole output lands in
    /// one read — so one Tab in an ssh pane disabled the local editor for good.
    #[test]
    fn a_full_command_cycle_in_one_chunk_still_reports_leaving_the_prompt() {
        let mut s = OscSniffer::new();
        s.feed(b"\x1b]133;A\x07\x1b]133;B\x07"); // sitting at the prompt

        // Enter → command → output → done → next prompt, all in one read.
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

    /// The flip side: marks that stay on one side of the boundary fold into a
    /// single state, so the ordinary prompt draw still costs exactly one frame.
    #[test]
    fn marks_on_the_same_side_of_the_prompt_boundary_fold_into_one_state() {
        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]133;D;3\x07\x1b]133;A\x07\x1b]133;B\x07");
        assert_eq!(sig.shell.len(), 1, "D/A/B is one at-prompt state");
        assert!(sig.shell[0].at_prompt);
        assert_eq!(sig.shell[0].last_exit_code, Some(3));
    }

    /// The C mark's command capture (tty7 extension, PowerShell integration) —
    /// the Windows agent-detection input: `C;<cmd>` records the submitted line
    /// percent-decoded, every prompt mark clears it, and
    /// [`agent_from_shell_mark`] turns it into the chip's agent.
    #[test]
    fn sniff_osc133_command_capture_drives_agent_detection() {
        let custom = std::collections::HashMap::new();
        let mut s = OscSniffer::new();

        // A submitted `claude --help` (space percent-encoded, as the
        // PowerShell body emits it).
        let c = s.feed(b"\x1b]133;C;claude%20--help\x07");
        let shell = c.shell.last().unwrap();
        assert!(!shell.at_prompt);
        assert_eq!(shell.command.as_deref(), Some("claude --help"));
        assert_eq!(
            agent_from_shell_mark(shell, &custom),
            Some(crate::core::cli_agent::CLIAgent::Claude)
        );

        // The command finishing (D) clears the capture → the agent clears.
        let d = s.feed(b"\x1b]133;D;0\x07");
        let shell = d.shell.last().unwrap();
        assert_eq!(shell.command, None);
        assert_eq!(agent_from_shell_mark(shell, &custom), None);

        // A non-agent command sets the capture but detects nothing.
        let c = s.feed(b"\x1b]133;C;git%20status\x07");
        let shell = c.shell.last().unwrap();
        assert_eq!(shell.command.as_deref(), Some("git status"));
        assert_eq!(agent_from_shell_mark(shell, &custom), None);

        // A bare `C` (a foreign shell integration) leaves no capture.
        let c = s.feed(b"\x1b]133;C\x07");
        assert_eq!(c.shell.last().unwrap().command, None);

        // A stray A/B mid-command (a nested/remote shell drawing its own
        // prompt) must NOT wipe the capture — only D (command finished) does.
        // Windows has no pgid gate to reject foreign marks with, so this is
        // what keeps the agent chip alive while the agent runs.
        let _ = s.feed(b"\x1b]133;C;codex\x07");
        let a = s.feed(b"\x1b]133;A\x1b]133;B\x07");
        assert_eq!(a.shell.last().unwrap().command.as_deref(), Some("codex"));
        let d = s.feed(b"\x1b]133;D;0\x07");
        assert_eq!(d.shell.last().unwrap().command, None);

        // A multi-line command arrives %0A-joined (fish re-joins the split
        // list with it) and decodes back to real newlines.
        let c = s.feed(b"\x1b]133;C;echo%20a%0Aecho%20b\x07");
        assert_eq!(
            c.shell.last().unwrap().command.as_deref(),
            Some("echo a\necho b")
        );

        // An all-whitespace payload is no capture, like a bare `C`.
        let c = s.feed(b"\x1b]133;C;%20%20\x07");
        assert_eq!(c.shell.last().unwrap().command, None);
    }

    /// The Windows apply gate ([`shell_mark_capture_changed`]): detection
    /// re-runs only when the capture changes, so a stray foreign A/B mark
    /// mid-command — which re-delivers the same capture — can't wipe a
    /// sentinel-established agent identity by re-applying the capture's `None`
    /// (a wrapper script the matcher can't see through detects nothing).
    #[test]
    fn sniff_osc133_stray_marks_do_not_reapply_mark_detection() {
        let mut s = OscSniffer::new();

        // A wrapper launch: capture set, but detection has no answer.
        let mut prev = ShellState::default();
        let c = s.feed(b"\x1b]133;C;.%5Cdev.ps1\x07").shell.pop().unwrap();
        assert!(shell_mark_capture_changed(&prev, &c));
        assert_eq!(
            agent_from_shell_mark(&c, &std::collections::HashMap::new()),
            None
        );
        prev = c;

        // Stray foreign prompt marks mid-command: same capture, no re-apply —
        // an agent branded by sentinel events keeps its chip.
        let ab = s.feed(b"\x1b]133;A\x1b]133;B\x07").shell.pop().unwrap();
        assert!(!shell_mark_capture_changed(&prev, &ab));
        prev = ab;

        // The command finishing clears the capture: that change applies (its
        // `None` is what clears the chip at the prompt).
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

    /// The foreground-command predicate: only a process group *other* than the
    /// shell counts as a running command; matching pids, or missing data, mean
    /// the shell is idle at its own prompt (so we never suppress a real prompt).
    #[test]
    fn foreground_command_distinguishes_the_shell_from_a_command() {
        // Shell idle at its prompt: the shell is the PTY's foreground group.
        assert!(!is_foreground_command(Some(1000), Some(1000)));
        // A command (ssh, vim, a nested shell) owns the PTY: a different group.
        assert!(is_foreground_command(Some(2000), Some(1000)));
        // Unknown foreground group, unknown shell pid, or a non-positive pgid all
        // answer "shell is foreground" — a bad reading must not disengage editing.
        assert!(!is_foreground_command(None, Some(1000)));
        assert!(!is_foreground_command(Some(2000), None));
        assert!(!is_foreground_command(Some(0), Some(1000)));
    }

    /// The reader's gate for issue #26: a remote shell over ssh emits its own
    /// OSC 133 marks, which the sniffer reads as "at prompt" — but because a
    /// foreground command (ssh) owns the PTY, the reader drops that flag so
    /// tty7's local line editor stays disengaged and Tab reaches the remote shell.
    #[test]
    fn foreground_program_prompt_marks_do_not_claim_the_prompt() {
        let mut s = OscSniffer::new();
        // The remote fish draws its prompt: A (start) then B (input begins).
        let mut signals = s.feed(b"\x1b]133;A\x1b]133;B\x07");
        assert!(
            signals.shell.last().unwrap().at_prompt,
            "the raw marks read as at-prompt"
        );

        // The reader consults the foreground gate before reporting. With ssh (a
        // different process group) on the PTY, the prompt flag is cleared.
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

        // Sanity: the very same marks with the shell itself foreground (idle at a
        // local prompt) keep at_prompt true — the local editor still engages.
        let mut local = s.feed(b"\x1b]133;A\x1b]133;B\x07");
        let shell_idle = is_foreground_command(Some(1000), Some(1000));
        if local.shell.iter().any(|st| st.at_prompt) && shell_idle {
            for st in local.shell.iter_mut() {
                st.at_prompt = false;
            }
        }
        assert!(local.shell.last().unwrap().at_prompt);
    }

    /// Regression: a well-formed OSC marker directly following an *unterminated*
    /// one must not be dropped. A bare ESC inside an OSC aborts the current
    /// sequence (VT semantics) and — when the next byte is `]` — introduces a new
    /// OSC. The scanner has to resync on that `]` rather than dropping it into
    /// Ground, or the following marker is silently lost. (The resync itself now
    /// lives in `core::osc::OscTokenizer`; this stays as a routing-level guard
    /// that cwd/prompt markers survive it end to end.)
    #[test]
    fn sniff_resyncs_on_new_osc_after_an_unterminated_one() {
        // OSC 133: an unterminated `133;A` (aborted by the bare ESC that opens the
        // next OSC) immediately followed by a well-formed `133;B`. The B marker
        // drives at_prompt and must survive — dropping it re-opens the "prompt
        // visible but keys mis-routed to the PTY" window this sniffer exists to close.
        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]133;A\x1b]133;B\x07");
        assert!(
            sig.shell.last().is_some_and(|sh| sh.at_prompt),
            "OSC 133;B after an unterminated 133;A was dropped (no resync on `]`)"
        );

        // OSC 7: an unterminated cwd report followed by a well-formed one — the
        // second path must win (the first is discarded, not the second).
        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]7;file://host/dropped\x1b]7;file://host/kept\x07");
        assert_eq!(sig.cwd, Some(PathBuf::from("/kept")));
    }

    /// Regression guard for the "un-deletable char / doubled prompt" glitch.
    ///
    /// A new prompt is emitted as `…[D][A][visible PS1 text][B]`, and only the
    /// trailing `B` used to flip `at_prompt` true. When the visible text and the
    /// trailing `B` landed in *different* PTY read chunks (long prompts with git
    /// status + color escapes make this likely), there was a window where the
    /// client had already rendered the prompt — the user sees it and starts typing
    /// — yet `at_prompt` was still false, so those keys were routed to the PTY
    /// (echoed by ZLE into the grid) instead of the local editor.
    ///
    /// The fix keys `at_prompt` off "no command running", so `D`/`A` (which precede
    /// the prompt text in the stream) already set it true. This test feeds the
    /// prompt as separate chunks and asserts `at_prompt` is true from the moment
    /// the prompt text is visible — i.e. the window is closed.
    #[test]
    fn at_prompt_covers_prompt_draw_gap_across_chunks() {
        let mut s = OscSniffer::new();

        // A command was running…
        assert!(!s.feed(b"\x1b]133;C\x07").shell.last().unwrap().at_prompt);

        // …then finishes: D (in its own chunk, before any prompt text) already
        // marks us back at the prompt.
        let d = s.feed(b"\x1b]133;D;0\x07");
        assert!(
            d.shell.last().unwrap().at_prompt,
            "D should mark us back at the prompt before the prompt text is drawn"
        );

        // The visible prompt text arrives in a later chunk, still WITHOUT the
        // trailing B. Because D already set at_prompt, the state stays true while
        // the prompt is on screen — so a key typed here routes to the editor, not
        // the PTY. This is the window that used to be open.
        let chunk = s.feed(
            b"\x1b]133;A\x07\x1b]7;file://host/repo/tty7\x07\r\ntty7 git:(main) \xe2\x9e\x9c ",
        );
        assert!(
            chunk.shell.last().unwrap().at_prompt,
            "prompt visible but at_prompt=false — the mis-routing window is still open"
        );

        // The trailing B finally arrives and keeps it true.
        assert!(s.feed(b"\x1b]133;B\x07").shell.last().unwrap().at_prompt);
    }

    /// `pty_size` never reports a zero dimension (a 0×0 window would make the
    /// child think it has no room) and derives pixel size from the cell metrics.
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

        // A degenerate 0×0 window clamps rows/cols up to 1.
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

        // Pixel dimensions saturate rather than overflow u16.
        let big = pty_size(WinSize {
            cols: u16::MAX,
            rows: u16::MAX,
            cell_w: u16::MAX,
            cell_h: u16::MAX,
        });
        assert_eq!(big.pixel_width, u16::MAX);
        assert_eq!(big.pixel_height, u16::MAX);
    }

    /// OSC 7 parsing accepts both `file://HOST/PATH` and a bare absolute path, and
    /// rejects anything else.
    #[test]
    fn parse_osc7_forms_and_rejections() {
        // file://HOST/PATH → the path after the host.
        assert_eq!(
            parse_osc7(b"7;file://host/Users/me/dev"),
            Some(PathBuf::from("/Users/me/dev"))
        );
        // An empty host (file:///path) still yields the absolute path.
        assert_eq!(parse_osc7(b"7;file:///etc"), Some(PathBuf::from("/etc")));
        // A bare absolute path (no file:// scheme) is taken verbatim.
        assert_eq!(parse_osc7(b"7;/var/log"), Some(PathBuf::from("/var/log")));
        // Percent-escapes in the path are decoded.
        assert_eq!(
            parse_osc7(b"7;file://host/a%20b"),
            Some(PathBuf::from("/a b"))
        );
        // Percent-encoded multibyte UTF-8 (a CJK dir name) decodes losslessly.
        assert_eq!(
            parse_osc7(b"7;file://host/%E4%B8%AD%E6%96%87"),
            Some(PathBuf::from("/中文"))
        );
        // Round-trip with the shell integration's `%` → `%25` escape: a dir
        // whose name contains a literal `%XX` survives the decode intact.
        assert_eq!(
            parse_osc7(b"7;file://host/tmp/a%2520b"),
            Some(PathBuf::from("/tmp/a%20b"))
        );
        // Missing the `7;` prefix.
        assert!(parse_osc7(b"8;file://host/x").is_none());
        // `file://` with no path slash after the host.
        assert!(parse_osc7(b"7;file://host").is_none());
        // Neither file:// nor an absolute path.
        assert!(parse_osc7(b"7;relative/path").is_none());
        // Decodes to empty → rejected.
        assert!(parse_osc7(b"7;file://host").is_none());
    }

    /// A `file://` URI path arrives with a leading slash; a Windows drive path
    /// (`/C:/…`, what PowerShell's OSC 7 reporter yields) must drop it, while
    /// POSIX and UNC paths keep theirs. This is what makes cwd-inheriting new
    /// tabs work on Windows.
    #[test]
    fn strip_uri_drive_slash_only_unwraps_drive_paths() {
        assert_eq!(strip_uri_drive_slash("/C:/Users/foo"), "C:/Users/foo");
        assert_eq!(strip_uri_drive_slash("/d:/x"), "d:/x");
        // POSIX paths keep their leading slash (no drive letter follows).
        assert_eq!(strip_uri_drive_slash("/home/me/dev"), "/home/me/dev");
        // A UNC share (`//host/share`) is left alone — the second byte is a slash.
        assert_eq!(strip_uri_drive_slash("//host/share"), "//host/share");
        // No leading slash, or too short to be a drive path: untouched.
        assert_eq!(strip_uri_drive_slash("C:/already"), "C:/already");
        assert_eq!(strip_uri_drive_slash("/"), "/");
    }

    /// `%XX` escapes decode; malformed or truncated escapes are kept literally.
    #[test]
    fn percent_decode_handles_escapes_and_garbage() {
        assert_eq!(percent_decode(b"a%20b"), b"a b");
        assert_eq!(percent_decode(b"%2F"), b"/");
        assert_eq!(percent_decode(b"%2f"), b"/"); // lowercase hex
        // Non-hex after % is left verbatim.
        assert_eq!(percent_decode(b"%GG"), b"%GG");
        // A truncated escape at the end has no two following digits → literal.
        assert_eq!(percent_decode(b"x%2"), b"x%2");
        assert_eq!(percent_decode(b"plain"), b"plain");
    }

    /// `hex_val` covers the three hex ranges and rejects everything else.
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

    /// OSC 133 `D` carries an optional exit code; a missing or unparseable code
    /// leaves it `None`, and a negative code parses.
    #[test]
    fn osc133_exit_code_parsing() {
        let mut s = OscSniffer::new();
        // D with no code.
        let d = s.feed(b"\x1b]133;D\x07");
        assert!(d.shell.last().unwrap().at_prompt);
        assert_eq!(d.shell.last().unwrap().last_exit_code, None);

        // D with a non-numeric code stays None.
        let d = s.feed(b"\x1b]133;D;oops\x07");
        assert_eq!(d.shell.last().unwrap().last_exit_code, None);

        // A negative exit code parses.
        let d = s.feed(b"\x1b]133;D;-1\x07");
        assert_eq!(d.shell.last().unwrap().last_exit_code, Some(-1));
    }

    /// A fresh `PaneState` for the PTY-less state-machine tests.
    fn test_state(alive: bool) -> PaneState {
        PaneState {
            // Unit tests publish observations nowhere (no store is installed
            // in this process), so the id is never consulted.
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

    /// What the machine tree is told about a pane is exactly what a successor
    /// needs: the cwd as a string, the session's own argv over the poll's
    /// capture (the session record survives chip churn), and the coarse
    /// status. No agent, no facts — a revival must not resume a session that
    /// was never there.
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

        // The poll's capture is the fallback until the session stamps its own.
        st.agent_session = None;
        let (_, agent) = observed_facts(&st);
        assert_eq!(
            agent.unwrap().launch_argv.as_deref(),
            Some(&["claude".to_string()][..])
        );
    }

    /// Ending a workspace's sessions has to leave a record its successor can
    /// resume from. The kill hangs up the whole process group, so the coding
    /// agent dies before the PTY EOFs — and a poll firing on whatever bytes
    /// still come out then sees nothing recognizable in the foreground.
    /// Published, that answer clears the record's agent, session id and all, and
    /// the reopened workspace comes back to a bare shell instead of the
    /// conversation. So a teardown publishes nothing.
    ///
    /// The second half is the behaviour that must *not* change: the same answer
    /// about a pane nobody is tearing down means the agent exited on its own.
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

        // One read carrying a prompt mark — which is what opens the publish
        // gate — while the poll answers "nothing recognizable in the
        // foreground", the reading a hung-up agent produces.
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

    /// The full daemon-side rich-status path: sentinel OSC events sniffed out
    /// of the byte stream drive the pane's session state machine, identify the
    /// agent when argv detection hasn't, and stream every change to the
    /// subscriber — while a plain notification only fires the opaque fallback.
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

        // The event branded the pane (argv detection never ran here)…
        assert_eq!(st.agent, Some(CLIAgent::Claude));
        // …and the state machine folded both events: idle → working, id kept.
        let sess = st.agent_session.clone().expect("session state exists");
        assert_eq!(sess.status, AgentStatus::Working);
        assert_eq!(sess.session_id.as_deref(), Some("sid-9"));
        assert!(sess.rich);

        // The subscriber saw the identity and the (final) status.
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonMsg::Agent(Some(CLIAgent::Claude)))
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonMsg::AgentStatus(Some(s))) if s.status == AgentStatus::Working
        ));

        // A waiting event lands with its message.
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

        // The agent leaving the foreground clears the session (and says so).
        apply_agent(&mut st, None);
        assert!(st.agent_session.is_none());
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::AgentStatus(None))));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Agent(None))));
    }

    /// The opaque fallback: with an agent detected but no hooks, a plain OSC 9
    /// notification marks the session waiting (non-rich); without an agent it
    /// does nothing; and it never clobbers live rich state.
    #[test]
    fn opaque_notifications_only_fall_back_when_no_rich_state() {
        use crate::core::cli_agent::{AgentSessionState, AgentStatus, CLIAgent};

        // No agent → the notification is ignored (it's just a toast).
        let mut st = test_state(true);
        let mut sniffer = OscSniffer::new();
        apply_signals(&mut st, sniffer.feed(b"\x1b]9;Build finished\x07"));
        assert!(st.agent_session.is_none());

        // Agent detected, no hooks → waiting, non-rich, body kept.
        st.agent = Some(CLIAgent::Codex);
        apply_signals(
            &mut st,
            sniffer.feed(b"\x1b]9;Codex wants to run tests\x07"),
        );
        let sess = st.agent_session.clone().unwrap();
        assert_eq!(sess.status, AgentStatus::Waiting);
        assert!(!sess.rich);

        // Rich state present → the opaque ping is ignored.
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

    /// Issue #187: a pane whose shell tty7 never managed to instrument — the
    /// user's `.zshrc` ends in `exec fish`, so no OSC 7 is ever emitted — keeps
    /// reporting the directory it was *spawned* in, and every new tab opens
    /// there instead of where the user actually is. The process-table reading
    /// corrects it, and the client is told.
    #[test]
    fn probed_cwd_corrects_a_pane_whose_shell_never_reports_osc7() {
        let mut st = test_state(true);
        // What `spawn` seeds: the pane's initial directory, and — with no OSC 7
        // — for the rest of the pane's life.
        st.cwd = Some(PathBuf::from("/Users/alice"));
        let (tx, rx) = mpsc::channel();
        st.subscriber = Some(tx);

        apply_probed_cwd(&mut st, Some(PathBuf::from("/Users/alice/dev/tty7")));

        assert_eq!(st.cwd.as_deref(), Some(Path::new("/Users/alice/dev/tty7")));
        assert!(
            matches!(rx.try_recv(), Ok(DaemonMsg::Cwd(p)) if p == PathBuf::from("/Users/alice/dev/tty7"))
        );
    }

    /// The shell's own spelling outranks the kernel's when both name the same
    /// directory: `$PWD` keeps the symlinked path the user walked in through,
    /// and that is the one a new tab should open in. Uses a real symlink so the
    /// canonicalization is the actual one, not a stand-in.
    ///
    /// Unix only, and not because the reconciliation is: creating a symlink on
    /// Windows needs `SeCreateSymbolicLinkPrivilege`, which an unelevated shell
    /// without Developer Mode does not hold, so the setup — not the assertion —
    /// fails on an ordinary Windows box with `ERROR_PRIVILEGE_NOT_HELD`. Nothing
    /// is lost by skipping it there: `foreground_cwd` answers `None` off
    /// macOS/Linux, so no Windows pane ever reaches this comparison.
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
        st.cwd = Some(link.clone()); // as OSC 7 reported it
        let (tx, rx) = mpsc::channel();
        st.subscriber = Some(tx);

        // The kernel reports the physical path — same directory, different name.
        apply_probed_cwd(&mut st, Some(real.canonicalize().unwrap()));

        assert_eq!(st.cwd.as_deref(), Some(link.as_path()));
        assert!(rx.try_recv().is_err(), "same directory → nothing to tell");

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// The same rule on the path every platform takes — a reading that matches
    /// what we already report letter for letter, needing no filesystem at all.
    /// The poll fires twice a second for the whole life of a pane, so a re-send
    /// here would be a `Cwd` frame (and the git probe behind it) twice a second
    /// on an idle shell.
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

    /// A remote pane's cwd lives in the remote's namespace; the only thing the
    /// local process table can see is the `ssh` client's own directory, which
    /// must never be attributed to the remote shell.
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

    /// No reading (native SSH, Windows, an unreadable process) is "no opinion",
    /// never "no cwd" — it must not clear what the pane already reports.
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

    /// Attaching replays Size → Snapshot (→ Cwd) in order and installs the
    /// subscriber under a fresh epoch.
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
        // A live pane replays no exit; the reader thread reports that live.
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

    /// Regression: attaching to a pane whose child already exited must replay
    /// the exit too — the reader thread that would have reported it is gone, so
    /// without this the client renders the snapshot and then waits forever.
    #[test]
    fn attach_to_a_dead_pane_replays_exited() {
        let mut st = test_state(false);
        st.ring.append(b"final screen");

        let (tx, rx) = mpsc::channel();
        attach_subscriber(&mut st, tx);
        // Skip the geometry + snapshot replay, then the exit must follow.
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Size(_))));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Snapshot(_))));
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonMsg::Exited { code: None })
        ));
    }

    /// EOF with a subscriber attached: `Exited` goes to the subscriber and the
    /// pane is NOT handed to `on_dead` — that connection's detach reclaims it.
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
            || false, // no PTY here → treat the shell as foreground
            ForegroundProbes {
                remote: Box::new(|| None),
                agent: Box::new(|| None),
                cwd: Box::new(|| None),
            },
            Arc::new(DeathReporter::new(move || {
                dead_flag.store(true, Ordering::SeqCst)
            })),
        );
        handle.join().unwrap(); // the Cursor EOFs immediately after "tail"

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

    /// Wiring for issue #187: the reader's foreground poll applies the cwd
    /// reading, so an uninstrumented shell's directory reaches the client with
    /// no OSC 7 anywhere in the byte stream. The poll gate opens on the first
    /// chunk, so one read is enough.
    #[test]
    fn reader_poll_applies_the_probed_cwd() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = mpsc::channel();
        {
            let mut st = state.lock().unwrap();
            st.cwd = Some(PathBuf::from("/Users/alice")); // the spawn directory
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

    /// Regression: EOF with *nobody* attached must fire `on_dead` so the server
    /// can drop the pane — otherwise a detached pane whose shell exits leaks its
    /// zombie child and replay ring in the registry for the daemon's lifetime.
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

    /// During owner-initiated teardown (`shutting_down`), EOF neither notifies
    /// nor fires `on_dead` — the killer owns the registry cleanup.
    #[test]
    fn reader_eof_during_shutdown_is_silent() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let dead = Arc::new(AtomicBool::new(false));
        let dead_flag = dead.clone();

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(true)), // teardown already initiated
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

    /// The latch that lets the reader's EOF and (on Windows) the child-exit
    /// monitor both report the same death without the subscriber seeing two
    /// `Exited`s: the first `report` notifies, the second is a silent no-op.
    #[test]
    fn death_reporter_notifies_once_across_racing_callers() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = mpsc::channel();
        state.lock().unwrap().subscriber = Some(sub_tx);
        let shutting_down = AtomicBool::new(false);
        let calls = Arc::new(AtomicBool::new(false));
        let calls_flag = calls.clone();
        let death = DeathReporter::new(move || calls_flag.store(true, Ordering::SeqCst));

        // Two reporters (stand-ins for the reader and the monitor) both fire.
        death.report(&state, &shutting_down);
        death.report(&state, &shutting_down);

        assert!(!state.lock().unwrap().alive);
        // Exactly one `Exited`, then nothing more.
        assert!(matches!(
            sub_rx.try_recv(),
            Ok(DaemonMsg::Exited { code: None })
        ));
        assert!(
            sub_rx.try_recv().is_err(),
            "a second report must not re-notify"
        );
    }

    /// With nobody attached, the *first* report hands the pane to `on_dead` and a
    /// racing second report neither re-fires it nor panics on the taken `FnOnce`.
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

    /// Every pane is told which terminal it is running in, under the names the
    /// rest of the world reads (`TERM_PROGRAM`/`TERM_PROGRAM_VERSION`) as well
    /// as our own `TTY7` marker. Nothing third-party looks for the marker, so
    /// dropping the standard pair would leave capability probes guessing.
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

    /// The user's `env` map may rename the terminal — posing as another program
    /// is how you get a tool that only recognises a fixed list to light up —
    /// but it may not contradict what our emulator can decode. Later entries
    /// win, so the ordering is the precedence.
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

    /// Windows environment blocks are case-insensitive — `portable-pty` keeps
    /// one slot per lowercased key — so a configured `Term` would replace
    /// `TERM` just as surely as the exact spelling. The capability filter must
    /// therefore drop any casing of a capability key, not just the canonical
    /// one. (On Unix a differently-cased key is a distinct variable and passes
    /// through untouched.)
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
        // Identity keys stay overridable in any casing the user spells.
        assert_eq!(get("term_program"), Some("x"));
    }

    /// The macOS UTF-8 fallback applies only when the inherited environment has
    /// no locale and the user has not taken control through the generic `env`
    /// map. Key presence is authoritative there, including an empty value.
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

    /// A CFLocale identifier is not a POSIX locale name: it can carry a script
    /// subtag, keywords, and hyphens, and it may name a region combination the
    /// machine has no locale for (`en_CN` is a perfectly ordinary macOS setting).
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

        // No region to name a locale with, so there is nothing to derive.
        assert_eq!(stem("zh"), None);
        assert_eq!(stem("zh_Hans"), None);
        assert_eq!(stem(""), None);
        assert_eq!(stem("@calendar=gregorian"), None);
    }

    /// The derived locale must exist on the machine before it is exported.
    /// `LC_CTYPE` outranks the `LANG` a remote host sets for itself and ssh
    /// forwards `LC_*`, so an unloadable name would break non-ASCII output on
    /// every host we ssh into — the exact failure this seeding exists to fix.
    #[test]
    fn character_locale_only_returns_installed_locales() {
        let installed = |names: &'static [&'static str]| move |n: &str| names.contains(&n);

        // The system locale is used when the machine actually has it.
        assert_eq!(
            character_locale(Some("zh_Hans_CN"), installed(&["zh_CN.UTF-8", "C.UTF-8"])),
            Some("zh_CN.UTF-8".to_string())
        );
        // `en_CN` resolves to no installed locale, so it falls through.
        assert_eq!(
            character_locale(Some("en_CN"), installed(&["C.UTF-8", "en_US.UTF-8"])),
            Some("C.UTF-8".to_string())
        );
        // Older macOS predates `C.UTF-8`; `en_US.UTF-8` is there on every macOS.
        assert_eq!(
            character_locale(Some("en_CN"), installed(&["en_US.UTF-8"])),
            Some("en_US.UTF-8".to_string())
        );
        // No identifier at all still yields a usable fallback.
        assert_eq!(
            character_locale(None, installed(&["C.UTF-8", "en_US.UTF-8"])),
            Some("C.UTF-8".to_string())
        );
        // Nothing installed means nothing exported — never a name that fails to load.
        assert_eq!(character_locale(Some("zh_Hans_CN"), installed(&[])), None);
    }

    /// The locale actually derived on this machine must be one the C library can
    /// load, since an unloadable `LC_CTYPE` is worse than none at all.
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

    /// Every spawned shell carries the `TTY7` marker, so the `tty7 agent-hook`
    /// emitter fires (it stays silent without it). This is the env side of the
    /// rich-status channel — a regression here silently breaks all hook-based
    /// agent status, which no other test would catch.
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

    /// `apply_signals` writes sniffed cwd/shell state into the pane state.
    #[test]
    fn apply_signals_updates_state() {
        let mut st = test_state(true);

        // A cwd signal lands in the state.
        apply_signals(
            &mut st,
            SniffSignals {
                cwd: Some(PathBuf::from("/tmp/x")),
                ..SniffSignals::default()
            },
        );
        assert_eq!(st.cwd, Some(PathBuf::from("/tmp/x")));

        // A shell signal updates the prompt state.
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

        // An empty signal set changes nothing.
        apply_signals(&mut st, SniffSignals::default());
        assert_eq!(st.cwd, Some(PathBuf::from("/tmp/x")));
    }
}

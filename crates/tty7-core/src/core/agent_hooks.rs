use std::io;
use std::io::{IsTerminal as _, Read as _};
use std::path::{Path, PathBuf};

use crate::core::cli_agent::AGENT_EVENT_SENTINEL;
use crate::host::Host;

pub const TTY7_ENV_MARKER: &str = "TTY7";

const GROK_HOOK_ENV: &str = "GROK_HOOK_EVENT";

const MAX_STDIN: u64 = 64 * 1024;

pub fn run_agent_hook(agent: &str, event: &str) {
    detach_console();
    if std::env::var_os(TTY7_ENV_MARKER).is_none() {
        return;
    }
    let agent = effective_agent(agent, std::env::var_os(GROK_HOOK_ENV).is_some());
    let mut input = String::new();
    if !std::io::stdin().is_terminal() {
        let _ = std::io::stdin().take(MAX_STDIN).read_to_string(&mut input);
    }
    let Some(event) = effective_event(agent, event, &input) else {
        return;
    };
    write_to_controlling_tty(&build_hook_sequence(agent, event, &input));
}

#[cfg(not(unix))]
fn detach_console() {
    use windows_sys::Win32::System::Console::FreeConsole;
    unsafe {
        FreeConsole();
    }
}

#[cfg(unix)]
fn detach_console() {}

fn effective_agent(agent: &str, ran_by_grok: bool) -> &str {
    if ran_by_grok { "grok" } else { agent }
}

fn effective_event<'a>(agent: &str, event: &'a str, stdin_json: &str) -> Option<&'a str> {
    if matches!(agent, "copilot" | "grok") && event == "notification" {
        let blocks = stdin_json.contains("elicitation_dialog")
            || (agent == "copilot" && stdin_json.contains("permission_prompt"));
        return blocks.then_some("permission-request");
    }
    Some(event)
}

fn build_hook_sequence(agent: &str, event: &str, stdin_json: &str) -> Vec<u8> {
    let payload: serde_json::Value =
        serde_json::from_str(stdin_json).unwrap_or(serde_json::json!({}));
    let mut body = serde_json::json!({
        "v": 1,
        "agent": agent,
        "event": event,
    });
    for (key, alias) in [
        ("session_id", "sessionId"),
        ("message", "message"),
        ("cwd", "cwd"),
    ] {
        if let Some(v) = payload
            .get(key)
            .or_else(|| payload.get(alias))
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
        {
            body[key] = serde_json::Value::String(v.to_string());
        }
    }
    format!("\x1b]777;notify;{AGENT_EVENT_SENTINEL};{body}\x07").into_bytes()
}

#[cfg(unix)]
fn write_to_controlling_tty(bytes: &[u8]) -> bool {
    if write_dev(std::path::Path::new("/dev/tty"), bytes) {
        return true;
    }
    if let Some(dev) = ancestor_tty_device() {
        return write_dev(&dev, bytes);
    }
    false
}

#[cfg(unix)]
fn write_dev(path: &std::path::Path, bytes: &[u8]) -> bool {
    use std::io::Write as _;
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(mut tty) => tty.write_all(bytes).and_then(|_| tty.flush()).is_ok(),
        Err(_) => false,
    }
}

#[cfg(unix)]
fn ancestor_tty_device() -> Option<std::path::PathBuf> {
    use std::process::Command;
    let mut pid = unsafe { libc::getppid() };
    for _ in 0..8 {
        if pid <= 1 {
            break;
        }
        let out = Command::new("ps")
            .args(["-o", "tty=", "-o", "ppid=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        let line = String::from_utf8_lossy(&out.stdout);
        let mut fields = line.split_whitespace();
        let tty = fields.next().unwrap_or("");
        let ppid: i32 = fields.next().and_then(|s| s.parse().ok()).unwrap_or(1);
        if !tty.is_empty() && tty != "??" && tty != "?" {
            return Some(std::path::PathBuf::from(format!("/dev/{tty}")));
        }
        pid = ppid;
    }
    None
}

#[cfg(not(unix))]
fn write_to_controlling_tty(bytes: &[u8]) -> bool {
    let procs = crate::daemon::winproc::snapshot();
    let ancestors = ancestor_pids(&procs);

    let name_of = |pid: u32| {
        procs
            .iter()
            .find(|p| p.pid == pid)
            .map(|p| p.name.to_ascii_lowercase())
    };
    let shell = ancestors.iter().copied().find(|&pid| {
        procs
            .iter()
            .find(|p| p.pid == pid)
            .and_then(|p| name_of(p.parent))
            .is_some_and(|n| is_tty7_host_exe(&n))
    });

    if let Some(pid) = shell {
        if attach_and_write(pid, bytes) {
            return true;
        }
    }

    let mut any = false;
    for pid in ancestors {
        any |= attach_and_write(pid, bytes);
    }
    any
}

#[cfg(any(not(unix), test))]
fn is_tty7_host_exe(name: &str) -> bool {
    matches!(name, "tty7-app.exe" | "tty7-server.exe" | "tty7.exe")
}

#[cfg(not(unix))]
fn attach_and_write(pid: u32, bytes: &[u8]) -> bool {
    use windows_sys::Win32::System::Console::{AttachConsole, FreeConsole};
    unsafe {
        FreeConsole();
        if AttachConsole(pid) == 0 {
            return false;
        }
    }
    let ok = write_conout(bytes);
    unsafe {
        FreeConsole();
    }
    ok
}

#[cfg(not(unix))]
fn write_conout(bytes: &[u8]) -> bool {
    use std::io::Write as _;
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONOUT$")
    {
        Ok(mut out) => out.write_all(bytes).and_then(|_| out.flush()).is_ok(),
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn ancestor_pids(procs: &[crate::daemon::winproc::Proc]) -> Vec<u32> {
    let parent_of = |pid: u32| procs.iter().find(|p| p.pid == pid).map(|p| p.parent);
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut cur = std::process::id();
    seen.insert(cur);
    for _ in 0..16 {
        match parent_of(cur) {
            Some(parent) if parent != 0 && seen.insert(parent) => {
                out.push(parent);
                cur = parent;
            }
            _ => break,
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HookAgent {
    Claude,
    Codex,
    Copilot,
    OpenCode,
    Pi,
    Grok,
}

impl HookAgent {
    pub const ALL: [HookAgent; 6] = [
        HookAgent::Claude,
        HookAgent::Codex,
        HookAgent::Copilot,
        HookAgent::OpenCode,
        HookAgent::Pi,
        HookAgent::Grok,
    ];

    pub fn slug(self) -> &'static str {
        match self {
            HookAgent::Claude => "claude",
            HookAgent::Codex => "codex",
            HookAgent::Copilot => "copilot",
            HookAgent::OpenCode => "opencode",
            HookAgent::Pi => "pi",
            HookAgent::Grok => "grok",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            HookAgent::Claude => "Claude Code",
            HookAgent::Codex => "Codex",
            HookAgent::Copilot => "Copilot CLI",
            HookAgent::OpenCode => "OpenCode",
            HookAgent::Pi => "Pi",
            HookAgent::Grok => "Grok Build",
        }
    }

    pub fn target_display(self, target: &HookTarget) -> String {
        target.abbreviate_home(&self.target_path(target))
    }

    fn target_path(self, target: &HookTarget) -> PathBuf {
        match self {
            HookAgent::Claude => target.claude_settings_path(),
            HookAgent::Codex => target.under_home(&[".codex", "hooks.json"]),
            HookAgent::Copilot => target.under_home(&[".copilot", "hooks", OWNED_FILE_STEM_JSON]),
            HookAgent::OpenCode => target.under(
                &target.xdg_config_dir(),
                &["opencode", "plugins", OWNED_FILE_STEM_JS],
            ),
            HookAgent::Pi => target.under_home(&[".pi", "agent", "extensions", "tty7", "index.ts"]),
            HookAgent::Grok => target.under_home(&[".grok", "hooks", OWNED_FILE_STEM_JSON]),
        }
    }

    fn marker(self) -> String {
        format!("agent-hook {}", self.slug())
    }
}

const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

pub struct HookTarget<'a> {
    host: &'a dyn Host,
    home: PathBuf,
    exe: PathBuf,
}

impl<'a> HookTarget<'a> {
    pub fn local(host: &'a dyn Host) -> Option<HookTarget<'a>> {
        Some(HookTarget {
            host,
            home: home_dir()?,
            exe: std::env::current_exe().ok()?,
        })
    }

    pub fn remote(host: &'a dyn Host, home: PathBuf) -> HookTarget<'a> {
        let dialect = crate::daemon::install::RemoteProtocol::of_this_build();
        let binary = crate::daemon::install::asset::remote_paths(
            &home.to_string_lossy(),
            dialect.control,
            dialect.protocol,
        )
        .binary;
        HookTarget {
            host,
            home,
            exe: PathBuf::from(binary),
        }
    }

    fn is_local(&self) -> bool {
        self.host.id().is_local()
    }

    fn under(&self, base: &Path, parts: &[&str]) -> PathBuf {
        let mut p = base.to_path_buf();
        for part in parts {
            p = self.host.join(&p, part);
        }
        p
    }

    fn under_home(&self, parts: &[&str]) -> PathBuf {
        self.under(&self.home, parts)
    }

    fn claude_settings_path(&self) -> PathBuf {
        if self.is_local()
            && let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty())
        {
            return PathBuf::from(dir).join("settings.json");
        }
        self.under_home(&[".claude", "settings.json"])
    }

    fn xdg_config_dir(&self) -> PathBuf {
        if self.is_local()
            && let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty())
        {
            return PathBuf::from(dir);
        }
        self.under_home(&[".config"])
    }

    fn hook_command(&self, agent: HookAgent, event: &str) -> String {
        if let Some(exe) = self.hook_command_exe() {
            return format!("{exe} agent-hook {} {event}", agent.slug());
        }
        format!(
            "\"{}\" agent-hook {} {event}",
            self.exe.display(),
            agent.slug()
        )
    }

    /// A shell-safe executable path for generated hook commands.
    ///
    /// On Windows, Codex runs hook commands through the session's shell, which
    /// is frequently PowerShell. `pwsh -Command` drops the quotes around a
    /// path containing spaces, so the usual `"C:\Program Files\..."` form is
    /// parsed as `C:\Program` and fails — and even a quoted path without
    /// spaces is a syntax error in PowerShell (invoking a quoted path requires
    /// the `&` call operator). Resolving the space-free 8.3 short path lets us
    /// emit the path bare, which both `cmd.exe` and PowerShell execute.
    ///
    /// Returns `None` when no space-free short path is available; callers then
    /// fall back to the quoted long path.
    fn hook_command_exe(&self) -> Option<String> {
        #[cfg(windows)]
        if self.is_local() {
            if let Some(short) = short_path(&self.exe) {
                return Some(short);
            }
        }
        None
    }

    fn read(&self, p: &Path) -> io::Result<String> {
        let bytes = self.host.read_file(p, MAX_CONFIG_BYTES)?;
        String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn write(&self, p: &Path, bytes: &[u8]) -> anyhow::Result<()> {
        if let Some(parent) = p.parent() {
            self.host.create_dir(parent, true)?;
        }
        if self.is_local() {
            crate::core::config::write_atomic(p, bytes)?;
        } else {
            self.host.write_file(p, bytes)?;
        }
        Ok(())
    }

    fn abbreviate_home(&self, path: &Path) -> String {
        match path.strip_prefix(&self.home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HooksState {
    NotInstalled,
    Installed,
    Outdated,
}

pub fn hooks_state(target: &HookTarget, agent: HookAgent) -> HooksState {
    let path = agent.target_path(target);
    match agent {
        HookAgent::Claude => hook_map_state(target, &path, agent, CLAUDE_HOOK_EVENTS),
        HookAgent::Codex => hook_map_state(target, &path, agent, CODEX_HOOK_EVENTS),
        HookAgent::Copilot | HookAgent::OpenCode | HookAgent::Pi | HookAgent::Grok => {
            let Some(expected) = owned_file_content(target, agent) else {
                return HooksState::NotInstalled;
            };
            owned_file_state(target, &path, &expected, &agent.marker())
        }
    }
}

pub fn install_hooks(target: &HookTarget, agent: HookAgent) -> anyhow::Result<String> {
    let path = agent.target_path(target);
    match agent {
        HookAgent::Claude => {
            hook_map_install(target, &path, agent, CLAUDE_HOOK_EVENTS)?;
            Ok("Installed".to_string())
        }
        HookAgent::Codex => {
            hook_map_install(target, &path, agent, CODEX_HOOK_EVENTS)?;
            if !target.is_local() {
                return Ok(
                    "Installed — run `codex features enable hooks` once on that machine"
                        .to_string(),
                );
            }
            Ok(match enable_codex_hooks_feature() {
                Ok(()) => "Installed".to_string(),
                Err(e) => format!(
                    "Installed, but couldn't run `codex features enable hooks` ({e}) — run it once manually"
                ),
            })
        }
        HookAgent::Copilot | HookAgent::OpenCode | HookAgent::Pi | HookAgent::Grok => {
            let content = owned_file_content(target, agent)
                .ok_or_else(|| anyhow::anyhow!("{agent:?} has no owned file"))?;
            owned_file_install(target, &path, &content, &agent.marker())?;
            Ok("Installed".to_string())
        }
    }
}

pub fn uninstall_hooks(target: &HookTarget, agent: HookAgent) -> anyhow::Result<String> {
    let path = agent.target_path(target);
    match agent {
        HookAgent::Claude | HookAgent::Codex => hook_map_uninstall(target, &path, agent),
        HookAgent::Copilot | HookAgent::OpenCode | HookAgent::Pi | HookAgent::Grok => {
            owned_file_uninstall(target, &path, &agent.marker())
        }
    }
}

pub fn refresh_hooks(target: &HookTarget) -> usize {
    let mut refreshed = 0;
    for agent in HookAgent::ALL {
        if hooks_state(target, agent) != HooksState::Outdated {
            continue;
        }
        match install_hooks(target, agent) {
            Ok(summary) => {
                refreshed += 1;
                log::info!(
                    "refreshed stale {} hooks at {}: {summary}",
                    agent.display_name(),
                    agent.target_display(target)
                );
            }
            Err(e) => log::warn!(
                "could not refresh stale {} hooks: {e}",
                agent.display_name()
            ),
        }
    }
    refreshed
}

pub fn refresh_remote_hooks(host: &dyn Host, home: PathBuf) -> usize {
    if home_dir().is_some_and(|ours| ours == home) {
        return 0;
    }
    refresh_hooks(&HookTarget::remote(host, home))
}

pub fn refresh_hooks_at_launch() -> usize {
    if cfg!(debug_assertions) {
        return 0;
    }
    let host = crate::host::local::LocalHost::new();
    let Some(target) = HookTarget::local(&*host) else {
        return 0;
    };
    refresh_hooks(&target)
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
    #[cfg(not(unix))]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
}

const OWNED_FILE_STEM_JSON: &str = "tty7.json";
const OWNED_FILE_STEM_JS: &str = "tty7.js";

const CLAUDE_HOOK_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "prompt-submit"),
    ("Notification", "notification"),
    ("PostToolUse", "tool-complete"),
    ("Stop", "stop"),
    ("SessionEnd", "session-end"),
];

const CODEX_HOOK_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "prompt-submit"),
    ("Stop", "stop"),
];

const GROK_HOOK_TIMEOUT_SECS: u32 = 10;

const GROK_HOOK_EVENTS: &[(&str, &str, Option<&str>)] = &[
    ("SessionStart", "session-start", None),
    ("UserPromptSubmit", "prompt-submit", None),
    ("Notification", "notification", Some("elicitation_dialog")),
    ("PostToolUse", "tool-complete", None),
    ("Stop", "stop", None),
    ("SessionEnd", "session-end", None),
];

fn hook_map_state(
    target: &HookTarget,
    path: &Path,
    agent: HookAgent,
    events: &[(&str, &str)],
) -> HooksState {
    let Ok(text) = target.read(path) else {
        return HooksState::NotInstalled;
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) else {
        return HooksState::NotInstalled;
    };
    let marker = agent.marker();
    let (mut any, mut complete) = (false, true);
    for (hook_event, tty7_event) in events {
        let ours = root
            .get("hooks")
            .and_then(|h| h.get(hook_event))
            .and_then(|e| e.as_array())
            .and_then(|list| list.iter().find_map(|m| marker_command(m, &marker)));
        match ours {
            Some(cmd) => {
                any = true;
                if cmd != target.hook_command(agent, tty7_event) {
                    complete = false;
                }
            }
            None => complete = false,
        }
    }
    match (any, complete) {
        (false, _) => HooksState::NotInstalled,
        (true, true) => HooksState::Installed,
        (true, false) => HooksState::Outdated,
    }
}

fn hook_map_install(
    target: &HookTarget,
    path: &Path,
    agent: HookAgent,
    events: &[(&str, &str)],
) -> anyhow::Result<()> {
    let mut root: serde_json::Value = match target.read(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            anyhow::anyhow!(
                "{} is not valid JSON ({e}); not touching it",
                path.display()
            )
        })?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
    };
    if !root.is_object() {
        return Err(anyhow::anyhow!(
            "{} is not a JSON object; not touching it",
            path.display()
        ));
    }

    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        return Err(anyhow::anyhow!(
            "\"hooks\" in {} is not an object; not touching it",
            path.display()
        ));
    }

    let marker = agent.marker();
    for (hook_event, tty7_event) in events {
        let command = target.hook_command(agent, tty7_event);
        let entries = hooks
            .as_object_mut()
            .unwrap()
            .entry(*hook_event)
            .or_insert_with(|| serde_json::json!([]));
        let Some(list) = entries.as_array_mut() else {
            continue;
        };
        list.retain(|matcher| marker_command(matcher, &marker).is_none());
        list.push(serde_json::json!({
            "hooks": [{ "type": "command", "command": command }]
        }));
    }

    target.write(path, serde_json::to_string_pretty(&root)?.as_bytes())
}

fn hook_map_uninstall(
    target: &HookTarget,
    path: &Path,
    agent: HookAgent,
) -> anyhow::Result<String> {
    let text = match target.read(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok("Nothing installed; nothing to remove".to_string());
        }
        Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
    };
    let mut root: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "{} is not valid JSON ({e}); not touching it",
            path.display()
        )
    })?;

    let marker = agent.marker();
    let mut removed = 0;
    if let Some(hooks) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for entries in hooks.values_mut() {
            if let Some(list) = entries.as_array_mut() {
                let before = list.len();
                list.retain(|matcher| marker_command(matcher, &marker).is_none());
                removed += before - list.len();
            }
        }
        hooks.retain(|_, entries| entries.as_array().is_none_or(|list| !list.is_empty()));
    }
    if removed == 0 {
        return Ok("No tty7 hooks found; nothing to remove".to_string());
    }
    target.write(path, serde_json::to_string_pretty(&root)?.as_bytes())?;
    Ok("Removed".to_string())
}

fn marker_command<'a>(matcher: &'a serde_json::Value, marker: &str) -> Option<&'a str> {
    matcher
        .get("hooks")
        .and_then(|h| h.as_array())?
        .iter()
        .find_map(|h| {
            h.get("command")
                .and_then(|c| c.as_str())
                .filter(|c| c.contains(marker))
        })
}

/// Resolves `path` to its Windows 8.3 short form when one exists and is free
/// of spaces, so generated hook commands survive PowerShell's `-Command`
/// quoting. Returns `None` when short names are unavailable or the short path
/// still contains spaces; callers then fall back to the long path.
#[cfg(windows)]
fn short_path(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let len = GetShortPathNameW(wide.as_ptr(), std::ptr::null_mut(), 0);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        let written = GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), len);
        if written == 0 {
            return None;
        }
        let short = String::from_utf16(&buf[..written as usize]).ok()?;
        let short = short.trim_end_matches('\0');
        (!short.contains(' ')).then(|| short.to_string())
    }
}

fn enable_codex_hooks_feature() -> Result<(), String> {
    let candidates = [
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
    ]
    .into_iter()
    .chain(home_dir().map(|h| h.join(".local/bin/codex")))
    .find(|p| p.exists());
    let program = candidates.unwrap_or_else(|| PathBuf::from("codex"));
    let mut cmd = std::process::Command::new(&program);
    cmd.args(["features", "enable", "hooks"]);
    match crate::core::proc::hide_console(&mut cmd).output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(format!(
            "codex exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Err(format!("{}: {e}", program.display())),
    }
}

fn owned_file_content(target: &HookTarget, agent: HookAgent) -> Option<String> {
    match agent {
        HookAgent::Copilot => copilot_hooks_json(target),
        HookAgent::OpenCode => opencode_plugin_js(target),
        HookAgent::Pi => pi_extension_ts(target),
        HookAgent::Grok => grok_hooks_json(target),
        HookAgent::Claude | HookAgent::Codex => None,
    }
}

fn owned_file_state(target: &HookTarget, path: &Path, expected: &str, marker: &str) -> HooksState {
    let Ok(contents) = target.read(path) else {
        return HooksState::NotInstalled;
    };
    if contents == expected {
        HooksState::Installed
    } else if contents.contains(marker) {
        HooksState::Outdated
    } else {
        HooksState::NotInstalled
    }
}

fn owned_file_install(
    target: &HookTarget,
    path: &Path,
    content: &str,
    marker: &str,
) -> anyhow::Result<()> {
    if let Ok(existing) = target.read(path)
        && !existing.contains(marker)
    {
        return Err(anyhow::anyhow!(
            "{} exists but wasn't written by tty7; not touching it",
            path.display()
        ));
    }
    target.write(path, content.as_bytes())
}

fn owned_file_uninstall(target: &HookTarget, path: &Path, marker: &str) -> anyhow::Result<String> {
    let contents = match target.read(path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok("Nothing installed; nothing to remove".to_string());
        }
        Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
    };
    if !contents.contains(marker) {
        return Err(anyhow::anyhow!(
            "{} wasn't written by tty7; not touching it",
            path.display()
        ));
    }
    target.host.remove(path, false)?;
    if let Some(parent) = path.parent()
        && parent.file_name().is_some_and(|n| n == "tty7")
    {
        let _ = target.host.remove(parent, false);
    }
    Ok("Removed".to_string())
}

fn copilot_hooks_json(target: &HookTarget) -> Option<String> {
    let hook = |event: &str, timeout: u32| {
        serde_json::json!([{
            "type": "command",
            "bash": target.hook_command(HookAgent::Copilot, event),
            "timeoutSec": timeout,
        }])
    };
    let root = serde_json::json!({
        "version": 1,
        "hooks": {
            "sessionStart": hook("session-start", 5),
            "userPromptSubmitted": hook("prompt-submit", 5),
            "agentStop": hook("stop", 10),
            "sessionEnd": hook("session-end", 5),
            "notification": hook("notification", 5),
        }
    });
    serde_json::to_string_pretty(&root).ok()
}

fn grok_hooks_json(target: &HookTarget) -> Option<String> {
    let mut hooks = serde_json::Map::new();
    for (event, sentinel, matcher) in GROK_HOOK_EVENTS {
        let mut group = serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": target.hook_command(HookAgent::Grok, sentinel),
                "timeout": GROK_HOOK_TIMEOUT_SECS,
            }]
        });
        if let Some(matcher) = matcher {
            group["matcher"] = serde_json::Value::String((*matcher).to_string());
        }
        hooks.insert((*event).to_string(), serde_json::json!([group]));
    }
    serde_json::to_string_pretty(&serde_json::json!({ "hooks": hooks })).ok()
}

fn opencode_plugin_js(target: &HookTarget) -> Option<String> {
    let prefix = serde_json::to_string(&format!(
        "{} ",
        target.hook_command(HookAgent::OpenCode, "").trim_end()
    ))
    .ok()?;
    Some(format!(
        r#"// tty7 agent-hook opencode bridge — generated by tty7, do not edit.
// Bridges OpenCode plugin events onto `tty7 agent-hook opencode <event>`,
// which is inert outside tty7 (gated on the TTY7 env var).
export const Tty7Presence = async ({{ $ }}) => {{
  if (!process.env["TTY7"]) return {{}}
  const cmd = {prefix}
  const emit = (event) => $`sh -c ${{cmd + event}}`.quiet().nothrow()

  // Plugin load = the agent is running in this pane.
  await emit("session-start")

  return {{
    dispose: async () => {{
      await emit("session-end")
    }},
    "tool.execute.before": async () => {{
      await emit("prompt-submit")
    }},
    "permission.ask": async () => {{
      await emit("permission-request")
    }},
    event: async ({{ event }}) => {{
      if (event.type === "session.idle") {{
        await emit("stop")
      }} else if (event.type === "permission.replied") {{
        await emit("prompt-submit")
      }}
    }},
  }}
}}
"#
    ))
}

fn pi_extension_ts(target: &HookTarget) -> Option<String> {
    let exe = serde_json::to_string(&target.exe.display().to_string()).ok()?;
    Some(format!(
        r#"/* tty7 agent-hook pi bridge — generated by tty7, do not edit. */
import type {{ ExtensionAPI }} from "@mariozechner/pi-coding-agent";
import {{ spawnSync }} from "node:child_process";

const EXE = {exe};

/** The slice of Pi's handler context we read — structural, so this bridge does
 *  not depend on the context type staying exported. */
type SessionCtx = {{ sessionManager?: {{ getSessionId?(): string | undefined }} }};

function emit(event: string, ctx?: SessionCtx): void {{
  try {{
    let payload = "";
    try {{
      const id = ctx?.sessionManager?.getSessionId?.();
      if (id) payload = JSON.stringify({{ session_id: id }});
    }} catch {{}}
    const args = ["agent-hook", "pi", event];
    // Nothing to send → leave stdin closed rather than handing the emitter a
    // pipe it has to read to EOF.
    if (payload) {{
      spawnSync(EXE, args, {{ input: payload, stdio: ["pipe", "ignore", "ignore"] }});
    }} else {{
      spawnSync(EXE, args, {{ stdio: ["ignore", "ignore", "ignore"] }});
    }}
  }} catch {{}}
}}

export default function (pi: ExtensionAPI) {{
  if (!process.env["TTY7"]) return;
  // Extension load = the agent is running in this pane. No context here yet,
  // so the id rides on session_start instead.
  emit("session-start");
  pi.on("agent_start", (_event, ctx) => emit("prompt-submit", ctx));
  pi.on("agent_end", (_event, ctx) => emit("stop", ctx));
  pi.on("session_shutdown", (_event, ctx) => emit("session-end", ctx));
  // Last, and guarded: the three above already worked, so a Pi build that
  // rejects this event name must not take them — or the whole extension —
  // down with it.
  try {{
    pi.on("session_start", (_event, ctx) => emit("session-start", ctx));
  }} catch {{}}
}}
"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tty7_daemon_host_takes_the_console_fast_path() {
        for name in ["tty7-app.exe", "tty7-server.exe", "tty7.exe"] {
            assert!(is_tty7_host_exe(name), "{name} hosts tty7 shells");
        }
        for name in ["explorer.exe", "cmd.exe", "tty7", "tty7-app", "wt.exe"] {
            assert!(!is_tty7_host_exe(name), "{name} is not a tty7 host process");
        }
    }

    #[test]
    fn hook_sequence_round_trips_through_the_daemon_parser() {
        use crate::core::cli_agent::{AgentEventKind, CLIAgent, parse_agent_event};

        let seq = build_hook_sequence(
            "claude",
            "notification",
            r#"{"session_id":"abc-123","message":"Claude needs your permission","cwd":"/w"}"#,
        );
        let payload = &seq[2..seq.len() - 1];
        let ev = parse_agent_event(payload).expect("daemon parses the emitted event");
        assert_eq!(ev.agent, Some(CLIAgent::Claude));
        assert_eq!(ev.kind, AgentEventKind::Notification);
        assert_eq!(ev.session_id.as_deref(), Some("abc-123"));
        assert!(ev.message.as_deref().unwrap().contains("permission"));
        assert_eq!(ev.cwd.as_deref(), Some(std::path::Path::new("/w")));

        let seq = build_hook_sequence("claude", "stop", "not json at all");
        let ev = parse_agent_event(&seq[2..seq.len() - 1]).expect("bare event still parses");
        assert_eq!(ev.kind, AgentEventKind::Stop);
        assert_eq!(ev.session_id, None);

        let seq = build_hook_sequence(
            "grok",
            "session-start",
            r#"{"hookEventName":"session_start","sessionId":"g-42","cwd":"/w"}"#,
        );
        let ev = parse_agent_event(&seq[2..seq.len() - 1]).expect("daemon parses the grok event");
        assert_eq!(ev.agent, Some(CLIAgent::Grok));
        assert_eq!(ev.session_id.as_deref(), Some("g-42"));
        assert_eq!(ev.cwd.as_deref(), Some(std::path::Path::new("/w")));
    }

    #[test]
    fn grok_run_hooks_are_relabeled_to_grok() {
        assert_eq!(effective_agent("claude", true), "grok");
        assert_eq!(effective_agent("grok", true), "grok");
        assert_eq!(effective_agent("claude", false), "claude");
        assert_eq!(effective_agent("grok", false), "grok");
    }

    #[test]
    fn every_installed_event_parses_as_a_sentinel_kind() {
        use crate::core::cli_agent::parse_agent_event;

        let mut events: Vec<&str> = CLAUDE_HOOK_EVENTS
            .iter()
            .chain(CODEX_HOOK_EVENTS)
            .map(|(_, e)| *e)
            .chain(GROK_HOOK_EVENTS.iter().map(|(_, e, _)| *e))
            .collect();
        events.extend([
            "prompt-submit",
            "permission-request",
            "stop",
            "session-end",
            "session-start",
        ]);
        for event in events {
            let seq = build_hook_sequence("codex", event, "{}");
            let ev = parse_agent_event(&seq[2..seq.len() - 1])
                .unwrap_or_else(|| panic!("event {event:?} must parse"));
            let kind_json = serde_json::to_value(ev.kind).unwrap();
            assert_eq!(kind_json, serde_json::Value::String(event.to_string()));
        }
    }

    #[test]
    fn copilot_notifications_filter_to_permission_requests() {
        assert_eq!(
            effective_event("copilot", "notification", r#"{"type":"permission_prompt"}"#),
            Some("permission-request")
        );
        assert_eq!(
            effective_event(
                "copilot",
                "notification",
                r#"{"type":"elicitation_dialog"}"#
            ),
            Some("permission-request")
        );
        assert_eq!(
            effective_event("copilot", "notification", r#"{"type":"turn_summary"}"#),
            None
        );
        assert_eq!(
            effective_event(
                "grok",
                "notification",
                r#"{"notificationType":"elicitation_dialog","message":"User question requested"}"#
            ),
            Some("permission-request")
        );
        for noisy in ["permission_prompt", "task_complete", "agent_error"] {
            assert_eq!(
                effective_event(
                    "grok",
                    "notification",
                    &format!(r#"{{"notificationType":"{noisy}"}}"#)
                ),
                None,
                "grok {noisy} is not a block"
            );
        }
        assert_eq!(
            effective_event("claude", "notification", "{}"),
            Some("notification")
        );
        assert_eq!(effective_event("copilot", "stop", "{}"), Some("stop"));
        assert_eq!(effective_event("grok", "stop", "{}"), Some("stop"));
    }

    #[cfg(unix)]
    #[test]
    fn ancestor_tty_device_is_none_or_a_dev_path() {
        match ancestor_tty_device() {
            None => {}
            Some(dev) => assert!(
                dev.starts_with("/dev/"),
                "a resolved tty must be an openable device path, got {dev:?}"
            ),
        }
    }

    #[test]
    fn marker_detection_matches_our_entries_only() {
        let ours = serde_json::json!({
            "hooks": [{ "type": "command", "command": "\"/x/tty7\" agent-hook claude stop" }]
        });
        assert!(marker_command(&ours, "agent-hook claude").is_some());
        assert!(marker_command(&ours, "agent-hook codex").is_none());
        let theirs = serde_json::json!({
            "hooks": [{ "type": "command", "command": "afplay /System/Library/Sounds/Glass.aiff" }]
        });
        assert!(marker_command(&theirs, "agent-hook claude").is_none());
        assert!(marker_command(&serde_json::json!({}), "agent-hook claude").is_none());
    }

    fn local_host() -> crate::host::SharedHost {
        crate::host::local::LocalHost::new()
    }

    struct FakeRemote(crate::host::SharedHost);

    impl FakeRemote {
        fn shared() -> crate::host::SharedHost {
            std::sync::Arc::new(FakeRemote(local_host()))
        }
    }

    impl Host for FakeRemote {
        fn id(&self) -> crate::host::HostId {
            crate::host::HostId::from_connection_key("ssh-direct:me@box:22")
        }
        fn separator(&self) -> char {
            '/'
        }
        fn is_absolute(&self, p: &Path) -> bool {
            p.to_string_lossy().starts_with('/')
        }
        fn read_dir(&self, dir: &Path, root: Option<&Path>) -> io::Result<Vec<crate::host::Entry>> {
            self.0.read_dir(dir, root)
        }
        fn stat(&self, p: &Path) -> io::Result<crate::host::Meta> {
            self.0.stat(p)
        }
        fn read_file(&self, p: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
            self.0.read_file(p, max_bytes)
        }
        fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
            self.0.canonicalize(p)
        }
        fn search(
            &self,
            roots: &[PathBuf],
            query: &str,
            limit: usize,
            max_dirs: usize,
            show_hidden: bool,
        ) -> io::Result<Vec<crate::host::SearchHit>> {
            self.0.search(roots, query, limit, max_dirs, show_hidden)
        }
        fn write_file(&self, p: &Path, bytes: &[u8]) -> io::Result<crate::host::Meta> {
            self.0.write_file(p, bytes)
        }
        fn create_file_new(&self, p: &Path) -> io::Result<()> {
            self.0.create_file_new(p)
        }
        fn create_dir(&self, p: &Path, recursive: bool) -> io::Result<()> {
            self.0.create_dir(p, recursive)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.0.rename(from, to)
        }
        fn remove(&self, p: &Path, recursive: bool) -> io::Result<()> {
            self.0.remove(p, recursive)
        }
        fn repo_root(&self, p: &Path) -> io::Result<Option<PathBuf>> {
            self.0.repo_root(p)
        }
        fn git(&self, cwd: &Path, args: &[&str]) -> io::Result<crate::host::Output> {
            self.0.git(cwd, args)
        }
        fn shells(&self) -> io::Result<crate::host::ShellInventory> {
            self.0.shells()
        }
        fn watch(&self, dirs: &[PathBuf]) -> io::Result<crate::host::WatchSub> {
            self.0.watch(dirs)
        }
    }

    #[test]
    fn hook_command_quotes_the_exe_path() {
        let host = local_host();
        let target = HookTarget::local(&*host).expect("home resolves in tests");
        let cmd = target.hook_command(HookAgent::Claude, "stop");
        // On Windows the executable may be rewritten to a space-free 8.3 short
        // path, which must be emitted bare: PowerShell cannot invoke a quoted
        // path without the `&` call operator.
        #[cfg(not(windows))]
        assert!(cmd.starts_with('"'));
        assert!(cmd.ends_with("agent-hook claude stop"));
    }

    #[test]
    fn remote_paths_are_built_in_the_remote_machine_s_spelling() {
        let host = FakeRemote::shared();
        let target = HookTarget::remote(&*host, PathBuf::from("/home/me"));

        for (agent, expected) in [
            (HookAgent::Claude, "/home/me/.claude/settings.json"),
            (HookAgent::Codex, "/home/me/.codex/hooks.json"),
            (HookAgent::Copilot, "/home/me/.copilot/hooks/tty7.json"),
            (
                HookAgent::OpenCode,
                "/home/me/.config/opencode/plugins/tty7.js",
            ),
            (HookAgent::Pi, "/home/me/.pi/agent/extensions/tty7/index.ts"),
            (HookAgent::Grok, "/home/me/.grok/hooks/tty7.json"),
        ] {
            assert_eq!(
                agent.target_path(&target),
                PathBuf::from(expected),
                "{agent:?} target path"
            );
            assert_eq!(
                agent.target_display(&target),
                expected.replacen("/home/me/", "~/", 1),
                "{agent:?} display path"
            );
        }
    }

    #[test]
    fn the_hook_command_names_the_binary_on_that_machine() {
        let host = FakeRemote::shared();
        let target = HookTarget::remote(&*host, PathBuf::from("/home/me"));
        let dialect = crate::daemon::install::RemoteProtocol::of_this_build();
        let name = format!("tty7-server-c{}p{}", dialect.control, dialect.protocol);
        assert_eq!(
            target.hook_command(HookAgent::Claude, "stop"),
            format!("\"/home/me/.local/share/tty7/bin/{name}\" agent-hook claude stop")
        );

        let local = local_host();
        let here = HookTarget::local(&*local).expect("home resolves in tests");
        let exe = std::env::current_exe().unwrap();
        assert_eq!(
            here.hook_command(HookAgent::Claude, "stop"),
            format!("\"{}\" agent-hook claude stop", exe.display())
        );
    }

    #[test]
    fn a_remote_install_round_trips_through_the_host() {
        let dir = std::env::temp_dir().join(format!("tty7-remote-hooks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let host = FakeRemote::shared();
        let target = HookTarget::remote(&*host, dir.clone());

        for agent in [HookAgent::Claude, HookAgent::Grok] {
            assert_eq!(hooks_state(&target, agent), HooksState::NotInstalled);
            install_hooks(&target, agent).expect("install succeeds");
            assert_eq!(hooks_state(&target, agent), HooksState::Installed);
            let path = agent.target_path(&target);
            let dialect = crate::daemon::install::RemoteProtocol::of_this_build();
            assert!(std::fs::read_to_string(&path).unwrap().contains(&format!(
                "tty7-server-c{}p{}",
                dialect.control, dialect.protocol
            )));
            uninstall_hooks(&target, agent).expect("uninstall succeeds");
            assert_eq!(hooks_state(&target, agent), HooksState::NotInstalled);
        }

        let summary = install_hooks(&target, HookAgent::Codex).expect("codex install succeeds");
        assert!(
            summary.contains("codex features enable hooks"),
            "remote codex install has to hand the flag back to the user: {summary}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn owned_file_contents_carry_marker_and_exe() {
        let exe_raw = std::env::current_exe().unwrap().display().to_string();
        let exe_json = serde_json::to_string(&exe_raw).unwrap();
        let exe = exe_json.trim_matches('"').to_string();
        let host = local_host();
        let target = HookTarget::local(&*host).expect("home resolves in tests");

        let copilot = copilot_hooks_json(&target).expect("copilot content builds");
        let parsed: serde_json::Value = serde_json::from_str(&copilot).expect("valid JSON");
        for event in [
            "sessionStart",
            "userPromptSubmitted",
            "agentStop",
            "sessionEnd",
            "notification",
        ] {
            assert!(
                parsed["hooks"][event][0]["bash"]
                    .as_str()
                    .is_some_and(|c| c.contains("agent-hook copilot")),
                "copilot {event} carries the emitter"
            );
        }
        assert!(copilot.contains(&exe));

        let opencode = opencode_plugin_js(&target).expect("opencode content builds");
        assert!(opencode.contains("agent-hook opencode"));
        assert!(opencode.contains(&exe));
        assert!(opencode.contains(r#"process.env["TTY7"]"#));

        let pi = pi_extension_ts(&target).expect("pi content builds");
        assert!(pi.contains("agent-hook pi"));
        assert!(pi.contains(&exe));
        assert!(pi.contains(r#"process.env["TTY7"]"#));
        assert!(pi.contains("getSessionId"));
        assert!(pi.contains("session_id"));
        assert!(pi.contains(r#"stdio: ["pipe", "ignore", "ignore"]"#));
        assert!(pi.contains(r#"pi.on("session_start""#));

        let grok = grok_hooks_json(&target).expect("grok content builds");
        let parsed: serde_json::Value = serde_json::from_str(&grok).expect("valid JSON");
        for (event, sentinel, matcher) in GROK_HOOK_EVENTS {
            let group = &parsed["hooks"][*event][0];
            let cmd = group["hooks"][0]["command"]
                .as_str()
                .unwrap_or_else(|| panic!("grok {event} carries a command"));
            assert!(
                cmd.ends_with(&format!("agent-hook grok {sentinel}")),
                "grok {event} runs the emitter with {sentinel}, got {cmd}"
            );
            assert_eq!(
                group.get("matcher").and_then(|m| m.as_str()),
                *matcher,
                "grok {event} matcher"
            );
        }
        assert!(grok.contains(&exe));
    }

    #[test]
    fn owned_file_round_trip_and_ownership_guard() {
        let dir = std::env::temp_dir().join(format!("tty7-owned-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tty7.json");
        let marker = "agent-hook copilot";
        let host = local_host();
        let t = HookTarget::local(&*host).expect("home resolves in tests");
        let content = copilot_hooks_json(&t).unwrap();

        assert_eq!(
            owned_file_state(&t, &path, &content, marker),
            HooksState::NotInstalled
        );
        owned_file_install(&t, &path, &content, marker).expect("fresh install succeeds");
        assert_eq!(
            owned_file_state(&t, &path, &content, marker),
            HooksState::Installed
        );

        std::fs::write(&path, content.replace(marker, "agent-hook copilot --old")).unwrap();
        assert_eq!(
            owned_file_state(&t, &path, &content, marker),
            HooksState::Outdated
        );
        owned_file_install(&t, &path, &content, marker).expect("reinstall over our own file");
        assert_eq!(
            owned_file_state(&t, &path, &content, marker),
            HooksState::Installed
        );

        std::fs::write(&path, "// my own hooks, hands off").unwrap();
        assert!(owned_file_install(&t, &path, &content, marker).is_err());
        assert!(owned_file_uninstall(&t, &path, marker).is_err());

        std::fs::write(&path, &content).unwrap();
        owned_file_uninstall(&t, &path, marker).expect("uninstall succeeds");
        assert!(!path.exists());
        owned_file_uninstall(&t, &path, marker).expect("uninstall is idempotent");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_is_idempotent_and_preserves_user_hooks() {
        let dir = std::env::temp_dir().join(format!("tty7-hooks-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        std::fs::write(
            &settings,
            serde_json::json!({
                "model": "opus",
                "hooks": {
                    "Stop": [{ "hooks": [{ "type": "command", "command": "afplay ding.aiff" }] }]
                }
            })
            .to_string(),
        )
        .unwrap();
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &dir) };

        let host = local_host();
        let t = HookTarget::local(&*host).expect("home resolves in tests");
        let remote_host = FakeRemote::shared();
        let remote = HookTarget::remote(&*remote_host, PathBuf::from("/home/me"));
        assert_eq!(
            HookAgent::Claude.target_path(&remote),
            PathBuf::from("/home/me/.claude/settings.json")
        );

        assert_eq!(hooks_state(&t, HookAgent::Claude), HooksState::NotInstalled);
        install_hooks(&t, HookAgent::Claude).expect("install succeeds");
        assert_eq!(hooks_state(&t, HookAgent::Claude), HooksState::Installed);

        install_hooks(&t, HookAgent::Claude).expect("re-install succeeds");
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(root["model"], "opus");
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(
            stop.iter()
                .filter(|m| marker_command(m, "agent-hook claude").is_some())
                .count(),
            1,
            "exactly one tty7 entry after two installs"
        );
        assert!(
            stop.iter()
                .any(|m| m.to_string().contains("afplay ding.aiff")),
            "the user's own Stop hook survives"
        );
        for (event, _) in CLAUDE_HOOK_EVENTS {
            assert!(
                root["hooks"][*event]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| marker_command(m, "agent-hook claude").is_some()),
                "{event} carries the tty7 hook"
            );
        }

        let healthy = std::fs::read_to_string(&settings).unwrap();
        std::fs::write(
            &settings,
            healthy.replace("agent-hook claude stop", "agent-hook claude stop --stale"),
        )
        .unwrap();
        assert_eq!(hooks_state(&t, HookAgent::Claude), HooksState::Outdated);
        install_hooks(&t, HookAgent::Claude).expect("reinstall over an outdated entry succeeds");
        assert_eq!(hooks_state(&t, HookAgent::Claude), HooksState::Installed);

        uninstall_hooks(&t, HookAgent::Claude).expect("uninstall succeeds");
        assert_eq!(hooks_state(&t, HookAgent::Claude), HooksState::NotInstalled);
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(root["model"], "opus");
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert!(
            stop.iter()
                .any(|m| m.to_string().contains("afplay ding.aiff")),
            "the user's own Stop hook survives uninstall"
        );
        assert!(
            root["hooks"].get("SessionStart").is_none(),
            "an event list that held only the tty7 hook is dropped"
        );
        uninstall_hooks(&t, HookAgent::Claude).expect("uninstall is idempotent");

        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}

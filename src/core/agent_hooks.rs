//! Agent-side hook integration: the emitter behind `tty7 agent-hook …` and the
//! per-agent installers that wire it into each CLI agent's own hook surface.
//!
//! The rich agent-status channel ([`crate::core::cli_agent`]) needs the agent
//! itself to say what it's doing. Each supported agent exposes that
//! differently — Claude Code and Codex take a declarative hooks map, Copilot
//! auto-loads JSON hook files from a directory, OpenCode loads JS plugins, Pi
//! loads TS extensions — but every integration bottoms out in the same tiny
//! emitter: `tty7 agent-hook <agent> <event>` reads the hook's JSON payload
//! from stdin and writes one sentinel OSC 777 sequence to the controlling
//! terminal, where tty7's daemon-side sniffer picks it up and folds it into
//! the pane's session state.
//!
//! Emission is gated on the `TTY7` environment variable (injected into every
//! shell tty7 spawns), so hooks installed globally stay silent when an agent
//! runs in another terminal.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use crate::core::cli_agent::AGENT_EVENT_SENTINEL;

/// The env var tty7 sets in every spawned shell; the hook emitter refuses to
/// write escape sequences into terminals that aren't tty7.
pub const TTY7_ENV_MARKER: &str = "TTY7";

/// Cap on how much hook stdin we'll read: real payloads are a few hundred
/// bytes of JSON; anything huge is not for us.
const MAX_STDIN: u64 = 64 * 1024;

/// Entry point for the `tty7 agent-hook <agent> <event>` subcommand: read the
/// hook's JSON payload from stdin, build the sentinel event, and write it to
/// the controlling terminal. Always exits quietly — a hook that fails must
/// never break the agent's own flow (agents surface nonzero exits).
pub fn run_agent_hook(agent: &str, event: &str) {
    // Not inside tty7 (or a remote shell): stay silent, so globally-installed
    // hooks don't leak escape sequences into other terminals.
    if std::env::var_os(TTY7_ENV_MARKER).is_none() {
        return;
    }
    // Hook payload: the agent writes JSON ({"session_id": …, "message": …, …})
    // and closes stdin. Absent/malformed input still emits the bare event —
    // the state machine works without ids or messages.
    let mut input = String::new();
    let _ = std::io::stdin().take(MAX_STDIN).read_to_string(&mut input);
    let Some(event) = effective_event(agent, event, &input) else {
        return;
    };
    write_to_controlling_tty(&build_hook_sequence(agent, event, &input));
}

/// The sentinel event one hook invocation maps onto, or `None` to stay silent.
/// Most hooks pass their event through; the exception is Copilot's single
/// `notification` hook, which fires for *every* notification type — only
/// permission/elicitation prompts are the amber "needs you" moment, so those
/// are escalated to `permission-request` and everything else is dropped
/// rather than parroted as a block.
fn effective_event<'a>(agent: &str, event: &'a str, stdin_json: &str) -> Option<&'a str> {
    if agent == "copilot" && event == "notification" {
        if stdin_json.contains("permission_prompt") || stdin_json.contains("elicitation_dialog") {
            return Some("permission-request");
        }
        return None;
    }
    Some(event)
}

/// Build the sentinel OSC sequence for one hook invocation — the pure core of
/// [`run_agent_hook`], separated so the wire bytes are testable without a PTY.
/// Round-trips through [`crate::core::cli_agent::parse_agent_event`] on the
/// daemon side.
fn build_hook_sequence(agent: &str, event: &str, stdin_json: &str) -> Vec<u8> {
    let payload: serde_json::Value =
        serde_json::from_str(stdin_json).unwrap_or(serde_json::json!({}));
    let mut body = serde_json::json!({
        "v": 1,
        "agent": agent,
        "event": event,
    });
    for key in ["session_id", "message"] {
        if let Some(v) = payload
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
        {
            body[key] = serde_json::Value::String(v.to_string());
        }
    }
    format!("\x1b]777;notify;{AGENT_EVENT_SENTINEL};{body}\x07").into_bytes()
}

/// Write raw bytes to the pane's PTY so the daemon's sniffer reads them as pane
/// output. Two routes, because agents run hooks differently:
///
/// 1. `/dev/tty` — the hook's own controlling terminal. Works when the agent
///    runs the hook attached to its tty.
/// 2. An ancestor's tty device — Claude Code runs hooks *detached* from the
///    controlling terminal (they have no `/dev/tty`), but the agent process
///    itself still owns the pane's PTY slave. So walk up the parent chain to
///    the nearest process that has a real tty (that's the agent) and write its
///    device (`/dev/ttysNNN` on macOS, `/dev/pts/N` on Linux) directly. Writing
///    the slave sends output to the master, exactly like `/dev/tty` would.
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

/// The controlling-tty device of the nearest ancestor that has one — the agent
/// process, when it ran us detached. Walks the parent chain via `ps` (the hook
/// runs at most a few times per turn, so the process spawn is negligible and
/// beats platform-specific sysctl/`/proc` FFI here).
#[cfg(unix)]
fn ancestor_tty_device() -> Option<std::path::PathBuf> {
    use std::process::Command;
    // SAFETY: getppid is always safe and never fails.
    let mut pid = unsafe { libc::getppid() };
    for _ in 0..8 {
        if pid <= 1 {
            break;
        }
        // `tty=` prints the terminal (`ttys004`, `pts/3`, or `??`/empty for none)
        // and `ppid=` the parent, both header-less so parsing is trivial.
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
fn write_to_controlling_tty(_bytes: &[u8]) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Integrations: one installer per agent that exposes a hook surface.
// ---------------------------------------------------------------------------

/// The agents tty7 can wire its rich-status channel into. Each carries a
/// different install mechanism (see [`install_hooks`]); the emitter side is
/// identical for all of them. Gemini/Aider/… are recognized in the sidebar
/// but have no hook surface to install into, so they are not listed here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HookAgent {
    /// Hooks map merged into `~/.claude/settings.json` (or
    /// `$CLAUDE_CONFIG_DIR/settings.json`).
    Claude,
    /// Hooks map merged into `~/.codex/hooks.json`, plus the one-time
    /// `codex features enable hooks` feature flag.
    Codex,
    /// A tty7-owned hook file in `~/.copilot/hooks/` (Copilot auto-loads
    /// every JSON file there).
    Copilot,
    /// A tty7-owned JS plugin in `~/.config/opencode/plugins/`.
    OpenCode,
    /// A tty7-owned TS extension in `~/.pi/agent/extensions/tty7/`.
    Pi,
}

impl HookAgent {
    pub const ALL: [HookAgent; 5] = [
        HookAgent::Claude,
        HookAgent::Codex,
        HookAgent::Copilot,
        HookAgent::OpenCode,
        HookAgent::Pi,
    ];

    /// The `agent` slug in hook commands and sentinel events — matches
    /// [`crate::core::cli_agent::CLIAgent::slug`] so events brand the pane.
    pub fn slug(self) -> &'static str {
        match self {
            HookAgent::Claude => "claude",
            HookAgent::Codex => "codex",
            HookAgent::Copilot => "copilot",
            HookAgent::OpenCode => "opencode",
            HookAgent::Pi => "pi",
        }
    }

    /// User-facing name for the settings row.
    pub fn display_name(self) -> &'static str {
        match self {
            HookAgent::Claude => "Claude Code",
            HookAgent::Codex => "Codex",
            HookAgent::Copilot => "Copilot CLI",
            HookAgent::OpenCode => "OpenCode",
            HookAgent::Pi => "Pi",
        }
    }

    /// The file the integration installs into, `~`-abbreviated for display.
    pub fn target_display(self) -> String {
        match self.target_path() {
            Some(p) => abbreviate_home(&p),
            None => "~ (home directory unresolved)".to_string(),
        }
    }

    /// The file this agent's integration lives in.
    fn target_path(self) -> Option<PathBuf> {
        match self {
            HookAgent::Claude => claude_settings_path(),
            HookAgent::Codex => Some(home_dir()?.join(".codex").join("hooks.json")),
            HookAgent::Copilot => Some(
                home_dir()?
                    .join(".copilot")
                    .join("hooks")
                    .join(OWNED_FILE_STEM_JSON),
            ),
            HookAgent::OpenCode => Some(
                xdg_config_dir()?
                    .join("opencode")
                    .join("plugins")
                    .join(OWNED_FILE_STEM_JS),
            ),
            HookAgent::Pi => Some(
                home_dir()?
                    .join(".pi")
                    .join("agent")
                    .join("extensions")
                    .join("tty7")
                    .join("index.ts"),
            ),
        }
    }

    /// Substring that identifies a hook entry / owned file as tty7's, for
    /// idempotent install/upgrade and ownership-guarded uninstall. Every
    /// generated command and file embeds `agent-hook <slug>` verbatim.
    fn marker(self) -> String {
        format!("agent-hook {}", self.slug())
    }
}

/// Install state of one agent's tty7 hooks, as shown in Settings → Agents.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HooksState {
    /// No tty7 hook entry / file anywhere.
    NotInstalled,
    /// The integration is present and points at this binary.
    Installed,
    /// tty7 wrote it, but it points at a different tty7 binary (the app
    /// moved/updated, or it was installed from another build) or is missing a
    /// piece — the hooks fire into the wrong or a vanished executable. A
    /// reinstall rewrites it in place.
    Outdated,
}

/// Read one agent's install state from disk. An unreadable or malformed
/// target reports `NotInstalled` — the same "nothing usable there" answer the
/// installer would start from.
pub fn hooks_state(agent: HookAgent) -> HooksState {
    let Some(path) = agent.target_path() else {
        return HooksState::NotInstalled;
    };
    match agent {
        HookAgent::Claude => hook_map_state(&path, agent, CLAUDE_HOOK_EVENTS),
        HookAgent::Codex => hook_map_state(&path, agent, CODEX_HOOK_EVENTS),
        HookAgent::Copilot | HookAgent::OpenCode | HookAgent::Pi => {
            let Some(expected) = owned_file_content(agent) else {
                return HooksState::NotInstalled;
            };
            owned_file_state(&path, &expected, &agent.marker())
        }
    }
}

/// Install (or rewrite in place) one agent's tty7 hooks. Idempotent: existing
/// tty7 entries/files are replaced, never duplicated, and anything
/// user-authored is left untouched. Returns a short human-readable summary.
pub fn install_hooks(agent: HookAgent) -> anyhow::Result<String> {
    let path = agent
        .target_path()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve home directory"))?;
    match agent {
        HookAgent::Claude => {
            hook_map_install(&path, agent, CLAUDE_HOOK_EVENTS)?;
            Ok(format!(
                "Claude Code hooks installed in {} — restart running claude sessions to pick them up",
                path.display()
            ))
        }
        HookAgent::Codex => {
            hook_map_install(&path, agent, CODEX_HOOK_EVENTS)?;
            // Codex only reads hooks.json once the hooks feature flag is on.
            // Best-effort: the file install above is complete and correct
            // either way, so a missing codex binary downgrades to advice
            // instead of failing the install.
            let summary = format!("Codex hooks installed in {}", path.display());
            Ok(match enable_codex_hooks_feature() {
                Ok(()) => summary,
                Err(e) => format!(
                    "{summary} — couldn't run `codex features enable hooks` ({e}); run it once manually"
                ),
            })
        }
        HookAgent::Copilot | HookAgent::OpenCode | HookAgent::Pi => {
            let content = owned_file_content(agent)
                .ok_or_else(|| anyhow::anyhow!("cannot resolve tty7's own executable path"))?;
            owned_file_install(&path, &content, &agent.marker())?;
            Ok(format!(
                "{} integration installed at {}",
                agent.display_name(),
                path.display()
            ))
        }
    }
}

/// Remove one agent's tty7 hooks, leaving user-authored hooks and settings
/// untouched. Ownership-guarded: only entries/files carrying the tty7 marker
/// are ever removed.
pub fn uninstall_hooks(agent: HookAgent) -> anyhow::Result<String> {
    let path = agent
        .target_path()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve home directory"))?;
    match agent {
        HookAgent::Claude | HookAgent::Codex => hook_map_uninstall(&path, agent),
        HookAgent::Copilot | HookAgent::OpenCode | HookAgent::Pi => {
            owned_file_uninstall(&path, &agent.marker())
        }
    }
}

/// Startup keeper: rewrite any integration that is installed but stale (see
/// [`HooksState::Outdated`]) so hooks keep pointing at a real tty7 after the
/// app moves or updates. Release builds only — a debug build auto-claiming
/// the hooks would steal them from the installed app on every dev launch
/// (installing *from* a dev build stays possible, just explicit). Returns how
/// many integrations were refreshed.
pub fn refresh_hooks_at_launch() -> usize {
    if cfg!(debug_assertions) {
        return 0;
    }
    let mut refreshed = 0;
    for agent in HookAgent::ALL {
        if hooks_state(agent) != HooksState::Outdated {
            continue;
        }
        match install_hooks(agent) {
            Ok(summary) => {
                refreshed += 1;
                log::info!("refreshed stale agent hooks: {summary}");
            }
            Err(e) => log::warn!(
                "could not refresh stale {} hooks: {e}",
                agent.display_name()
            ),
        }
    }
    refreshed
}

// ---------------------------------------------------------------------------
// Shared: paths and the hook command line.
// ---------------------------------------------------------------------------

/// Claude Code's user settings file: `$CLAUDE_CONFIG_DIR/settings.json`,
/// defaulting to `~/.claude/settings.json`.
fn claude_settings_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir).join("settings.json"));
    }
    Some(home_dir()?.join(".claude").join("settings.json"))
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

/// `$XDG_CONFIG_HOME`, defaulting to `~/.config` (OpenCode's config root).
fn xdg_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    Some(home_dir()?.join(".config"))
}

/// Abbreviate the user's home-directory prefix to `~` for display.
fn abbreviate_home(path: &Path) -> String {
    if let Some(home) = home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// The hook command line written into an agent's config — this binary, by
/// absolute path, so it works regardless of PATH. Quoted because macOS app
/// paths ("/Applications/…") can carry spaces. `event` is one of tty7's
/// kebab-case sentinel events, passed straight through by the emitter.
fn hook_command(agent: HookAgent, event: &str) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!(
        "\"{}\" agent-hook {} {event}",
        exe.display(),
        agent.slug()
    ))
}

/// Owned-file names, shared by path resolution and tests.
const OWNED_FILE_STEM_JSON: &str = "tty7.json";
const OWNED_FILE_STEM_JS: &str = "tty7.js";

// ---------------------------------------------------------------------------
// Hooks-map installer (Claude Code, Codex): a JSON object with a top-level
// `"hooks"` key mapping event names to entry lists; tty7 owns exactly one
// marker-carrying entry per event and never touches the rest of the file.
// ---------------------------------------------------------------------------

/// Claude Code's hook events and the sentinel event each maps onto.
/// `Notification` covers both "needs permission" and "waiting for input" —
/// exactly the Waiting state.
const CLAUDE_HOOK_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "prompt-submit"),
    ("Notification", "notification"),
    ("Stop", "stop"),
    ("SessionEnd", "session-end"),
];

/// Codex's hook events (`~/.codex/hooks.json`, Claude-shaped). Codex is
/// turn-level only: no Notification hook, and no SessionEnd — the pane's
/// foreground detection clears the badge when Codex exits.
const CODEX_HOOK_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "prompt-submit"),
    ("Stop", "stop"),
];

fn hook_map_state(path: &Path, agent: HookAgent, events: &[(&str, &str)]) -> HooksState {
    let Ok(text) = std::fs::read_to_string(path) else {
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
                if Some(cmd) != hook_command(agent, tty7_event).as_deref() {
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

/// Merge tty7's hook entries into the file at `path`, preserving everything
/// else. Idempotent: entries carrying the agent's marker are rewritten in
/// place (e.g. after the binary moved); user-defined hooks on the same events
/// are left untouched.
fn hook_map_install(path: &Path, agent: HookAgent, events: &[(&str, &str)]) -> anyhow::Result<()> {
    let mut root: serde_json::Value = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            anyhow::anyhow!(
                "{} is not valid JSON ({e}); not touching it",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
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
        let command = hook_command(agent, tty7_event)
            .ok_or_else(|| anyhow::anyhow!("cannot resolve tty7's own executable path"))?;
        let entries = hooks
            .as_object_mut()
            .unwrap()
            .entry(*hook_event)
            .or_insert_with(|| serde_json::json!([]));
        let Some(list) = entries.as_array_mut() else {
            continue; // malformed user config on this event; leave it alone
        };
        // Drop any previous tty7 entry (stale exe path), then append ours.
        list.retain(|matcher| marker_command(matcher, &marker).is_none());
        list.push(serde_json::json!({
            "hooks": [{ "type": "command", "command": command }]
        }));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::core::config::write_atomic(path, serde_json::to_string_pretty(&root)?.as_bytes())?;
    Ok(())
}

/// Remove every tty7 hook entry from the file, leaving user-defined hooks and
/// all other settings untouched. Sweeps *all* hook events (not just the ones
/// we currently subscribe to) so entries left by an older tty7 with a
/// different event set are cleaned up too.
fn hook_map_uninstall(path: &Path, agent: HookAgent) -> anyhow::Result<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
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
        // Drop event lists we emptied; a user's own hooks keep their event key.
        hooks.retain(|_, entries| entries.as_array().is_none_or(|list| !list.is_empty()));
    }
    if removed == 0 {
        return Ok("No tty7 hooks found; nothing to remove".to_string());
    }
    crate::core::config::write_atomic(path, serde_json::to_string_pretty(&root)?.as_bytes())?;
    Ok(format!(
        "{} hooks removed from {}",
        agent.display_name(),
        path.display()
    ))
}

/// The tty7 hook command inside one matcher entry
/// (`{"matcher": …, "hooks": [{"command": …}]}`), if it carries one.
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

/// Turn Codex's hooks feature flag on (`[features] hooks = true`), which gates
/// whether `~/.codex/hooks.json` is read at all. Runs the codex CLI itself so
/// the flag lands wherever the installed version keeps it; probes the common
/// install locations first because the GUI's PATH is often minimal. Uninstall
/// deliberately leaves the flag on — other tools' hooks may rely on it.
fn enable_codex_hooks_feature() -> Result<(), String> {
    let candidates = [
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
    ]
    .into_iter()
    .chain(home_dir().map(|h| h.join(".local/bin/codex")))
    .find(|p| p.exists());
    let program = candidates.unwrap_or_else(|| PathBuf::from("codex"));
    match std::process::Command::new(&program)
        .args(["features", "enable", "hooks"])
        .output()
    {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(format!(
            "codex exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Err(format!("{}: {e}", program.display())),
    }
}

// ---------------------------------------------------------------------------
// Owned-file installer (Copilot, OpenCode, Pi): tty7 writes a whole file it
// owns outright, identified by the marker. Install refuses to clobber a file
// tty7 didn't write; uninstall only ever deletes a marker-carrying file.
// ---------------------------------------------------------------------------

/// The exact file content for an owned-file agent, deterministic so state
/// detection can byte-compare (drift ⇒ `Outdated`). `None` when tty7's own
/// executable path can't resolve.
fn owned_file_content(agent: HookAgent) -> Option<String> {
    match agent {
        HookAgent::Copilot => copilot_hooks_json(),
        HookAgent::OpenCode => opencode_plugin_js(),
        HookAgent::Pi => pi_extension_ts(),
        HookAgent::Claude | HookAgent::Codex => None,
    }
}

fn owned_file_state(path: &Path, expected: &str, marker: &str) -> HooksState {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return HooksState::NotInstalled;
    };
    if contents == expected {
        HooksState::Installed
    } else if contents.contains(marker) {
        // tty7 wrote it (the marker survives), but from another binary or an
        // older version of the content.
        HooksState::Outdated
    } else {
        HooksState::NotInstalled
    }
}

fn owned_file_install(path: &Path, content: &str, marker: &str) -> anyhow::Result<()> {
    // Refuse to clobber a user-authored file at the managed path, symmetric
    // with uninstall's ownership guard.
    if let Ok(existing) = std::fs::read_to_string(path)
        && !existing.contains(marker)
    {
        return Err(anyhow::anyhow!(
            "{} exists but wasn't written by tty7; not touching it",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::core::config::write_atomic(path, content.as_bytes())?;
    Ok(())
}

fn owned_file_uninstall(path: &Path, marker: &str) -> anyhow::Result<String> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
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
    std::fs::remove_file(path)?;
    // Pi's extension lives in its own directory; sweep it if now empty so
    // uninstall leaves no husk (ignore failure — a non-empty dir is the
    // user's).
    if let Some(parent) = path.parent()
        && parent.file_name().is_some_and(|n| n == "tty7")
    {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(format!("Removed {}", path.display()))
}

/// Copilot hook file (`~/.copilot/hooks/tty7.json`): Copilot auto-loads every
/// JSON file in that directory, so tty7 owns its own file and never touches
/// the user's. Event names are Copilot's camelCase vocabulary; each runs the
/// emitter with the sentinel event it maps onto. `notification` is passed
/// through and filtered in the emitter (see [`effective_event`]).
fn copilot_hooks_json() -> Option<String> {
    let cmd = |event: &str| hook_command(HookAgent::Copilot, event);
    let hook = |event: &str, timeout: u32| {
        Some(serde_json::json!([{ "type": "command", "bash": cmd(event)?, "timeoutSec": timeout }]))
    };
    let root = serde_json::json!({
        "version": 1,
        "hooks": {
            "sessionStart": hook("session-start", 5)?,
            "userPromptSubmitted": hook("prompt-submit", 5)?,
            "agentStop": hook("stop", 10)?,
            "sessionEnd": hook("session-end", 5)?,
            "notification": hook("notification", 5)?,
        }
    });
    serde_json::to_string_pretty(&root).ok()
}

/// OpenCode plugin (`~/.config/opencode/plugins/tty7.js`). OpenCode has no
/// declarative hooks — its extensibility surface is JS plugins auto-loaded
/// from that directory — so the plugin bridges its events onto the same
/// emitter every other agent's hooks run. Inert outside tty7 (both the JS
/// guard and the emitter check `TTY7`).
fn opencode_plugin_js() -> Option<String> {
    // The command prefix as a JS string literal (JSON string escaping is
    // valid JS), completed with the event name at call time.
    let prefix = serde_json::to_string(&format!(
        "{} ",
        hook_command(HookAgent::OpenCode, "")?.trim_end()
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

/// Pi extension (`~/.pi/agent/extensions/tty7/index.ts`). Pi auto-loads TS
/// extensions from per-directory `index.ts` files; this one forwards Pi's
/// lifecycle events to the emitter. Inert outside tty7 (both the TS guard and
/// the emitter check `TTY7`).
fn pi_extension_ts() -> Option<String> {
    let exe = serde_json::to_string(&std::env::current_exe().ok()?.display().to_string()).ok()?;
    Some(format!(
        r#"/* tty7 agent-hook pi bridge — generated by tty7, do not edit. */
import type {{ ExtensionAPI }} from "@mariozechner/pi-coding-agent";
import {{ spawnSync }} from "node:child_process";

const EXE = {exe};

function emit(event: string): void {{
  try {{
    spawnSync(EXE, ["agent-hook", "pi", event], {{ stdio: ["ignore", "ignore", "ignore"] }});
  }} catch {{}}
}}

export default function (pi: ExtensionAPI) {{
  if (!process.env["TTY7"]) return;
  // Extension load = the agent is running in this pane; Pi has no separate
  // session-start event.
  emit("session-start");
  pi.on("agent_start", () => emit("prompt-submit"));
  pi.on("agent_end", () => emit("stop"));
  pi.on("session_shutdown", () => emit("session-end"));
}}
"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The emitter's bytes must parse back into the exact event the daemon's
    /// sniffer expects — the two ends of the protocol locked together.
    #[test]
    fn hook_sequence_round_trips_through_the_daemon_parser() {
        use crate::core::cli_agent::{AgentEventKind, CLIAgent, parse_agent_event};

        let seq = build_hook_sequence(
            "claude",
            "notification",
            r#"{"session_id":"abc-123","message":"Claude needs your permission","cwd":"/w"}"#,
        );
        // Strip the OSC framing (`ESC ]` … `BEL`) to get the payload the
        // tokenizer would deliver.
        let payload = &seq[2..seq.len() - 1];
        let ev = parse_agent_event(payload).expect("daemon parses the emitted event");
        assert_eq!(ev.agent, Some(CLIAgent::Claude));
        assert_eq!(ev.kind, AgentEventKind::Notification);
        assert_eq!(ev.session_id.as_deref(), Some("abc-123"));
        assert!(ev.message.as_deref().unwrap().contains("permission"));

        // Garbage stdin still yields a well-formed bare event.
        let seq = build_hook_sequence("claude", "stop", "not json at all");
        let ev = parse_agent_event(&seq[2..seq.len() - 1]).expect("bare event still parses");
        assert_eq!(ev.kind, AgentEventKind::Stop);
        assert_eq!(ev.session_id, None);
    }

    /// Every event name any installer writes must be one the daemon's parser
    /// accepts — a typo here would install hooks that emit into the void.
    #[test]
    fn every_installed_event_parses_as_a_sentinel_kind() {
        use crate::core::cli_agent::parse_agent_event;

        let mut events: Vec<&str> = CLAUDE_HOOK_EVENTS
            .iter()
            .chain(CODEX_HOOK_EVENTS)
            .map(|(_, e)| *e)
            .collect();
        // Owned-file integrations embed their events in generated source.
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
            // Round-trip sanity: serde derives kebab-case names from the enum.
            let kind_json = serde_json::to_value(ev.kind).unwrap();
            assert_eq!(kind_json, serde_json::Value::String(event.to_string()));
        }
    }

    /// Copilot's catch-all `notification` hook is filtered in the emitter:
    /// permission/elicitation prompts escalate to `permission-request`, and
    /// everything else stays silent instead of masquerading as a block.
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
        // Other agents and events pass through untouched.
        assert_eq!(
            effective_event("claude", "notification", "{}"),
            Some("notification")
        );
        assert_eq!(effective_event("copilot", "stop", "{}"), Some("stop"));
    }

    /// The controlling-tty fallback (`ancestor_tty_device`) is what makes the
    /// hook work at all: Claude Code runs hooks detached from the controlling
    /// terminal, so `/dev/tty` fails and we must reach the agent's tty via the
    /// parent chain (verified end-to-end against a real detached-hook PTY
    /// setup). The device path itself is environment-dependent, so this guards
    /// only the invariant that survives CI: the `ps`-walk never panics and only
    /// ever yields a `/dev/…` device (never a bare tty name we'd fail to open).
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
        // Another agent's entry in the same file is not ours.
        assert!(marker_command(&ours, "agent-hook codex").is_none());
        let theirs = serde_json::json!({
            "hooks": [{ "type": "command", "command": "afplay /System/Library/Sounds/Glass.aiff" }]
        });
        assert!(marker_command(&theirs, "agent-hook claude").is_none());
        assert!(marker_command(&serde_json::json!({}), "agent-hook claude").is_none());
    }

    #[test]
    fn hook_command_quotes_the_exe_path() {
        let cmd = hook_command(HookAgent::Claude, "stop").expect("current_exe resolves in tests");
        assert!(cmd.starts_with('"'));
        assert!(cmd.ends_with("agent-hook claude stop"));
    }

    /// Owned-file content sanity: valid/parseable where applicable, and every
    /// generated file carries its ownership marker and this binary's path.
    #[test]
    fn owned_file_contents_carry_marker_and_exe() {
        // The exe path is embedded inside JSON / JS string literals, so look
        // for its string-escaped form — on Windows the raw path's backslashes
        // appear as `\\` in the generated content.
        let exe_raw = std::env::current_exe().unwrap().display().to_string();
        let exe_json = serde_json::to_string(&exe_raw).unwrap();
        let exe = exe_json.trim_matches('"').to_string();

        let copilot = copilot_hooks_json().expect("copilot content builds");
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

        let opencode = opencode_plugin_js().expect("opencode content builds");
        assert!(opencode.contains("agent-hook opencode"));
        assert!(opencode.contains(&exe));
        assert!(opencode.contains(r#"process.env["TTY7"]"#));

        let pi = pi_extension_ts().expect("pi content builds");
        assert!(pi.contains("agent-hook pi"));
        assert!(pi.contains(&exe));
        assert!(pi.contains(r#"process.env["TTY7"]"#));
    }

    /// Owned-file lifecycle against a scratch path: install → Installed,
    /// drift → Outdated, foreign file → refused, uninstall → gone.
    #[test]
    fn owned_file_round_trip_and_ownership_guard() {
        let dir = std::env::temp_dir().join(format!("tty7-owned-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tty7.json");
        let marker = "agent-hook copilot";
        let content = copilot_hooks_json().unwrap();

        assert_eq!(
            owned_file_state(&path, &content, marker),
            HooksState::NotInstalled
        );
        owned_file_install(&path, &content, marker).expect("fresh install succeeds");
        assert_eq!(
            owned_file_state(&path, &content, marker),
            HooksState::Installed
        );

        // Drift (e.g. written by an older tty7 or another binary) reads as
        // Outdated, and a reinstall heals it.
        std::fs::write(&path, content.replace(marker, "agent-hook copilot --old")).unwrap();
        assert_eq!(
            owned_file_state(&path, &content, marker),
            HooksState::Outdated
        );
        owned_file_install(&path, &content, marker).expect("reinstall over our own file");
        assert_eq!(
            owned_file_state(&path, &content, marker),
            HooksState::Installed
        );

        // A user-authored file at the managed path is never clobbered or
        // deleted.
        std::fs::write(&path, "// my own hooks, hands off").unwrap();
        assert!(owned_file_install(&path, &content, marker).is_err());
        assert!(owned_file_uninstall(&path, marker).is_err());

        // Restore ours, then uninstall removes it; a second uninstall no-ops.
        std::fs::write(&path, &content).unwrap();
        owned_file_uninstall(&path, marker).expect("uninstall succeeds");
        assert!(!path.exists());
        owned_file_uninstall(&path, marker).expect("uninstall is idempotent");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Full install → verify → re-install → outdated → uninstall round trip
    /// for the hooks-map installer, against a scratch settings file
    /// (`CLAUDE_CONFIG_DIR` is honored, so the test never touches the real
    /// `~/.claude`). One test on purpose: it is the only place
    /// CLAUDE_CONFIG_DIR is mutated, so `cargo test` threads never race the
    /// env var.
    #[test]
    fn install_is_idempotent_and_preserves_user_hooks() {
        let dir = std::env::temp_dir().join(format!("tty7-hooks-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        // Pre-existing user config: a model pick and their own Stop hook.
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
        // SAFETY: test-only env mutation; no other test reads this var.
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &dir) };

        assert_eq!(hooks_state(HookAgent::Claude), HooksState::NotInstalled);
        install_hooks(HookAgent::Claude).expect("install succeeds");
        assert_eq!(hooks_state(HookAgent::Claude), HooksState::Installed);

        // Install again: no duplicates.
        install_hooks(HookAgent::Claude).expect("re-install succeeds");
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        // User settings and user hooks survive.
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
        // All five events are wired.
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

        // A tty7 entry rewritten to another binary's path reads as Outdated —
        // the state the launch-time refresh keys on — and a reinstall heals it.
        let healthy = std::fs::read_to_string(&settings).unwrap();
        std::fs::write(
            &settings,
            healthy.replace("agent-hook claude stop", "agent-hook claude stop --stale"),
        )
        .unwrap();
        assert_eq!(hooks_state(HookAgent::Claude), HooksState::Outdated);
        install_hooks(HookAgent::Claude).expect("reinstall over an outdated entry succeeds");
        assert_eq!(hooks_state(HookAgent::Claude), HooksState::Installed);

        // Uninstall removes exactly our entries: the user's Stop hook and their
        // other settings survive, and the emptied event keys are dropped.
        uninstall_hooks(HookAgent::Claude).expect("uninstall succeeds");
        assert_eq!(hooks_state(HookAgent::Claude), HooksState::NotInstalled);
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
        // Nothing left to remove: a second uninstall is a no-op, not an error.
        uninstall_hooks(HookAgent::Claude).expect("uninstall is idempotent");

        // SAFETY: restore for any later test relying on the default path.
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}

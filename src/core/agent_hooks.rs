//! Agent-side hook integration: the emitter behind `tty7 agent-hook …` and the
//! installer that wires it into Claude Code's `settings.json`.
//!
//! The rich agent-status channel ([`crate::core::cli_agent`]) needs the agent
//! itself to say what it's doing. For Claude Code that is its hooks system:
//! each lifecycle hook runs `tty7 agent-hook claude <event>`, which reads the
//! hook's JSON payload from stdin and writes one sentinel OSC 777 sequence to
//! the controlling terminal (`/dev/tty`) — where tty7's daemon-side sniffer
//! picks it up and folds it into the pane's session state. The same idea can
//! ship as a Claude *plugin*; a hook + our own binary needs no plugin
//! marketplace and no jq.
//!
//! Emission is gated on the `TTY7` environment variable (injected into every
//! shell tty7 spawns), so hooks installed globally stay silent when Claude
//! runs in another terminal.

use std::io::Read as _;
use std::path::PathBuf;

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
/// never break the agent's own flow (Claude Code surfaces nonzero exits).
pub fn run_agent_hook(agent: &str, event: &str) {
    // Not inside tty7 (or a remote shell): stay silent, so globally-installed
    // hooks don't leak escape sequences into other terminals.
    if std::env::var_os(TTY7_ENV_MARKER).is_none() {
        return;
    }
    // Hook payload: Claude Code writes {"session_id": …, "message": …, …} and
    // closes stdin. Absent/malformed input still emits the bare event — the
    // state machine works without ids or messages.
    let mut input = String::new();
    let _ = std::io::stdin().take(MAX_STDIN).read_to_string(&mut input);
    write_to_controlling_tty(&build_hook_sequence(agent, event, &input));
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
// Claude Code installer.
// ---------------------------------------------------------------------------

/// The Claude Code hook events we subscribe to, and the sentinel event each
/// maps onto. `Notification` covers both "needs permission" and "waiting for
/// input" — exactly the Waiting state.
const CLAUDE_HOOK_EVENTS: [(&str, &str); 5] = [
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "prompt-submit"),
    ("Notification", "notification"),
    ("Stop", "stop"),
    ("SessionEnd", "session-end"),
];

/// Substring that identifies a hook entry as ours, for idempotent
/// install/upgrade (an entry containing it is replaced, never duplicated).
const HOOK_MARKER: &str = "agent-hook claude";

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

/// The hook command line written into Claude's settings — this binary, by
/// absolute path, so it works regardless of PATH. Quoted because macOS app
/// paths ("/Applications/…") can carry spaces.
fn hook_command(event: &str) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" agent-hook claude {event}", exe.display()))
}

/// Install (or upgrade) the tty7 hooks in Claude Code's `settings.json`,
/// preserving everything else in the file. Idempotent: entries carrying
/// [`HOOK_MARKER`] are rewritten in place (e.g. after the binary moved);
/// user-defined hooks on the same events are left untouched. Returns a short
/// human-readable summary for the caller to toast.
pub fn install_claude_hooks() -> anyhow::Result<String> {
    let path =
        claude_settings_path().ok_or_else(|| anyhow::anyhow!("cannot resolve home directory"))?;

    let mut root: serde_json::Value = match std::fs::read_to_string(&path) {
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

    for (claude_event, tty7_event) in CLAUDE_HOOK_EVENTS {
        let command = hook_command(tty7_event)
            .ok_or_else(|| anyhow::anyhow!("cannot resolve tty7's own executable path"))?;
        let entries = hooks
            .as_object_mut()
            .unwrap()
            .entry(claude_event)
            .or_insert_with(|| serde_json::json!([]));
        let Some(list) = entries.as_array_mut() else {
            continue; // malformed user config on this event; leave it alone
        };
        // Drop any previous tty7 entry (stale exe path), then append ours.
        list.retain(|matcher| !matcher_contains_marker(matcher));
        list.push(serde_json::json!({
            "hooks": [{ "type": "command", "command": command }]
        }));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::core::config::write_atomic(&path, serde_json::to_string_pretty(&root)?.as_bytes())?;
    Ok(format!(
        "Claude Code hooks installed in {} — restart running claude sessions to pick them up",
        path.display()
    ))
}

/// Whether the tty7 hooks are already present in Claude Code's settings (all
/// five events carry a marker entry).
pub fn claude_hooks_installed() -> bool {
    let Some(path) = claude_settings_path() else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    CLAUDE_HOOK_EVENTS.iter().all(|(claude_event, _)| {
        root.get("hooks")
            .and_then(|h| h.get(claude_event))
            .and_then(|e| e.as_array())
            .is_some_and(|list| list.iter().any(matcher_contains_marker))
    })
}

/// Whether one matcher entry (`{"matcher": …, "hooks": [{"command": …}]}`)
/// carries a tty7 hook command.
fn matcher_contains_marker(matcher: &serde_json::Value) -> bool {
    matcher
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.contains(HOOK_MARKER))
            })
        })
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
        assert!(matcher_contains_marker(&ours));
        let theirs = serde_json::json!({
            "hooks": [{ "type": "command", "command": "afplay /System/Library/Sounds/Glass.aiff" }]
        });
        assert!(!matcher_contains_marker(&theirs));
        assert!(!matcher_contains_marker(&serde_json::json!({})));
    }

    #[test]
    fn hook_command_quotes_the_exe_path() {
        let cmd = hook_command("stop").expect("current_exe resolves in tests");
        assert!(cmd.starts_with('"'));
        assert!(cmd.ends_with("agent-hook claude stop"));
    }

    /// Full install → verify → re-install round trip against a scratch
    /// settings file (`CLAUDE_CONFIG_DIR` is honored, so the test never
    /// touches the real `~/.claude`). Serialized with the env-var lock other
    /// env-mutating tests use… none exists for this var, so the test sets it
    /// once and relies on `cargo test` threads not racing the same var (only
    /// this test touches CLAUDE_CONFIG_DIR).
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

        assert!(!claude_hooks_installed());
        install_claude_hooks().expect("install succeeds");
        assert!(claude_hooks_installed());

        // Install again: no duplicates.
        install_claude_hooks().expect("re-install succeeds");
        let root: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        // User settings and user hooks survive.
        assert_eq!(root["model"], "opus");
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(
            stop.iter().filter(|m| matcher_contains_marker(m)).count(),
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
                root["hooks"][event]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(matcher_contains_marker),
                "{event} carries the tty7 hook"
            );
        }

        // SAFETY: restore for any later test relying on the default path.
        unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}

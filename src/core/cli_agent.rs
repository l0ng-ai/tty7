//! Third-party CLI coding-agent registry + detection.
//!
//! tty7 recognizes when a pane is running someone else's coding agent (Claude
//! Code, Codex, Gemini CLI, …) so the tab chip can brand it and desktop
//! notifications can say *which* agent finished or needs you. This is
//! deliberately *not* tty7's own agent: it only observes and enriches whatever
//! agent the user launched.
//!
//! Detection is command-based: the daemon already
//! reads the foreground process's `argv` for SSH-context sniffing, so we reuse
//! that to match the invoked command against a known agent. Matching is a pure
//! function over `argv` — [`CLIAgent::detect_from_argv`] — kept here in `core`
//! (framework-light, unit-tested) and called daemon-side, with the resulting
//! `Option<CLIAgent>` streamed to the client for the UI.
//!
//! The enum is serialized across the daemon↔client protocol, so its variants
//! are the wire contract; add new agents at the end.
//!
//! Beyond identity, this module also defines the *rich status* layer (a
//! second detection tier): agents whose hooks/plugins emit tty7's OSC 777
//! sentinel events ([`AGENT_EVENT_SENTINEL`]) get a per-session state machine
//! ([`AgentSessionState`]: idle / working / waiting-for-you / done) plus the
//! native session id used for resume-after-restart. Everything here is pure
//! and unit-tested; the daemon sniffs the events and streams state changes to
//! the client.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// A recognized third-party CLI coding agent. Ordering is the wire contract
/// (serialized in [`crate::daemon::protocol`]); append, never reorder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CLIAgent {
    Claude,
    Codex,
    Gemini,
    Aider,
    Amp,
    OpenCode,
    Copilot,
    Cursor,
    Goose,
    Droid,
    Pi,
    Auggie,
    Hermes,
    Vibe,
    Antigravity,
    Grok,
    Qwen,
}

impl CLIAgent {
    /// Every known agent, for iteration in detection and tests.
    pub const ALL: [CLIAgent; 17] = [
        CLIAgent::Claude,
        CLIAgent::Codex,
        CLIAgent::Gemini,
        CLIAgent::Aider,
        CLIAgent::Amp,
        CLIAgent::OpenCode,
        CLIAgent::Copilot,
        CLIAgent::Cursor,
        CLIAgent::Goose,
        CLIAgent::Droid,
        CLIAgent::Pi,
        CLIAgent::Auggie,
        CLIAgent::Hermes,
        CLIAgent::Vibe,
        CLIAgent::Antigravity,
        CLIAgent::Grok,
        CLIAgent::Qwen,
    ];

    /// The command names that identify this agent — the launcher binary plus any
    /// npm/pip package-dir aliases that show up in an interpreter-wrapped `argv`
    /// (e.g. `node …/@anthropic-ai/claude-code/cli.js`, where the launcher is
    /// `node` and only the `claude-code` path segment names the agent). All
    /// lowercase; matched against extension-stripped path segments.
    fn aliases(self) -> &'static [&'static str] {
        match self {
            CLIAgent::Claude => &["claude", "claude-code"],
            CLIAgent::Codex => &["codex", "codex-cli"],
            CLIAgent::Gemini => &["gemini", "gemini-cli"],
            CLIAgent::Aider => &["aider", "aider-chat"],
            CLIAgent::Amp => &["amp"],
            CLIAgent::OpenCode => &["opencode"],
            CLIAgent::Copilot => &["copilot"],
            CLIAgent::Cursor => &["cursor-agent"],
            CLIAgent::Goose => &["goose"],
            CLIAgent::Droid => &["droid"],
            CLIAgent::Pi => &["pi"],
            CLIAgent::Auggie => &["auggie"],
            CLIAgent::Hermes => &["hermes"],
            CLIAgent::Vibe => &["vibe", "vibe-acp"],
            CLIAgent::Antigravity => &["agy", "antigravity"],
            CLIAgent::Grok => &["grok"],
            CLIAgent::Qwen => &["qwen", "qwen-code"],
        }
    }

    /// Stable machine name (lowercase), used as the `agent` field of the OSC
    /// event protocol and as the value side of user-defined detection rules in
    /// `config.json` (`agent_commands: {"my-wrapper": "claude"}`).
    pub fn slug(self) -> &'static str {
        match self {
            CLIAgent::Claude => "claude",
            CLIAgent::Codex => "codex",
            CLIAgent::Gemini => "gemini",
            CLIAgent::Aider => "aider",
            CLIAgent::Amp => "amp",
            CLIAgent::OpenCode => "opencode",
            CLIAgent::Copilot => "copilot",
            CLIAgent::Cursor => "cursor",
            CLIAgent::Goose => "goose",
            CLIAgent::Droid => "droid",
            CLIAgent::Pi => "pi",
            CLIAgent::Auggie => "auggie",
            CLIAgent::Hermes => "hermes",
            CLIAgent::Vibe => "vibe",
            CLIAgent::Antigravity => "antigravity",
            CLIAgent::Grok => "grok",
            CLIAgent::Qwen => "qwen",
        }
    }

    /// Look an agent up by its [`slug`](Self::slug) (case-insensitive).
    pub fn from_slug(name: &str) -> Option<CLIAgent> {
        let name = name.trim().to_ascii_lowercase();
        CLIAgent::ALL.into_iter().find(|a| a.slug() == name)
    }

    /// Human-readable name for tab chips, notifications, and menus.
    pub fn display_name(self) -> &'static str {
        match self {
            CLIAgent::Claude => "Claude Code",
            CLIAgent::Codex => "Codex",
            CLIAgent::Gemini => "Gemini",
            CLIAgent::Aider => "Aider",
            CLIAgent::Amp => "Amp",
            CLIAgent::OpenCode => "OpenCode",
            CLIAgent::Copilot => "Copilot",
            CLIAgent::Cursor => "Cursor",
            CLIAgent::Goose => "Goose",
            CLIAgent::Droid => "Droid",
            CLIAgent::Pi => "Pi",
            CLIAgent::Auggie => "Auggie",
            CLIAgent::Hermes => "Hermes",
            CLIAgent::Vibe => "Vibe",
            CLIAgent::Antigravity => "Antigravity",
            CLIAgent::Grok => "Grok",
            CLIAgent::Qwen => "Qwen Code",
        }
    }

    /// The shell command that resumes a previous session of this agent by its
    /// native session id, or `None` for agents without a known resume flag.
    /// The id is what the agent reported in its `session-start` event (see
    /// [`AgentEvent`]); commands mirror cmux's per-agent resume table.
    pub fn resume_command(self, session_id: &str) -> Option<String> {
        // Ids come from the agent's own events, but they still land on a shell
        // command line — refuse anything that isn't a plain token so a
        // malicious/corrupt id can't smuggle shell syntax.
        if session_id.is_empty()
            || !session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        {
            return None;
        }
        match self {
            CLIAgent::Claude => Some(format!("claude --resume {session_id}")),
            CLIAgent::Codex => Some(format!("codex resume {session_id}")),
            CLIAgent::Gemini => Some(format!("gemini --resume {session_id}")),
            CLIAgent::OpenCode => Some(format!("opencode --session {session_id}")),
            CLIAgent::Amp => Some(format!("amp threads continue {session_id}")),
            CLIAgent::Cursor => Some(format!("cursor-agent --resume {session_id}")),
            _ => None,
        }
    }

    /// Brand accent (0xRRGGBB) for the tab chip's agent dot. Chosen for legibility
    /// on both light and dark themes rather than exact brand black/white — a pure
    /// black or white dot vanishes against one theme, so vendors whose mark is
    /// monochrome (Codex/OpenAI, Cursor) get a recognizable mid-tone hue instead.
    pub fn accent_rgb(self) -> u32 {
        match self {
            CLIAgent::Claude => 0xD97757,      // Claude terracotta
            CLIAgent::Codex => 0x10A37F,       // OpenAI green (black mark reads as this)
            CLIAgent::Gemini => 0x4285F4,      // Google blue
            CLIAgent::Aider => 0x14B8A6,       // teal
            CLIAgent::Amp => 0xF34E3F,         // Amp red
            CLIAgent::OpenCode => 0x6E56CF,    // violet
            CLIAgent::Copilot => 0x8957E5,     // GitHub purple
            CLIAgent::Cursor => 0x9AA0A6,      // Cursor is monochrome → neutral grey
            CLIAgent::Goose => 0x9A8CFF,       // periwinkle
            CLIAgent::Droid => 0xF59E0B,       // amber
            CLIAgent::Pi => 0x0EA5E9,          // sky
            CLIAgent::Auggie => 0x16A34A,      // Augment green
            CLIAgent::Hermes => 0x8B5CF6,      // violet
            CLIAgent::Vibe => 0xFF7000,        // Mistral orange
            CLIAgent::Antigravity => 0x2563EB, // Google blue (darker than Gemini's)
            CLIAgent::Grok => 0x64748B,        // xAI is monochrome → slate
            CLIAgent::Qwen => 0x7C3AED,        // Qwen purple
        }
    }

    /// Asset path of this agent's brand glyph, resolved through the app's
    /// [`crate::ui::assets`] source and rendered as a white silhouette on the
    /// brand-colored avatar (gpui rasterizes SVGs to a tinted alpha mask, so the
    /// mark's own fill is irrelevant — geometry only). Vendors we ship a brand
    /// mark for point at `icons/agents/…`; the rest fall back to the generic
    /// gpui-component `bot` glyph so every recognized agent still gets an avatar.
    pub fn icon_path(self) -> &'static str {
        match self {
            CLIAgent::Claude => "icons/agents/claude.svg",
            CLIAgent::Codex => "icons/agents/codex.svg",
            CLIAgent::Gemini => "icons/agents/gemini.svg",
            CLIAgent::Amp => "icons/agents/amp.svg",
            CLIAgent::OpenCode => "icons/agents/opencode.svg",
            CLIAgent::Copilot => "icons/agents/copilot.svg",
            CLIAgent::Cursor => "icons/agents/cursor.svg",
            CLIAgent::Goose => "icons/agents/goose.svg",
            CLIAgent::Droid => "icons/agents/droid.svg",
            // No brand mark bundled → generic robot glyph.
            CLIAgent::Aider
            | CLIAgent::Pi
            | CLIAgent::Auggie
            | CLIAgent::Hermes
            | CLIAgent::Vibe
            | CLIAgent::Antigravity
            | CLIAgent::Grok
            | CLIAgent::Qwen => "icons/bot.svg",
        }
    }

    /// Match a single extension-stripped, lowercased command token against the
    /// registry. `None` when nothing matches.
    fn match_token(token: &str) -> Option<CLIAgent> {
        CLIAgent::ALL
            .into_iter()
            .find(|a| a.aliases().contains(&token))
    }

    /// Identify the coding agent a foreground `argv` is running, or `None`.
    ///
    /// The strategy is command-name detection:
    /// 1. Strip any leading `VAR=value` environment assignments (`FOO=1 claude`).
    /// 2. If the launcher's own basename matches a known agent, that's it — the
    ///    native-binary case (`claude`, `codex`, `gemini`, `aider`, …).
    /// 3. Otherwise, if the launcher is a script *interpreter* (`node`, `bun`,
    ///    `python`, `npx`, …), scan the remaining path-like arguments for a
    ///    segment that names an agent — the npm/pip-wrapped case
    ///    (`node …/claude-code/cli.js`, `npx @anthropic-ai/claude-code`).
    ///
    /// The interpreter gate is what keeps `cat codex.md` or `vim aider.py` from
    /// false-matching: a non-interpreter launcher only ever matches on its own
    /// name, never on its arguments.
    ///
    /// The production caller (the daemon's foreground poll) goes through
    /// [`detect_from_argv_with`](Self::detect_from_argv_with) to honor
    /// user-defined rules; this rule-free form is the pure core the test suite
    /// exercises.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn detect_from_argv(argv: &[String]) -> Option<CLIAgent> {
        Self::detect_from_argv_with(argv, &HashMap::new())
    }

    /// [`detect_from_argv`](Self::detect_from_argv) extended with user-defined
    /// rules (`config.json`'s `agent_commands`): a map from a command basename
    /// to an agent [`slug`](Self::slug), so a personal wrapper (`"cc":
    /// "claude"`) is branded like the agent it launches — a command allowlist
    /// keyed by exact basename instead of regex. Custom rules apply to the
    /// *launcher* only (never to
    /// interpreter arguments) and lose to a built-in match on the same name.
    pub fn detect_from_argv_with(
        argv: &[String],
        custom: &HashMap<String, String>,
    ) -> Option<CLIAgent> {
        // 1. Skip leading environment assignments (`KEY=val`). A bare `env` prefix
        //    (`env claude`) is treated as an interpreter below so its target is
        //    scanned.
        let mut rest = argv
            .iter()
            .map(String::as_str)
            .skip_while(|t| is_env_assignment(t));

        let launcher = rest.next()?;
        let launcher_stem = base_stem(launcher);

        // 2. Native binary: the launcher itself is the agent — by the built-in
        //    registry first, then by a user-defined rule.
        if let Some(agent) = CLIAgent::match_token(launcher_stem) {
            return Some(agent);
        }
        if let Some(agent) = custom
            .get(&launcher_stem.to_ascii_lowercase())
            .and_then(|slug| CLIAgent::from_slug(slug))
        {
            return Some(agent);
        }

        // 3. Interpreter wrapper: scan the script path / package arg it runs.
        if is_interpreter(launcher_stem) {
            for arg in rest {
                // Only inspect path-like / package-like tokens (the script it
                // runs), never bare flags or option values.
                if arg.starts_with('-') {
                    continue;
                }
                for segment in arg.split('/') {
                    if let Some(agent) =
                        CLIAgent::match_token(&base_stem(segment).to_ascii_lowercase())
                    {
                        return Some(agent);
                    }
                }
            }
        }

        None
    }
}

/// A `KEY=value` shell environment assignment prefix (`FOO=bar cmd`). The `KEY`
/// must be a non-empty run of identifier chars before the first `=`.
fn is_env_assignment(token: &str) -> bool {
    match token.split_once('=') {
        Some((key, _)) => {
            // A real env var starts with a letter/underscore and is otherwise
            // alphanumerics/underscores — this rejects things like `a=b` paths or
            // `--flag=val` that merely contain `=`.
            let mut bytes = key.bytes();
            bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }
        None => false,
    }
}

/// The final path component with a leading dir and a trailing script extension
/// stripped, lowercased-ready but case preserved (callers lowercase when they
/// match interpreter args). `/usr/bin/claude` → `claude`, `cli.js` → `cli`.
fn base_stem(token: &str) -> &str {
    let name = Path::new(token)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(token);
    // Strip one known script extension; leave unknown suffixes intact so
    // `claude-code` stays whole.
    for ext in [".js", ".mjs", ".cjs", ".ts", ".py", ".rb", ".sh"] {
        if let Some(stem) = name.strip_suffix(ext) {
            return stem;
        }
    }
    name
}

/// Whether a launcher basename is a script interpreter whose argument (rather
/// than the launcher itself) names the real program — so agent detection should
/// scan past it. Covers the common Node/Python/Ruby/`env`/`npx` wrappers agents
/// ship as.
fn is_interpreter(stem: &str) -> bool {
    matches!(
        stem.to_ascii_lowercase().as_str(),
        "node"
            | "nodejs"
            | "bun"
            | "deno"
            | "npx"
            | "pnpm"
            | "yarn"
            | "python"
            | "python3"
            | "ruby"
            | "uv"
            | "uvx"
            | "env"
    )
}

// ---------------------------------------------------------------------------
// Rich session status — the OSC event protocol + per-pane state machine.
//
// Identity detection above answers "*which* agent runs here"; this layer
// answers "what is it doing". Agent-side hooks (installed by
// `core::agent_hooks`, or hand-wired for any agent) emit an OSC 777
// notification whose title is the [`AGENT_EVENT_SENTINEL`] and whose body is a
// small JSON event. The daemon sniffs those out of the PTY stream, folds them
// through [`AgentSessionState::apply_event`], and streams the state to the
// client (`DaemonMsg::AgentStatus`) for status dots, "needs your input"
// notifications, and session resume. It's a self-describing sentinel channel
// (OSC 777 + `tty7://cli-agent` sentinel + versioned JSON).
// ---------------------------------------------------------------------------

/// The OSC 777 notification title that marks a payload as a tty7 agent event
/// rather than a user-facing notification:
/// `ESC ] 777;notify;tty7://cli-agent;{json} BEL`.
pub const AGENT_EVENT_SENTINEL: &str = "tty7://cli-agent";

/// What an agent session is doing right now, coarsely. `Waiting` is the state
/// the whole feature exists for: the agent stopped mid-turn and needs the user
/// (a permission prompt, a question) — the moment worth a notification and an
/// amber dot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentStatus {
    /// Session open, no turn in flight (freshly started, or the user hasn't
    /// prompted since the last turn ended and was seen).
    #[default]
    Idle,
    /// A turn is in flight (prompt submitted, tools running).
    Working,
    /// Stopped mid-turn on the user: permission request, question, or an
    /// opaque "the agent pinged you" notification.
    Waiting,
    /// The turn finished; the result is sitting there waiting to be read.
    Done,
}

/// Per-pane agent session state, maintained daemon-side and mirrored to the
/// client. Exists only while an agent is detected in the pane's foreground.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionState {
    #[serde(default = "AgentSessionState::default_status")]
    pub status: AgentStatus,
    /// Human-readable context for `Waiting`/`Done` (e.g. "Claude needs your
    /// permission to use Bash"), straight from the event.
    #[serde(default)]
    pub message: Option<String>,
    /// The agent's *native* session id (from its `session-start` event), the
    /// key its own `--resume` flag takes — persisted for restore.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Whether this state came from the rich sentinel channel (hooks
    /// installed) rather than the opaque OSC 9/777 fallback. Rich state drives
    /// turn-level notifications; fallback state only paints the dot (the
    /// agent's own notification text was already toasted by the client).
    #[serde(default)]
    pub rich: bool,
}

impl AgentStatus {
    /// The status dot color (0xRRGGBB) shared by the tab chip and the sidebar,
    /// or `None` for `Idle` (no dot — a resting agent is just its brand mark).
    pub fn dot_rgb(self) -> Option<u32> {
        match self {
            AgentStatus::Idle => None,
            AgentStatus::Working => Some(0x3B82F6), // blue: in flight
            AgentStatus::Waiting => Some(0xF59E0B), // amber: needs you
            AgentStatus::Done => Some(0x22C55E),    // green: result ready
        }
    }
}

impl AgentSessionState {
    fn default_status() -> AgentStatus {
        AgentStatus::Idle
    }

    /// Fold one rich event into the state. Pure transition function — the
    /// daemon owns *when* to call it and who to tell.
    pub fn apply_event(&mut self, ev: &AgentEvent) {
        self.rich = true;
        if let Some(id) = &ev.session_id {
            self.session_id = Some(id.clone());
        }
        match ev.kind {
            AgentEventKind::SessionStart => {
                self.status = AgentStatus::Idle;
                self.message = None;
            }
            AgentEventKind::PromptSubmit => {
                self.status = AgentStatus::Working;
                self.message = None;
            }
            // Explicit blocks from agents that distinguish them (Codex/OpenCode
            // plugins): always the urgent "needs you" state.
            AgentEventKind::PermissionRequest | AgentEventKind::QuestionAsked => {
                self.status = AgentStatus::Waiting;
                self.message = ev.message.clone();
            }
            // Claude Code overloads its single Notification hook: it fires
            // *mid-turn* for a permission/decision prompt (a genuine block worth
            // the amber "needs you" state), but ALSO fires *between* turns as an
            // idle "Claude is waiting for your input" reminder — which must not
            // masquerade as a block. Escalate only when a turn is actually in
            // flight; otherwise it's a passive nudge and the current state
            // (typically Done, freshly replied) stands. Keyed on turn phase, not
            // the message text, so it survives version/locale changes.
            AgentEventKind::Notification => {
                if self.status == AgentStatus::Working {
                    self.status = AgentStatus::Waiting;
                    self.message = ev.message.clone();
                }
            }
            AgentEventKind::Stop => {
                self.status = AgentStatus::Done;
                self.message = ev.message.clone();
            }
            // The agent session ended but its id stays: Claude & friends can
            // resume an *ended* session, which is exactly what restore does.
            AgentEventKind::SessionEnd => {
                self.status = AgentStatus::Idle;
                self.message = None;
            }
        }
    }
}

/// The event vocabulary of the sentinel protocol (`"event"` in the JSON).
/// Deliberately a superset of what any one agent emits: Claude Code hooks map
/// onto session-start / prompt-submit / notification / stop / session-end,
/// while permission-request / question-asked are there for agents (Codex,
/// OpenCode plugins) that can distinguish them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentEventKind {
    SessionStart,
    PromptSubmit,
    PermissionRequest,
    QuestionAsked,
    Notification,
    Stop,
    SessionEnd,
}

/// One parsed sentinel event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEvent {
    /// Which agent sent it, when the payload names one we know. Lets the event
    /// brand a pane even where argv detection can't see the process (a wrapper
    /// we don't recognize).
    pub agent: Option<CLIAgent>,
    pub kind: AgentEventKind,
    pub session_id: Option<String>,
    pub message: Option<String>,
}

/// Parse a complete OSC payload (identifier included, e.g.
/// `777;notify;tty7://cli-agent;{"v":1,…}`) into an [`AgentEvent`]. `None` for
/// anything that isn't a well-formed sentinel event — including unknown
/// `event` values, so the protocol can grow without old daemons
/// mis-classifying new events.
pub fn parse_agent_event(payload: &[u8]) -> Option<AgentEvent> {
    let rest = payload.strip_prefix(b"777;notify;")?;
    let rest = rest.strip_prefix(AGENT_EVENT_SENTINEL.as_bytes())?;
    let json = rest.strip_prefix(b";")?;

    #[derive(Deserialize)]
    struct Wire {
        // Protocol version; v1 is all that exists. Kept for forward evolution.
        #[serde(default)]
        #[allow(dead_code)]
        v: u32,
        #[serde(default)]
        agent: Option<String>,
        event: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        message: Option<String>,
    }

    let w: Wire = serde_json::from_slice(json).ok()?;
    let kind = serde_json::from_value::<AgentEventKind>(serde_json::Value::String(w.event)).ok()?;
    let nonempty = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
    Some(AgentEvent {
        agent: w.agent.as_deref().and_then(CLIAgent::from_slug),
        kind,
        session_id: nonempty(w.session_id),
        message: nonempty(w.message),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detects_native_binaries() {
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&["claude"])),
            Some(CLIAgent::Claude)
        );
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&["/opt/homebrew/bin/codex", "--model", "o3"])),
            Some(CLIAgent::Codex)
        );
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&["/usr/local/bin/gemini"])),
            Some(CLIAgent::Gemini)
        );
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&["cursor-agent"])),
            Some(CLIAgent::Cursor)
        );
    }

    #[test]
    fn strips_leading_env_assignments() {
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&["FOO=1", "BAR=baz", "claude"])),
            Some(CLIAgent::Claude)
        );
    }

    #[test]
    fn detects_node_wrapped_claude_by_package_dir() {
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&[
                "node",
                "/Users/x/.npm/_npx/node_modules/@anthropic-ai/claude-code/cli.js",
            ])),
            Some(CLIAgent::Claude)
        );
    }

    #[test]
    fn detects_npx_package_form() {
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&["npx", "@anthropic-ai/claude-code"])),
            Some(CLIAgent::Claude)
        );
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&["npx", "@google/gemini-cli"])),
            Some(CLIAgent::Gemini)
        );
    }

    #[test]
    fn detects_python_wrapped_aider() {
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&[
                "python3",
                "/usr/lib/python3.12/site-packages/aider/__main__.py",
            ])),
            Some(CLIAgent::Aider)
        );
    }

    #[test]
    fn non_interpreter_does_not_match_on_arguments() {
        // A file *named* like an agent, opened by an unrelated tool, must not
        // trip detection — only interpreters have their args scanned.
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&["cat", "codex.md"])),
            None
        );
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&["vim", "claude-code/notes.txt"])),
            None
        );
        assert_eq!(CLIAgent::detect_from_argv(&argv(&["less", "aider"])), None);
    }

    #[test]
    fn unrelated_commands_are_none() {
        assert_eq!(CLIAgent::detect_from_argv(&argv(&["zsh"])), None);
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&["node", "server.js"])),
            None
        );
        assert_eq!(CLIAgent::detect_from_argv(&argv(&[])), None);
    }

    #[test]
    fn every_agent_has_metadata() {
        for a in CLIAgent::ALL {
            assert!(!a.display_name().is_empty());
            assert!(!a.aliases().is_empty());
            assert!(a.accent_rgb() <= 0xFFFFFF);
            assert_eq!(CLIAgent::from_slug(a.slug()), Some(a));
        }
    }

    #[test]
    fn detects_newer_agents_by_command() {
        for (cmd, agent) in [
            ("auggie", CLIAgent::Auggie),
            ("agy", CLIAgent::Antigravity),
            ("vibe-acp", CLIAgent::Vibe),
            ("grok", CLIAgent::Grok),
            ("/usr/local/bin/qwen", CLIAgent::Qwen),
            ("pi", CLIAgent::Pi),
            ("hermes", CLIAgent::Hermes),
        ] {
            assert_eq!(CLIAgent::detect_from_argv(&argv(&[cmd])), Some(agent));
        }
    }

    #[test]
    fn custom_rules_map_wrappers_to_agents() {
        let custom: HashMap<String, String> = [("cc".to_string(), "claude".to_string())].into();
        assert_eq!(
            CLIAgent::detect_from_argv_with(&argv(&["/home/x/bin/cc", "-c"]), &custom),
            Some(CLIAgent::Claude)
        );
        // A rule naming an unknown agent is ignored, not an error.
        let bogus: HashMap<String, String> = [("cc".to_string(), "hal9000".to_string())].into();
        assert_eq!(
            CLIAgent::detect_from_argv_with(&argv(&["cc"]), &bogus),
            None
        );
        // Custom rules never scan interpreter arguments.
        assert_eq!(
            CLIAgent::detect_from_argv_with(&argv(&["node", "cc/cli.js"]), &custom),
            None
        );
        // Built-ins still win on their own names.
        let shadow: HashMap<String, String> = [("codex".to_string(), "claude".to_string())].into();
        assert_eq!(
            CLIAgent::detect_from_argv_with(&argv(&["codex"]), &shadow),
            Some(CLIAgent::Codex)
        );
    }

    #[test]
    fn parses_sentinel_events() {
        let ev = parse_agent_event(
            br#"777;notify;tty7://cli-agent;{"v":1,"agent":"claude","event":"permission-request","session_id":"abc-123","message":"Claude needs your permission to use Bash"}"#,
        )
        .expect("well-formed sentinel event");
        assert_eq!(ev.agent, Some(CLIAgent::Claude));
        assert_eq!(ev.kind, AgentEventKind::PermissionRequest);
        assert_eq!(ev.session_id.as_deref(), Some("abc-123"));
        assert!(ev.message.as_deref().unwrap().contains("permission"));

        // A plain OSC 777 notification is NOT an event.
        assert_eq!(parse_agent_event(b"777;notify;Build;done"), None);
        // Unknown event names are dropped (forward evolution).
        assert_eq!(
            parse_agent_event(br#"777;notify;tty7://cli-agent;{"event":"quantum-leap"}"#),
            None
        );
        // Malformed JSON is dropped.
        assert_eq!(
            parse_agent_event(b"777;notify;tty7://cli-agent;{oops"),
            None
        );
    }

    #[test]
    fn session_state_machine_follows_the_turn() {
        let mut s = AgentSessionState::default();
        assert_eq!(s.status, AgentStatus::Idle);

        let ev = |kind, msg: Option<&str>, id: Option<&str>| AgentEvent {
            agent: Some(CLIAgent::Claude),
            kind,
            session_id: id.map(String::from),
            message: msg.map(String::from),
        };

        s.apply_event(&ev(AgentEventKind::SessionStart, None, Some("sid-1")));
        assert_eq!(s.status, AgentStatus::Idle);
        assert_eq!(s.session_id.as_deref(), Some("sid-1"));
        assert!(s.rich);

        s.apply_event(&ev(AgentEventKind::PromptSubmit, None, None));
        assert_eq!(s.status, AgentStatus::Working);

        // A Notification arriving MID-TURN (while Working) is a real block —
        // a permission/decision prompt — so it escalates to Waiting.
        s.apply_event(&ev(
            AgentEventKind::Notification,
            Some("Claude needs your permission"),
            None,
        ));
        assert_eq!(s.status, AgentStatus::Waiting);
        assert!(s.message.as_deref().unwrap().contains("permission"));

        s.apply_event(&ev(AgentEventKind::Stop, None, None));
        assert_eq!(s.status, AgentStatus::Done);

        // A Notification arriving BETWEEN turns (while Done) is Claude Code's
        // idle "waiting for your input" nudge, NOT a block — it must not flip
        // the finished-and-green session to amber "needs you".
        s.apply_event(&ev(
            AgentEventKind::Notification,
            Some("Claude is waiting for your input"),
            None,
        ));
        assert_eq!(
            s.status,
            AgentStatus::Done,
            "an idle notification between turns must not fabricate a block"
        );

        // Session end goes idle but KEEPS the id — ended sessions resume.
        s.apply_event(&ev(AgentEventKind::SessionEnd, None, None));
        assert_eq!(s.status, AgentStatus::Idle);
        assert_eq!(s.session_id.as_deref(), Some("sid-1"));
    }

    #[test]
    fn resume_commands_are_shell_safe() {
        assert_eq!(
            CLIAgent::Claude.resume_command("abc-123").as_deref(),
            Some("claude --resume abc-123")
        );
        assert_eq!(
            CLIAgent::Codex.resume_command("th_read.9").as_deref(),
            Some("codex resume th_read.9")
        );
        // No resume flag known → None.
        assert_eq!(CLIAgent::Aider.resume_command("abc"), None);
        // An id carrying shell syntax is refused outright.
        assert_eq!(CLIAgent::Claude.resume_command("abc; rm -rf /"), None);
        assert_eq!(CLIAgent::Claude.resume_command("$(boom)"), None);
        assert_eq!(CLIAgent::Claude.resume_command(""), None);
    }

    #[test]
    fn status_metadata_is_consistent() {
        assert_eq!(AgentStatus::Idle.dot_rgb(), None);
        for st in [
            AgentStatus::Working,
            AgentStatus::Waiting,
            AgentStatus::Done,
        ] {
            assert!(st.dot_rgb().is_some());
        }
        // Wire form is kebab-case (shared with the JSON protocol).
        assert_eq!(
            serde_json::to_string(&AgentStatus::Waiting).unwrap(),
            "\"waiting\""
        );
    }
}

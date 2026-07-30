use std::collections::HashMap;

use serde::{Deserialize, Serialize};

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

    pub fn from_slug(name: &str) -> Option<CLIAgent> {
        let name = name.trim().to_ascii_lowercase();
        CLIAgent::ALL.into_iter().find(|a| a.slug() == name)
    }

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

    pub fn resume_command(
        self,
        session_id: &str,
        launch_argv: Option<&[String]>,
    ) -> Option<String> {
        if launch_argv.is_some_and(|argv| self.opts_out_of_sessions(argv)) {
            return None;
        }
        let flags = self.session_command_flags(session_id, launch_argv)?;
        match self {
            CLIAgent::Claude => Some(format!("claude{flags} --resume {session_id}")),
            CLIAgent::Codex => Some(format!("codex resume {session_id}{flags}")),
            CLIAgent::Gemini => Some(format!("gemini{flags} --resume {session_id}")),
            CLIAgent::OpenCode => Some(format!("opencode{flags} --session {session_id}")),
            CLIAgent::Amp => Some(format!("amp threads continue {session_id}{flags}")),
            CLIAgent::Cursor => Some(format!("cursor-agent{flags} --resume {session_id}")),
            CLIAgent::Copilot => Some(format!("copilot{flags} --resume {session_id}")),
            CLIAgent::Grok => Some(format!("grok{flags} --resume {session_id}")),
            CLIAgent::Pi => Some(format!("pi{flags} --session {session_id}")),
            _ => None,
        }
    }

    fn opts_out_of_sessions(self, argv: &[String]) -> bool {
        let ephemeral: &[&str] = match self {
            CLIAgent::Pi => &["--no-session"],
            _ => &[],
        };
        argv.iter().any(|t| ephemeral.contains(&t.as_str()))
    }

    pub fn fork_command(self, session_id: &str, launch_argv: Option<&[String]>) -> Option<String> {
        let flags = self.session_command_flags(session_id, launch_argv)?;
        match self {
            CLIAgent::Codex => Some(format!("codex fork {session_id}{flags}")),
            CLIAgent::Claude => Some(format!(
                "claude{flags} --resume {session_id} --fork-session"
            )),
            CLIAgent::Grok => Some(format!("grok{flags} --resume {session_id} --fork-session")),
            CLIAgent::OpenCode => Some(format!("opencode{flags} --session {session_id} --fork")),
            _ => None,
        }
    }

    pub fn fork_label(self) -> Option<&'static str> {
        match self {
            CLIAgent::Claude | CLIAgent::Codex | CLIAgent::Grok | CLIAgent::OpenCode => {
                Some("Fork Session")
            }
            _ => None,
        }
    }

    fn session_command_flags(
        self,
        session_id: &str,
        launch_argv: Option<&[String]>,
    ) -> Option<String> {
        if session_id.is_empty()
            || !session_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        {
            return None;
        }
        Some(
            launch_argv
                .and_then(|argv| self.replay_flags(argv))
                .map(|flags| {
                    flags.iter().fold(String::new(), |mut s, f| {
                        s.push(' ');
                        s.push_str(f);
                        s
                    })
                })
                .unwrap_or_default(),
        )
    }

    fn replay_flags(self, argv: &[String]) -> Option<Vec<String>> {
        let names_self = |token: &str| {
            token.split(['/', '\\']).any(|seg| {
                CLIAgent::match_token(&base_stem(seg).to_ascii_lowercase()) == Some(self)
            })
        };
        let argv = &argv[argv.iter().take_while(|t| is_env_assignment(t)).count()..];
        let named = argv.iter().position(|t| names_self(t))?;
        let mut tail: Vec<&str> = argv[named + 1..].iter().map(String::as_str).collect();

        if self == CLIAgent::Codex && matches!(tail.first(), Some(&"resume") | Some(&"fork")) {
            tail.remove(0);
            if tail.first().is_some_and(|t| !t.starts_with('-')) {
                tail.remove(0);
            }
        }

        let stale: &[&str] = match self {
            CLIAgent::Claude => &[
                "--resume",
                "-r",
                "--continue",
                "-c",
                "--session-id",
                "--from-pr",
                "--fork-session",
            ],
            CLIAgent::Gemini | CLIAgent::Cursor => &["--resume", "-r"],
            CLIAgent::Copilot => &["--resume", "-r", "--continue", "-c"],
            CLIAgent::OpenCode => &["--session", "-s", "--continue", "-c", "--fork"],
            CLIAgent::Codex => &["--last"],
            CLIAgent::Pi => &[
                "--session",
                "--session-id",
                "--fork",
                "--resume",
                "-r",
                "--continue",
                "-c",
            ],
            CLIAgent::Grok => &[
                "--resume",
                "-r",
                "--load",
                "--continue",
                "-c",
                "--session-id",
                "-s",
                "--fork-session",
                "--worktree",
                "-w",
                "--worktree-ref",
                "--ref",
            ],
            _ => &[],
        };
        let mut i = 0;
        while i < tail.len() {
            let t = tail[i];
            if stale.contains(&t)
                || stale
                    .iter()
                    .any(|f| f.len() > 2 && t.starts_with(&format!("{f}=")))
            {
                tail.remove(i);
                if i < tail.len() && !tail[i].starts_with('-') {
                    tail.remove(i);
                }
            } else {
                i += 1;
            }
        }

        let safe = |t: &str| {
            !t.is_empty()
                && t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_=./,:@+~".contains(&b))
        };
        if !tail.iter().all(|t| safe(t)) {
            return None;
        }
        let mut prev_was_flag = false;
        for t in &tail {
            let is_flag = t.starts_with('-');
            if !is_flag && !prev_was_flag {
                return None;
            }
            prev_was_flag = is_flag;
        }
        Some(tail.into_iter().map(String::from).collect())
    }

    pub fn accent_rgb(self) -> u32 {
        match self {
            CLIAgent::Claude => 0xD97757,
            CLIAgent::Codex => 0x000000,
            CLIAgent::Gemini => 0x4285F4,
            CLIAgent::Aider => 0x14B8A6,
            CLIAgent::Amp => 0xF34E3F,
            CLIAgent::OpenCode => 0x6E56CF,
            CLIAgent::Copilot => 0x8957E5,
            CLIAgent::Cursor => 0x9AA0A6,
            CLIAgent::Goose => 0x9A8CFF,
            CLIAgent::Droid => 0xF59E0B,
            CLIAgent::Pi => 0x0EA5E9,
            CLIAgent::Auggie => 0x16A34A,
            CLIAgent::Hermes => 0x8B5CF6,
            CLIAgent::Vibe => 0xFF7000,
            CLIAgent::Antigravity => 0x2563EB,
            CLIAgent::Grok => 0x000000,
            CLIAgent::Qwen => 0x7C3AED,
        }
    }

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
            CLIAgent::Grok => "icons/agents/grok.svg",
            CLIAgent::Pi => "icons/agents/pi.svg",
            CLIAgent::Aider
            | CLIAgent::Auggie
            | CLIAgent::Hermes
            | CLIAgent::Vibe
            | CLIAgent::Antigravity
            | CLIAgent::Qwen => "icons/bot.svg",
        }
    }

    fn match_token(token: &str) -> Option<CLIAgent> {
        CLIAgent::ALL
            .into_iter()
            .find(|a| a.aliases().contains(&token))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn detect_from_argv(argv: &[String]) -> Option<CLIAgent> {
        Self::detect_from_argv_with(argv, &HashMap::new())
    }

    pub fn detect_from_argv_with(
        argv: &[String],
        custom: &HashMap<String, String>,
    ) -> Option<CLIAgent> {
        let mut rest = argv
            .iter()
            .map(String::as_str)
            .skip_while(|t| is_env_assignment(t));

        let launcher = rest.next()?;
        let launcher_stem = base_stem(launcher);

        if let Some(agent) = CLIAgent::match_token(launcher_stem) {
            return Some(agent);
        }
        if let Some(agent) = custom
            .get(&launcher_stem.to_ascii_lowercase())
            .and_then(|slug| CLIAgent::from_slug(slug))
        {
            return Some(agent);
        }

        if is_interpreter(launcher_stem) {
            for arg in rest {
                if arg.starts_with('-') {
                    continue;
                }
                for segment in arg.split(['/', '\\']) {
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

    pub fn detect_from_command_with(
        command: &str,
        custom: &HashMap<String, String>,
    ) -> Option<CLIAgent> {
        let mut argv: Vec<String> = command
            .split_whitespace()
            .map(|t| t.trim_matches(['"', '\'']).to_ascii_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        if argv.first().is_some_and(|t| t == "&") {
            argv.remove(0);
        }
        Self::detect_from_argv_with(&argv, custom)
    }
}

fn is_env_assignment(token: &str) -> bool {
    match token.split_once('=') {
        Some((key, _)) => {
            let mut bytes = key.bytes();
            bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }
        None => false,
    }
}

fn base_stem(token: &str) -> &str {
    let trimmed = token.trim_end_matches(['/', '\\']);
    let name = match trimmed.rfind(['/', '\\']) {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    };
    for ext in [
        ".js", ".mjs", ".cjs", ".ts", ".py", ".rb", ".sh", ".exe", ".cmd", ".bat", ".ps1",
    ] {
        if let Some(stem) = name.strip_suffix(ext) {
            return stem;
        }
    }
    name
}

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

pub const AGENT_EVENT_SENTINEL: &str = "tty7://cli-agent";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentStatus {
    #[default]
    Idle,
    Working,
    Waiting,
    Done,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionState {
    #[serde(default = "AgentSessionState::default_status")]
    pub status: AgentStatus,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub launch_argv: Option<Vec<String>>,
    #[serde(default)]
    pub rich: bool,
    #[serde(default)]
    pub cwd: Option<std::path::PathBuf>,
    #[serde(default)]
    pub activity: u64,
}

impl AgentStatus {
    pub fn dot_rgb(self) -> Option<u32> {
        match self {
            AgentStatus::Idle => None,
            AgentStatus::Working => Some(0x3B82F6),
            AgentStatus::Waiting => Some(0xF59E0B),
            AgentStatus::Done => Some(0x22C55E),
        }
    }
}

impl AgentSessionState {
    fn default_status() -> AgentStatus {
        AgentStatus::Idle
    }

    pub fn apply_event(&mut self, ev: &AgentEvent) {
        self.rich = true;
        if let Some(id) = &ev.session_id {
            self.session_id = Some(id.clone());
        }
        if let Some(cwd) = &ev.cwd {
            self.cwd = Some(cwd.clone());
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
            AgentEventKind::PermissionRequest | AgentEventKind::QuestionAsked => {
                self.status = AgentStatus::Waiting;
                self.message = ev.message.clone();
            }
            AgentEventKind::Notification => {
                if self.status == AgentStatus::Working {
                    self.status = AgentStatus::Waiting;
                    self.message = ev.message.clone();
                }
            }
            AgentEventKind::ToolComplete => {
                self.activity = self.activity.wrapping_add(1);
                if self.status == AgentStatus::Waiting {
                    self.status = AgentStatus::Working;
                    self.message = None;
                }
            }
            AgentEventKind::Stop => {
                self.status = AgentStatus::Done;
                self.message = ev.message.clone();
            }
            AgentEventKind::SessionEnd => {
                self.status = AgentStatus::Idle;
                self.message = None;
                self.cwd = None;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentEventKind {
    SessionStart,
    PromptSubmit,
    PermissionRequest,
    QuestionAsked,
    ToolComplete,
    Notification,
    Stop,
    SessionEnd,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEvent {
    pub agent: Option<CLIAgent>,
    pub kind: AgentEventKind,
    pub session_id: Option<String>,
    pub message: Option<String>,
    pub cwd: Option<std::path::PathBuf>,
}

pub fn parse_agent_event(payload: &[u8]) -> Option<AgentEvent> {
    let rest = payload.strip_prefix(b"777;notify;")?;
    let rest = rest.strip_prefix(AGENT_EVENT_SENTINEL.as_bytes())?;
    let json = rest.strip_prefix(b";")?;

    #[derive(Deserialize)]
    struct Wire {
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
        #[serde(default)]
        cwd: Option<String>,
    }

    let w: Wire = serde_json::from_slice(json).ok()?;
    let kind = serde_json::from_value::<AgentEventKind>(serde_json::Value::String(w.event)).ok()?;
    let nonempty = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
    Some(AgentEvent {
        agent: w.agent.as_deref().and_then(CLIAgent::from_slug),
        kind,
        session_id: nonempty(w.session_id),
        message: nonempty(w.message),
        cwd: nonempty(w.cwd).map(std::path::PathBuf::from),
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
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&["claude/"])),
            Some(CLIAgent::Claude)
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
    fn black_branded_avatars_keep_their_brand_field() {
        assert_eq!(CLIAgent::Codex.accent_rgb(), 0x000000);
        assert_eq!(CLIAgent::Grok.accent_rgb(), 0x000000);
    }

    #[test]
    fn only_the_unbranded_agents_use_the_fallback_glyph() {
        let fallback: Vec<&str> = CLIAgent::ALL
            .into_iter()
            .filter(|a| a.icon_path() == "icons/bot.svg")
            .map(CLIAgent::slug)
            .collect();
        assert_eq!(
            fallback,
            ["aider", "auggie", "hermes", "vibe", "antigravity", "qwen"]
        );
        for a in CLIAgent::ALL {
            let path = a.icon_path();
            assert!(
                path == "icons/bot.svg" || path == format!("icons/agents/{}.svg", a.slug()),
                "{} points at an unexpected {path}",
                a.display_name()
            );
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
        let bogus: HashMap<String, String> = [("cc".to_string(), "hal9000".to_string())].into();
        assert_eq!(
            CLIAgent::detect_from_argv_with(&argv(&["cc"]), &bogus),
            None
        );
        assert_eq!(
            CLIAgent::detect_from_argv_with(&argv(&["node", "cc/cli.js"]), &custom),
            None
        );
        let shadow: HashMap<String, String> = [("codex".to_string(), "claude".to_string())].into();
        assert_eq!(
            CLIAgent::detect_from_argv_with(&argv(&["codex"]), &shadow),
            Some(CLIAgent::Codex)
        );
    }

    #[test]
    fn detects_from_typed_command_lines() {
        let none = HashMap::new();
        assert_eq!(
            CLIAgent::detect_from_command_with("claude --resume abc", &none),
            Some(CLIAgent::Claude)
        );
        assert_eq!(
            CLIAgent::detect_from_command_with("claude.exe", &none),
            Some(CLIAgent::Claude)
        );
        assert_eq!(
            CLIAgent::detect_from_command_with(
                r"C:\Users\x\AppData\Roaming\npm\claude.cmd --model opus",
                &none
            ),
            Some(CLIAgent::Claude)
        );
        assert_eq!(
            CLIAgent::detect_from_command_with("CLAUDE", &none),
            Some(CLIAgent::Claude)
        );
        assert_eq!(
            CLIAgent::detect_from_command_with(r#"& "C:\tools\codex.exe""#, &none),
            Some(CLIAgent::Codex)
        );
        assert_eq!(
            CLIAgent::detect_from_command_with(
                r"node C:\x\node_modules\@anthropic-ai\claude-code\cli.js",
                &none
            ),
            Some(CLIAgent::Claude)
        );
        assert_eq!(
            CLIAgent::detect_from_command_with("npx.cmd @google/gemini-cli", &none),
            Some(CLIAgent::Gemini)
        );
        assert_eq!(
            CLIAgent::detect_from_command_with("notepad claude.txt", &none),
            None
        );
        assert_eq!(
            CLIAgent::detect_from_command_with("cat codex.md", &none),
            None
        );
        assert_eq!(CLIAgent::detect_from_command_with("", &none), None);
        let custom: HashMap<String, String> = [("cc".to_string(), "claude".to_string())].into();
        assert_eq!(
            CLIAgent::detect_from_command_with("cc -c", &custom),
            Some(CLIAgent::Claude)
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

        assert_eq!(parse_agent_event(b"777;notify;Build;done"), None);
        assert_eq!(
            parse_agent_event(br#"777;notify;tty7://cli-agent;{"event":"quantum-leap"}"#),
            None
        );
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
            cwd: None,
        };

        s.apply_event(&ev(AgentEventKind::SessionStart, None, Some("sid-1")));
        assert_eq!(s.status, AgentStatus::Idle);
        assert_eq!(s.session_id.as_deref(), Some("sid-1"));
        assert!(s.rich);

        s.apply_event(&ev(AgentEventKind::PromptSubmit, None, None));
        assert_eq!(s.status, AgentStatus::Working);

        s.apply_event(&ev(
            AgentEventKind::Notification,
            Some("Claude needs your permission"),
            None,
        ));
        assert_eq!(s.status, AgentStatus::Waiting);
        assert!(s.message.as_deref().unwrap().contains("permission"));

        s.apply_event(&ev(AgentEventKind::ToolComplete, None, None));
        assert_eq!(s.status, AgentStatus::Working);
        assert_eq!(s.message, None, "the stale permission prompt is cleared");

        s.apply_event(&ev(AgentEventKind::ToolComplete, None, None));
        assert_eq!(s.status, AgentStatus::Working);

        s.apply_event(&ev(AgentEventKind::Stop, None, None));
        assert_eq!(s.status, AgentStatus::Done);

        s.apply_event(&ev(AgentEventKind::ToolComplete, None, None));
        assert_eq!(s.status, AgentStatus::Done);

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

        s.apply_event(&ev(AgentEventKind::SessionEnd, None, None));
        assert_eq!(s.status, AgentStatus::Idle);
        assert_eq!(s.session_id.as_deref(), Some("sid-1"));
    }

    #[test]
    fn tool_completions_count_even_when_the_status_holds_still() {
        let ev = |kind| AgentEvent {
            agent: Some(CLIAgent::Claude),
            kind,
            session_id: None,
            message: None,
            cwd: None,
        };

        let mut s = AgentSessionState::default();
        s.apply_event(&ev(AgentEventKind::PromptSubmit));
        assert_eq!(s.activity, 0, "a turn starting is not tool activity");

        for n in 1..=3 {
            s.apply_event(&ev(AgentEventKind::ToolComplete));
            assert_eq!(s.status, AgentStatus::Working, "the status holds still…");
            assert_eq!(s.activity, n, "…while the counter is what moves");
        }

        s.apply_event(&ev(AgentEventKind::Stop));
        s.apply_event(&ev(AgentEventKind::ToolComplete));
        assert_eq!(
            s.status,
            AgentStatus::Done,
            "and still doesn't resurrect the turn"
        );
        assert_eq!(s.activity, 4);

        s.apply_event(&ev(AgentEventKind::SessionEnd));
        assert_eq!(s.activity, 4);
    }

    #[test]
    fn session_state_tracks_and_releases_the_agent_cwd() {
        use std::path::PathBuf;

        let ev = |kind, cwd: Option<&str>| AgentEvent {
            agent: Some(CLIAgent::Claude),
            kind,
            session_id: None,
            message: None,
            cwd: cwd.map(PathBuf::from),
        };

        let mut s = AgentSessionState::default();
        s.apply_event(&ev(AgentEventKind::SessionStart, Some("/repo")));
        assert_eq!(s.cwd.as_deref(), Some(std::path::Path::new("/repo")));

        s.apply_event(&ev(
            AgentEventKind::ToolComplete,
            Some("/repo/.claude/worktrees/fix-x"),
        ));
        assert_eq!(
            s.cwd.as_deref(),
            Some(std::path::Path::new("/repo/.claude/worktrees/fix-x"))
        );

        s.apply_event(&ev(AgentEventKind::Stop, None));
        assert_eq!(
            s.cwd.as_deref(),
            Some(std::path::Path::new("/repo/.claude/worktrees/fix-x"))
        );

        s.apply_event(&ev(AgentEventKind::SessionEnd, None));
        assert_eq!(s.cwd, None, "session end releases the cwd claim");
    }

    #[test]
    fn resume_commands_are_shell_safe() {
        assert_eq!(
            CLIAgent::Claude.resume_command("abc-123", None).as_deref(),
            Some("claude --resume abc-123")
        );
        assert_eq!(
            CLIAgent::Codex.resume_command("th_read.9", None).as_deref(),
            Some("codex resume th_read.9")
        );
        assert_eq!(
            CLIAgent::Pi
                .resume_command("0199c3f2-1b0e-7c3a-9f21-6d4b8e2a5c17", None)
                .as_deref(),
            Some("pi --session 0199c3f2-1b0e-7c3a-9f21-6d4b8e2a5c17")
        );
        assert_eq!(CLIAgent::Aider.resume_command("abc", None), None);
        assert_eq!(CLIAgent::Claude.resume_command("abc; rm -rf /", None), None);
        assert_eq!(CLIAgent::Claude.resume_command("$(boom)", None), None);
        assert_eq!(CLIAgent::Claude.resume_command("", None), None);
    }

    #[test]
    fn resume_carries_launch_flags() {
        let argv = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        assert_eq!(
            CLIAgent::Claude
                .resume_command(
                    "abc-123",
                    Some(&argv(&["claude", "--dangerously-skip-permissions"]))
                )
                .as_deref(),
            Some("claude --dangerously-skip-permissions --resume abc-123")
        );
        assert_eq!(
            CLIAgent::Claude
                .resume_command("abc", Some(&argv(&["claude", "--model", "opus"])))
                .as_deref(),
            Some("claude --model opus --resume abc")
        );
        assert_eq!(
            CLIAgent::Claude
                .resume_command(
                    "abc",
                    Some(&argv(&[
                        "node",
                        "/x/node_modules/@anthropic-ai/claude-code/cli.js",
                        "--dangerously-skip-permissions",
                    ]))
                )
                .as_deref(),
            Some("claude --dangerously-skip-permissions --resume abc")
        );
        assert_eq!(
            CLIAgent::Claude
                .resume_command(
                    "new-id",
                    Some(&argv(&["claude", "--resume", "old-id", "--model", "opus"]))
                )
                .as_deref(),
            Some("claude --model opus --resume new-id")
        );
        assert_eq!(
            CLIAgent::Codex
                .resume_command("id-1", Some(&argv(&["codex", "--yolo"])))
                .as_deref(),
            Some("codex resume id-1 --yolo")
        );
        assert_eq!(
            CLIAgent::Codex
                .resume_command("id-2", Some(&argv(&["codex", "resume", "id-1", "--yolo"])))
                .as_deref(),
            Some("codex resume id-2 --yolo")
        );
        assert_eq!(
            CLIAgent::Claude
                .resume_command(
                    "abc",
                    Some(&argv(&["claude", "--allowedTools", "Bash(git:*)"]))
                )
                .as_deref(),
            Some("claude --resume abc")
        );
        assert_eq!(
            CLIAgent::Claude
                .resume_command("abc", Some(&argv(&["claude", "fix-the-bug"])))
                .as_deref(),
            Some("claude --resume abc")
        );
        assert_eq!(
            CLIAgent::Claude
                .resume_command(
                    "abc",
                    Some(&argv(&[
                        "CLAUDE_CONFIG_DIR=/opt/claude",
                        "claude",
                        "--dangerously-skip-permissions",
                    ]))
                )
                .as_deref(),
            Some("claude --dangerously-skip-permissions --resume abc")
        );
        assert_eq!(
            CLIAgent::Claude
                .resume_command(
                    "abc",
                    Some(&argv(&["claude", "--model", "opus", "review", "this"]))
                )
                .as_deref(),
            Some("claude --resume abc")
        );
        assert_eq!(
            CLIAgent::Codex
                .resume_command(
                    "id-3",
                    Some(&argv(&["codex", "resume", "--last", "--yolo"]))
                )
                .as_deref(),
            Some("codex resume id-3 --yolo")
        );
        assert_eq!(
            CLIAgent::Pi
                .resume_command("id-a", Some(&argv(&["pi", "--model", "opus"])))
                .as_deref(),
            Some("pi --model opus --session id-a")
        );
        assert_eq!(
            CLIAgent::Pi
                .resume_command(
                    "id-b",
                    Some(&argv(&[
                        "pi",
                        "--session",
                        "old-id",
                        "--fork",
                        "old",
                        "-c",
                        "--model",
                        "opus"
                    ]))
                )
                .as_deref(),
            Some("pi --model opus --session id-b")
        );
        assert_eq!(
            CLIAgent::Pi.resume_command(
                "id-x",
                Some(&argv(&["pi", "--no-session", "--model", "opus"]))
            ),
            None
        );
        assert_eq!(
            CLIAgent::Pi
                .resume_command(
                    "id-c",
                    Some(&argv(&[
                        "pi",
                        "--session-dir",
                        "/w/.sessions",
                        "--fork",
                        "old"
                    ]))
                )
                .as_deref(),
            Some("pi --session-dir /w/.sessions --session id-c")
        );
        assert_eq!(
            CLIAgent::Claude
                .resume_command(
                    "abc",
                    Some(&argv(&["cc", "--dangerously-skip-permissions"]))
                )
                .as_deref(),
            Some("claude --resume abc")
        );
        assert_eq!(
            CLIAgent::Amp
                .resume_command("t-1", Some(&argv(&["amp", "--dangerously-allow-all"])))
                .as_deref(),
            Some("amp threads continue t-1 --dangerously-allow-all")
        );
        assert_eq!(
            CLIAgent::Amp
                .resume_command("t-2", Some(&argv(&["amp", "threads", "continue", "t-1"])))
                .as_deref(),
            Some("amp threads continue t-2")
        );
        assert_eq!(
            CLIAgent::Copilot
                .resume_command(
                    "s-9",
                    Some(&argv(&["copilot", "--resume", "s-1", "--allow-all-tools"]))
                )
                .as_deref(),
            Some("copilot --allow-all-tools --resume s-9")
        );
        assert_eq!(
            CLIAgent::Copilot.resume_command("s-9", None).as_deref(),
            Some("copilot --resume s-9")
        );
        assert_eq!(
            CLIAgent::Grok
                .resume_command("g-2", Some(&argv(&["grok", "--model", "grok-code"])))
                .as_deref(),
            Some("grok --model grok-code --resume g-2")
        );
        assert_eq!(
            CLIAgent::Grok
                .resume_command(
                    "g-2",
                    Some(&argv(&["grok", "--resume", "g-1", "--fork-session"]))
                )
                .as_deref(),
            Some("grok --resume g-2")
        );
        assert_eq!(
            CLIAgent::Grok
                .resume_command(
                    "g-3",
                    Some(&argv(&["grok", "-w", "--worktree-ref", "main", "--yolo"]))
                )
                .as_deref(),
            Some("grok --yolo --resume g-3")
        );
    }

    #[test]
    fn fork_commands_cover_exactly_the_agents_with_a_verified_fork() {
        assert_eq!(
            CLIAgent::Codex.fork_command("abc-123", None).as_deref(),
            Some("codex fork abc-123")
        );
        assert_eq!(
            CLIAgent::Claude.fork_command("abc-123", None).as_deref(),
            Some("claude --resume abc-123 --fork-session")
        );
        assert_eq!(
            CLIAgent::Grok.fork_command("g-1", None).as_deref(),
            Some("grok --resume g-1 --fork-session")
        );
        assert_eq!(
            CLIAgent::OpenCode.fork_command("s-1", None).as_deref(),
            Some("opencode --session s-1 --fork")
        );

        for agent in [
            CLIAgent::Gemini,
            CLIAgent::Copilot,
            CLIAgent::Cursor,
            CLIAgent::Amp,
            CLIAgent::Aider,
            CLIAgent::Qwen,
        ] {
            assert_eq!(
                agent.fork_command("abc", None),
                None,
                "{} must not claim a fork command",
                agent.slug()
            );
        }

        for agent in CLIAgent::ALL {
            assert_eq!(
                agent.fork_label().is_some(),
                agent.fork_command("abc", None).is_some(),
                "{}: fork_label and fork_command disagree",
                agent.slug()
            );
        }
    }

    #[test]
    fn fork_commands_are_shell_safe() {
        for id in ["abc; rm -rf /", "$(boom)", "", "a b"] {
            assert_eq!(
                CLIAgent::Codex.fork_command(id, None),
                None,
                "codex accepted a non-token id: {id:?}"
            );
            assert_eq!(CLIAgent::Claude.fork_command(id, None), None);
        }
    }

    #[test]
    fn fork_carries_launch_flags_and_sheds_stale_session_targeting() {
        let argv = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        assert_eq!(
            CLIAgent::Codex
                .fork_command("id-1", Some(&argv(&["codex", "--yolo"])))
                .as_deref(),
            Some("codex fork id-1 --yolo")
        );
        assert_eq!(
            CLIAgent::Claude
                .fork_command(
                    "abc",
                    Some(&argv(&["claude", "--dangerously-skip-permissions"]))
                )
                .as_deref(),
            Some("claude --dangerously-skip-permissions --resume abc --fork-session")
        );

        assert_eq!(
            CLIAgent::Codex
                .fork_command("id-2", Some(&argv(&["codex", "fork", "id-1", "--yolo"])))
                .as_deref(),
            Some("codex fork id-2 --yolo")
        );
        assert_eq!(
            CLIAgent::Claude
                .fork_command(
                    "new",
                    Some(&argv(&["claude", "--resume", "old", "--fork-session"]))
                )
                .as_deref(),
            Some("claude --resume new --fork-session")
        );
        assert_eq!(
            CLIAgent::Grok
                .fork_command(
                    "g-2",
                    Some(&argv(&["grok", "--resume", "g-1", "--fork-session"]))
                )
                .as_deref(),
            Some("grok --resume g-2 --fork-session")
        );
        assert_eq!(
            CLIAgent::OpenCode
                .fork_command(
                    "s-2",
                    Some(&argv(&["opencode", "--session", "s-1", "--fork"]))
                )
                .as_deref(),
            Some("opencode --session s-2 --fork")
        );

        assert_eq!(
            CLIAgent::Codex
                .resume_command("id-2", Some(&argv(&["codex", "fork", "id-1", "--yolo"])))
                .as_deref(),
            Some("codex resume id-2 --yolo")
        );
        assert_eq!(
            CLIAgent::Claude
                .resume_command(
                    "new",
                    Some(&argv(&["claude", "--resume", "old", "--fork-session"]))
                )
                .as_deref(),
            Some("claude --resume new")
        );
        assert_eq!(
            CLIAgent::OpenCode
                .resume_command(
                    "s-2",
                    Some(&argv(&["opencode", "--session", "s-1", "--fork"]))
                )
                .as_deref(),
            Some("opencode --session s-2")
        );
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
        assert_eq!(
            serde_json::to_string(&AgentStatus::Waiting).unwrap(),
            "\"waiting\""
        );
    }
}

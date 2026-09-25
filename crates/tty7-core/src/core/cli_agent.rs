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
    OhMyPi,
    Kimi,
    // Keep new variants at the end: daemon messages serialize this enum and
    // moving an existing discriminant would break mixed-version clients.
    TraeCode,
    QoderCLI,
    Crush,
    CodeBuddy,
}

impl CLIAgent {
    pub const ALL: [CLIAgent; 23] = [
        CLIAgent::Claude,
        CLIAgent::Codex,
        CLIAgent::TraeCode,
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
        CLIAgent::OhMyPi,
        CLIAgent::Kimi,
        CLIAgent::QoderCLI,
        CLIAgent::Crush,
        CLIAgent::CodeBuddy,
    ];

    fn aliases(self) -> &'static [&'static str] {
        match self {
            CLIAgent::Claude => &["claude", "claude-code"],
            CLIAgent::Codex => &["codex", "codex-cli"],
            CLIAgent::TraeCode => &["traecli", "traex"],
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
            // `agy` only. The `antigravity` binary the IDE installs is a
            // launcher shim in the shape of VS Code's `code`, not the terminal
            // agent — and the name also collides with `python3 -m antigravity`,
            // the standard way to trigger Python's own easter egg.
            CLIAgent::Antigravity => &["agy"],
            CLIAgent::Grok => &["grok"],
            CLIAgent::Qwen => &["qwen", "qwen-code"],
            // Oh My Pi is a fork of Pi, but it ships one binary of its own and
            // never installs a `pi`, so the two names stay disjoint.
            CLIAgent::OhMyPi => &["omp"],
            // Both the standalone Kimi Code CLI and the legacy open-source
            // kimi-cli install a `kimi` — same vendor, same brand, so one
            // detection covers them. Only the standalone one has hooks.
            CLIAgent::Kimi => &["kimi", "kimi-code"],
            // The npm package installs two binaries and `qoder` is the one the
            // documentation tells people to run: it dispatches to the CLI for a
            // bare invocation, a flag, or a prompt, and only hands off to the
            // IDE for `ide`/`chat`/`serve-web`/`tunnel` or a path that exists.
            // Detecting only `qodercli` would miss every session started the
            // documented way, since the dispatcher is what the pty sees. An IDE
            // launch is the cost: it wears the CLI's avatar for as long as the
            // launcher takes to exit.
            CLIAgent::QoderCLI => &["qoder", "qodercli"],
            // Charm's terminal agent. One binary, and the name on `PATH` is
            // the one it starts as.
            CLIAgent::Crush => &["crush"],
            // Tencent's terminal agent. The npm package installs `codebuddy`,
            // `codebuddy-code` and the short `cbc`, all pointing at one script.
            // `cbc` also names the COIN-OR solver; a solver run wearing the
            // avatar until it exits is the cost of catching the short name.
            CLIAgent::CodeBuddy => &["codebuddy", "codebuddy-code", "cbc"],
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            CLIAgent::Claude => "claude",
            CLIAgent::Codex => "codex",
            CLIAgent::TraeCode => "traecli",
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
            CLIAgent::OhMyPi => "omp",
            CLIAgent::Kimi => "kimi",
            CLIAgent::QoderCLI => "qodercli",
            CLIAgent::Crush => "crush",
            CLIAgent::CodeBuddy => "codebuddy",
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
            CLIAgent::TraeCode => "TraeCode",
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
            CLIAgent::OhMyPi => "Oh My Pi",
            CLIAgent::Kimi => "Kimi Code",
            CLIAgent::QoderCLI => "Qoder CLI",
            CLIAgent::Crush => "Crush",
            CLIAgent::CodeBuddy => "CodeBuddy",
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
            CLIAgent::TraeCode => Some(format!("traecli resume {session_id}{flags}")),
            CLIAgent::Gemini => Some(format!("gemini{flags} --resume {session_id}")),
            CLIAgent::OpenCode => Some(format!("opencode{flags} --session {session_id}")),
            CLIAgent::Amp => Some(format!("amp threads continue {session_id}{flags}")),
            CLIAgent::Auggie => Some(format!("auggie{flags} --resume {session_id}")),
            CLIAgent::Hermes => Some(format!("hermes chat{flags} --resume {session_id}")),
            CLIAgent::Qwen => Some(format!("qwen{flags} --resume {session_id}")),
            CLIAgent::Goose => Some(format!(
                "goose session{flags} --resume --session-id {session_id}"
            )),
            CLIAgent::Vibe => Some(format!("vibe{flags} --resume {session_id}")),
            CLIAgent::Antigravity => Some(format!("agy{flags} --conversation {session_id}")),
            CLIAgent::Cursor => Some(format!("cursor-agent{flags} --resume {session_id}")),
            CLIAgent::Droid => Some(format!("droid{flags} --resume {session_id}")),
            CLIAgent::Copilot => Some(format!("copilot{flags} --resume {session_id}")),
            CLIAgent::Grok => Some(format!("grok{flags} --resume {session_id}")),
            CLIAgent::QoderCLI => Some(format!("qodercli{flags} --resume {session_id}")),
            CLIAgent::Pi => Some(format!("pi{flags} --session {session_id}")),
            CLIAgent::OhMyPi => Some(format!("omp{flags} --resume {session_id}")),
            CLIAgent::Kimi => Some(format!("kimi{flags} --session {session_id}")),
            CLIAgent::Crush => Some(format!("crush{flags} --session {session_id}")),
            CLIAgent::CodeBuddy => Some(format!("codebuddy{flags} --resume {session_id}")),
            _ => None,
        }
    }

    fn opts_out_of_sessions(self, argv: &[String]) -> bool {
        let ephemeral: &[&str] = match self {
            CLIAgent::Pi | CLIAgent::OhMyPi => &["--no-session"],
            // "Do not save conversation history" — nothing is persisted, so
            // there is no session left to resume from.
            CLIAgent::Auggie => &["--dont-save-session"],
            // "If false, chat history is not saved and --continue/--resume
            // will not work" — the yargs negation of `--chat-recording`.
            CLIAgent::Qwen => &["--no-chat-recording"],
            // Print mode still emits a session id in hooks when persistence
            // is disabled, but there is no saved conversation to reopen.
            CLIAgent::QoderCLI | CLIAgent::CodeBuddy => &["--no-session-persistence"],
            _ => &[],
        };
        argv.iter().any(|t| ephemeral.contains(&t.as_str()))
    }

    pub fn fork_command(self, session_id: &str, launch_argv: Option<&[String]>) -> Option<String> {
        if launch_argv.is_some_and(|argv| self.opts_out_of_sessions(argv)) {
            return None;
        }
        let flags = self.session_command_flags(session_id, launch_argv)?;
        match self {
            CLIAgent::Codex => Some(format!("codex fork {session_id}{flags}")),
            CLIAgent::TraeCode => Some(format!("traecli fork {session_id}{flags}")),
            CLIAgent::Claude => Some(format!(
                "claude{flags} --resume {session_id} --fork-session"
            )),
            CLIAgent::Grok => Some(format!("grok{flags} --resume {session_id} --fork-session")),
            CLIAgent::QoderCLI => Some(format!(
                "qodercli{flags} --resume {session_id} --fork-session"
            )),
            CLIAgent::CodeBuddy => Some(format!(
                "codebuddy{flags} --resume {session_id} --fork-session"
            )),
            CLIAgent::OpenCode => Some(format!("opencode{flags} --session {session_id} --fork")),
            CLIAgent::OhMyPi => Some(format!("omp{flags} --fork {session_id}")),
            // Droid forks with a standalone flag rather than resume-plus-a-switch.
            CLIAgent::Droid => Some(format!("droid{flags} --fork {session_id}")),
            // `fork` is missing from `amp threads --help`, but the subcommand is
            // real — `amp threads fork --help` prints its own usage.
            CLIAgent::Amp => Some(format!("amp threads fork {session_id}{flags}")),
            CLIAgent::Qwen => Some(format!("qwen{flags} --resume {session_id} --fork-session")),
            // Goose forks by adding a switch to the same resume invocation.
            CLIAgent::Goose => Some(format!(
                "goose session{flags} --resume --fork --session-id {session_id}"
            )),
            _ => None,
        }
    }

    pub fn fork_label(self) -> Option<&'static str> {
        match self {
            CLIAgent::Claude
            | CLIAgent::Codex
            | CLIAgent::TraeCode
            | CLIAgent::Grok
            | CLIAgent::OpenCode
            | CLIAgent::OhMyPi
            | CLIAgent::Droid
            | CLIAgent::Amp
            | CLIAgent::Qwen
            | CLIAgent::Goose
            | CLIAgent::QoderCLI
            | CLIAgent::CodeBuddy => Some("Fork Session"),
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
        let tail = self.session_free_tail(argv)?;
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

    /// What follows the agent's own name in `argv`, with everything that names
    /// a session taken out: the resume/fork subcommands and the flags listed as
    /// stale for this agent, each with its value. `None` when no token in
    /// `argv` names this agent — a wrapper, where there is no telling which
    /// arguments are the agent's.
    fn session_free_tail(self, argv: &[String]) -> Option<Vec<&str>> {
        let names_self = |token: &str| {
            token.split(['/', '\\']).any(|seg| {
                CLIAgent::match_token(&base_stem(seg).to_ascii_lowercase()) == Some(self)
            })
        };
        let argv = &argv[argv.iter().take_while(|t| is_env_assignment(t)).count()..];
        let named = argv.iter().position(|t| names_self(t))?;
        let mut tail: Vec<&str> = argv[named + 1..].iter().map(String::as_str).collect();

        if matches!(self, CLIAgent::Codex | CLIAgent::TraeCode)
            && matches!(tail.first(), Some(&"resume") | Some(&"fork"))
        {
            tail.remove(0);
            if tail.first().is_some_and(|t| !t.starts_with('-')) {
                tail.remove(0);
            }
        }

        // Agents that reach their session through subcommands leave `stale`
        // nothing to drop — `amp threads continue <id>` names the thread with a
        // positional argument, and `goose session --resume` hides the flags one
        // level down. Either way the prefix has to come off here, because the
        // "a bare token must follow a flag" check below would otherwise reject
        // the tail wholesale and take every launch flag down with it. The
        // replacement command spells the subcommand out again itself.
        let (groups, verbs): (&[&str], &[&str]) = match self {
            CLIAgent::Amp => (
                &["threads", "t"],
                &["continue", "c", "fork", "f", "handoff", "h"],
            ),
            CLIAgent::Auggie => (&["session"], &["resume", "continue"]),
            CLIAgent::Goose => (&["session", "s"], &[]),
            CLIAgent::Hermes => (&["chat"], &[]),
            _ => (&[], &[]),
        };
        if tail.first().is_some_and(|t| groups.contains(t)) {
            tail.remove(0);
            if tail.first().is_some_and(|t| verbs.contains(t)) {
                tail.remove(0);
                if tail.first().is_some_and(|t| !t.starts_with('-')) {
                    tail.remove(0);
                }
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
            // `--session-id` and `--session-file` name a session too, and Gemini
            // rejects them outright alongside `--resume`.
            CLIAgent::Gemini => &["--resume", "-r", "--session-id", "--session-file"],
            CLIAgent::Cursor => &["--resume", "-r", "--continue"],
            CLIAgent::Copilot | CLIAgent::Auggie | CLIAgent::Hermes => {
                &["--resume", "-r", "--continue", "-c"]
            }
            // `--session-id` names a *new* session and Qwen rejects it
            // alongside `--resume`, so it is as stale as the resume flags.
            CLIAgent::Qwen => &[
                "--resume",
                "-r",
                "--continue",
                "-c",
                "--fork-session",
                "--session-id",
            ],
            CLIAgent::Droid => &["--resume", "-r", "--fork", "--session-id", "-s"],
            // `--session-id`/`--id`, `-n`/`--name` and the legacy `--path` are
            // one mutually-exclusive clap group in Goose; any of them surviving
            // next to the `--session-id` this command appends is a parse error.
            CLIAgent::Goose => &[
                "--resume",
                "-r",
                "--fork",
                "--session-id",
                "--id",
                "--name",
                "-n",
                "--path",
            ],
            CLIAgent::Vibe => &["--resume", "--continue", "-c"],
            CLIAgent::Antigravity => &["--conversation", "--continue", "-c"],
            CLIAgent::OpenCode => &["--session", "-s", "--continue", "-c", "--fork"],
            CLIAgent::Codex => &["--last"],
            CLIAgent::TraeCode => &["--last", "--resume", "--session-id"],
            CLIAgent::Pi => &[
                "--session",
                "--session-id",
                "--fork",
                "--resume",
                "-r",
                "--continue",
                "-c",
            ],
            // `--resume`, `-r` and `--session` are three spellings of one flag
            // in Oh My Pi; `--session-dir` is a different one and survives.
            CLIAgent::OhMyPi => &["--resume", "-r", "--session", "--fork", "--continue", "-c"],
            // `--resume`/`-r` is Kimi's hidden alias for `--session`/`-S`.
            // `--agent`/`--agent-file` bind the main agent at session creation
            // and Kimi rejects either next to `--session` outright; resuming
            // restores the bound agent by itself, so replaying them would only
            // turn a working resume into a startup error.
            CLIAgent::Kimi => &[
                "--session",
                "-S",
                "--resume",
                "-r",
                "--continue",
                "-c",
                "--agent",
                "--agent-file",
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
            // `--resume`/`-r` restores a past session and `--continue`/`-c` the
            // most recent one; `--session-id` is a third spelling of the same
            // thing. All three clash with the `--resume {id}` this command
            // appends, and `--fork-session` is the flag the fork variant
            // appends itself. `--worktree` would create or switch trees again;
            // Qoder's `-w` means `--cwd` and must survive.
            CLIAgent::QoderCLI => &[
                "--resume",
                "-r",
                "--continue",
                "-c",
                "--session-id",
                "--fork-session",
                "--worktree",
            ],
            // `--session`/`-s` names the conversation to restore and
            // `--continue`/`-C` the most recent one; both clash with the
            // `--session {id}` this command appends. `-c` is *not* in that
            // group here — Crush spells `--cwd` with it, and it must survive.
            CLIAgent::Crush => &["--session", "-s", "--continue", "-C"],
            // CodeBuddy keeps Claude Code's session flags. `--resume-session-at`
            // cuts a resumed conversation short and only makes sense next to
            // the resume it came with. Unlike Qoder, `-w` *is* `--worktree`
            // here, and replaying either would branch off a fresh tree.
            CLIAgent::CodeBuddy => &[
                "--resume",
                "-r",
                "--continue",
                "-c",
                "--session-id",
                "--fork-session",
                "--resume-session-at",
                "--worktree",
                "-w",
                "--worktree-branch",
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
        Some(tail)
    }

    pub fn accent_rgb(self) -> u32 {
        match self {
            CLIAgent::Claude => 0xD97757,
            CLIAgent::Codex => 0x000000,
            // The brand mark carries its own green foreground on a black
            // field, so the surrounding tab avatar needs to stay black too.
            CLIAgent::TraeCode => 0x000000,
            CLIAgent::Gemini => 0x4285F4,
            CLIAgent::Aider => 0x14B014,
            CLIAgent::Amp => 0xF34E3F,
            CLIAgent::OpenCode => 0x6E56CF,
            CLIAgent::Copilot => 0x8957E5,
            CLIAgent::Cursor => 0x9AA0A6,
            CLIAgent::Goose => 0x3ECC5F,
            CLIAgent::Droid => 0xEF6F2E,
            CLIAgent::Pi => 0x0EA5E9,
            CLIAgent::Auggie => 0x16A34A,
            CLIAgent::Hermes => 0x8B5CF6,
            CLIAgent::Vibe => 0xFA520F,
            CLIAgent::Antigravity => 0x3186FF,
            CLIAgent::Grok => 0x000000,
            CLIAgent::Qwen => 0x6D44E8,
            CLIAgent::OhMyPi => 0xF97316,
            // The blue of the flame in Kimi's brand mark; the glyph itself is
            // black, which Codex and Grok already have covered.
            CLIAgent::Kimi => 0x027AFF,
            CLIAgent::QoderCLI => 0xFFFFFF,
            // The blue-violet field Charm ships the Crush heart on.
            CLIAgent::Crush => 0x6B50FF,
            // The near-black field CodeBuddy's own app icon sits on.
            CLIAgent::CodeBuddy => 0x1F1F1F,
        }
    }

    /// The colour the agent's mark is drawn in.
    ///
    /// SVG assets render as a single-colour mask, so whatever `fill` the file
    /// carries is thrown away and the colour has to come from here. It lives
    /// next to [`Self::accent_rgb`] rather than at either draw site because
    /// there are two of those — the tab avatar and the tray icon — and a mark
    /// that answers differently depending on which one is asking is the same
    /// agent wearing two faces.
    ///
    /// White for every mark that is a silhouette sitting on its brand colour.
    /// TraeCode is the one whose mark is the coloured half of the pair, on an
    /// accent that is deliberately black.
    pub fn icon_rgb(self) -> u32 {
        match self {
            CLIAgent::TraeCode => 0x32F08C,
            CLIAgent::QoderCLI => 0x000000,
            _ => 0xFFFFFF,
        }
    }

    pub fn icon_path(self) -> &'static str {
        match self {
            CLIAgent::Claude => "icons/agents/claude.svg",
            CLIAgent::Codex => "icons/agents/codex.svg",
            CLIAgent::TraeCode => "icons/agents/traecli.svg",
            CLIAgent::Gemini => "icons/agents/gemini.svg",
            CLIAgent::Amp => "icons/agents/amp.svg",
            CLIAgent::OpenCode => "icons/agents/opencode.svg",
            CLIAgent::Copilot => "icons/agents/copilot.svg",
            CLIAgent::Cursor => "icons/agents/cursor.svg",
            CLIAgent::Goose => "icons/agents/goose.svg",
            CLIAgent::Droid => "icons/agents/droid.svg",
            CLIAgent::Grok => "icons/agents/grok.svg",
            CLIAgent::Pi => "icons/agents/pi.svg",
            CLIAgent::OhMyPi => "icons/agents/omp.svg",
            CLIAgent::Qwen => "icons/agents/qwen.svg",
            CLIAgent::Kimi => "icons/agents/kimi.svg",
            CLIAgent::QoderCLI => "icons/agents/qodercli.svg",
            CLIAgent::Crush => "icons/agents/crush.svg",
            CLIAgent::CodeBuddy => "icons/agents/codebuddy.svg",
            CLIAgent::Aider
            | CLIAgent::Auggie
            | CLIAgent::Hermes
            | CLIAgent::Vibe
            | CLIAgent::Antigravity => "icons/bot.svg",
        }
    }

    fn match_token(token: &str) -> Option<CLIAgent> {
        CLIAgent::ALL
            .into_iter()
            .find(|a| a.aliases().contains(&token))
    }

    /// The program a launch with no override runs: the first name the agent
    /// is detected by, which is the one its installer puts on `PATH`.
    pub fn binary(self) -> &'static str {
        self.aliases()[0]
    }

    /// The command line a quick launch types for this agent: the user's
    /// `agent_launch` entry when there is one, the bare binary otherwise.
    ///
    /// Keys are matched the way [`CLIAgent::from_slug`] reads them, so a
    /// hand-written `"Claude"` still counts. A blank entry is no override.
    pub fn launch_command(self, overrides: &HashMap<String, String>) -> String {
        overrides
            .iter()
            .find(|(slug, _)| CLIAgent::from_slug(slug) == Some(self))
            .map(|(_, line)| line.trim())
            .filter(|line| !line.is_empty())
            .map_or_else(|| self.binary().to_string(), str::to_string)
    }

    /// `argv` as a command line worth launching this agent with again: its
    /// binary followed by the flags it ran with, minus everything that names
    /// one particular session (a resume, a fork, a session id) and minus
    /// positional arguments such as a prompt, which belong to that one run.
    ///
    /// A run that went through something that does not name the agent — a
    /// wrapper script mapped by `agent_commands` — cannot be taken apart, so
    /// it comes back whole: it is what was typed, and it launched the agent.
    pub fn launch_argv_for_default(self, argv: &[String]) -> Option<Vec<String>> {
        let env = argv.iter().take_while(|t| is_env_assignment(t)).count();
        if argv.len() == env {
            return None;
        }
        let Some(tail) = self.session_free_tail(argv) else {
            return Some(argv.to_vec());
        };
        let mut out: Vec<String> = argv[..env].to_vec();
        out.push(self.binary().to_string());
        let mut takes_value = false;
        for t in tail {
            let is_flag = t.starts_with('-') && t.len() > 1;
            if is_flag {
                out.push(t.to_string());
                takes_value = !t.contains('=');
            } else if takes_value {
                out.push(t.to_string());
                takes_value = false;
            }
        }
        Some(out)
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

        // A wrapper that is a script runs as its interpreter, so the name the
        // user mapped is the script's, one token in: `bash ~/bin/cc`,
        // `node ~/bin/cc.js`. Only the mapped names are looked for there — the
        // built-in ones already have the scan below.
        if (is_interpreter(launcher_stem) || is_shell(launcher_stem))
            && let Some(agent) = rest
                .clone()
                .find(|a| !a.starts_with('-') && !is_env_assignment(a))
                .and_then(|script| custom.get(&base_stem(script).to_ascii_lowercase()))
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
        let argv: Vec<String> = command_argv(command)
            .iter()
            .map(|t| t.to_ascii_lowercase())
            .collect();
        Self::detect_from_argv_with(&argv, custom)
    }
}

/// Splits a shell-integration command capture into argv tokens, preserving
/// case so the result can serve as `launch_argv` for flag replay on resume.
/// Quoted arguments containing spaces come out split; `replay_flags` rejects
/// such tokens rather than replaying them wrong.
pub fn command_argv(command: &str) -> Vec<String> {
    let mut argv: Vec<String> = command
        .split_whitespace()
        .map(|t| t.trim_matches(['"', '\'']).to_string())
        .filter(|t| !t.is_empty())
        .collect();
    if argv.first().is_some_and(|t| t == "&") {
        argv.remove(0);
    }
    argv
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

fn is_shell(stem: &str) -> bool {
    matches!(
        stem.to_ascii_lowercase().as_str(),
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish"
    )
}

/// The program a launch command line runs: its first word that is not an
/// environment assignment.
pub fn launch_program(command: &str) -> Option<String> {
    command_argv(command)
        .into_iter()
        .find(|t| !is_env_assignment(t))
}

/// The wrapper names `agent_launch` implies, in the shape `agent_commands`
/// takes: `{"claude": "cc --fast"}` means a `cc` in a pane is Claude Code.
///
/// This is how a pane opened by a quick launch through a wrapper is still
/// recognised without the user also writing an `agent_commands` entry: the
/// launch typed exactly this command, so its program is known to be that
/// agent. A program that is already an agent's own name needs no entry, and
/// an interpreter (`npx`, `node`, `env`, …) or a shell is never mapped
/// wholesale — `npx` is not Claude Code just because Claude Code is one of the
/// things it can run.
pub fn launch_aliases(overrides: &HashMap<String, String>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (slug, line) in overrides {
        let Some(agent) = CLIAgent::from_slug(slug) else {
            continue;
        };
        let Some(program) = launch_program(line) else {
            continue;
        };
        let stem = base_stem(&program).to_ascii_lowercase();
        if stem.is_empty()
            || CLIAgent::match_token(&stem).is_some()
            || is_interpreter(&stem)
            || is_shell(&stem)
        {
            continue;
        }
        out.insert(stem, agent.slug().to_string());
    }
    out
}

/// Whether `program` names an executable file, either directly (it has a
/// directory in it) or through one of the directories in `path`.
///
/// On Windows a bare name is also tried with the extensions an npm or winget
/// install leaves behind, since `claude` there is really `claude.cmd`.
pub fn program_on_path(program: &str, path: &std::ffi::OsStr) -> bool {
    let program = program.trim();
    if program.is_empty() {
        return false;
    }
    if program.contains(['/', '\\']) {
        // The launch is typed into a shell, which expands a leading `~/`.
        let home = program
            .strip_prefix("~/")
            .and_then(|rest| Some(std::path::PathBuf::from(std::env::var_os("HOME")?).join(rest)));
        return is_executable(home.as_deref().unwrap_or(std::path::Path::new(program)));
    }
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat", ".ps1"]
    } else {
        &[""]
    };
    std::env::split_paths(path).any(|dir| {
        exts.iter()
            .any(|ext| is_executable(&dir.join(format!("{program}{ext}"))))
    })
}

fn is_executable(path: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
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
    /// How many turns this session has finished: bumped each time it settles
    /// into `Done`. The status alone cannot tell a client that attaches to a
    /// `Done` pane whether that is the turn it already showed the reader or a
    /// later one that finished while nobody was watching (#870).
    #[serde(default)]
    pub turns: u64,
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
                if self.status != AgentStatus::Done {
                    self.turns = self.turns.wrapping_add(1);
                }
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
    /// What the user typed, on a `PromptSubmit` — already clamped to a label's
    /// worth of text by the hook that sent it, since this rides an OSC payload
    /// the tokenizer abandons rather than truncates past 8 KiB.
    ///
    /// Separate from `message`, which carries what the *agent* said and is
    /// deliberately cleared when a turn starts.
    pub prompt: Option<String>,
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
        #[serde(default)]
        prompt: Option<String>,
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
        prompt: nonempty(w.prompt),
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
            CLIAgent::detect_from_argv(&argv(&["/Users/me/.local/bin/traecli"])),
            Some(CLIAgent::TraeCode)
        );
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&["traex"])),
            Some(CLIAgent::TraeCode)
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

    /// The npm package installs `qoder` and `qodercli`, and the documentation
    /// tells people to run the first one. Both are `#!/usr/bin/env node`
    /// scripts, so what the pty carries is node plus the path to the shim —
    /// the dispatcher's own child, which is where the name `qodercli` appears
    /// on that path, is not the process group leader and is never read.
    #[test]
    fn qoder_is_detected_through_either_of_its_binaries() {
        for launcher in [
            "qoder",
            "qodercli",
            "/opt/homebrew/bin/qoder",
            "/opt/homebrew/bin/qodercli",
        ] {
            assert_eq!(
                CLIAgent::detect_from_argv(&argv(&["node", launcher])),
                Some(CLIAgent::QoderCLI),
                "on {launcher}"
            );
            assert_eq!(
                CLIAgent::detect_from_argv(&argv(&[launcher])),
                Some(CLIAgent::QoderCLI),
                "on {launcher}"
            );
        }
    }

    /// All three of CodeBuddy's npm bins are node scripts pointing at one file,
    /// so the pty sees node plus whichever shim name was typed.
    #[test]
    fn codebuddy_is_detected_through_each_of_its_binaries() {
        for launcher in [
            "codebuddy",
            "codebuddy-code",
            "cbc",
            "/opt/homebrew/bin/codebuddy",
            "/usr/local/bin/cbc",
        ] {
            assert_eq!(
                CLIAgent::detect_from_argv(&argv(&["node", launcher])),
                Some(CLIAgent::CodeBuddy),
                "on {launcher}"
            );
            assert_eq!(
                CLIAgent::detect_from_argv(&argv(&[launcher])),
                Some(CLIAgent::CodeBuddy),
                "on {launcher}"
            );
        }
        assert_eq!(
            CLIAgent::from_slug("codebuddy"),
            Some(CLIAgent::CodeBuddy),
            "hook events name the agent by this slug"
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
            ["aider", "auggie", "hermes", "vibe", "antigravity"]
        );
        assert!(
            !fallback.contains(&"omp"),
            "Oh My Pi ships its own mark and must not fall back"
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
            ("omp", CLIAgent::OhMyPi),
            ("/opt/homebrew/bin/omp", CLIAgent::OhMyPi),
            ("kimi", CLIAgent::Kimi),
            ("/usr/local/bin/kimi", CLIAgent::Kimi),
            ("crush", CLIAgent::Crush),
            ("/opt/homebrew/bin/crush", CLIAgent::Crush),
        ] {
            assert_eq!(CLIAgent::detect_from_argv(&argv(&[cmd])), Some(agent));
        }
    }

    /// Oh My Pi is a Pi fork, but it is its own binary with its own config
    /// directory and its own flags — one must never be detected as the other.
    #[test]
    fn oh_my_pi_and_pi_stay_distinct() {
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&["omp"])),
            Some(CLIAgent::OhMyPi)
        );
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&["pi"])),
            Some(CLIAgent::Pi)
        );
        assert_eq!(CLIAgent::from_slug("omp"), Some(CLIAgent::OhMyPi));
        assert_eq!(CLIAgent::from_slug("oh-my-pi"), None);
        assert_ne!(CLIAgent::Pi.icon_path(), CLIAgent::OhMyPi.icon_path());
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
    fn command_argv_preserves_case_for_flag_replay() {
        assert_eq!(
            command_argv("claude --dangerously-skip-permissions"),
            ["claude", "--dangerously-skip-permissions"]
        );
        assert_eq!(
            command_argv("claude --model Opus --resume Abc-123"),
            ["claude", "--model", "Opus", "--resume", "Abc-123"]
        );
        assert_eq!(
            command_argv(r#"& "C:\Tools\claude.exe" --continue"#),
            [r"C:\Tools\claude.exe", "--continue"]
        );
        assert_eq!(command_argv("  "), [""; 0]);
        assert_eq!(
            CLIAgent::Claude
                .resume_command(
                    "abc",
                    Some(&command_argv("claude --dangerously-skip-permissions"))
                )
                .as_deref(),
            Some("claude --dangerously-skip-permissions --resume abc"),
            "a shell-integration command capture must round-trip into a resume \
             command that keeps the launch flags"
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
            prompt: None,
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
            prompt: None,
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
    fn each_finished_turn_is_counted_once() {
        let ev = |kind| AgentEvent {
            agent: Some(CLIAgent::Claude),
            kind,
            session_id: None,
            message: None,
            cwd: None,
            prompt: None,
        };

        let mut s = AgentSessionState::default();
        s.apply_event(&ev(AgentEventKind::PromptSubmit));
        assert_eq!(s.turns, 0, "a turn starting has not finished anything");

        s.apply_event(&ev(AgentEventKind::Stop));
        assert_eq!(s.turns, 1);
        s.apply_event(&ev(AgentEventKind::Stop));
        assert_eq!(s.turns, 1, "a repeated stop is the same turn");
        s.apply_event(&ev(AgentEventKind::Notification));
        assert_eq!(s.turns, 1);

        s.apply_event(&ev(AgentEventKind::PromptSubmit));
        s.apply_event(&ev(AgentEventKind::Stop));
        assert_eq!(s.turns, 2, "a second turn is a second count");

        s.apply_event(&ev(AgentEventKind::SessionEnd));
        assert_eq!(s.turns, 2);
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
            prompt: None,
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
            CLIAgent::TraeCode
                .resume_command("019c-123", None)
                .as_deref(),
            Some("traecli resume 019c-123")
        );
        assert_eq!(
            CLIAgent::Pi
                .resume_command("0199c3f2-1b0e-7c3a-9f21-6d4b8e2a5c17", None)
                .as_deref(),
            Some("pi --session 0199c3f2-1b0e-7c3a-9f21-6d4b8e2a5c17")
        );
        assert_eq!(
            CLIAgent::Kimi.resume_command("abc-123", None).as_deref(),
            Some("kimi --session abc-123")
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
            CLIAgent::Kimi
                .resume_command(
                    "abc-123",
                    Some(&argv(&["kimi", "--session", "old", "--yolo"]))
                )
                .as_deref(),
            Some("kimi --yolo --session abc-123"),
            "a stale --session flag comes off before the new one goes on"
        );
        assert_eq!(
            CLIAgent::Kimi
                .resume_command("abc-123", Some(&argv(&["kimi", "--session=old", "--yolo"])))
                .as_deref(),
            Some("kimi --yolo --session abc-123"),
            "and so does the one-token spelling of it"
        );
        assert_eq!(
            CLIAgent::Kimi
                .resume_command(
                    "abc-123",
                    Some(&argv(&["kimi", "--resume", "--model", "kimi-k2"]))
                )
                .as_deref(),
            Some("kimi --model kimi-k2 --session abc-123"),
            "`--session` takes an optional id, so a bare one must not eat the flag after it"
        );
        assert_eq!(
            CLIAgent::Kimi
                .resume_command(
                    "abc-123",
                    Some(&argv(&["kimi", "--continue", "--model", "kimi-k2"]))
                )
                .as_deref(),
            Some("kimi --model kimi-k2 --session abc-123"),
            "`--continue` is mutually exclusive with `--session` and takes no value"
        );
        assert_eq!(
            CLIAgent::Kimi
                .resume_command(
                    "abc-123",
                    Some(&argv(&["kimi", "--agent", "reviewer", "--yolo"]))
                )
                .as_deref(),
            Some("kimi --yolo --session abc-123"),
            "Kimi rejects `--agent` next to `--session`, and resume rebinds the agent itself"
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
        assert_eq!(
            CLIAgent::QoderCLI
                .resume_command("q-1", Some(&argv(&["qodercli", "--model", "qoder-1"])))
                .as_deref(),
            Some("qodercli --model qoder-1 --resume q-1")
        );
        assert_eq!(
            CLIAgent::QoderCLI
                .resume_command(
                    "q-2",
                    Some(&argv(&["qodercli", "--resume", "q-1", "--fork-session"]))
                )
                .as_deref(),
            Some("qodercli --resume q-2"),
            "a stale --resume id and --fork-session come off before the new one goes on"
        );
        assert_eq!(
            CLIAgent::QoderCLI
                .resume_command(
                    "q-3",
                    Some(&argv(&["qodercli", "--session-id", "old", "--yolo"]))
                )
                .as_deref(),
            Some("qodercli --yolo --resume q-3"),
            "`--session-id` names a new session and is rejected next to `--resume`"
        );
    }

    #[test]
    fn qoder_resume_and_fork_do_not_recreate_worktrees() {
        for worktree in [
            vec!["--worktree"],
            vec!["--worktree", "old-tree"],
            vec!["--worktree=old-tree"],
        ] {
            for cwd_flag in ["-w", "--cwd"] {
                let mut launch = argv(&["qodercli", "--model", "qoder-1"]);
                launch.extend(argv(&worktree));
                launch.extend(argv(&[cwd_flag, "/repo/current-tree"]));
                assert_eq!(
                    CLIAgent::QoderCLI.resume_command("q-1", Some(&launch)),
                    Some(format!(
                        "qodercli --model qoder-1 {cwd_flag} /repo/current-tree --resume q-1"
                    )),
                    "launch argv: {launch:?}"
                );
                assert_eq!(
                    CLIAgent::QoderCLI.fork_command("q-1", Some(&launch)),
                    Some(format!(
                        "qodercli --model qoder-1 {cwd_flag} /repo/current-tree --resume q-1 --fork-session"
                    )),
                    "launch argv: {launch:?}"
                );
            }
        }
    }

    #[test]
    fn qoder_session_commands_require_persistence() {
        let ephemeral = argv(&["qodercli", "--print", "--no-session-persistence"]);
        assert_eq!(
            CLIAgent::QoderCLI.resume_command("q-1", Some(&ephemeral)),
            None
        );
        assert_eq!(
            CLIAgent::QoderCLI.fork_command("q-1", Some(&ephemeral)),
            None
        );

        let persistent = argv(&["qodercli", "--model", "qoder-1"]);
        assert_eq!(
            CLIAgent::QoderCLI
                .resume_command("q-1", Some(&persistent))
                .as_deref(),
            Some("qodercli --model qoder-1 --resume q-1")
        );
        assert_eq!(
            CLIAgent::QoderCLI
                .fork_command("q-1", Some(&persistent))
                .as_deref(),
            Some("qodercli --model qoder-1 --resume q-1 --fork-session")
        );
    }

    #[test]
    fn crush_resumes_by_session_and_keeps_its_cwd_flag() {
        let argv = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        assert_eq!(
            CLIAgent::Crush.resume_command("c-1", None).as_deref(),
            Some("crush --session c-1")
        );
        assert_eq!(
            CLIAgent::Crush
                .resume_command("c-2", Some(&argv(&["crush", "--session", "c-1", "--yolo"])))
                .as_deref(),
            Some("crush --yolo --session c-2"),
            "a stale --session and its id come off before the new one goes on"
        );
        assert_eq!(
            CLIAgent::Crush
                .resume_command("c-3", Some(&argv(&["crush", "--continue", "-C", "--yolo"])))
                .as_deref(),
            Some("crush --yolo --session c-3"),
            "both spellings of continue are stale with it"
        );
        // `-c` is `--cwd` in Crush, not `--continue`, and must survive.
        assert_eq!(
            CLIAgent::Crush
                .resume_command("c-4", Some(&argv(&["crush", "-c", "/repo/tree", "--yolo"])))
                .as_deref(),
            Some("crush -c /repo/tree --yolo --session c-4")
        );
        assert_eq!(
            CLIAgent::Crush.fork_command("c-1", None),
            None,
            "Crush has no fork command"
        );
    }

    #[test]
    fn codebuddy_resume_and_fork_drop_stale_session_and_worktree_flags() {
        assert_eq!(
            CLIAgent::CodeBuddy.resume_command("cb-1", None).as_deref(),
            Some("codebuddy --resume cb-1")
        );
        assert_eq!(
            CLIAgent::CodeBuddy.fork_command("cb-1", None).as_deref(),
            Some("codebuddy --resume cb-1 --fork-session")
        );
        for stale in [
            vec!["--resume", "cb-0"],
            vec!["-r", "cb-0"],
            vec!["--continue"],
            vec!["-c"],
            vec!["--session-id", "cb-0"],
            vec!["--resume", "cb-0", "--resume-session-at", "m-9"],
            vec!["--worktree"],
            vec!["--worktree", "old-tree", "--worktree-branch", "main"],
            vec!["-w", "old-tree"],
            vec!["--worktree=old-tree"],
            vec!["--resume", "cb-0", "--fork-session"],
        ] {
            let mut launch = argv(&["codebuddy", "--model", "m-1"]);
            launch.extend(argv(&stale));
            launch.extend(argv(&["--permission-mode", "acceptEdits"]));
            assert_eq!(
                CLIAgent::CodeBuddy
                    .resume_command("cb-2", Some(&launch))
                    .as_deref(),
                Some("codebuddy --model m-1 --permission-mode acceptEdits --resume cb-2"),
                "launch argv: {launch:?}"
            );
            assert_eq!(
                CLIAgent::CodeBuddy
                    .fork_command("cb-2", Some(&launch))
                    .as_deref(),
                Some(
                    "codebuddy --model m-1 --permission-mode acceptEdits --resume cb-2 --fork-session"
                ),
                "launch argv: {launch:?}"
            );
        }

        let ephemeral = argv(&["codebuddy", "-p", "--no-session-persistence"]);
        assert_eq!(
            CLIAgent::CodeBuddy.resume_command("cb-1", Some(&ephemeral)),
            None,
            "nothing was saved, so there is nothing to reopen"
        );
        assert_eq!(
            CLIAgent::CodeBuddy.fork_command("cb-1", Some(&ephemeral)),
            None
        );
    }

    #[test]
    fn oh_my_pi_resume_and_fork_use_its_own_flags() {
        let argv = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        assert_eq!(
            CLIAgent::OhMyPi.resume_command("s-1", None).as_deref(),
            Some("omp --resume s-1")
        );
        assert_eq!(
            CLIAgent::OhMyPi.fork_command("s-1", None).as_deref(),
            Some("omp --fork s-1")
        );
        assert_eq!(
            CLIAgent::OhMyPi
                .resume_command(
                    "s-2",
                    Some(&argv(&["omp", "--session", "s-1", "--model", "opus"]))
                )
                .as_deref(),
            Some("omp --model opus --resume s-2"),
            "--session is a third spelling of --resume and sheds with it"
        );
        assert_eq!(
            CLIAgent::OhMyPi
                .fork_command(
                    "s-2",
                    Some(&argv(&["omp", "--fork", "s-1", "-c", "--yolo"]))
                )
                .as_deref(),
            Some("omp --yolo --fork s-2")
        );
        assert_eq!(
            CLIAgent::OhMyPi
                .resume_command(
                    "s-3",
                    Some(&argv(&["omp", "--session-dir", "/w/.sessions"]))
                )
                .as_deref(),
            Some("omp --session-dir /w/.sessions --resume s-3"),
            "--session-dir is a different flag and rides along"
        );

        // `--no-session` means the run never persisted one, so there is
        // nothing to resume from and Oh My Pi rejects `--fork` outright.
        for id in ["s-4"] {
            assert_eq!(
                CLIAgent::OhMyPi.resume_command(id, Some(&argv(&["omp", "--no-session"]))),
                None
            );
            assert_eq!(
                CLIAgent::OhMyPi.fork_command(id, Some(&argv(&["omp", "--no-session"]))),
                None
            );
        }
    }

    #[test]
    fn fork_commands_cover_exactly_the_agents_with_a_verified_fork() {
        assert_eq!(
            CLIAgent::Codex.fork_command("abc-123", None).as_deref(),
            Some("codex fork abc-123")
        );
        assert_eq!(
            CLIAgent::TraeCode.fork_command("019c-123", None).as_deref(),
            Some("traecli fork 019c-123")
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
        assert_eq!(
            CLIAgent::Droid.fork_command("session-abc", None).as_deref(),
            Some("droid --fork session-abc")
        );
        assert_eq!(
            CLIAgent::Qwen.fork_command("q-1", None).as_deref(),
            Some("qwen --resume q-1 --fork-session")
        );
        assert_eq!(
            CLIAgent::Goose.fork_command("20260213_9", None).as_deref(),
            Some("goose session --resume --fork --session-id 20260213_9")
        );
        // Undocumented in `amp threads --help`, but `amp threads fork --help`
        // prints its own usage, so the subcommand is real.
        assert_eq!(
            CLIAgent::Amp.fork_command("T-abc", None).as_deref(),
            Some("amp threads fork T-abc")
        );

        // Cursor and Antigravity fork only from inside a running TUI (`/fork`),
        // which is not something a launch command line can reach.
        for agent in [
            CLIAgent::Gemini,
            CLIAgent::Copilot,
            CLIAgent::Cursor,
            CLIAgent::Aider,
            CLIAgent::Auggie,
            CLIAgent::Hermes,
            CLIAgent::Vibe,
            CLIAgent::Antigravity,
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
    fn newly_wired_agents_resume_the_way_their_own_cli_spells_it() {
        let argv = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        for (agent, id, want) in [
            (CLIAgent::Droid, "session-abc", "droid --resume session-abc"),
            (CLIAgent::Qwen, "q-1", "qwen --resume q-1"),
            (CLIAgent::Auggie, "a-1", "auggie --resume a-1"),
            (
                CLIAgent::Goose,
                "20260213_9",
                "goose session --resume --session-id 20260213_9",
            ),
            (
                CLIAgent::Hermes,
                "20260812_213234_5de948",
                "hermes chat --resume 20260812_213234_5de948",
            ),
            (CLIAgent::Vibe, "v-1", "vibe --resume v-1"),
            (
                CLIAgent::Antigravity,
                "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
                "agy --conversation a1b2c3d4-e5f6-7890-abcd-ef1234567890",
            ),
        ] {
            assert_eq!(
                agent.resume_command(id, None).as_deref(),
                Some(want),
                "{} resumes with the wrong command",
                agent.slug()
            );
        }

        // A subcommand-addressed session leaves nothing in `stale` to strip, so
        // the prefix has to be dropped structurally — otherwise the launch flags
        // go down with it.
        assert_eq!(
            CLIAgent::Amp
                .resume_command(
                    "T-2",
                    Some(&argv(&[
                        "amp",
                        "threads",
                        "continue",
                        "T-1",
                        "--dangerously-allow-all",
                    ]))
                )
                .as_deref(),
            Some("amp threads continue T-2 --dangerously-allow-all")
        );
        assert_eq!(
            CLIAgent::Goose
                .resume_command(
                    "20260213_9",
                    Some(&argv(&["goose", "session", "--resume", "--name", "old"]))
                )
                .as_deref(),
            Some("goose session --resume --session-id 20260213_9")
        );
        assert_eq!(
            CLIAgent::Auggie
                .resume_command(
                    "a-2",
                    Some(&argv(&["auggie", "session", "resume", "a-1", "--verbose"]))
                )
                .as_deref(),
            Some("auggie --verbose --resume a-2")
        );
        assert_eq!(
            CLIAgent::Droid
                .resume_command(
                    "s-2",
                    Some(&argv(&["droid", "--fork", "s-1", "--auto", "low"]))
                )
                .as_deref(),
            Some("droid --auto low --resume s-2")
        );

        // `--id` is an alias of `--session-id` and `-n` of `--name`, and the
        // three share one exclusive clap group — any of them surviving next to
        // the `--session-id` the command appends would fail to parse.
        assert_eq!(
            CLIAgent::Goose
                .resume_command(
                    "20260213_9",
                    Some(&argv(&["goose", "s", "--resume", "--id", "20260101_1"]))
                )
                .as_deref(),
            Some("goose session --resume --session-id 20260213_9")
        );
        assert_eq!(
            CLIAgent::Goose
                .resume_command(
                    "20260213_9",
                    Some(&argv(&["goose", "session", "-r", "-n", "old"]))
                )
                .as_deref(),
            Some("goose session --resume --session-id 20260213_9")
        );
        // Qwen rejects `--session-id` alongside `--resume`; Vibe spells
        // `--continue` as `-c` too.
        assert_eq!(
            CLIAgent::Qwen
                .resume_command("q-2", Some(&argv(&["qwen", "--session-id", "old"])))
                .as_deref(),
            Some("qwen --resume q-2")
        );
        assert_eq!(
            CLIAgent::Vibe
                .resume_command("v-2", Some(&argv(&["vibe", "-c"])))
                .as_deref(),
            Some("vibe --resume v-2")
        );

        // Nothing was persisted, so there is nothing to resume or fork.
        for id in ["a-1"] {
            assert_eq!(
                CLIAgent::Auggie
                    .resume_command(id, Some(&argv(&["auggie", "--dont-save-session"]))),
                None
            );
        }
        // "If false, chat history is not saved and --continue/--resume will
        // not work" — so neither resume nor fork is offered.
        let no_recording = argv(&["qwen", "--no-chat-recording"]);
        assert_eq!(
            CLIAgent::Qwen.resume_command("q-1", Some(&no_recording)),
            None
        );
        assert_eq!(
            CLIAgent::Qwen.fork_command("q-1", Some(&no_recording)),
            None
        );
    }

    /// `python3 -m antigravity` opens an xkcd comic. It is the standard way to
    /// trigger Python's easter egg, and the interpreter branch used to read that
    /// module name as an agent.
    #[test]
    fn the_python_easter_egg_is_not_a_coding_agent() {
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&["python3", "-m", "antigravity"])),
            None
        );
        assert_eq!(
            CLIAgent::detect_from_argv(&argv(&["agy"])),
            Some(CLIAgent::Antigravity)
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

    fn overrides(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_launch_is_the_bare_binary_unless_overridden() {
        let none = HashMap::new();
        assert_eq!(CLIAgent::Claude.launch_command(&none), "claude");
        assert_eq!(CLIAgent::Codex.launch_command(&none), "codex");
        assert_eq!(CLIAgent::Cursor.launch_command(&none), "cursor-agent");
        assert_eq!(CLIAgent::Antigravity.launch_command(&none), "agy");
        assert_eq!(CLIAgent::QoderCLI.launch_command(&none), "qoder");

        let set = overrides(&[
            ("claude", "  claude --dangerously-skip-permissions "),
            // Keys are read the way `from_slug` reads them.
            ("Codex", "codex --model o3"),
            // A blank entry is no override at all.
            ("gemini", "   "),
        ]);
        assert_eq!(
            CLIAgent::Claude.launch_command(&set),
            "claude --dangerously-skip-permissions"
        );
        assert_eq!(CLIAgent::Codex.launch_command(&set), "codex --model o3");
        assert_eq!(CLIAgent::Gemini.launch_command(&set), "gemini");
        assert_eq!(CLIAgent::Aider.launch_command(&set), "aider");
    }

    #[test]
    fn every_agent_launches_as_a_name_it_is_detected_by() {
        let none = HashMap::new();
        for agent in CLIAgent::ALL {
            let argv = command_argv(&agent.launch_command(&none));
            assert_eq!(
                CLIAgent::detect_from_argv(&argv),
                Some(agent),
                "{agent:?} would launch as something detection does not know"
            );
        }
    }

    #[test]
    fn a_wrapper_named_by_a_launch_override_is_detected_as_its_agent() {
        let aliases = launch_aliases(&overrides(&[("claude", "FOO=1 cc --fast")]));
        // Run as a binary, and run as a script under its interpreter.
        for run in [
            argv(&["cc", "--fast"]),
            argv(&["/Users/me/bin/cc", "--fast"]),
            argv(&["/bin/bash", "/Users/me/bin/cc", "--fast"]),
            argv(&["node", "/Users/me/bin/cc.js", "--fast"]),
        ] {
            assert_eq!(
                CLIAgent::detect_from_argv_with(&run, &aliases),
                Some(CLIAgent::Claude),
                "{run:?}"
            );
        }
        // Without the override the same wrapper is nobody.
        assert_eq!(
            CLIAgent::detect_from_argv_with(&argv(&["cc", "--fast"]), &HashMap::new()),
            None
        );
    }

    #[test]
    fn a_launch_override_never_maps_an_interpreter_or_a_real_agent_name() {
        let aliases = launch_aliases(&overrides(&[
            ("claude", "npx @anthropic-ai/claude-code"),
            ("codex", "bash -lc codex"),
            // Already detected as itself; mapping it would add nothing.
            ("gemini", "gemini --yolo"),
            ("not-an-agent", "cc"),
        ]));
        assert!(aliases.is_empty(), "{aliases:?}");
        // A shell running something else is still nobody.
        assert_eq!(
            CLIAgent::detect_from_argv_with(
                &argv(&["bash", "./build.sh"]),
                &launch_aliases(&overrides(&[("claude", "cc")]))
            ),
            None
        );
    }

    #[test]
    fn launch_args_drop_the_session_and_the_prompt_but_keep_the_flags() {
        assert_eq!(
            CLIAgent::Claude.launch_argv_for_default(&argv(&[
                "node",
                "/opt/homebrew/lib/node_modules/@anthropic-ai/claude-code/cli.js",
                "--model",
                "opus",
                "--resume",
                "abc-123",
                "--append-system-prompt",
                "be terse",
                "fix the bug",
            ])),
            Some(argv(&[
                "claude",
                "--model",
                "opus",
                "--append-system-prompt",
                "be terse",
            ]))
        );
        assert_eq!(
            CLIAgent::Codex.launch_argv_for_default(&argv(&[
                "codex",
                "resume",
                "019a",
                "--model=o3",
                "prompt",
            ])),
            Some(argv(&["codex", "--model=o3"]))
        );
        // A wrapper cannot be taken apart; what was typed is what launched it.
        assert_eq!(
            CLIAgent::Claude.launch_argv_for_default(&argv(&["cc", "--fast", "x"])),
            Some(argv(&["cc", "--fast", "x"]))
        );
        assert_eq!(CLIAgent::Claude.launch_argv_for_default(&[]), None);
    }

    #[test]
    fn program_on_path_finds_only_executables() {
        let dir = tempfile::TempDir::new().unwrap();
        let bin = dir.path().join("claude");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::write(dir.path().join("codex"), "not executable").unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(!program_on_path("codex", dir.path().as_os_str()));
        }
        std::fs::create_dir(dir.path().join("gemini")).unwrap();
        let path =
            std::env::join_paths([std::path::Path::new("/nonexistent"), dir.path()]).unwrap();
        assert!(program_on_path("claude", &path));
        assert!(
            !program_on_path("gemini", &path),
            "a directory is not a program"
        );
        assert!(!program_on_path("aider", &path));
        assert!(program_on_path(
            bin.to_str().unwrap(),
            std::ffi::OsStr::new("")
        ));
        assert!(!program_on_path("", &path));
    }
}

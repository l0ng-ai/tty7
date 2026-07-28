//! Third-party CLI coding-agent registry + detection.
//!
//! tty7 recognizes when a pane is running someone else's coding agent (Claude
//! Code, Codex, Gemini CLI, …) so the tab chip can brand it and desktop
//! notifications can say *which* agent finished or needs you. This is
//! deliberately *not* tty7's own agent: it only observes and enriches whatever
//! agent the user launched.
//!
//! Detection is command-based: on macOS/Linux the daemon already
//! reads the foreground process's `argv` for SSH-context sniffing, so we reuse
//! that to match the invoked command against a known agent. Matching is a pure
//! function over `argv` — [`CLIAgent::detect_from_argv`] — kept here in `core`
//! (framework-light, unit-tested) and called daemon-side, with the resulting
//! `Option<CLIAgent>` streamed to the client for the UI. On Windows ConPTY
//! exposes no foreground process group, so the input is the *typed command
//! line* the shell integration captures at preexec and carries on the `133;C`
//! mark — [`CLIAgent::detect_from_command_with`] matches it the same way.
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
    ///
    /// `launch_argv` is the argv the agent was originally launched with, when
    /// the daemon observed one. Its flags (`--dangerously-skip-permissions`,
    /// `--model …`) are carried onto the resume command so the restored
    /// session runs in the same mode the user picked — verbatim only when the
    /// whole tail passes the conservative shell-safety gate; otherwise the
    /// bare table command still resumes, just without the flags.
    pub fn resume_command(
        self,
        session_id: &str,
        launch_argv: Option<&[String]>,
    ) -> Option<String> {
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
        // A pane the user launched as deliberately ephemeral has nothing on
        // disk to come back to, whatever id the agent reported.
        if launch_argv.is_some_and(|argv| self.opts_out_of_sessions(argv)) {
            return None;
        }
        // The user's launch flags, pre-joined with a leading space so they
        // splice into the format strings below; empty when none survive.
        let flags = launch_argv
            .and_then(|argv| self.replay_flags(argv))
            .map(|flags| {
                flags.iter().fold(String::new(), |mut s, f| {
                    s.push(' ');
                    s.push_str(f);
                    s
                })
            })
            .unwrap_or_default();
        match self {
            CLIAgent::Claude => Some(format!("claude{flags} --resume {session_id}")),
            // Codex resumes via a subcommand that accepts the interactive
            // options after the positional id (`codex resume [OPTIONS]
            // [SESSION_ID]`).
            CLIAgent::Codex => Some(format!("codex resume {session_id}{flags}")),
            CLIAgent::Gemini => Some(format!("gemini{flags} --resume {session_id}")),
            CLIAgent::OpenCode => Some(format!("opencode{flags} --session {session_id}")),
            // Amp's global options (`--dangerously-allow-all`, …) are accepted
            // by the `threads continue` subcommand (verified: unknown options
            // are a parse error, globals pass).
            CLIAgent::Amp => Some(format!("amp threads continue {session_id}{flags}")),
            CLIAgent::Cursor => Some(format!("cursor-agent{flags} --resume {session_id}")),
            // Copilot CLI: `copilot --resume <sessionId>` (`-r` shorthand) —
            // the one hooks-covered agent that was missing from this table.
            CLIAgent::Copilot => Some(format!("copilot{flags} --resume {session_id}")),
            // Grok Build: `grok --resume <id-or-title>`; a UUID-shaped value
            // always takes the id path, which is what its hooks report.
            CLIAgent::Grok => Some(format!("grok{flags} --resume {session_id}")),
            // Pi's `--resume`/`-r` is a *boolean* that opens the interactive
            // session picker and `--continue`/`-c` just takes the newest
            // session; the flag that targets one by id is `--session
            // <path|id>` ("Use specific session file or partial UUID"). Its
            // ids are uuidv7, so they clear the token gate above.
            CLIAgent::Pi => Some(format!("pi{flags} --session {session_id}")),
            _ => None,
        }
    }

    /// Whether `argv` launched the agent with session persistence turned off,
    /// which makes the pane unresumable: nothing was written to disk, so a
    /// replayed id would point at a session file that never existed *and*
    /// would quietly undo the user's opt-out. Distinct from the stale flags in
    /// [`Self::replay_flags`], which name a different session and merely have
    /// to lose to the injected id.
    fn opts_out_of_sessions(self, argv: &[String]) -> bool {
        let ephemeral: &[&str] = match self {
            // Pi still mints an in-memory session id under `--no-session` — it
            // only skips the write — so tty7 does observe an id to replay.
            CLIAgent::Pi => &["--no-session"],
            _ => &[],
        };
        argv.iter().any(|t| ephemeral.contains(&t.as_str()))
    }

    /// The launch-flag tail of `argv` worth replaying on a resume command, or
    /// `None` to resume bare. Deliberately conservative: anything ambiguous
    /// falls back to no flags rather than a corrupted command line.
    ///
    /// - The tail is everything after the token that names this agent (the
    ///   launcher itself, or the script path in an interpreter-wrapped argv);
    ///   leading `VAR=value` env assignments are skipped first so they can't
    ///   mis-anchor (`CLAUDE_CONFIG_DIR=/opt/claude claude …`). No naming
    ///   token at all (custom wrapper rules) → no flags.
    /// - Stale session-targeting flags (`--resume old-id`, `--continue`, a
    ///   re-launched `codex resume <id>`) are stripped — the new id must win.
    /// - Every surviving token must be a plain shell-safe word, the first must
    ///   be a `-` flag, and no two bare words may run consecutively — a bare
    ///   word is only acceptable as the value directly behind a flag; anything
    ///   else is a positional prompt that must not re-submit itself into the
    ///   resumed session. Any violation drops the whole tail.
    fn replay_flags(self, argv: &[String]) -> Option<Vec<String>> {
        let names_self = |token: &str| {
            token.split(['/', '\\']).any(|seg| {
                CLIAgent::match_token(&base_stem(seg).to_ascii_lowercase()) == Some(self)
            })
        };
        let argv = &argv[argv.iter().take_while(|t| is_env_assignment(t)).count()..];
        let named = argv.iter().position(|t| names_self(t))?;
        let mut tail: Vec<&str> = argv[named + 1..].iter().map(String::as_str).collect();

        // A relaunched `codex resume <old-id>`: drop the subcommand and its id
        // so they don't replay as a positional prompt.
        if self == CLIAgent::Codex && tail.first() == Some(&"resume") {
            tail.remove(0);
            if tail.first().is_some_and(|t| !t.starts_with('-')) {
                tail.remove(0);
            }
        }

        // Session-targeting flags whose old value must not survive; each is
        // stripped together with one following non-flag value token (harmless
        // for the value-less ones — anything trailing them is positional).
        let stale: &[&str] = match self {
            CLIAgent::Claude => &[
                "--resume",
                "-r",
                "--continue",
                "-c",
                "--session-id",
                "--from-pr",
            ],
            CLIAgent::Gemini | CLIAgent::Cursor => &["--resume", "-r"],
            CLIAgent::Copilot => &["--resume", "-r", "--continue", "-c"],
            CLIAgent::OpenCode => &["--session", "-s", "--continue", "-c"],
            // `--last` targets "the most recent session" and would contradict
            // the explicit id we inject.
            CLIAgent::Codex => &["--last"],
            // Pi's ways of picking a session all fight the `--session <id>` we
            // inject: `--session`/`--session-id` name a different one,
            // `--fork` would branch instead of continue, and the boolean
            // `-r`/`-c` re-open the picker or the newest session.
            // `--session-dir` is *not* here — it says where sessions live, so
            // the id we inject needs it to still be there. `--no-session` is
            // not here either: it isn't stale, it means there is nothing to
            // resume at all (see [`Self::opts_out_of_sessions`]).
            CLIAgent::Pi => &[
                "--session",
                "--session-id",
                "--fork",
                "--resume",
                "-r",
                "--continue",
                "-c",
            ],
            // Beyond the session-targeting flags (`--load` is grok's hidden
            // alias for `--resume`; `--session-id` names a *new* session and
            // `--fork-session` would branch off the one we mean to continue),
            // the worktree pair goes too: `--worktree` with no value mints a
            // fresh git worktree on every relaunch, and `--worktree-ref`
            // requires `--worktree`, so leaving it behind would make grok
            // reject the resume outright.
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

        // The safety gate: plain tokens only, and a flag-shaped tail — every
        // bare word must sit directly behind a `-` flag (its value slot); the
        // first token being bare, or two bare words in a row, is a positional
        // prompt and drops the whole tail. (A single bare word behind a
        // boolean flag is indistinguishable from a flag value and slips
        // through — the residual ambiguity of not knowing each flag's arity.)
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

    /// Brand accent (0xRRGGBB) for the tab chip's agent dot. Chosen for legibility
    /// on both light and dark themes rather than exact brand black/white. A pure
    /// *white* field vanishes against a light theme, so vendors whose mark is a
    /// grey or gradient monochrome (Cursor) get a recognizable mid-tone hue
    /// instead. A black field is a different case: it stays darker than even the
    /// darkest theme background and the white mark on it carries the badge, so
    /// vendors who actually brand in black (Codex, Grok) keep it.
    pub fn accent_rgb(self) -> u32 {
        match self {
            CLIAgent::Claude => 0xD97757,      // Claude terracotta
            CLIAgent::Codex => 0x000000,       // Codex black field
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
            CLIAgent::Grok => 0x000000,        // xAI brands in black
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
            CLIAgent::Grok => "icons/agents/grok.svg",
            CLIAgent::Pi => "icons/agents/pi.svg",
            // No brand mark bundled → generic robot glyph.
            CLIAgent::Aider
            | CLIAgent::Auggie
            | CLIAgent::Hermes
            | CLIAgent::Vibe
            | CLIAgent::Antigravity
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

    /// [`detect_from_argv_with`](Self::detect_from_argv_with) over a *typed
    /// command line* rather than a live process `argv` — the Windows detection
    /// input. ConPTY has no foreground process group to resolve to an argv, so
    /// there the daemon learns what runs from the shell integration instead:
    /// PowerShell's `PSConsoleHostReadLine` wrapper reports the submitted line
    /// on the `133;C` mark (the same capture Warp's Windows integration uses),
    /// and this matches it like an argv.
    ///
    /// The tokenization is deliberately naive — whitespace split, surrounding
    /// quotes trimmed, a leading PowerShell call operator (`&`) dropped, and
    /// everything lowercased (Windows commands are case-insensitive). A quoted
    /// launcher path containing spaces splits wrong and misses — notably
    /// PSReadLine tab-completion's `& 'C:\Program Files\…\claude.exe'` — the
    /// accepted trade-off for not writing a shell parser; the dominant shapes
    /// (a bare shim on PATH, `npx …`) tokenize fine, and `agent_commands`
    /// rules cover personal wrappers.
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
/// Splits on both separators by hand (not [`Path`]) so a Windows path in a
/// captured command line (`C:\…\claude.cmd`) resolves the same on every
/// platform — including in tests run on Unix.
fn base_stem(token: &str) -> &str {
    // Trailing separators are dropped first (`claude/` → `claude`, matching
    // the old `Path::file_name` behavior), then everything up to the last
    // separator.
    let trimmed = token.trim_end_matches(['/', '\\']);
    let name = match trimmed.rfind(['/', '\\']) {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    };
    // Strip one known script/launcher extension; leave unknown suffixes intact
    // so `claude-code` stays whole. The Windows set covers npm's shim trio
    // (`claude.cmd` / `claude.ps1` / `claude.exe`).
    for ext in [
        ".js", ".mjs", ".cjs", ".ts", ".py", ".rb", ".sh", ".exe", ".cmd", ".bat", ".ps1",
    ] {
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
    /// The argv the agent was launched with, as the daemon observed it (the
    /// foreground process-table poll on Unix, the shell integration's typed
    /// `133;C` capture on Windows). Persisted alongside the session id so
    /// restore can carry the user's launch flags
    /// (`--dangerously-skip-permissions`, `--model …`) onto the resume
    /// command — see [`CLIAgent::resume_command`]. Not touched by
    /// [`apply_event`](Self::apply_event); the daemon stamps it from the
    /// identity-detection side.
    #[serde(default)]
    pub launch_argv: Option<Vec<String>>,
    /// Whether this state came from the rich sentinel channel (hooks
    /// installed) rather than the opaque OSC 9/777 fallback. Rich state drives
    /// turn-level notifications; fallback state only paints the dot (the
    /// agent's own notification text was already toasted by the client).
    #[serde(default)]
    pub rich: bool,
    /// The agent's working directory as its hook payloads report it — the
    /// agent's own claim, which tracks internal chdirs the PTY can't show
    /// (Claude Code's EnterWorktree moves the session without any shell `cd`).
    /// Cleared on `session-end` so a finished session can't pin consumers to
    /// a stale path; while absent, consumers fall back to the pane's proc cwd.
    #[serde(default)]
    pub cwd: Option<std::path::PathBuf>,
    /// Tool completions seen in this session, counted only so consumers can
    /// spot *that* the agent did something — a turn's edits land tool by tool,
    /// and the status alone can't say so (`ToolComplete` is a no-op transition
    /// during normal work, by design). The sidebar's git probe watches this to
    /// refresh mid-turn instead of waiting for `stop`; see
    /// [`TerminalView::refresh_git_status`](crate::terminal::view::TerminalView).
    /// Monotonic within a session and never reset — consumers compare against
    /// the value they last saw, so only the *change* means anything.
    #[serde(default)]
    pub activity: u64,
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
            // A tool call finished. Only meaningful as the recovery edge out
            // of a block: the user answered the permission prompt / question,
            // the approved tool ran, so the turn is moving again — no agent
            // emits an explicit "permission replied" signal here, so the next
            // tool completion is that signal. Guarded on Waiting so the steady
            // stream of completions during normal work is a no-op and can
            // never overwrite Done between turns.
            AgentEventKind::ToolComplete => {
                // The count moves even when the status doesn't: a tool call is
                // the one signal that the working tree may have just changed
                // under a turn that won't end for minutes.
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
            // The agent session ended but its id stays: Claude & friends can
            // resume an *ended* session, which is exactly what restore does.
            // Its cwd claim does NOT stay: with no agent running, the pane's
            // real (proc-observed) directory is the truth again.
            AgentEventKind::SessionEnd => {
                self.status = AgentStatus::Idle;
                self.message = None;
                self.cwd = None;
            }
        }
    }
}

/// The event vocabulary of the sentinel protocol (`"event"` in the JSON).
/// Deliberately a superset of what any one agent emits: Claude Code hooks map
/// onto session-start / prompt-submit / notification / tool-complete / stop /
/// session-end, while permission-request / question-asked are there for
/// agents (Codex, OpenCode plugins) that can distinguish them.
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
    /// The agent's working directory at the moment the hook fired, when the
    /// payload carries one (Claude Code sends it on every hook event).
    pub cwd: Option<std::path::PathBuf>,
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
        // A trailing separator is tolerated, matching Path::file_name.
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

    /// The two vendors who actually brand in black keep the black field rather
    /// than the mid-tone substitute monochrome marks otherwise get.
    #[test]
    fn black_branded_avatars_keep_their_brand_field() {
        assert_eq!(CLIAgent::Codex.accent_rgb(), 0x000000);
        assert_eq!(CLIAgent::Grok.accent_rgb(), 0x000000);
    }

    /// The fallback robot glyph is a placeholder, not a resting state: an agent
    /// may only sit on it while no mark is bundled, and a bundled mark may not
    /// silently fall back off. Pinning the exact fallback set makes either
    /// direction a deliberate edit here rather than something noticed in the UI.
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
        // Everything else names a bundled mark under the agents directory.
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
    fn detects_from_typed_command_lines() {
        let none = HashMap::new();
        // Plain invocations, flags in tow.
        assert_eq!(
            CLIAgent::detect_from_command_with("claude --resume abc", &none),
            Some(CLIAgent::Claude)
        );
        // Windows launcher shapes: npm shims, absolute backslash paths, and
        // case-insensitive names.
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
        // PowerShell call operator + a quoted (space-free) path.
        assert_eq!(
            CLIAgent::detect_from_command_with(r#"& "C:\tools\codex.exe""#, &none),
            Some(CLIAgent::Codex)
        );
        // Interpreter-wrapped, Windows separators in the script path.
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
        // Non-interpreter launchers never match on their arguments.
        assert_eq!(
            CLIAgent::detect_from_command_with("notepad claude.txt", &none),
            None
        );
        assert_eq!(
            CLIAgent::detect_from_command_with("cat codex.md", &none),
            None
        );
        assert_eq!(CLIAgent::detect_from_command_with("", &none), None);
        // Custom rules apply to the typed launcher too.
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
            cwd: None,
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

        // The user approved: the granted tool runs to completion, and that
        // completion is the "back to work" edge — amber flips back to blue
        // instead of lingering for the rest of the turn.
        s.apply_event(&ev(AgentEventKind::ToolComplete, None, None));
        assert_eq!(s.status, AgentStatus::Working);
        assert_eq!(s.message, None, "the stale permission prompt is cleared");

        // Tool completions during normal work are a no-op, not state churn.
        s.apply_event(&ev(AgentEventKind::ToolComplete, None, None));
        assert_eq!(s.status, AgentStatus::Working);

        s.apply_event(&ev(AgentEventKind::Stop, None, None));
        assert_eq!(s.status, AgentStatus::Done);

        // A straggler tool-complete after the turn ended must not resurrect
        // Working and hide the unread green dot.
        s.apply_event(&ev(AgentEventKind::ToolComplete, None, None));
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

    /// Tool completions are deliberately a *status* no-op during normal work
    /// (the assertions above), which leaves consumers watching the status with
    /// no way to tell that an agent mid-turn just wrote a file. `activity` is
    /// what makes them observable: it moves on every completion, in every
    /// status, and never rewinds — the sidebar's git probe compares it against
    /// the value it last saw.
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

        // A straggler after the turn ended still counts: it may well have
        // written a file, and it must not be mistaken for "nothing happened".
        s.apply_event(&ev(AgentEventKind::Stop));
        s.apply_event(&ev(AgentEventKind::ToolComplete));
        assert_eq!(
            s.status,
            AgentStatus::Done,
            "and still doesn't resurrect the turn"
        );
        assert_eq!(s.activity, 4);

        // Session end resets plenty of state but not this — a rewind to 0 would
        // read to a delta-comparing consumer as one more tool call.
        s.apply_event(&ev(AgentEventKind::SessionEnd));
        assert_eq!(s.activity, 4);
    }

    /// The agent's cwd claim: any event carrying one sets it, later events
    /// without one leave it alone (mid-turn events keep the worktree path
    /// alive), and session end drops it — an exited agent must not pin the
    /// pane's git line to a directory nothing runs in anymore.
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

        // EnterWorktree lands as a tool-complete carrying the new directory.
        s.apply_event(&ev(
            AgentEventKind::ToolComplete,
            Some("/repo/.claude/worktrees/fix-x"),
        ));
        assert_eq!(
            s.cwd.as_deref(),
            Some(std::path::Path::new("/repo/.claude/worktrees/fix-x"))
        );

        // An event without a cwd (another agent's sparser payload) keeps it.
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
        // Pi targets a session by id through `--session`, not through its
        // boolean `--resume` (which only opens the picker).
        assert_eq!(
            CLIAgent::Pi
                .resume_command("0199c3f2-1b0e-7c3a-9f21-6d4b8e2a5c17", None)
                .as_deref(),
            Some("pi --session 0199c3f2-1b0e-7c3a-9f21-6d4b8e2a5c17")
        );
        // No resume flag known → None.
        assert_eq!(CLIAgent::Aider.resume_command("abc", None), None);
        // An id carrying shell syntax is refused outright.
        assert_eq!(CLIAgent::Claude.resume_command("abc; rm -rf /", None), None);
        assert_eq!(CLIAgent::Claude.resume_command("$(boom)", None), None);
        assert_eq!(CLIAgent::Claude.resume_command("", None), None);
    }

    #[test]
    fn resume_carries_launch_flags() {
        let argv = |parts: &[&str]| parts.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // The headline case: the user's mode flags survive the restart.
        assert_eq!(
            CLIAgent::Claude
                .resume_command(
                    "abc-123",
                    Some(&argv(&["claude", "--dangerously-skip-permissions"]))
                )
                .as_deref(),
            Some("claude --dangerously-skip-permissions --resume abc-123")
        );
        // Value-taking flags ride along whole.
        assert_eq!(
            CLIAgent::Claude
                .resume_command("abc", Some(&argv(&["claude", "--model", "opus"])))
                .as_deref(),
            Some("claude --model opus --resume abc")
        );
        // Interpreter-wrapped launch: flags start after the token naming the
        // agent, and the table's launcher name is what replays.
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
        // A stale session-targeting flag is stripped — the new id must win.
        assert_eq!(
            CLIAgent::Claude
                .resume_command(
                    "new-id",
                    Some(&argv(&["claude", "--resume", "old-id", "--model", "opus"]))
                )
                .as_deref(),
            Some("claude --model opus --resume new-id")
        );
        // Codex resumes via its subcommand, flags after the positional id; a
        // relaunched `codex resume <old>` sheds the old subcommand + id.
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
        // Anything shell-unsafe or positional-shaped drops the WHOLE tail —
        // resume still works, just bare.
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
        // A leading env assignment doesn't mis-anchor the flag tail.
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
        // Two consecutive bare words = a positional prompt, not a flag value —
        // it must not re-submit itself into the resumed session.
        assert_eq!(
            CLIAgent::Claude
                .resume_command(
                    "abc",
                    Some(&argv(&["claude", "--model", "opus", "review", "this"]))
                )
                .as_deref(),
            Some("claude --resume abc")
        );
        // Codex `--last` targets "most recent" and would contradict the
        // explicit id → stripped.
        assert_eq!(
            CLIAgent::Codex
                .resume_command(
                    "id-3",
                    Some(&argv(&["codex", "resume", "--last", "--yolo"]))
                )
                .as_deref(),
            Some("codex resume id-3 --yolo")
        );
        // Pi: mode flags replay, but every way of naming a *different* session
        // is stripped so the injected id wins — including the boolean picker
        // flags.
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
        // `--no-session` is the user asking for an ephemeral pane: Pi mints an
        // id but never writes the file, so resuming it would open an empty
        // session *and* override the opt-out — no resume command at all.
        assert_eq!(
            CLIAgent::Pi.resume_command(
                "id-x",
                Some(&argv(&["pi", "--no-session", "--model", "opus"]))
            ),
            None
        );
        // `--session-dir` says where sessions live — the injected id needs it,
        // so it is deliberately *not* stripped.
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
        // No token names the agent (custom wrapper rule) → bare.
        assert_eq!(
            CLIAgent::Claude
                .resume_command(
                    "abc",
                    Some(&argv(&["cc", "--dangerously-skip-permissions"]))
                )
                .as_deref(),
            Some("claude --resume abc")
        );
        // Amp: global mode flags ride after the `threads continue` positional.
        assert_eq!(
            CLIAgent::Amp
                .resume_command("t-1", Some(&argv(&["amp", "--dangerously-allow-all"])))
                .as_deref(),
            Some("amp threads continue t-1 --dangerously-allow-all")
        );
        // A relaunch via `amp threads continue …` is subcommand-shaped, not
        // flag-shaped → bare (the gate rejects the leading bare word).
        assert_eq!(
            CLIAgent::Amp
                .resume_command("t-2", Some(&argv(&["amp", "threads", "continue", "t-1"])))
                .as_deref(),
            Some("amp threads continue t-2")
        );
        // Copilot resumes by flag, stale session targeting stripped.
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
        // Grok: mode flags survive, and every way of naming another session is
        // stripped so the injected id is the only target left.
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
        // `--worktree` would mint a fresh git worktree on every restore, and
        // `--worktree-ref` can't survive without it.
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

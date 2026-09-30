//! The message composer: a real text box for writing a prompt to a coding
//! agent the way a chat box lets you — mouse selection, the platform's own
//! editing keys, an IME with its candidates in place, files attached as chips,
//! `/` and `@` menus — and handing it over whole.
//!
//! **Where it sits.** For an agent whose input area tty7 can find on the grid
//! (Claude Code, Codex, Gemini) the box is laid *over* that area: the agent's
//! own input line and the status rows under it are covered, so the pane has
//! one input rather than two. Whenever the agent puts something else there —
//! a permission prompt, a model picker, a question — the area stops looking
//! like an input, the box steps aside, and the keyboard goes to the TUI until
//! the input comes back. Any other agent gets the box docked under the grid,
//! taking its rows from it.
//!
//! **What it sends.** Sending is typing, not an API: the text and then Enter,
//! written to the pty — see [`submit_plan`] for why each agent is spoken to
//! slightly differently. The toolbar's controls are the agent's own keys
//! (Shift+Tab, its model picker, its thinking toggle), and what they show is
//! what the agent said: the mode off the status row the box covers, the model
//! and context off what its hooks report ([`crate::core::cli_agent::AgentReadout`]). Nothing reported
//! means nothing shown.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermMode};
use gpui::{
    Context, Entity, ExternalPaths, Focusable as _, MouseButton, MouseDownEvent, SharedString,
    Subscription, Window, div, prelude::*, px,
};
use gpui_component::input::{self, Input, InputEvent, InputState, RopeExt as _};
use gpui_component::progress::ProgressCircle;
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, Size, h_flex};

use super::view::{GRID_PAD_X, GRID_PAD_Y, TerminalView, pasted_paths_text, types_cleanly};
use crate::core::cli_agent::{AgentStatus, CLIAgent};
use crate::ui::host_ops::{HostId, HostOps};
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::search::files::{FileIndex, FileList, IndexedFile, rank, walk};

/// How tall the text may grow, in lines, before it scrolls instead.
const MAX_ROWS: usize = 8;

/// The pause between two writes that must not arrive as one read.
///
/// An agent's input layer tells typing from pasting by how bytes arrive, and a
/// CR landing in the same read as the text before it is taken as part of that
/// text — a newline in the message rather than the key that sends it. Long
/// enough to put the two in separate reads on a loaded machine, short enough
/// to be inside the time a key press takes to feel instant.
const SETTLE: Duration = Duration::from_millis(50);

/// Copilot's input treats a CR that follows a paste too closely as part of it.
const SETTLE_AFTER_PASTE_SLOW: Duration = Duration::from_millis(300);

/// How long the agent's input area has to stay gone before the box steps
/// aside. An agent redrawing its screen can pass through a frame without the
/// input on it; a picker or a prompt stays.
pub(super) const STEP_ASIDE_AFTER: Duration = Duration::from_millis(250);

/// Rows the `/` and `@` menu shows at once.
const MENU_ROWS: usize = 8;

/// A file walk or command scan this recent answers the next menu as it is.
const SOURCES_FRESH_FOR: Duration = Duration::from_secs(30);

/// How far up from the bottom of the screen an agent's input area can start.
/// Past this the thing found is transcript, not the input.
const INPUT_AREA_MAX_ROWS: usize = 40;

/// The design's type: 13.5px text in the box. gpui-component sizes an input's
/// text at 7/8 of a custom size, and pads it 8px across and 2px down.
const TEXT_PX: f32 = 13.5;
const INPUT_PAD_X: f32 = 8.;
const INPUT_PAD_Y: f32 = 2.;

/// The keys the toolbar sends. Shift+Tab cycles the permission mode in every
/// agent that has one; Alt+P and Alt+T are Claude Code's model picker and
/// thinking toggle.
const BACK_TAB: &[u8] = b"\x1b[Z";
const CLAUDE_MODEL_PICKER: &[u8] = b"\x1bp";
const CLAUDE_THINKING_TOGGLE: &[u8] = b"\x1bt";

// ---------------------------------------------------------------------------
// Sending

/// One write of a submission, and how long to wait before making it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Step {
    pub delay: Duration,
    pub bytes: Vec<u8>,
}

/// The writes that hand `text` to `agent` and press Enter on it.
///
/// - **The text and the Enter are separate writes**, [`SETTLE`] apart. In one
///   write, the CR is part of the text as far as the agent can tell.
/// - **A single plain line is typed**, not pasted: it goes in the way the
///   keyboard would have sent it, so the agent treats it like typing — a `/`
///   command opens its menu, nothing is folded into a "pasted text"
///   placeholder. Anything with a line break, a tab or other control
///   character, or past [`types_cleanly`]'s bound goes as one bracketed paste,
///   so the lines are the message's rather than a series of Enters.
/// - **Codex is always pasted.** It watches for bursts of fast keystrokes to
///   spot pastes from terminals that do not bracket them, and the Enter after
///   a typed burst is swallowed into it.
/// - **A leading `!` goes to Claude Code on its own.** It switches the input
///   into shell mode only when it is typed into an empty box as a key of its
///   own; arriving with the rest of the line it is just a character.
///
/// Without bracketed paste switched on, line breaks go as LF: to every agent
/// input that is Ctrl+J, a newline in the message, where a CR would send each
/// line as a message of its own.
pub(super) fn submit_plan(agent: CLIAgent, text: &str, bracketed: bool) -> Vec<Step> {
    let clean: String = text
        .replace("\r\n", "\n")
        .chars()
        .filter(|&c| c != '\x1b')
        .map(|c| if c == '\r' { '\n' } else { c })
        .collect();
    let mut body = clean.trim_end();
    let mut steps = Vec::new();
    let mut delay = Duration::ZERO;

    if agent == CLIAgent::Claude
        && let Some(rest) = body.strip_prefix('!')
        && !rest.is_empty()
    {
        steps.push(Step {
            delay,
            bytes: b"!".to_vec(),
        });
        body = rest;
        delay = SETTLE;
    }

    let mut pasted = false;
    if !body.is_empty() {
        pasted = bracketed && (agent == CLIAgent::Codex || !types_cleanly(body));
        let bytes = match pasted {
            true => tty7_core::core::paste::bracket(body.as_bytes()),
            false => body.as_bytes().to_vec(),
        };
        steps.push(Step { delay, bytes });
    }

    let enter_delay = match (steps.is_empty(), agent) {
        (true, _) => Duration::ZERO,
        (false, CLIAgent::Copilot) if pasted => SETTLE_AFTER_PASTE_SLOW,
        (false, _) => SETTLE,
    };
    steps.push(Step {
        delay: enter_delay,
        bytes: b"\r".to_vec(),
    });
    steps
}

/// The message that goes out: what was written, then the attachments as the
/// words a drop onto the terminal would have typed — which is how an agent is
/// pointed at a file or shown an image.
pub(super) fn compose_message(text: &str, attached: &[String], shell: Option<&str>) -> String {
    let text = text.trim_end();
    if attached.is_empty() {
        return text.to_string();
    }
    let words = pasted_paths_text(attached, shell);
    let words = words.trim_end();
    match text.is_empty() {
        true => words.to_string(),
        false => format!("{text} {words}"),
    }
}

// ---------------------------------------------------------------------------
// Finding the agent's input area

/// Where an agent's input area starts on the screen, and what its status rows
/// say about the permission mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InputArea {
    /// Screen row of the area's first line.
    pub top: usize,
    /// The permission mode, in the agent's own words (`acceptEdits`, …),
    /// when its status rows name one.
    pub mode: Option<&'static str>,
}

/// Whether the box can be laid over this agent's input.
fn covers(agent: CLIAgent) -> bool {
    matches!(agent, CLIAgent::Claude | CLIAgent::Codex | CLIAgent::Gemini)
}

fn is_rule(row: &str, width: usize) -> bool {
    let t = row.trim();
    t.chars().count() >= width * 3 / 5 && t.chars().all(|c| c == '─')
}

/// Find `agent`'s input area in `rows`, the screen's lines top to bottom.
///
/// - **Claude Code** draws its input between two full-width rules with `❯`
///   on the first line, and its status rows under the lower rule.
/// - **Codex** starts its input line with `›`, on a padded block.
/// - **Gemini** frames it in a rounded box whose first line is `> `.
///
/// `None` is the agent showing something else there — which is exactly when
/// the box must get out of the way.
pub(super) fn input_area(agent: CLIAgent, rows: &[String], width: usize) -> Option<InputArea> {
    let floor = rows.len().saturating_sub(INPUT_AREA_MAX_ROWS);
    let starts = |i: usize, p: char| rows[i].trim_start().starts_with(p);
    match agent {
        CLIAgent::Claude => {
            let rules: Vec<usize> = (floor..rows.len())
                .filter(|&i| is_rule(&rows[i], width))
                .collect();
            rules.windows(2).rev().find_map(|pair| {
                let (upper, lower) = (pair[0], pair[1]);
                (upper + 1..lower)
                    .any(|i| starts(i, '❯'))
                    .then(|| InputArea {
                        top: upper,
                        mode: Some(claude_mode(&rows[lower + 1..])),
                    })
            })
        }
        CLIAgent::Codex => {
            let line = (floor..rows.len()).rev().find(|&i| starts(i, '›'))?;
            let top = match line > 0 && rows[line - 1].trim().is_empty() {
                true => line - 1,
                false => line,
            };
            Some(InputArea { top, mode: None })
        }
        CLIAgent::Gemini => {
            let bottom = (floor..rows.len()).rev().find(|&i| starts(i, '╰'))?;
            let top = (floor..bottom).rev().find(|&i| starts(i, '╭'))?;
            (top + 1..bottom)
                .any(|i| {
                    rows[i]
                        .trim_start()
                        .trim_start_matches('│')
                        .trim_start()
                        .starts_with('>')
                })
                .then_some(InputArea { top, mode: None })
        }
        _ => None,
    }
}

/// Claude Code's permission mode, off the status rows under its input. The
/// default mode is the one it does not name.
fn claude_mode(status_rows: &[String]) -> &'static str {
    let text = status_rows.join(" ").to_lowercase();
    [
        ("bypass permissions on", "bypassPermissions"),
        ("accept edits on", "acceptEdits"),
        ("plan mode on", "plan"),
        ("auto mode on", "auto"),
    ]
    .iter()
    .find(|(said, _)| text.contains(said))
    .map_or("default", |(_, mode)| mode)
}

/// The screen as text, one string per line, wide characters' spacer cells
/// left out.
fn screen_rows<T: EventListener>(term: &Term<T>) -> Vec<String> {
    let grid = term.grid();
    (0..grid.screen_lines())
        .map(|l| {
            let row = &grid[Line(l as i32)];
            (0..grid.columns())
                .map(|c| &row[Column(c)])
                .filter(|cell| !cell.flags.contains(Flags::WIDE_CHAR_SPACER))
                .map(|cell| cell.c)
                .collect()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Menus

/// A `/` or `@` being typed at the caret: which one, where it starts, and what
/// follows it so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Trigger {
    pub sigil: char,
    /// Byte offset of the sigil.
    pub start: usize,
    pub query: String,
}

/// The word the caret is at the end of, when it opens with `/` or `@`.
///
/// A `/` counts only as the first thing in the message: that is the only
/// place an agent reads a slash command, and anywhere else it is a path or a
/// fraction. An `@` counts at the start of any word.
pub(super) fn trigger_at(text: &str, cursor: usize) -> Option<Trigger> {
    let before = text.get(..cursor)?;
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let word = &before[start..];
    let sigil = word.chars().next()?;
    let valid = match sigil {
        '/' => before[..start].trim().is_empty(),
        '@' => true,
        _ => false,
    };
    valid.then(|| Trigger {
        sigil,
        start,
        query: word[1..].to_string(),
    })
}

/// One row of the `/` or `@` menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MenuItem {
    pub label: String,
    pub detail: String,
    /// What replaces the trigger word when the row is picked.
    pub insert: String,
    pub is_file: bool,
}

/// The agent's own commands, the ones it ships with. Descriptions stay in the
/// agent's language — they name what its UI will do, in the words it uses.
fn builtin_commands(agent: CLIAgent) -> &'static [(&'static str, &'static str)] {
    match agent {
        CLIAgent::Claude => &[
            ("/compact", "Summarize and free up context"),
            ("/clear", "Start a fresh conversation"),
            ("/review", "Review the current changes"),
            ("/model", "Switch model"),
            ("/init", "Create a CLAUDE.md for this repo"),
            ("/context", "Show context usage"),
            ("/cost", "Show token usage"),
            ("/resume", "Resume a previous conversation"),
            ("/memory", "Edit memory files"),
            ("/permissions", "Manage tool permissions"),
            ("/mcp", "Manage MCP servers"),
            ("/agents", "Manage subagents"),
            ("/config", "Open settings"),
            ("/help", "Show help"),
        ],
        CLIAgent::Codex => &[
            ("/model", "Choose model and reasoning effort"),
            ("/approvals", "Choose what Codex may do without asking"),
            ("/review", "Review the current changes"),
            ("/new", "Start a new chat"),
            ("/compact", "Summarize to free up context"),
            ("/init", "Create an AGENTS.md for this repo"),
            ("/diff", "Show the git diff"),
            ("/mention", "Mention a file"),
            ("/status", "Show session configuration and usage"),
            ("/mcp", "List MCP tools"),
        ],
        CLIAgent::Gemini => &[
            ("/compress", "Summarize to free up context"),
            ("/clear", "Clear the screen and history"),
            ("/memory", "Manage memory"),
            ("/chat", "Save or resume a conversation"),
            ("/tools", "List available tools"),
            ("/mcp", "List MCP servers"),
            ("/stats", "Show session statistics"),
            ("/help", "Show help"),
        ],
        _ => &[],
    }
}

/// Custom slash commands Claude Code reads from `.claude/commands` — the
/// project's under `cwd`, the user's under `home`. A file's name is its
/// command; subdirectories only namespace the description.
fn custom_commands(cwd: Option<&Path>, home: Option<&Path>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let roots = [
        (
            cwd.map(|c| c.join(".claude/commands")),
            L10nKey::ComposerCmdProject,
        ),
        (
            home.map(|h| h.join(".claude/commands")),
            L10nKey::ComposerCmdUser,
        ),
    ];
    for (dir, scope) in roots {
        let Some(dir) = dir else { continue };
        let mut stack = vec![dir];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "md")
                    && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                {
                    let name = format!("/{stem}");
                    if !out.iter().any(|(n, _)| *n == name) {
                        out.push((name, t(scope).to_string()));
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// The `/` menu for `query`: names starting with it first, in the order the
/// agent lists them, then names that merely contain it.
pub(super) fn command_items(commands: &[(String, String)], query: &str) -> Vec<MenuItem> {
    let q = query.to_lowercase();
    let starts = commands
        .iter()
        .filter(|(n, _)| n[1..].to_lowercase().starts_with(&q));
    let contains = commands.iter().filter(|(n, _)| {
        let n = n[1..].to_lowercase();
        !n.starts_with(&q) && n.contains(&q)
    });
    starts
        .chain(contains)
        .take(MENU_ROWS)
        .map(|(name, detail)| MenuItem {
            label: name.clone(),
            detail: detail.clone(),
            insert: name.clone(),
            is_file: false,
        })
        .collect()
}

/// The `@` menu for `query`. The mention is spelled relative to `cwd` — which
/// is what the agent resolves it against — and in full for a file outside it.
fn file_items(index: &FileIndex, query: &str, cwd: Option<&Path>) -> Vec<MenuItem> {
    let spell = |path: &Path| -> String {
        cwd.and_then(|c| path.strip_prefix(c).ok())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    };
    let item = |f: &IndexedFile| MenuItem {
        label: f.name().to_string(),
        detail: f.dir().to_string(),
        insert: format!("@{}", spell(&f.path)),
        is_file: true,
    };
    match query.is_empty() {
        // The walk is breadth-first, so its head is the top of the project.
        true => index.files.iter().take(MENU_ROWS).map(item).collect(),
        false => rank(index, query, MENU_ROWS)
            .into_iter()
            .map(|(_, f)| item(f))
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// The toolbar's readings

/// A model id the way a person says it: `claude-opus-5-5` is Opus 5.5.
/// Anything that is not a Claude id is shown as the agent spelled it.
pub(super) fn model_label(id: &str) -> String {
    let id = id.trim_end_matches("[1m]");
    let Some(rest) = id.strip_prefix("claude-") else {
        return id.to_string();
    };
    let mut parts: Vec<&str> = rest.split('-').collect();
    if parts
        .last()
        .is_some_and(|p| p.len() == 8 && p.bytes().all(|b| b.is_ascii_digit()))
    {
        parts.pop();
    }
    let Some((family, version)) = parts.split_first() else {
        return id.to_string();
    };
    let mut name: String = family
        .chars()
        .enumerate()
        .map(|(i, c)| if i == 0 { c.to_ascii_uppercase() } else { c })
        .collect();
    if !version.is_empty() {
        name.push(' ');
        name.push_str(&version.join("."));
    }
    name
}

/// Tokens the way the status rows count them: `62k`, `1M`.
pub(super) fn tokens_label(n: u64) -> String {
    match n {
        n if n >= 1_000_000 && n % 1_000_000 == 0 => format!("{}M", n / 1_000_000),
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.),
        n if n >= 1_000 => format!("{}k", n / 1_000),
        n => n.to_string(),
    }
}

fn mode_label(mode: &str) -> String {
    match mode {
        "default" => t(L10nKey::ComposerModeDefault).to_string(),
        "acceptEdits" => t(L10nKey::ComposerModeAcceptEdits).to_string(),
        "plan" => t(L10nKey::ComposerModePlan).to_string(),
        "bypassPermissions" => t(L10nKey::ComposerModeBypass).to_string(),
        "auto" => t(L10nKey::ComposerModeAuto).to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// State

/// What a pane's composer holds that outlives the view drawing it.
///
/// A view is rebuilt over the same daemon pane whenever its workspace is
/// switched out and back, and a half-written prompt must not be the price of
/// looking at another workspace. Kept by pane, for the life of the app.
#[derive(Default)]
struct ComposerMemory(HashMap<(HostId, u64), Remembered>);

impl gpui::Global for ComposerMemory {}

#[derive(Default, Clone)]
struct Remembered {
    draft: String,
    attached: Vec<String>,
    open: bool,
}

enum Files {
    Unwalked,
    Walking,
    Ready(Arc<FileIndex>, Instant),
    Failed(Instant),
}

/// How the box is on screen this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Presence {
    /// Not wanted, or no agent to talk to.
    Hidden,
    /// Under the grid, taking its rows.
    Docked,
    /// Over the agent's input area, which starts this many rows up from the
    /// bottom of the screen.
    Covering(usize),
    /// Wanted, but the agent is showing something else where its input goes —
    /// the box waits for it to come back.
    SteppedAside,
}

pub(super) struct Composer {
    pub(super) input: Entity<InputState>,
    /// Whether the user wants the box on this pane.
    open: bool,
    presence: Presence,
    /// The input area as last found, and since when it has been missing.
    area: Option<InputArea>,
    missing_since: Option<Instant>,
    /// The box had the keyboard when it stepped aside, and gets it back when
    /// it returns.
    refocus: bool,
    /// Files to send with the message, spelled the way the pane's host reads
    /// them — uploaded already, for a remote pane.
    attached: Vec<String>,
    /// Text typed at the grid while the box covers the input, and whether the
    /// box should take the keyboard with it. Taken in at the next draw, the
    /// first place with a window to edit and focus in.
    typed: String,
    grab: bool,
    /// Writes waiting their turn. Submissions queue rather than interleave, so
    /// a second message sent inside the first one's settle time cannot land
    /// its text between the first one's text and its Enter.
    queue: VecDeque<Step>,
    pumping: bool,
    /// Whose name the placeholder carries.
    named: Option<CLIAgent>,
    /// The row of the `/` or `@` menu the keyboard is on.
    highlighted: usize,
    /// The text as it stood when Esc closed the menu. The menu stays shut
    /// until the text moves on from there.
    dismissed: Option<String>,
    files: Files,
    commands: Option<(Vec<(String, String)>, Instant)>,
    /// Thinking as toggled from the toolbar, over what the settings say —
    /// the agent does not report the toggle, only the setting.
    thinking: Option<bool>,
    _subs: Vec<Subscription>,
}

impl TerminalView {
    fn composer_key(&self) -> (HostId, u64) {
        (self.host_id(), self.pane_id)
    }

    fn remember_composer(&self, cx: &mut Context<Self>) {
        let Some(c) = self.composer.as_ref() else {
            return;
        };
        let entry = Remembered {
            draft: c.input.read(cx).value().to_string(),
            attached: c.attached.clone(),
            open: c.open,
        };
        let key = self.composer_key();
        let memory = cx.default_global::<ComposerMemory>();
        match entry.draft.is_empty() && entry.attached.is_empty() && !entry.open {
            true => memory.0.remove(&key),
            false => memory.0.insert(key, entry),
        };
    }

    fn ensure_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.composer.is_some() {
            return;
        }
        let remembered = cx
            .try_global::<ComposerMemory>()
            .and_then(|m| m.0.get(&self.composer_key()))
            .cloned()
            .unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(1, MAX_ROWS)
                .submit_on_enter(true)
                .default_value(remembered.draft)
        });
        let subs = vec![cx.subscribe_in(&input, window, Self::on_composer_event)];
        self.composer = Some(Composer {
            input,
            open: remembered.open,
            presence: Presence::Hidden,
            area: None,
            missing_since: None,
            refocus: false,
            attached: remembered.attached,
            typed: String::new(),
            grab: false,
            queue: VecDeque::new(),
            pumping: false,
            named: None,
            highlighted: 0,
            dismissed: None,
            files: Files::Unwalked,
            commands: None,
            thinking: None,
            _subs: subs,
        });
    }

    pub(super) fn presence(&self) -> Presence {
        self.composer
            .as_ref()
            .map_or(Presence::Hidden, |c| c.presence)
    }

    /// Whether the box is on screen.
    pub(super) fn composer_shown(&self) -> bool {
        matches!(self.presence(), Presence::Docked | Presence::Covering(_))
    }

    fn focus_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.composer.as_ref() else {
            return;
        };
        let focus = c.input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        // Said here as well as by the input's own Focus event, which only
        // comes round on the next frame: what is about to land — a drop, a
        // picked file — has to find the box already holding the keyboard.
        self.composer_focused = true;
    }

    /// Open the box and put the caret in it; from inside it, close it; with
    /// it open but the terminal focused, go back into it.
    ///
    /// A pane with no agent in the foreground has nobody to compose for, so
    /// the chord does nothing there.
    pub fn toggle_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.agent().is_none() {
            return;
        }
        self.ensure_composer(window, cx);
        let Some(c) = self.composer.as_mut() else {
            return;
        };
        let focused = c.input.read(cx).focus_handle(cx).is_focused(window);
        match (c.open, focused) {
            (true, true) => {
                c.open = false;
                c.refocus = false;
                window.focus(&self.focus_handle, cx);
            }
            _ => {
                c.open = true;
                c.refocus = true;
                self.focus_composer(window, cx);
            }
        }
        self.remember_composer(cx);
        cx.notify();
    }

    /// Esc in the box. Over the agent's input it is the agent's Esc — the one
    /// that interrupts a turn — since the box is standing in for that input.
    /// Docked, it hands the keyboard back to the terminal and leaves the box
    /// where it is, so the next Esc is the agent's.
    pub(super) fn composer_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.presence() {
            Presence::Covering(_) => self.send_to_pty(b"\x1b", cx),
            _ => {
                window.focus(&self.focus_handle, cx);
                cx.notify();
            }
        }
    }

    /// Ctrl+C in the box: clear what is written, or — with nothing written —
    /// the agent's Ctrl+C.
    pub(super) fn composer_interrupt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.composer.as_mut() else {
            return;
        };
        let empty = c.input.read(cx).value().is_empty() && c.attached.is_empty();
        if empty {
            self.send_to_pty(b"\x03", cx);
            return;
        }
        c.attached.clear();
        c.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.remember_composer(cx);
    }

    /// Typing at the grid while the box covers the agent's input: the input
    /// the keys were meant for is under the box, so they go into the box.
    pub(super) fn composer_takes_typing(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        if self.composer_focused || !matches!(self.presence(), Presence::Covering(_)) {
            return false;
        }
        let Some(c) = self.composer.as_mut() else {
            return false;
        };
        c.typed.push_str(text);
        c.grab = true;
        cx.notify();
        true
    }

    /// Files arriving by way of the terminal's paste path — dropped on the
    /// box, a copied file or a screenshot pasted into it, an upload to a
    /// remote pane landing — become attachments while the box has the
    /// keyboard. Hands them back otherwise, for the terminal to paste.
    pub(super) fn composer_takes_paths(
        &mut self,
        spelled: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Option<Vec<String>> {
        if !self.composer_focused || !self.composer_shown() {
            return Some(spelled);
        }
        let c = self.composer.as_mut()?;
        for path in spelled {
            if !c.attached.contains(&path) {
                c.attached.push(path);
            }
        }
        self.remember_composer(cx);
        cx.notify();
        None
    }

    fn detach(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(c) = self.composer.as_mut()
            && index < c.attached.len()
        {
            c.attached.remove(index);
        }
        self.remember_composer(cx);
        cx.notify();
    }

    /// The attach button: pick files on this computer, which then go the way
    /// dropped files do — uploaded first for a remote pane.
    fn pick_attachments(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_composer(window, cx);
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update_in(cx, |view, window, cx| {
                    view.focus_composer(window, cx);
                    view.paste_local_paths(paths, cx);
                });
            }
        })
        .detach();
    }

    /// Send one of the agent's own keys from the toolbar, keeping the caret in
    /// the box.
    fn toolbar_key(&mut self, bytes: &'static [u8], window: &mut Window, cx: &mut Context<Self>) {
        self.send_to_pty(bytes, cx);
        self.focus_composer(window, cx);
    }

    fn toggle_thinking(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let reported = self.agent_session().and_then(|s| s.readout.thinking);
        if let Some(c) = self.composer.as_mut() {
            c.thinking = Some(!c.thinking.or(reported).unwrap_or(true));
        }
        self.toolbar_key(CLAUDE_THINKING_TOGGLE, window, cx);
    }

    /// Per-frame upkeep: restore a box the pane had open before this view
    /// existed, find the agent's input area, decide where the box goes, and
    /// move the keyboard with it.
    pub(super) fn sync_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let agent = self.agent();
        if self.composer.is_none()
            && agent.is_some()
            && cx
                .try_global::<ComposerMemory>()
                .is_some_and(|m| m.0.contains_key(&self.composer_key()))
        {
            self.ensure_composer(window, cx);
        }
        let asking = self.agent_is_asking();
        let (found, screen_lines, offset) = {
            let term = self.terminal.term.lock();
            let found = agent
                .filter(|a| covers(*a))
                .and_then(|a| input_area(a, &screen_rows(&term), term.columns()));
            (found, term.screen_lines(), term.grid().display_offset())
        };
        let Some(c) = self.composer.as_mut() else {
            return;
        };

        // The area, with a grace period before it counts as gone.
        let now = Instant::now();
        let mut recheck = None;
        match found {
            Some(area) => {
                c.area = Some(area);
                c.missing_since = None;
            }
            None => {
                let since = *c.missing_since.get_or_insert(now);
                if now.duration_since(since) >= STEP_ASIDE_AFTER {
                    c.area = None;
                } else {
                    recheck = Some(STEP_ASIDE_AFTER - now.duration_since(since));
                }
            }
        }

        c.presence = match agent {
            _ if !c.open => Presence::Hidden,
            None => Presence::Hidden,
            Some(a) if !covers(a) => Presence::Docked,
            Some(_) if asking => Presence::SteppedAside,
            Some(_) => match &c.area {
                Some(area) => Presence::Covering(screen_lines.saturating_sub(area.top + offset)),
                None => Presence::SteppedAside,
            },
        };

        if let Some(agent) = agent
            && c.named != Some(agent)
        {
            c.named = Some(agent);
            let key = match builtin_commands(agent).is_empty() {
                true => L10nKey::ComposerPlaceholderFiles,
                false => L10nKey::ComposerPlaceholder,
            };
            let placeholder = t_fmt(key, &[("agent", agent.display_name())]);
            c.input.update(cx, |state, cx| {
                state.set_placeholder(placeholder, window, cx)
            });
        }

        let shown = matches!(c.presence, Presence::Docked | Presence::Covering(_));
        if !shown && self.composer_focused {
            c.refocus = c.presence == Presence::SteppedAside;
            self.composer_focused = false;
            window.focus(&self.focus_handle, cx);
        } else if shown && (c.refocus || c.grab) && !self.composer_focused {
            c.refocus = false;
            c.grab = false;
            let typed = std::mem::take(&mut c.typed);
            let input = c.input.clone();
            if !typed.is_empty() {
                input.update(cx, |state, cx| state.insert(typed, window, cx));
            }
            self.focus_composer(window, cx);
        }

        if let Some(wait) = recheck {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                let _ = this.update(cx, |_, cx| cx.notify());
            })
            .detach();
        }
    }

    fn on_composer_event(
        &mut self,
        _input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                if let Some(c) = self.composer.as_mut() {
                    c.highlighted = 0;
                }
                self.remember_composer(cx);
                self.warm_menu_sources(cx);
            }
            InputEvent::PressEnter { shift: false, .. } => self.submit_composer(window, cx),
            InputEvent::PressEnter { .. } => {}
            InputEvent::Focus => {
                self.composer_focused = true;
                cx.notify();
            }
            InputEvent::Blur => {
                self.composer_focused = false;
                cx.notify();
            }
        }
    }

    fn composer_trigger(&self, cx: &gpui::App) -> Option<Trigger> {
        let c = self.composer.as_ref()?;
        let state = c.input.read(cx);
        let text = state.value();
        if c.dismissed.as_deref() == Some(text.as_ref()) {
            return None;
        }
        trigger_at(&text, state.cursor())
    }

    /// Start what the menu about to open needs: the file walk for `@`, the
    /// custom command list for `/`. Both are kept a while, so typing through
    /// a query does not walk the project once per keystroke.
    fn warm_menu_sources(&mut self, cx: &mut Context<Self>) {
        let Some(trigger) = self.composer_trigger(cx) else {
            return;
        };
        let host_id = self.host_id();
        let local = host_id.is_local();
        let cwd = self.files_cwd();
        let agent = self.agent();
        let Some(c) = self.composer.as_mut() else {
            return;
        };
        match trigger.sigil {
            '/' => {
                if c.commands
                    .as_ref()
                    .is_some_and(|(_, at)| at.elapsed() < SOURCES_FRESH_FOR)
                {
                    return;
                }
                let custom = match (local, agent) {
                    (true, Some(CLIAgent::Claude)) => {
                        let home = std::env::var_os("HOME").map(PathBuf::from);
                        custom_commands(cwd.as_deref(), home.as_deref())
                    }
                    _ => Vec::new(),
                };
                c.commands = Some((custom, Instant::now()));
            }
            '@' => {
                let stale = match &c.files {
                    Files::Unwalked => true,
                    Files::Walking => false,
                    Files::Ready(_, at) | Files::Failed(at) => at.elapsed() > SOURCES_FRESH_FOR,
                };
                let Some(cwd) = cwd.filter(|_| stale) else {
                    return;
                };
                let Some(host) = crate::ui::host_registry::HostRegistry::lookup(cx, host_id) else {
                    return;
                };
                if !matches!(c.files, Files::Ready(..)) {
                    c.files = Files::Walking;
                }
                let home = local
                    .then(|| std::env::var_os("HOME").map(PathBuf::from))
                    .flatten();
                HostOps::run(
                    host,
                    cx,
                    move |h| walk(h, &[cwd], home.as_deref()),
                    |view: &mut TerminalView, list, cx| {
                        let Some(c) = view.composer.as_mut() else {
                            return;
                        };
                        c.files = match list {
                            FileList::Ready(index) => Files::Ready(index, Instant::now()),
                            _ => Files::Failed(Instant::now()),
                        };
                        cx.notify();
                    },
                );
            }
            _ => {}
        }
    }

    /// The `/` or `@` menu as it stands at the caret, if one is open.
    fn composer_menu(&self, cx: &gpui::App) -> Option<(Trigger, Vec<MenuItem>)> {
        let trigger = self.composer_trigger(cx)?;
        let c = self.composer.as_ref()?;
        let items = match trigger.sigil {
            '/' => {
                let agent = self.agent()?;
                let mut all: Vec<(String, String)> = builtin_commands(agent)
                    .iter()
                    .map(|(n, d)| (n.to_string(), d.to_string()))
                    .collect();
                if let Some((custom, _)) = &c.commands {
                    for (name, detail) in custom {
                        if !all.iter().any(|(n, _)| n == name) {
                            all.push((name.clone(), detail.clone()));
                        }
                    }
                }
                command_items(&all, &trigger.query)
            }
            _ => match &c.files {
                Files::Ready(index, _) => {
                    file_items(index, &trigger.query, self.files_cwd().as_deref())
                }
                _ => Vec::new(),
            },
        };
        (!items.is_empty()).then_some((trigger, items))
    }

    /// Replace the word being typed with the picked row, and a space after it
    /// so the next word starts clean.
    fn pick_menu_item(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some((trigger, items)) = self.composer_menu(cx) else {
            return;
        };
        let Some(item) = items.get(index) else {
            return;
        };
        let Some(c) = self.composer.as_ref() else {
            return;
        };
        let input = c.input.clone();
        input.update(cx, |state, cx| {
            let text = state.value().to_string();
            let cursor = state.cursor().min(text.len());
            let inserted = format!("{} ", item.insert);
            let next = format!("{}{inserted}{}", &text[..trigger.start], &text[cursor..]);
            let caret = trigger.start + inserted.len();
            state.set_value(next, window, cx);
            let position = state.text().offset_to_position(caret);
            state.set_cursor_position(position, window, cx);
        });
    }

    fn step_menu(&mut self, forward: bool, cx: &mut Context<Self>) -> bool {
        let Some((_, items)) = self.composer_menu(cx) else {
            return false;
        };
        let Some(c) = self.composer.as_mut() else {
            return false;
        };
        let n = items.len();
        let at = c.highlighted.min(n - 1);
        c.highlighted = match forward {
            true => (at + 1) % n,
            false => (at + n - 1) % n,
        };
        cx.notify();
        true
    }

    /// The agent is asking a question only its TUI can put — a permission
    /// prompt, a choice. Whatever the box sent would be taken as the answer,
    /// so nothing is sent until the question is gone.
    fn agent_is_asking(&self) -> bool {
        self.agent_session()
            .is_some_and(|s| s.status == AgentStatus::Waiting)
    }

    fn composer_can_send(&self, cx: &gpui::App) -> bool {
        self.composer
            .as_ref()
            .is_some_and(|c| !c.attached.is_empty() || !c.input.read(cx).value().trim().is_empty())
            && !self.agent_is_asking()
    }

    pub(super) fn submit_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent) = self.agent() else {
            return;
        };
        if !self.composer_can_send(cx) {
            return;
        }
        let shell = self.shell_program();
        let Some(c) = self.composer.as_mut() else {
            return;
        };
        let message = compose_message(&c.input.read(cx).value(), &c.attached, shell.as_deref());
        let bracketed = self
            .terminal
            .term
            .lock()
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        let steps = submit_plan(agent, &message, bracketed);
        c.attached.clear();
        c.dismissed = None;
        c.queue.extend(steps);
        c.input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.remember_composer(cx);
        self.pump_composer(agent, cx);
    }

    /// Write queued steps one at a time, each after its own delay.
    ///
    /// Every write after a wait checks that `agent` is still what the pane is
    /// running: an agent that quit inside the wait has handed the pty back to
    /// the shell, and the rest of the message — its Enter above all — would
    /// run there as a command.
    fn pump_composer(&mut self, agent: CLIAgent, cx: &mut Context<Self>) {
        let Some(c) = self.composer.as_mut() else {
            return;
        };
        if c.pumping {
            return;
        }
        c.pumping = true;
        cx.spawn(async move |this, cx| {
            loop {
                // Popping the last step and standing the pump down are one
                // update, so a submission can never find it still "pumping"
                // after it has stopped looking at the queue.
                let next = this
                    .update(cx, |view, _| {
                        let c = view.composer.as_mut()?;
                        let step = c.queue.pop_front();
                        c.pumping = step.is_some();
                        step
                    })
                    .ok()
                    .flatten();
                let Some(step) = next else { return };
                if !step.delay.is_zero() {
                    cx.background_executor().timer(step.delay).await;
                }
                let sent = this.update(cx, |view, cx| {
                    if view.agent() != Some(agent) {
                        if let Some(c) = view.composer.as_mut() {
                            c.queue.clear();
                        }
                        return;
                    }
                    view.send_to_pty(&step.bytes, cx);
                });
                if sent.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    // -----------------------------------------------------------------------
    // Drawing

    /// The box, and whether it is docked (laid out under the grid) rather
    /// than laid over it.
    pub(super) fn render_composer(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<(gpui::AnyElement, bool)> {
        let presence = self.presence();
        let covering = match presence {
            Presence::Covering(rows) => Some(rows),
            Presence::Docked => None,
            Presence::Hidden | Presence::SteppedAside => return None,
        };
        let frame = self.render_composer_frame(window, cx)?;
        let element = match covering {
            // Over the input area, painted in the grid's own background so
            // what is under it is gone rather than showing through. At least
            // as tall as the area; taller when the box needs it.
            Some(rows) => div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .min_h(self.line_height * rows as f32 + px(GRID_PAD_Y))
                .flex()
                .flex_col()
                .justify_end()
                .px(px(GRID_PAD_X))
                .pb(px(12.))
                .bg(cx.theme().background)
                .occlude()
                .child(frame)
                .into_any_element(),
            None => div()
                .flex_none()
                .w_full()
                .pt(px(8.))
                .pb(px(8.))
                .child(frame)
                .into_any_element(),
        };
        Some((element, covering.is_none()))
    }

    fn render_composer_frame(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let c = self.composer.as_ref()?;
        let agent = self.agent()?;
        let readout = self.agent_session().map(|s| s.readout).unwrap_or_default();
        let theme = cx.theme();
        let focused = c.input.read(cx).focus_handle(cx).is_focused(window);
        let can_send = self.composer_can_send(cx);
        let ink = theme.foreground;
        let muted = theme.muted_foreground;
        let amber = theme.warning;
        let ring = match focused {
            true => ink.opacity(0.22),
            false => ink.opacity(0.12),
        };
        let menu = self.composer_menu(cx);

        let chips = (!c.attached.is_empty()).then(|| {
            h_flex()
                .flex_wrap()
                .gap(px(6.))
                .px(px(10.))
                .pt(px(10.))
                .children(c.attached.iter().enumerate().map(|(i, path)| {
                    let name = path
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(path)
                        .trim_matches('\'')
                        .to_string();
                    let image = ["png", "jpg", "jpeg", "gif", "webp"]
                        .iter()
                        .any(|ext| name.to_lowercase().ends_with(&format!(".{ext}")));
                    let glyph = match image {
                        true => div()
                            .size(px(18.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .bg(ink.opacity(0.08))
                            .child(
                                gpui::svg()
                                    .path("icons/image.svg")
                                    .size(px(11.))
                                    .text_color(muted),
                            )
                            .into_any_element(),
                        false => gpui::svg()
                            .path("icons/file.svg")
                            .mx(px(2.))
                            .size(px(12.))
                            .text_color(muted)
                            .into_any_element(),
                    };
                    h_flex()
                        .id(("composer-chip", i))
                        .h(px(28.))
                        .max_w(px(260.))
                        .gap(px(7.))
                        .pl(px(6.))
                        .pr(px(4.))
                        .rounded(px(7.))
                        .bg(ink.opacity(0.05))
                        .text_size(px(12.))
                        .tooltip({
                            let path: SharedString = path.clone().into();
                            move |window, cx| Tooltip::new(path.clone()).build(window, cx)
                        })
                        .child(glyph)
                        .child(div().min_w_0().truncate().child(name))
                        .child(
                            div()
                                .id(("composer-chip-remove", i))
                                .size(px(18.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(4.))
                                .hover(|s| s.bg(ink.opacity(0.07)))
                                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                    window.prevent_default();
                                    cx.stop_propagation();
                                })
                                .on_click(cx.listener(move |this, _, _w, cx| this.detach(i, cx)))
                                .child(Icon::new(IconName::Close).size(px(9.)).text_color(muted)),
                        )
                }))
        });

        // A toolbar button: 28px tall, the box's own hover tint.
        let tool = |id: &'static str| {
            h_flex()
                .id(id)
                .flex_none()
                .h(px(28.))
                .gap(px(6.))
                .px(px(9.))
                .rounded(px(7.))
                .text_size(px(12.))
                .text_color(ink.opacity(0.5))
                .hover(move |s| s.bg(ink.opacity(0.05)))
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
        };

        let attach = tool("composer-attach")
            .w(px(28.))
            .px_0()
            .justify_center()
            .tooltip(|window, cx| Tooltip::new(t(L10nKey::ComposerAttach)).build(window, cx))
            .on_click(cx.listener(|this, _, window, cx| this.pick_attachments(window, cx)))
            .child(
                Icon::new(IconName::Plus)
                    .size(px(12.))
                    .text_color(ink.opacity(0.5)),
            );

        let claude = agent == CLIAgent::Claude;
        let mode = claude.then(|| {
            // The status row under the input says it as it changes; the
            // hooks only say it at the next event.
            let mode = c
                .area
                .as_ref()
                .and_then(|a| a.mode)
                .map(str::to_string)
                .or_else(|| readout.permission_mode.clone())
                .unwrap_or_else(|| "default".into());
            let bypass = mode == "bypassPermissions";
            tool("composer-mode")
                .when(bypass, |s| s.text_color(amber))
                .tooltip(|window, cx| Tooltip::new(t(L10nKey::ComposerModeTip)).build(window, cx))
                .on_click(cx.listener(|this, _, window, cx| this.toolbar_key(BACK_TAB, window, cx)))
                .when(bypass, |s| {
                    s.child(div().size(px(5.)).rounded_full().bg(amber))
                })
                .child(mode_label(&mode))
        });
        let divider = claude.then(|| {
            div()
                .flex_none()
                .w(px(1.))
                .h(px(14.))
                .mx(px(4.))
                .bg(ink.opacity(0.12))
        });
        let model = claude.then(|| {
            let label = readout
                .model
                .as_deref()
                .map(model_label)
                .unwrap_or_else(|| t(L10nKey::ComposerModel).to_string());
            tool("composer-model")
                .gap(px(5.))
                .tooltip(|window, cx| Tooltip::new(t(L10nKey::ComposerModelTip)).build(window, cx))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.toolbar_key(CLAUDE_MODEL_PICKER, window, cx)
                }))
                .child(label)
                .child(
                    Icon::new(IconName::ChevronDown)
                        .size(px(9.))
                        .text_color(ink.opacity(0.4)),
                )
        });
        let think = claude.then(|| {
            let on = c.thinking.or(readout.thinking).unwrap_or(true);
            tool("composer-think")
                .when(on, |s| s.bg(ink.opacity(0.05)).text_color(ink))
                .when(!on, |s| s.text_color(ink.opacity(0.45)))
                .tooltip(|window, cx| Tooltip::new(t(L10nKey::ComposerThinkTip)).build(window, cx))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_thinking(window, cx)))
                .child(t(L10nKey::ComposerThink))
        });
        let context = readout
            .context_tokens
            .zip(readout.context_window)
            .filter(|(_, window)| *window > 0)
            .map(|(used, window)| {
                let pct = (used as f64 / window as f64 * 100.).min(100.);
                let tip: SharedString = t_fmt(
                    L10nKey::ComposerContextTip,
                    &[
                        ("used", &tokens_label(used)),
                        ("window", &tokens_label(window)),
                    ],
                )
                .into();
                h_flex()
                    .id("composer-context")
                    .flex_none()
                    .h(px(28.))
                    .gap(px(6.))
                    .px(px(8.))
                    .text_size(px(11.5))
                    .text_color(ink.opacity(0.4))
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .child(
                        ProgressCircle::new("composer-context-ring")
                            .value(pct as f32)
                            .color(ink.opacity(0.5))
                            .size(px(12.)),
                    )
                    .child(format!("{}%", pct.round() as u64))
            });

        let send = div()
            .id("composer-send")
            .flex_none()
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(match can_send {
                true => ink,
                false => ink.opacity(0.07),
            })
            .tooltip(|window, cx| Tooltip::new(t(L10nKey::ComposerSendTip)).build(window, cx))
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(cx.listener(|this, _, window, cx| this.submit_composer(window, cx)))
            .child(
                Icon::new(IconName::ArrowUp)
                    .size(px(12.))
                    .text_color(match can_send {
                        true => theme.background,
                        false => ink.opacity(0.35),
                    }),
            );

        let asking = self.agent_is_asking().then(|| {
            div()
                .min_w_0()
                .truncate()
                .px(px(6.))
                .text_size(px(12.))
                .text_color(amber)
                .child(t_fmt(
                    L10nKey::ComposerAgentAsking,
                    &[("agent", agent.display_name())],
                ))
        });

        let popup = menu.as_ref().map(|(trigger, items)| {
            let highlighted = c.highlighted.min(items.len() - 1);
            let title = match trigger.sigil {
                '/' => t(L10nKey::ComposerMenuCommands),
                _ => t(L10nKey::ComposerMenuFiles),
            };
            div()
                .absolute()
                .left_0()
                .bottom_full()
                .mb(px(6.))
                .w(px(380.))
                .max_w_full()
                .p(px(5.))
                .flex()
                .flex_col()
                .gap(px(1.))
                .rounded(px(10.))
                .bg(theme.popover)
                .border_1()
                .border_color(theme.border)
                .shadow_md()
                .text_size(px(13.))
                .child(
                    div()
                        .h(px(24.))
                        .px(px(9.))
                        .flex()
                        .items_center()
                        .text_size(px(11.5))
                        .text_color(ink.opacity(0.4))
                        .child(title),
                )
                .children(items.iter().enumerate().map(|(i, item)| {
                    h_flex()
                        .id(("composer-menu", i))
                        .h(px(30.))
                        .px(px(9.))
                        .gap(px(10.))
                        .rounded(px(6.))
                        .when(i == highlighted, |s| s.bg(ink.opacity(0.05)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                window.prevent_default();
                                cx.stop_propagation();
                                this.pick_menu_item(i, window, cx);
                            }),
                        )
                        .when(item.is_file, |s| {
                            s.child(
                                gpui::svg()
                                    .path("icons/file.svg")
                                    .size(px(12.))
                                    .text_color(ink.opacity(0.4)),
                            )
                        })
                        .child(
                            div()
                                .flex_none()
                                .font_family(self.font.family.clone())
                                .text_size(px(12.))
                                .child(item.label.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_right()
                                .text_size(px(12.))
                                .text_color(ink.opacity(0.4))
                                .child(item.detail.clone()),
                        )
                }))
        });

        let menu_open = menu.is_some();
        Some(
            div()
                .id("composer")
                .relative()
                .w_full()
                // The terminal surface this sits in focuses the grid on any
                // click and opens its own context menu on a right one. Neither
                // is right for a click on the box.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        window.prevent_default();
                        // A click on the frame around the text, not only on
                        // the text, is a click on the box.
                        if !this.composer_focused {
                            this.focus_composer(window, cx);
                        }
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _: &MouseDownEvent, _w, cx| {
                        this.context_menu_allowed = false;
                        cx.stop_propagation();
                    }),
                )
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                // Files dropped on the box are attachments, not words for the
                // terminal's line; the surface's own drop handler would focus
                // the grid first.
                .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                    cx.stop_propagation();
                    this.focus_composer(window, cx);
                    this.drop_files(paths, cx);
                }))
                .on_drop(cx.listener(
                    |this, drag: &crate::ui::file_tree::RemotePathDrag, window, cx| {
                        cx.stop_propagation();
                        this.focus_composer(window, cx);
                        this.drop_remote_path(drag, cx);
                    },
                ))
                .capture_action(cx.listener(|this, _: &input::Paste, _w, cx| {
                    // Text pastes are the box's own. A copied file or a
                    // screenshot has no text to paste, and the terminal
                    // already knows how to turn those into a path.
                    let Some(item) = cx.read_from_clipboard() else {
                        return;
                    };
                    if item.text().is_some() && !super::view::clipboard_has_paths(&item) {
                        return;
                    }
                    cx.stop_propagation();
                    this.paste_from_clipboard(cx);
                }))
                // Shift+Tab is the agent's: it cycles the permission mode.
                .capture_action(cx.listener(|this, _: &input::OutdentInline, _w, cx| {
                    cx.stop_propagation();
                    this.send_to_pty(BACK_TAB, cx);
                }))
                .capture_action(cx.listener(|this, _: &input::Backspace, _w, cx| {
                    let Some(c) = this.composer.as_ref() else {
                        return;
                    };
                    let state = c.input.read(cx);
                    if state.cursor() != 0 || !state.selected_range().is_empty() {
                        return;
                    }
                    if let Some(last) = c.attached.len().checked_sub(1) {
                        cx.stop_propagation();
                        this.detach(last, cx);
                    }
                }))
                .when(menu_open, |el| {
                    el.capture_action(cx.listener(|this, _: &input::MoveUp, _w, cx| {
                        if this.step_menu(false, cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .capture_action(cx.listener(|this, _: &input::MoveDown, _w, cx| {
                        if this.step_menu(true, cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .capture_action(cx.listener(|this, action: &input::Enter, window, cx| {
                        if action.shift {
                            return;
                        }
                        cx.stop_propagation();
                        let at = this.composer.as_ref().map_or(0, |c| c.highlighted);
                        this.pick_menu_item(at, window, cx);
                    }))
                    .capture_action(cx.listener(|this, _: &input::IndentInline, window, cx| {
                        cx.stop_propagation();
                        let at = this.composer.as_ref().map_or(0, |c| c.highlighted);
                        this.pick_menu_item(at, window, cx);
                    }))
                    .capture_action(cx.listener(
                        |this, _: &input::Escape, _w, cx| {
                            cx.stop_propagation();
                            if let Some(c) = this.composer.as_mut() {
                                c.dismissed = Some(c.input.read(cx).value().to_string());
                            }
                            cx.notify();
                        },
                    ))
                })
                .children(popup)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .rounded(px(12.))
                        .bg(ink.opacity(0.035))
                        .border_1()
                        .border_color(ring)
                        .drag_over::<ExternalPaths>(move |s, _, _, _| {
                            s.border_color(ink.opacity(0.45))
                        })
                        .children(chips)
                        .child(
                            div()
                                .min_h(px(44.))
                                .px(px(14. - INPUT_PAD_X))
                                .pt(px(12. - INPUT_PAD_Y))
                                .pb(px(4. - INPUT_PAD_Y))
                                .child(
                                    Input::new(&c.input)
                                        .appearance(false)
                                        .with_size(Size::Size(px(TEXT_PX / 0.875))),
                                ),
                        )
                        .child(
                            h_flex()
                                .h(px(40.))
                                .px(px(6.))
                                .gap(px(2.))
                                .child(attach)
                                .children(mode)
                                .children(divider)
                                .children(model)
                                .children(think)
                                .children(asking)
                                .child(div().flex_1())
                                .children(context)
                                .child(send),
                        ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(steps: &[Step]) -> Vec<&[u8]> {
        steps.iter().map(|s| s.bytes.as_slice()).collect()
    }

    #[test]
    fn a_plain_line_is_typed_then_entered_separately() {
        let steps = submit_plan(CLIAgent::Claude, "fix the tests", true);
        assert_eq!(bytes(&steps), [&b"fix the tests"[..], b"\r"]);
        assert_eq!(steps[0].delay, Duration::ZERO);
        assert_eq!(steps[1].delay, SETTLE);
    }

    #[test]
    fn several_lines_go_as_one_paste() {
        let steps = submit_plan(CLIAgent::Claude, "one\r\ntwo\n", true);
        assert_eq!(bytes(&steps), [&b"\x1b[200~one\ntwo\x1b[201~"[..], b"\r"]);
    }

    #[test]
    fn without_bracketed_paste_line_breaks_stay_newlines_not_enters() {
        let steps = submit_plan(CLIAgent::Gemini, "one\ntwo", false);
        assert_eq!(bytes(&steps), [&b"one\ntwo"[..], b"\r"]);
    }

    #[test]
    fn codex_is_always_pasted() {
        let steps = submit_plan(CLIAgent::Codex, "hi", true);
        assert_eq!(bytes(&steps), [&b"\x1b[200~hi\x1b[201~"[..], b"\r"]);
    }

    #[test]
    fn a_leading_bang_reaches_claude_as_a_key_of_its_own() {
        let steps = submit_plan(CLIAgent::Claude, "!git status", true);
        assert_eq!(bytes(&steps), [&b"!"[..], b"git status", b"\r"]);
        assert_eq!(steps[1].delay, SETTLE);
        // Anyone else gets the line as written.
        let steps = submit_plan(CLIAgent::Gemini, "!git status", true);
        assert_eq!(bytes(&steps), [&b"!git status"[..], b"\r"]);
    }

    #[test]
    fn escapes_cannot_close_the_paste_early() {
        let steps = submit_plan(CLIAgent::Claude, "a\n\x1b[201~b", true);
        assert_eq!(bytes(&steps)[0], b"\x1b[200~a\n[201~b\x1b[201~");
    }

    #[test]
    fn copilot_waits_longer_after_a_paste() {
        let steps = submit_plan(CLIAgent::Copilot, "a\nb", true);
        assert_eq!(steps[1].delay, SETTLE_AFTER_PASTE_SLOW);
        let steps = submit_plan(CLIAgent::Copilot, "ab", true);
        assert_eq!(steps[1].delay, SETTLE);
    }

    #[test]
    fn attachments_follow_the_text_as_shell_words() {
        let attached = vec!["/tmp/a b.png".to_string(), "/src/x.rs".to_string()];
        assert_eq!(
            compose_message("look at these\n", &attached, Some("zsh")),
            "look at these '/tmp/a b.png' /src/x.rs"
        );
        assert_eq!(
            compose_message("", &attached[1..], Some("zsh")),
            "/src/x.rs"
        );
        assert_eq!(
            compose_message("just text  ", &[], Some("zsh")),
            "just text"
        );
    }

    fn screen(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    const RULE: &str = "────────────────────────────────────────";

    #[test]
    fn claudes_input_is_the_ruled_block_with_the_prompt_in_it() {
        let rows = screen(&[
            "⏺ Done. The tests pass.",
            "",
            RULE,
            "❯ Try \"fix lint errors\"",
            RULE,
            "  ⚠ Transcript saving is off",
            "  ►► bypass permissions on (shift+tab to cycle)",
        ]);
        assert_eq!(
            input_area(CLIAgent::Claude, &rows, 40),
            Some(InputArea {
                top: 2,
                mode: Some("bypassPermissions")
            })
        );
    }

    #[test]
    fn claudes_default_mode_is_the_one_it_does_not_name() {
        let rows = screen(&[RULE, "❯ ", RULE, "  ? for shortcuts"]);
        assert_eq!(
            input_area(CLIAgent::Claude, &rows, 40).and_then(|a| a.mode),
            Some("default")
        );
        let rows = screen(&[RULE, "❯ ", RULE, "  ⏸ plan mode on (shift+tab to cycle)"]);
        assert_eq!(
            input_area(CLIAgent::Claude, &rows, 40).and_then(|a| a.mode),
            Some("plan")
        );
    }

    /// A permission prompt or a picker takes the input's place: no prompt
    /// line between the rules, so no input area, so the box steps aside.
    #[test]
    fn claude_asking_something_is_not_an_input_area() {
        let rows = screen(&[
            RULE,
            " Do you want to make this edit to route.py?",
            " ❯ 1. Yes",
            "   2. No",
        ]);
        assert_eq!(input_area(CLIAgent::Claude, &rows, 40), None);
        let rows = screen(&["some output", "", "  ────── a short rule ──"]);
        assert_eq!(input_area(CLIAgent::Claude, &rows, 40), None);
    }

    #[test]
    fn codex_input_starts_on_its_prompt_line_or_the_padding_above_it() {
        let rows = screen(&[
            "• Ran tests",
            "",
            "› Ask Codex to do anything",
            "",
            "  ⏎ send",
        ]);
        assert_eq!(
            input_area(CLIAgent::Codex, &rows, 40),
            Some(InputArea { top: 1, mode: None })
        );
    }

    #[test]
    fn geminis_input_is_its_framed_prompt() {
        let rows = screen(&[
            "✦ Here you go.",
            "╭──────────────────────╮",
            "│ >   Type your message │",
            "╰──────────────────────╯",
            "~/code  (main)  gemini-2.5-pro",
        ]);
        assert_eq!(
            input_area(CLIAgent::Gemini, &rows, 40),
            Some(InputArea { top: 1, mode: None })
        );
    }

    #[test]
    fn model_ids_read_the_way_people_say_them() {
        assert_eq!(model_label("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_label("claude-sonnet-5-5[1m]"), "Sonnet 5.5");
        assert_eq!(model_label("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(model_label("gpt-5-codex"), "gpt-5-codex");
    }

    #[test]
    fn token_counts_read_the_way_status_rows_count_them() {
        assert_eq!(tokens_label(62_400), "62k");
        assert_eq!(tokens_label(1_000_000), "1M");
        assert_eq!(tokens_label(1_250_000), "1.2M");
        assert_eq!(tokens_label(900), "900");
    }

    #[test]
    fn a_slash_opens_the_menu_only_at_the_start_of_the_message() {
        let t = |text: &str| trigger_at(text, text.len());
        assert_eq!(
            t("/comp"),
            Some(Trigger {
                sigil: '/',
                start: 0,
                query: "comp".into()
            })
        );
        assert_eq!(t("  /").map(|t| t.start), Some(2));
        assert_eq!(t("see src/main.rs"), None);
        assert_eq!(t("fix /tmp"), None);
    }

    #[test]
    fn an_at_opens_the_menu_at_the_start_of_any_word() {
        let text = "look at @src/ma and";
        let cursor = "look at @src/ma".len();
        assert_eq!(
            trigger_at(text, cursor),
            Some(Trigger {
                sigil: '@',
                start: 8,
                query: "src/ma".into()
            })
        );
        assert_eq!(trigger_at("mail me@host", 12), None);
        assert_eq!(
            trigger_at("done @x ", 8),
            None,
            "a finished word is not a query"
        );
    }

    #[test]
    fn commands_that_start_with_the_query_come_before_ones_that_contain_it() {
        let all: Vec<(String, String)> = [("/compact", ""), ("/clear", ""), ("/memory", "")]
            .iter()
            .map(|(n, d)| (n.to_string(), d.to_string()))
            .collect();
        let labels = |q| {
            command_items(&all, q)
                .into_iter()
                .map(|i| i.label)
                .collect::<Vec<_>>()
        };
        assert_eq!(labels("c"), ["/compact", "/clear"]);
        assert_eq!(labels("m"), ["/memory", "/compact"]);
        assert_eq!(labels(""), ["/compact", "/clear", "/memory"]);
    }

    #[test]
    fn a_file_mention_is_spelled_from_the_panes_directory() {
        let index = crate::ui::search::files::build_index(
            &[PathBuf::from("/repo")],
            vec![
                PathBuf::from("/repo/app/src/main.rs"),
                PathBuf::from("/repo/README.md"),
            ],
            false,
        );
        let items = file_items(&index, "main", Some(Path::new("/repo/app")));
        assert_eq!(items[0].insert, "@src/main.rs");
        let items = file_items(&index, "readme", Some(Path::new("/repo/app")));
        assert_eq!(
            items[0].insert, "@/repo/README.md",
            "outside the cwd: in full"
        );
    }

    #[test]
    fn claude_custom_commands_come_from_the_project_and_the_user() {
        let dir = std::env::temp_dir().join(format!("tty7-cmds-{}", std::process::id()));
        let project = dir.join("proj");
        let home = dir.join("home");
        std::fs::create_dir_all(project.join(".claude/commands/team")).unwrap();
        std::fs::create_dir_all(home.join(".claude/commands")).unwrap();
        std::fs::write(project.join(".claude/commands/ship.md"), "").unwrap();
        std::fs::write(project.join(".claude/commands/team/triage.md"), "").unwrap();
        std::fs::write(home.join(".claude/commands/standup.md"), "").unwrap();
        std::fs::write(home.join(".claude/commands/notes.txt"), "").unwrap();
        let names: Vec<String> = custom_commands(Some(&project), Some(&home))
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(names, ["/ship", "/standup", "/triage"]);
    }
}

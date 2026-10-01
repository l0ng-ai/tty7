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
//! (Shift+Tab, its `/model` and `/effort` commands), and what they show is
//! what the agent said: the mode off the status row the box covers, the model
//! and context off what its hooks report ([`crate::core::cli_agent::AgentReadout`]). Nothing reported
//! means nothing shown.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, Rgb};
use gpui::{
    Context, Entity, ExternalPaths, Focusable as _, MouseButton, MouseDownEvent, Pixels,
    SharedString, Subscription, Window, div, prelude::*, px,
};
use gpui_component::input::{self, Input, InputEvent, InputState, RopeExt as _};
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, Size, h_flex};

use super::view::{GRID_PAD_X, GRID_PAD_Y, TerminalView, pasted_paths_text, types_cleanly};
use crate::core::cli_agent::{AgentStatus, CLIAgent};
use crate::core::shell_quote::{Quoting, quoting_for};
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

/// Between keys an agent must read one at a time: apart enough that each
/// arrives as a read of its own.
const KEY_GAP: Duration = Duration::from_millis(15);

/// Copilot's input treats a CR that follows a paste too closely as part of it.
/// So does Gemini's: it takes an Enter within 40ms of a paste it has finished
/// reading for a line break — and a long paste takes it a while to read, so
/// [`SETTLE`] after the write is not 40ms after that.
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
/// How far the box stands in from the pane's sides and bottom.
const BOX_INSET: f32 = 16.;

/// The key the toolbar sends: Shift+Tab cycles the permission mode in every
/// agent that has one.
const BACK_TAB: &[u8] = b"\x1b[Z";

/// The models `/model` takes by alias, in the order its own list gives the
/// families, with the 1M-context variants it offers. An account without one
/// says so itself.
const CLAUDE_MODELS: [&str; 7] = [
    "default",
    "opus",
    "opus[1m]",
    "fable",
    "sonnet",
    "sonnet[1m]",
    "haiku",
];

/// Claude Code's effort levels, lowest first. A model that takes fewer says so
/// itself when `/effort` names one it does not.
const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

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
/// - **So is Gemini**, and Qwen Code and Qoder with it
///   ([`reads_like_gemini`]). It judges each key of a burst against the
///   input as it was before the burst, so every `?` in a typed line reads as
///   the key that
///   opens its shortcuts on an empty input and is lost, and every `!` as the
///   one that switches it into shell mode — "hi! there" would run `there`.
/// - **So are OpenCode and Amp.** OpenCode reads a typed burst the same way
///   Gemini does — the `!` in "PONG! ok" switches it into shell mode and the
///   line runs as a command — and Amp takes a typed leading `?` for the key
///   that opens its shortcuts, and a typed leading `/` for the one that opens
///   its command palette, with the rest of the burst landing in the input
///   under it and Enter running whatever the palette had on top.
/// - **So is Copilot.** A typed leading `?` is the key that opens its help,
///   and the rest of the line goes out without it. Its shell mode is a
///   leading `!` it reads off a paste as well.
/// - **So is Kimi Code.** It takes keys arriving faster than typing for an
///   unbracketed paste, and the Enter right behind a typed line for one of its
///   newlines: the message sat in its input, never sent. A bracketed paste it
///   knows for one, and reads its `!`, `?` and `/` off it as typed.
/// - **So is Prime Agent.** Its `@` list opens as a typed mention is
///   typed, stays open past the space after it, and takes the Enter: "read
///   @a.txt " went out as "read @a.txt .git/". A paste it reads whole, its
///   `!` and `/` included.
/// - **A leading `!` goes to Claude Code, CodeBuddy, Gemini, Qwen Code and
///   OpenCode on its own.** It
///   switches the input into shell mode only when it is typed into an empty
///   box as a key of its own; arriving with the rest of the line it is just a
///   character. (Amp's shell mode is a leading `$`, which it reads off a paste
///   as well.)
/// - **A message with an `@` mention gets a space after it for Gemini, Qwen
///   Code, Copilot, Oh My Pi and Grok Build.**
///   With the caret at the end of an `@` word it lists matching files, and
///   Enter then takes the list's pick instead of sending — so a message whose
///   attachments close it would sit in the input, under the box, unsent.
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

    if (matches!(
        agent,
        CLIAgent::Claude | CLIAgent::CodeBuddy | CLIAgent::OpenCode
    ) || reads_like_gemini(agent))
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

    let spaced;
    // Where the last word starts is a matter of Gemini's escaping and quoting,
    // so any `@` will do: a space after the message costs nothing. Not after a
    // command, though, where it would open the list of its arguments.
    if (reads_like_gemini(agent)
        || matches!(agent, CLIAgent::Copilot | CLIAgent::OhMyPi | CLIAgent::Grok))
        && body.contains('@')
        && !body.starts_with('/')
    {
        spaced = format!("{body} ");
        body = &spaced;
    }

    let mut pasted = false;
    if !body.is_empty() {
        pasted = bracketed
            && (matches!(
                agent,
                CLIAgent::Codex
                    | CLIAgent::OpenCode
                    | CLIAgent::Amp
                    | CLIAgent::Copilot
                    | CLIAgent::Kimi
                    | CLIAgent::PrimeAgent
            ) || reads_like_gemini(agent)
                || !types_cleanly(body));
        let bytes = match pasted {
            true => tty7_core::core::paste::bracket(body.as_bytes()),
            false => body.as_bytes().to_vec(),
        };
        steps.push(Step { delay, bytes });
    }

    let enter_delay = match (steps.is_empty(), agent) {
        (true, _) => Duration::ZERO,
        (false, CLIAgent::Copilot) if pasted => SETTLE_AFTER_PASTE_SLOW,
        (false, agent) if pasted && reads_like_gemini(agent) => SETTLE_AFTER_PASTE_SLOW,
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
///
/// Gemini (and Qwen Code and Qoder with it) is the exception. It turns a drop
/// into `@` mentions itself, but only a paste that is nothing but paths; after the
/// message's text, a path is just words to it and the file never reaches the
/// model. So its attachments go as the mentions it would have made.
///
/// Copilot and CodeBuddy attach a file only from an `@` mention — CodeBuddy
/// reads it, an image included, into the turn, Copilot tags it — and a path
/// is just words to them. Neither reads a mention back past a space, though,
/// so a path with one still goes as words.
pub(super) fn compose_message(
    agent: CLIAgent,
    text: &str,
    attached: &[String],
    shell: Option<&str>,
) -> String {
    let text = text.trim_end();
    if attached.is_empty() {
        return text.to_string();
    }
    let words = match agent {
        _ if reads_like_gemini(agent) => attached
            .iter()
            .map(|p| mention(Some(agent), p, shell))
            .collect::<Vec<_>>()
            .join(" "),
        CLIAgent::Copilot | CLIAgent::CodeBuddy => attached
            .iter()
            .map(|p| match p.chars().any(char::is_whitespace) {
                true => pasted_paths_text(std::slice::from_ref(p), shell),
                false => format!("@{p}"),
            })
            .collect::<Vec<_>>()
            .join(" "),
        // Oh My Pi reads a mentioned file — an image included — into the
        // turn itself; a path is just words to it.
        CLIAgent::OhMyPi => attached
            .iter()
            .map(|p| mention(Some(agent), p, shell))
            .collect::<Vec<_>>()
            .join(" "),
        _ => pasted_paths_text(attached, shell),
    };
    let words = words.trim_end();
    match text.is_empty() {
        true => words.to_string(),
        false => format!("{text} {words}"),
    }
}

/// Whether `agent` takes a file only from a paste that is nothing but its
/// path. OpenCode and Amp turn such a paste of an image into an attached
/// image, and leave a path that follows the message's text as words — the
/// image never reaches the model.
fn attaches_pasted_paths(agent: CLIAgent) -> bool {
    matches!(agent, CLIAgent::OpenCode | CLIAgent::Amp)
}

/// The writes that send `text` with `attached` — [`compose_message`] handed
/// to [`submit_plan`], except for an agent that takes a file only from a
/// paste of its own ([`attaches_pasted_paths`]). That one gets the text, then
/// each path as a paste by itself after a typed space, and Enter once the
/// last of them has had time to land.
pub(super) fn submit_message(
    agent: CLIAgent,
    text: &str,
    attached: &[String],
    shell: Option<&str>,
    bracketed: bool,
) -> Vec<Step> {
    if attached.is_empty() || !bracketed || !attaches_pasted_paths(agent) {
        return submit_plan(
            agent,
            &compose_message(agent, text, attached, shell),
            bracketed,
        );
    }
    let mut steps = submit_plan(agent, text, bracketed);
    let enter = steps.pop();
    for path in attached {
        if !steps.is_empty() {
            steps.push(Step {
                delay: SETTLE,
                bytes: b" ".to_vec(),
            });
        }
        let delay = match steps.is_empty() {
            true => Duration::ZERO,
            false => SETTLE,
        };
        steps.push(Step {
            delay,
            bytes: tty7_core::core::paste::bracket(path.as_bytes()),
        });
    }
    steps.extend(enter.map(|enter| Step {
        delay: SETTLE_AFTER_PASTE_SLOW,
        ..enter
    }));
    steps
}

/// How long OpenCode's `@` list takes to search the project for a query.
const OPENCODE_LIST_WAIT: Duration = Duration::from_millis(700);

/// One stretch of a message cut at its mentions — see [`mention_pieces`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Piece<'a> {
    Text(&'a str),
    /// The path of an `@` mention, without the `@`.
    Mention(&'a str),
}

/// `text` cut at its `@` mentions of paths that `exists` — an `@` at the
/// start of a word, followed by the longest run of up to a few words that
/// names one, a closing `.`, `,` and the like left out. An `@` that names
/// nothing is left in the text.
pub(super) fn mention_pieces<'a>(text: &'a str, exists: impl Fn(&str) -> bool) -> Vec<Piece<'a>> {
    const MAX_WORDS: usize = 4;
    let mut pieces = Vec::new();
    let mut from = 0;
    let mut at = 0;
    while let Some(found) = text[at..].find('@') {
        let sign = at + found;
        at = sign + 1;
        let starts_word = text[..sign]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace);
        if !starts_word {
            continue;
        }
        let rest = &text[sign + 1..];
        // Where each of the next few words ends.
        let mut ends = Vec::new();
        let mut in_word = false;
        for (i, c) in rest.char_indices() {
            match (c.is_whitespace(), in_word) {
                (true, true) => {
                    ends.push(i);
                    in_word = false;
                    if ends.len() == MAX_WORDS {
                        break;
                    }
                }
                (false, false) => in_word = true,
                _ => {}
            }
        }
        if in_word && ends.len() < MAX_WORDS {
            ends.push(rest.len());
        }
        let path = ends.iter().rev().find_map(|&end| {
            let path = rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '"', '\'']);
            (!path.is_empty() && exists(path)).then_some(path)
        });
        if let Some(path) = path {
            if from < sign {
                pieces.push(Piece::Text(&text[from..sign]));
            }
            pieces.push(Piece::Mention(path));
            from = sign + 1 + path.len();
            at = from;
        }
    }
    if from < text.len() {
        pieces.push(Piece::Text(&text[from..]));
    }
    pieces
}

/// The writes that send OpenCode a message whose `pieces` mention files.
///
/// OpenCode attaches a file to the message only when it was picked from its
/// own `@` list; a mention typed or pasted is words to it, with or without a
/// space in the path, and the model has to go looking for the file. So each
/// mention goes the way a pick does: a typed `@` opens the list, the path —
/// its spaces left out, as the list would end at one — goes in as the query,
/// and Enter, once the list has had time to search, takes its top match.
/// A space after it closes a list that found nothing, which would take the
/// Enter that sends.
///
/// The `attached` paths follow, each a paste of its own as
/// [`submit_message`] hands them over.
///
/// `None` for a message without mentions, a command or a shell line, which
/// go the way [`submit_message`] sends them.
pub(super) fn opencode_mention_plan(
    pieces: &[Piece],
    attached: &[String],
    bracketed: bool,
) -> Option<Vec<Step>> {
    let first = match pieces.first()? {
        Piece::Text(t) => t.trim_start(),
        Piece::Mention(_) => "",
    };
    if !bracketed
        || first.starts_with(['!', '/'])
        || !pieces.iter().any(|p| matches!(p, Piece::Mention(_)))
    {
        return None;
    }
    let mut steps: Vec<Step> = Vec::new();
    let step = |steps: &mut Vec<Step>, delay: Duration, bytes: Vec<u8>| {
        let delay = match steps.is_empty() {
            true => Duration::ZERO,
            false => delay,
        };
        steps.push(Step { delay, bytes });
    };
    for (i, piece) in pieces.iter().enumerate() {
        match piece {
            Piece::Text(text) => {
                let text = match i + 1 == pieces.len() {
                    true => text.trim_end(),
                    false => text,
                };
                // A pick ends in a space of its own.
                let text = match i > 0 {
                    true => text.strip_prefix(' ').unwrap_or(text),
                    false => text,
                };
                if !text.is_empty() {
                    step(
                        &mut steps,
                        SETTLE,
                        tty7_core::core::paste::bracket(text.as_bytes()),
                    );
                }
            }
            Piece::Mention(path) => {
                let query: String = path.chars().filter(|c| !c.is_whitespace()).collect();
                step(&mut steps, SETTLE, b"@".to_vec());
                step(
                    &mut steps,
                    SETTLE,
                    tty7_core::core::paste::bracket(query.as_bytes()),
                );
                step(&mut steps, OPENCODE_LIST_WAIT, b"\r".to_vec());
                // What follows closes a list that found nothing when it
                // has a space in it; otherwise a space of its own does.
                let spaced = match pieces.get(i + 1) {
                    Some(Piece::Text(next)) => next
                        .strip_prefix(' ')
                        .unwrap_or(next)
                        .trim_end()
                        .contains(char::is_whitespace),
                    _ => false,
                };
                if !spaced {
                    step(&mut steps, SETTLE, b" ".to_vec());
                }
            }
        }
    }
    for path in attached {
        step(&mut steps, SETTLE, b" ".to_vec());
        step(
            &mut steps,
            SETTLE,
            tty7_core::core::paste::bracket(path.as_bytes()),
        );
    }
    step(&mut steps, SETTLE_AFTER_PASTE_SLOW, b"\r".to_vec());
    Some(steps)
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

/// Whether `agent` reads its input the way Gemini CLI does. Qwen Code and
/// Qoder (both builds) are forks of it and kept its input: the burst-typing
/// traps, the shell-mode `!`, the `@` list that takes Enter, the Enter right
/// behind a paste taken for a newline, and `@` mentions escaped the same way.
fn reads_like_gemini(agent: CLIAgent) -> bool {
    matches!(
        agent,
        CLIAgent::Gemini | CLIAgent::Qwen | CLIAgent::QoderCLI | CLIAgent::QoderCLICn
    )
}

/// Whether the box can be laid over this agent's input.
fn covers(agent: CLIAgent) -> bool {
    matches!(
        agent,
        CLIAgent::Claude
            | CLIAgent::Codex
            | CLIAgent::Gemini
            | CLIAgent::Pi
            | CLIAgent::OhMyPi
            | CLIAgent::PrimeAgent
            | CLIAgent::Grok
            | CLIAgent::Crush
            | CLIAgent::Goose
            | CLIAgent::Amp
            | CLIAgent::Copilot
            | CLIAgent::Qwen
            | CLIAgent::CodeBuddy
            | CLIAgent::Kimi
            | CLIAgent::OpenCode
            | CLIAgent::QoderCLI
            | CLIAgent::QoderCLICn
    )
}

/// Whether the grid gives up rows for the box covering `agent`'s input — see
/// [`TerminalView::composer_reserved_rows`]. Prime Agent's four-row block
/// falls a row short, which cut its hint line above in half.
fn makes_room(agent: CLIAgent) -> bool {
    matches!(
        agent,
        CLIAgent::OhMyPi | CLIAgent::PrimeAgent | CLIAgent::Goose
    )
}

/// Whole rows of `line` it takes to make up what `area` rows of `line`, with
/// `below` under them, fall short of `need` — none when they don't.
pub(super) fn rows_short(need: Pixels, line: Pixels, area: usize, below: Pixels) -> usize {
    let line = line.as_f32();
    if line <= 0. {
        return 0;
    }
    let short = need.as_f32() - line * area as f32 - below.as_f32();
    match short > 0. {
        true => (short / line).ceil() as usize,
        false => 0,
    }
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
/// - **Gemini** shades it as a block whose first line is `> ` — see
///   [`gemini_input_top`] for its other drawings.
/// - **Pi** draws it between two rules, over its status rows — see
///   [`pi_input_top`].
/// - **Oh My Pi** hangs it under its status line — see [`omp_input_top`].
/// - **Prime Agent** shades it as a block over its footer — see
///   [`prime_input_top`].
/// - **Grok Build** frames it in a rounded box — see [`grok_input_top`].
/// - **Crush** draws its editor over its key help, its dialogs framed over
///   it — see [`crush_input_top`].
/// - **Goose** prints its prompt under its context gauge, wherever its output
///   ended — see [`goose_input_top`].
/// - **Amp** frames it at the bottom of the screen — see [`amp_input_top`].
/// - **Copilot**, **Qwen Code** and **CodeBuddy** rule it off top and bottom,
///   over their status rows — see [`ruled_input_top`].
/// - **Kimi Code** frames it over its model line — see [`kimi_input_top`].
/// - **OpenCode** draws it on a bar over its key hints — see
///   [`opencode_input_top`].
/// - **Qoder** rules it off under its mode line, over its model line — see
///   [`qoder_input`].
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
            // Codex points at the selected row of its own lists — approvals,
            // `/model`, `/permissions`, the hook review, the update offer —
            // with the same `›`. Such a row is a numbered option with its
            // siblings around it; the input is not.
            let option = |row: &str| numbered_option(row.trim_start().trim_start_matches('›'));
            if option(&rows[line])
                && (line.saturating_sub(CODEX_LIST_REACH)..rows.len())
                    .any(|i| i != line && option(&rows[i]))
            {
                return None;
            }
            let top = match line > 0 && rows[line - 1].trim().is_empty() {
                true => line - 1,
                false => line,
            };
            Some(InputArea { top, mode: None })
        }
        CLIAgent::Gemini => gemini_input_top(rows, floor, width).map(|top| InputArea {
            top: over_mode_line(rows, floor, width, top),
            mode: None,
        }),
        CLIAgent::Pi => pi_input_top(rows, floor, width).map(|top| InputArea { top, mode: None }),
        CLIAgent::OhMyPi => {
            omp_input_top(rows, floor, width).map(|top| InputArea { top, mode: None })
        }
        CLIAgent::PrimeAgent => {
            prime_input_top(rows, floor).map(|top| InputArea { top, mode: None })
        }
        CLIAgent::Grok => {
            grok_input_top(rows, floor, width).map(|top| InputArea { top, mode: None })
        }
        CLIAgent::Crush => crush_input_top(rows, floor).map(|top| InputArea { top, mode: None }),
        // Goose's input is the last thing printed, wherever that is: nothing
        // is drawn under it, so it counts from any height.
        CLIAgent::Goose => goose_input_top(rows).map(|top| InputArea { top, mode: None }),
        CLIAgent::Amp => amp_input_top(rows, floor, width).map(|top| InputArea { top, mode: None }),
        CLIAgent::Copilot => copilot_shaded_input(rows, floor, width)
            .map(|(top, _)| top)
            .or_else(|| ruled_input_top(rows, floor, width, ruled_prompts(agent)))
            .map(|top| InputArea { top, mode: None }),
        CLIAgent::Qwen | CLIAgent::CodeBuddy => {
            ruled_input_top(rows, floor, width, ruled_prompts(agent))
                .map(|top| InputArea { top, mode: None })
        }
        CLIAgent::Kimi => kimi_input_top(rows, floor).map(|top| InputArea { top, mode: None }),
        CLIAgent::OpenCode => {
            opencode_input_top(rows, floor).map(|top| InputArea { top, mode: None })
        }
        CLIAgent::QoderCLI | CLIAgent::QoderCLICn => {
            qoder_input(rows, floor, width).map(|q| InputArea {
                top: q.top,
                mode: None,
            })
        }
        _ => None,
    }
}

/// The prompts an input ruled off top and bottom opens with: Copilot's `❯`;
/// Qwen Code's `>`, `*` in YOLO mode and `!` in shell mode; CodeBuddy's `>`,
/// `!` in bash mode.
fn ruled_prompts(agent: CLIAgent) -> &'static [char] {
    match agent {
        CLIAgent::Copilot => &['❯'],
        CLIAgent::Qwen => &['>', '*', '!'],
        _ => &['>', '!'],
    }
}

/// How many rows Crush draws under its editor at most: a blank row and its
/// key help, which `ctrl+g` opens out to several.
const CRUSH_FOOTER_MAX_ROWS: usize = 8;

/// Where Crush's editor starts: its prompt line, `> ` (`!` in yolo mode),
/// with every further line opening with `:::` — and the first one too while
/// the chat, not the editor, has the keys. A blank row and the key help are
/// under it, at the bottom of the screen.
///
/// Crush draws its dialogs — the command palette, the model and session
/// lists, a permission request — framed over the screen, the editor left
/// standing under them; they take the keys while open, so with a frame on
/// screen the box steps aside.
fn crush_input_top(rows: &[String], floor: usize) -> Option<usize> {
    let frame_top = |r: &String| {
        r.find('╭').is_some_and(|at| {
            let edge = &r[at + '╭'.len_utf8()..];
            let run = edge.chars().take_while(|&c| c == '─').count();
            run >= 2 && edge.chars().nth(run) == Some('╮')
        })
    };
    if rows.iter().any(frame_top) {
        return None;
    }
    let more = |i: usize| rows[i].trim_start().starts_with(":::");
    let last = (floor..rows.len()).rev().find(|&i| more(i))?;
    let mut first = last;
    while first > floor && more(first - 1) {
        first -= 1;
    }
    let prompt = |row: &str| {
        let t = row.trim_start();
        t == ">" || t.starts_with("> ") || t.starts_with('!')
    };
    let top = match first.checked_sub(1) {
        Some(above) if above >= floor && prompt(&rows[above]) => above,
        _ => first,
    };
    let blank_under = rows.get(last + 1).is_some_and(|r| r.trim().is_empty());
    let footer = rows[last + 1..]
        .iter()
        .filter(|r| !r.trim().is_empty())
        .count();
    (blank_under && footer <= CRUSH_FOOTER_MAX_ROWS).then_some(top)
}

/// Where Goose's input starts: the context gauge (`╌╌╌ 1% 5k/1.0M`) right
/// over its `> ` line, which is the last thing on the screen.
///
/// Goose is a line-at-a-time prompt, not a full-screen one: while a turn
/// runs its spinner and the turn's output are printed under the line just
/// sent, and a question it asks — a tool approval — is printed there too,
/// so the box steps aside until the next gauge and `> ` close the turn.
fn goose_input_top(rows: &[String]) -> Option<usize> {
    let last = rows.iter().rposition(|r| !r.trim().is_empty())?;
    let gauge = last.checked_sub(1)?;
    let t = rows[gauge].trim();
    let prompt = rows[last] == ">" || rows[last].starts_with("> ");
    (t.starts_with('╌') && t.contains('%') && prompt).then_some(gauge)
}

/// Where Amp's input starts: the top edge of the frame at the bottom of the
/// screen, its mode named in that edge and the directory in the bottom one.
///
/// Its command palette and pickers open framed over the screen, indented from
/// its left edge; a notice it puts in the input's place — out of credits — is
/// a full-width frame with nothing in its bottom edge. The box steps aside
/// for both.
fn amp_input_top(rows: &[String], floor: usize, width: usize) -> Option<usize> {
    let bottom = (floor..rows.len())
        .rev()
        .find(|&i| !rows[i].trim().is_empty())?;
    let edge = rows[bottom].trim_end();
    let labelled = edge.chars().any(|c| !matches!(c, '╰' | '─' | '╯' | ' '));
    if !(edge.starts_with('╰')
        && edge.ends_with('╯')
        && edge.chars().count() >= width * 3 / 5
        && labelled)
    {
        return None;
    }
    let top = (floor..bottom).rev().find(|&i| !rows[i].starts_with('│'))?;
    let framed = rows[top].starts_with('╭') && rows[top].trim_end().ends_with('╮');
    let overlay = rows[..top]
        .iter()
        .any(|r| !r.starts_with('╭') && r.trim_start().starts_with('╭'));
    (framed && top + 1 < bottom && !overlay).then_some(top)
}

/// How many rows Copilot, Qwen Code and CodeBuddy draw under their input's
/// lower rule at most: their status rows, which wrap in a narrow pane.
const RULED_FOOTER_MAX_ROWS: usize = 5;

/// Where an input ruled off top and bottom starts — Copilot's, Qwen Code's,
/// CodeBuddy's: the upper of the last two full-width rules on the screen,
/// with the prompt (one of `prompts`, at the very start of the row) on the
/// line right under it and no more than the status rows under the lower one.
///
/// What they put in the input's place is no such block: their tool approvals
/// and pickers are drawn between rules of their own — or in a frame — with
/// the question, not the prompt, on the first line, or with the options
/// under the lower rule; and their `/` and `@` lists open under the lower
/// rule (Qwen Code, CodeBuddy) or right over the upper one (Copilot, its rows
/// on a `┃` bar). They take the keys while open, so the box steps aside.
fn ruled_input_top(rows: &[String], floor: usize, width: usize, prompts: &[char]) -> Option<usize> {
    let mut rules = (floor..rows.len())
        .rev()
        .filter(|&i| is_rule(&rows[i], width));
    let lower = rules.next()?;
    let upper = rules.next()?;
    let footer = rows[lower + 1..]
        .iter()
        .filter(|r| !r.trim().is_empty())
        .count();
    let prompt = rows[upper + 1]
        .chars()
        .next()
        .is_some_and(|c| prompts.contains(&c));
    let list_above = upper > 0 && rows[upper - 1].starts_with('┃');
    (upper + 1 < lower && prompt && footer <= RULED_FOOTER_MAX_ROWS && !list_above).then_some(upper)
}

/// Where Copilot's input starts, and where it ends, when it shades it — as
/// it does in a terminal with true colour: a block opened by a `╻▄▄▄` edge
/// and closed by a `╹▀▀▀` one, its lines on a `┃` bar, the status rows under
/// it. Elsewhere it rules the input off — see [`ruled_input_top`].
///
/// Its `/` and `@` lists open on a `┃` bar right over the block, and take the
/// keys while open, so the box steps aside for them.
fn copilot_shaded_input(rows: &[String], floor: usize, width: usize) -> Option<(usize, usize)> {
    let edge = |r: &str, corner: char, run: char| {
        let t = r.trim();
        t.starts_with(corner)
            && t.chars().count() >= width * 3 / 5
            && t.chars().skip(1).all(|c| c == run)
    };
    let bottom = (floor..rows.len())
        .rev()
        .find(|&i| edge(&rows[i], '╹', '▀'))?;
    let footer = rows[bottom + 1..]
        .iter()
        .filter(|r| !r.trim().is_empty())
        .count();
    let top = (floor..bottom).rev().find(|&i| !rows[i].starts_with('┃'))?;
    let list_above = copilot_list_over(rows, top);
    (edge(&rows[top], '╻', '▄')
        && top + 1 < bottom
        && footer <= RULED_FOOTER_MAX_ROWS
        && !list_above)
        .then_some((top, bottom))
}

/// How far over Copilot's shaded input its `/` and `@` lists reach: their
/// rows, and the blank ones a short list leaves under it.
const COPILOT_LIST_REACH: usize = 12;

/// Whether one of Copilot's `/` or `@` lists is open over its input, whose
/// top is `top`: on a `┃` bar right over it, or — drawn shaded — as rows
/// whose selected one opens with `❯ /` or `❯ @`. A message sent before is
/// drawn with a `❯` too, but with the time it was sent at the end.
fn copilot_list_over(rows: &[String], top: usize) -> bool {
    if top > 0 && rows[top - 1].starts_with('┃') {
        return true;
    }
    rows[top.saturating_sub(COPILOT_LIST_REACH)..top]
        .iter()
        .any(|r| {
            let t = r.trim();
            let sent_at = t.len() >= 5
                && t.is_char_boundary(t.len() - 5)
                && t[t.len() - 5..].chars().enumerate().all(|(i, c)| match i {
                    2 => c == ':',
                    _ => c.is_ascii_digit(),
                });
            (t.starts_with("❯ /") || t.starts_with("❯ @")) && !sent_at
        })
}

/// Whether `agent`'s input, as `rows` show it, is in its shell mode — which
/// Qwen Code and CodeBuddy stay in after a `!` command has run, their prompt
/// a `!`, until Esc takes them out.
fn in_shell_mode(agent: CLIAgent, rows: &[String], width: usize) -> bool {
    let floor = rows.len().saturating_sub(INPUT_AREA_MAX_ROWS);
    match agent {
        CLIAgent::Qwen | CLIAgent::CodeBuddy => {
            ruled_input_top(rows, floor, width, ruled_prompts(agent))
                .is_some_and(|top| rows[top + 1].starts_with('!'))
        }
        CLIAgent::QoderCLI | CLIAgent::QoderCLICn => qoder_input(rows, floor, width)
            .is_some_and(|q| rows[q.first].trim_start().starts_with('!')),
        _ => false,
    }
}

/// How many rows Qoder draws under its input's lower rule at most: its model
/// line, which wraps in a narrow pane.
const QODER_FOOTER_MAX_ROWS: usize = 3;

/// How far over its input the mode line of Gemini CLI and Qoder reaches,
/// from the rule over it: in a narrow pane Qoder's mode and MCP count go on
/// rows of their own, a blank one between them.
const MODE_LINE_MAX_ROWS: usize = 4;

/// Qoder's input, as [`qoder_input`] finds it.
struct QoderInput {
    /// The first row the box covers.
    top: usize,
    /// The input's first line, the prompt's.
    first: usize,
    /// The rule under its last line.
    lower: usize,
}

/// Where Qoder's input is: the lines between the last two full-width rules,
/// the first opening with its prompt — ` > `, ` * ` in YOLO mode, ` ! ` in
/// shell mode — and only its model line under the lower rule. Over the upper
/// rule its mode line has a rule of its own, and the `? for shortcuts` hint
/// sits over that; the box covers them too, or they would be left standing
/// over it.
///
/// Its `/` and `@` lists open under the lower rule, in the model line's
/// place, the selected row opening with `❯`, and take the keys while open.
/// Its dialogs — the folder trust and sign-in prompts, `/model`, `/help` —
/// open under a rule of their own with no prompt under it, and a sent message
/// is a ` > ` line with no rules around it. The box steps aside for all of
/// them.
fn qoder_input(rows: &[String], floor: usize, width: usize) -> Option<QoderInput> {
    let mut rules = (floor..rows.len())
        .rev()
        .filter(|&i| is_rule(&rows[i], width));
    let lower = rules.next()?;
    let upper = rules.next()?;
    let first = upper + 1;
    let prompt = rows[first].strip_prefix(' ').is_some_and(|t| {
        let mut chars = t.chars();
        matches!(chars.next(), Some('>' | '*' | '!')) && matches!(chars.next(), None | Some(' '))
    });
    let footer: Vec<&str> = rows[lower + 1..]
        .iter()
        .map(|r| r.trim())
        .filter(|t| !t.is_empty())
        .collect();
    let list = footer.iter().any(|t| t.starts_with(['❯', '▲', '▼']));
    if !(first < lower && prompt && footer.len() <= QODER_FOOTER_MAX_ROWS && !list) {
        return None;
    }
    let top = over_mode_line(rows, floor, width, upper);
    Some(QoderInput { top, first, lower })
}

/// The row to cover from for an input starting at `top` that Gemini CLI or
/// a fork of it draws: from the rule over its mode line ("Shift+Tab to
/// accept edits", "shell mode enabled"), and the `? for shortcuts` hint over
/// that, when they are there — left standing, they would show over the box.
fn over_mode_line(rows: &[String], floor: usize, width: usize, top: usize) -> usize {
    let Some(rule) = (top.saturating_sub(MODE_LINE_MAX_ROWS).max(floor)..top)
        .rev()
        .find(|&i| is_rule(&rows[i], width))
    else {
        return top;
    };
    let between = &rows[rule + 1..top];
    let mode_line = between.iter().any(|r| !r.trim().is_empty())
        && between
            .iter()
            .all(|r| !r.trim_start().starts_with(['>', '❯', '│', '╭', '╰']));
    if !mode_line {
        return top;
    }
    match rule > floor && rows[rule - 1].trim() == "? for shortcuts" {
        true => rule - 1,
        false => rule,
    }
}

/// How many rows Kimi Code draws under its input's frame at most: its model
/// and directory line and the context gauge, which wrap in a narrow pane.
const KIMI_FOOTER_MAX_ROWS: usize = 4;

/// Where Kimi Code's input starts: the top edge of the rounded frame whose
/// first line inside opens with `>`, with its model line and context gauge
/// under it.
///
/// Its `/` and `@` lists open under the frame, between it and the model
/// line; a tool approval takes the frame's place between two rules; the
/// welcome banner is a frame of its own, far up with the transcript under
/// it. The box steps aside for the lists and approvals.
fn kimi_input_top(rows: &[String], floor: usize) -> Option<usize> {
    let edge = |r: &str, open: char, close: char| {
        let t = r.trim();
        t.starts_with(open) && t.ends_with(close) && t.chars().count() > 2
    };
    let bottom = (floor..rows.len())
        .rev()
        .find(|&i| edge(&rows[i], '╰', '╯'))?;
    let footer = rows[bottom + 1..]
        .iter()
        .filter(|r| !r.trim().is_empty())
        .count();
    let top = (floor..bottom)
        .rev()
        .find(|&i| !rows[i].trim_start().starts_with('│'))?;
    let prompt = rows.get(top + 1).is_some_and(|r| {
        r.trim_start()
            .trim_start_matches('│')
            .trim_start()
            .starts_with('>')
    });
    (edge(&rows[top], '╭', '╮') && top + 1 < bottom && prompt && footer <= KIMI_FOOTER_MAX_ROWS)
        .then_some(top)
}

/// How many rows OpenCode draws under its input at most: its key hints, and
/// on its home screen the directory at the bottom of the screen.
const OPENCODE_FOOTER_MAX_ROWS: usize = 3;

/// Where OpenCode's input starts: the top of the block it draws on a `┃`
/// bar, the agent and model on its last line, closed by a `╹▀▀▀` edge with
/// the key hints under it.
///
/// Its `/` and `@` lists open on the same bar right over the input, each row
/// closed by a `┃` at its right; a permission request takes the input's
/// place on a bar with no edge under it. The sent messages in the
/// transcript are on a bar too, without the edge. The box steps aside for
/// the lists and requests.
fn opencode_input_top(rows: &[String], floor: usize) -> Option<usize> {
    let bar = |r: &str| r.trim_start().starts_with('┃');
    let edge = (floor..rows.len()).rev().find(|&i| {
        let t = rows[i].trim();
        t.starts_with('╹') && t.chars().skip(1).all(|c| c == '▀') && t.chars().count() > 2
    })?;
    let footer = rows[edge + 1..]
        .iter()
        .filter(|r| !r.trim().is_empty())
        .count();
    let mut top = edge;
    while top > floor && bar(&rows[top - 1]) {
        top -= 1;
    }
    let list = rows[top..edge].iter().any(|r| {
        let t = r.trim();
        t.chars().count() > 1 && t.ends_with('┃')
    });
    (top + 2 <= edge && footer <= OPENCODE_FOOTER_MAX_ROWS && !list).then_some(top)
}

/// Where Oh My Pi's input starts, in the composer shapes that keep it findable.
///
/// - **The status band**, its default, and **the rounded box**: the status
///   line — model, directory, branch, context gauge — on the row above the
///   input, whose first line opens with `╰─ `. Its own lists open below that
///   line. A dialog's frame closes with `╰──…╯` instead, and a tool approval
///   or a picker takes the input's place, so neither reads as the input.
/// - **The Pi and Claude Code shapes** rule the input off the way Pi does —
///   see [`pi_input_top`].
///
/// Its borderless, field and rail shapes leave no frame to find, and the box
/// stays aside for them.
/// How many rows Grok Build draws under its input's frame at most: a blank
/// row and its key hints.
const GROK_FOOTER_MAX_ROWS: usize = 3;

/// Where Grok Build's input starts: the top of its rounded frame, whose first
/// line inside opens with `❯`, with the model named in its bottom edge and
/// the key hints under it.
///
/// Its lists — `/` commands, the `/model` picker — open above the frame
/// between two rules, the lower one right on top of the frame, and take the
/// arrow keys and Enter there, so with one open the box steps aside. Its
/// dialogs — the release notes, the shortcuts — are drawn over the frame,
/// breaking its top edge, and the box steps aside for them too.
fn grok_input_top(rows: &[String], floor: usize, width: usize) -> Option<usize> {
    let bottom = (floor..rows.len()).rev().find(|&i| {
        let t = rows[i].trim();
        t.starts_with('╰') && t.ends_with('╯')
    })?;
    let footer = rows[bottom + 1..]
        .iter()
        .filter(|r| !r.trim().is_empty())
        .count();
    let top = (floor..bottom)
        .rev()
        .find(|&i| rows[i].trim_start().starts_with('╭'))?;
    // An edge with anything on it is a dialog drawn over the frame.
    let edge = rows[top]
        .trim()
        .chars()
        .all(|c| matches!(c, '╭' | '─' | '╮'));
    let prompt = rows.get(top + 1).is_some_and(|r| {
        r.trim_start()
            .trim_start_matches('│')
            .trim_start()
            .starts_with('❯')
    });
    let list_open = top > 0 && is_rule(&rows[top - 1], width);
    (footer <= GROK_FOOTER_MAX_ROWS && top + 1 < bottom && edge && prompt && !list_open)
        .then_some(top)
}

/// The keys that stop `agent`'s turn. Grok Build's Esc leaves a running
/// turn running; its Ctrl+C stops one, and empties the input when idle.
/// Crush takes a first Esc as the question "press again to cancel", asked in
/// the key help under the box where nobody reads it, and stops on a second
/// one; the two must arrive as separate reads, or they are one key.
fn interrupt_keys(agent: Option<CLIAgent>) -> Vec<Step> {
    let key = |delay, bytes: &[u8]| Step {
        delay,
        bytes: bytes.to_vec(),
    };
    match agent {
        Some(CLIAgent::Grok) => vec![key(Duration::ZERO, b"\x03")],
        Some(CLIAgent::Crush) => vec![key(Duration::ZERO, b"\x1b"), key(SETTLE, b"\x1b")],
        _ => vec![key(Duration::ZERO, b"\x1b")],
    }
}

/// The keys that empty `agent`'s input of `lines` lines of text, where some
/// do so without doing anything else; none for an input with nothing in it.
///
/// Grok Build's Ctrl+U empties its input whole. The others' empty the
/// caret's line, and a Backspace then steps up into the line above — a line
/// at a time, from the end, where the caret is left. CodeBuddy takes keys
/// that arrive together for a paste and does none of them, so it gets them
/// one at a time.
fn clear_input(agent: CLIAgent, lines: usize) -> Option<Vec<Step>> {
    if lines == 0 {
        return None;
    }
    let keys = match agent {
        CLIAgent::Grok => b"\x15".to_vec(),
        CLIAgent::Crush
        | CLIAgent::Goose
        | CLIAgent::Amp
        | CLIAgent::Copilot
        | CLIAgent::Qwen
        | CLIAgent::CodeBuddy
        | CLIAgent::Kimi
        | CLIAgent::OpenCode
        | CLIAgent::QoderCLI
        | CLIAgent::QoderCLICn => b"\x15\x7f".repeat(lines),
        _ => return None,
    };
    Some(match agent {
        CLIAgent::CodeBuddy => keys
            .iter()
            .enumerate()
            .map(|(i, &key)| Step {
                delay: match i {
                    0 => Duration::ZERO,
                    _ => KEY_GAP,
                },
                bytes: vec![key],
            })
            .collect(),
        _ => vec![Step {
            delay: Duration::ZERO,
            bytes: keys,
        }],
    })
}

/// Crush's placeholders: what its empty editor says, idle, mid-turn and in
/// yolo mode.
const CRUSH_PLACEHOLDERS: [&str; 11] = [
    "Ready!",
    "Ready?",
    "Ready...",
    "Ready for instructions",
    "Working!",
    "Working...",
    "Brrrrr...",
    "Prrrrrrrr...",
    "Processing...",
    "Thinking...",
    "Go crazy",
];

/// How many lines of text `agent`'s input, as `rows` show it, holds — down
/// to the last one with anything on it.
///
/// Under the box nobody sees it, and the next message would be typed on
/// after it. Grok Build keeps a command whose list was dismissed — `/model`,
/// closed with Esc — in its input; Crush, Goose and Amp keep whatever was
/// typed into them before the box was opened over them.
fn held_lines(agent: CLIAgent, rows: &[String], width: usize) -> usize {
    let floor = rows.len().saturating_sub(INPUT_AREA_MAX_ROWS);
    // Lines `first..` hold text; how many, down to the last that does.
    let count = |lines: Vec<&str>| {
        lines
            .iter()
            .rposition(|l| !l.trim().is_empty())
            .map_or(0, |last| last + 1)
    };
    match agent {
        CLIAgent::Grok => grok_input_top(rows, floor, width).map_or(0, |top| {
            let line = rows[top + 1].trim().trim_matches('│').trim();
            usize::from(!line.trim_start_matches('❯').trim().is_empty())
        }),
        CLIAgent::Crush => crush_input_top(rows, floor).map_or(0, |top| {
            let mut lines: Vec<&str> = rows[top..]
                .iter()
                .take_while(|r| !r.trim().is_empty())
                .map(|r| {
                    let t = r.trim_start();
                    let t = t.strip_prefix(":::").unwrap_or(t);
                    let t = t.strip_prefix('>').unwrap_or(t);
                    t.strip_prefix('!').unwrap_or(t)
                })
                .collect();
            if lines
                .first()
                .is_some_and(|l| CRUSH_PLACEHOLDERS.contains(&l.trim()))
            {
                lines[0] = "";
            }
            count(lines)
        }),
        CLIAgent::Goose => goose_input_top(rows).map_or(0, |top| {
            let line = rows[top + 1].trim_start_matches('>').trim();
            usize::from(!line.is_empty() && line != "Enter to send · Ctrl+J newline")
        }),
        CLIAgent::Amp => amp_input_top(rows, floor, width).map_or(0, |top| {
            count(
                rows[top + 1..]
                    .iter()
                    .take_while(|r| r.starts_with('│'))
                    .map(|r| r.trim_matches(|c: char| c == '│' || c == ' '))
                    .collect(),
            )
        }),
        CLIAgent::Copilot if let Some((top, bottom)) = copilot_shaded_input(rows, floor, width) => {
            count(
                rows[top + 1..bottom]
                    .iter()
                    .map(|r| r.trim_start_matches('┃').trim())
                    .collect(),
            )
        }
        CLIAgent::Copilot | CLIAgent::Qwen | CLIAgent::CodeBuddy => {
            ruled_input_top(rows, floor, width, ruled_prompts(agent)).map_or(0, |top| {
                let lines: Vec<&str> = rows[top + 1..]
                    .iter()
                    .take_while(|r| !is_rule(r, width))
                    .enumerate()
                    .map(|(i, r)| {
                        let mut chars = r.chars();
                        if i == 0 {
                            chars.next();
                        }
                        let t = chars
                            .as_str()
                            .trim_matches(|c: char| c.is_whitespace() || c == '\u{200b}');
                        // Placeholders, and a suggestion offered to send as is.
                        match t == "Type your message or @path/to/file" || t.ends_with("↵ send") {
                            true => "",
                            false => t,
                        }
                    })
                    .collect();
                count(lines)
            })
        }
        CLIAgent::Kimi => kimi_input_top(rows, floor).map_or(0, |top| {
            count(
                rows[top + 1..]
                    .iter()
                    .take_while(|r| r.trim_start().starts_with('│'))
                    .enumerate()
                    .map(|(i, r)| {
                        let t = r.trim().trim_matches('│').trim();
                        match i {
                            0 => t.trim_start_matches('>').trim(),
                            _ => t,
                        }
                    })
                    .collect(),
            )
        }),
        CLIAgent::OpenCode => opencode_input_top(rows, floor).map_or(0, |top| {
            let block: Vec<&str> = rows[top..]
                .iter()
                .take_while(|r| r.trim_start().starts_with('┃'))
                .map(|r| r.trim().trim_start_matches('┃').trim())
                .collect();
            // The last line names the agent and model.
            let lines = block[..block.len().saturating_sub(1)]
                .iter()
                .map(|l| match l.starts_with("Ask anything...") {
                    true => "",
                    false => l,
                })
                .collect();
            count(lines)
        }),
        CLIAgent::QoderCLI | CLIAgent::QoderCLICn => {
            qoder_input(rows, floor, width).map_or(0, |q| {
                count(
                    rows[q.first..q.lower]
                        .iter()
                        .enumerate()
                        .map(|(i, r)| {
                            let t = r.trim();
                            let t = match i {
                                0 => t.get(1..).unwrap_or("").trim(),
                                _ => t,
                            };
                            // Its placeholders, idle and in shell mode.
                            match t {
                                "Type your message or @path/to/file"
                                | "Type your shell command" => "",
                                _ => t,
                            }
                        })
                        .collect(),
                )
            })
        }
        _ => 0,
    }
}

/// Where Prime Agent's input starts: the padding row atop its shaded block.
///
/// The block is a blank row, the prompt line ` >  …` with any further lines
/// indented under it, and another blank row, right over the footer — `←
/// manage` and the model — which is the last thing on the screen. Its `/`
/// list opens above the block. The model picker draws its own ` >` search
/// line between two rules, away from the footer, and is not the input.
fn prime_input_top(rows: &[String], floor: usize) -> Option<usize> {
    let footer = (floor..rows.len())
        .rev()
        .find(|&i| !rows[i].trim().is_empty())?;
    // The block's lower padding row, then its lines up to the prompt's.
    let mut line = footer.checked_sub(1)?;
    if line < floor || !rows[line].trim().is_empty() {
        return None;
    }
    loop {
        line = line.checked_sub(1)?;
        if line < floor {
            return None;
        }
        let row = &rows[line];
        if row.trim().is_empty() {
            return None;
        }
        if row.starts_with(' ') && row.trim_start().starts_with('>') {
            break;
        }
        if !row.starts_with("    ") {
            return None;
        }
    }
    let top = line.checked_sub(1)?;
    (top >= floor && rows[top].trim().is_empty()).then_some(top)
}

fn omp_input_top(rows: &[String], floor: usize, width: usize) -> Option<usize> {
    let line = (floor + 1..rows.len()).rev().find(|&i| {
        rows[i]
            .trim_start()
            .strip_prefix("╰─")
            .is_some_and(|rest| !rest.starts_with('─'))
    });
    match line {
        Some(line) => (!rows[line - 1].trim().is_empty()).then_some(line - 1),
        None => pi_input_top(rows, floor, width),
    }
}

/// How many rows Pi draws under its input at most: the directory and usage
/// rows, an extension's status, and its own completion list, which opens
/// there.
const PI_FOOTER_MAX_ROWS: usize = 10;

/// Where Pi's input starts: its upper rule.
///
/// Pi draws its editor between two full-width rules, the upper one carrying
/// the turn's spinner (`── ⠹ Working ──`) while it works, with the
/// directory and usage rows under the lower one. Its selectors — `/model`,
/// `/resume`, `/tree`, `/login` — take the editor's place between the same
/// rules, but open on a blank row and run on for several, and `/settings`
/// opens on its `>` search line; the editor is one row when empty, and opens
/// on its text otherwise. Only the lowest pair of rules counts: anything
/// ruled higher up is the transcript's.
fn pi_input_top(rows: &[String], floor: usize, width: usize) -> Option<usize> {
    let rule = |row: &str| {
        let t = row.trim();
        t.starts_with("──")
            && t.ends_with('─')
            && t.chars().filter(|&c| c == '─').count() >= width * 3 / 5
    };
    let rules: Vec<usize> = (floor..rows.len()).filter(|&i| rule(&rows[i])).collect();
    let (&lower, higher) = rules.split_last()?;
    let &upper = higher.last()?;
    let footer = rows[lower + 1..]
        .iter()
        .filter(|r| !r.trim().is_empty())
        .count();
    let inside = &rows[upper + 1..lower];
    let opens_on_text = |row: &String| {
        let t = row.trim_end();
        !t.trim().is_empty() && t != ">" && !t.starts_with("> ")
    };
    let editor = inside.len() == 1 || inside.first().is_some_and(opens_on_text);
    (footer <= PI_FOOTER_MAX_ROWS && editor).then_some(upper)
}

/// How many rows Gemini's footer — the workspace, sandbox and model, under
/// its labels — takes below the input at most.
const GEMINI_FOOTER_MAX_ROWS: usize = 4;

/// Where Gemini's input starts, in whichever of its drawings is on screen.
///
/// - **A shaded block**, the usual one: a row of `▄` and a row of `▀`, as wide
///   as the screen, round the prompt line. A message already sent is drawn the
///   same way in the transcript, so the block counts only with nothing under
///   it but the footer — a permission prompt or a list in the input's place
///   puts itself below the last message instead.
/// - **A rule over the prompt line**, where the block has no colour to shade
///   with (`NO_COLOR`, or its background colour switched off).
/// - **A rounded frame**, `╭ … ╰` with `> ` inside, in versions before the
///   block.
///
/// The prompt line opens with `>`, or with `!` in shell mode, `*` in YOLO
/// mode and `(r:)` while searching the history.
fn gemini_input_top(rows: &[String], floor: usize, width: usize) -> Option<usize> {
    let prompt = |row: &str| {
        let t = row.trim_start();
        ['>', '!', '*'].iter().any(|p| t.starts_with(*p)) || t.starts_with("(r:)")
    };
    let band = |row: &str, c: char| {
        let t = row.trim();
        t.chars().count() >= width * 3 / 5 && t.chars().all(|x| x == c)
    };

    // Shading is on for the whole screen or off for all of it, so a band
    // anywhere says which drawing to look for.
    if let Some(lower) = (floor..rows.len()).rev().find(|&i| band(&rows[i], '▀')) {
        let footer = rows[lower + 1..]
            .iter()
            .filter(|r| !r.trim().is_empty())
            .count();
        let upper = (floor..lower).rev().find(|&i| band(&rows[i], '▄'))?;
        return (footer <= GEMINI_FOOTER_MAX_ROWS && upper + 1 < lower && prompt(&rows[upper + 1]))
            .then_some(upper);
    }

    if let Some(line) = (floor + 1..rows.len())
        .rev()
        .find(|&i| prompt(&rows[i]) && is_rule(&rows[i - 1], width))
    {
        return Some(line - 1);
    }

    let bottom = (floor..rows.len())
        .rev()
        .find(|&i| rows[i].trim_start().starts_with('╰'))?;
    let top = (floor..bottom)
        .rev()
        .find(|&i| rows[i].trim_start().starts_with('╭'))?;
    (top + 1..bottom)
        .any(|i| {
            rows[i]
                .trim_start()
                .trim_start_matches('│')
                .trim_start()
                .starts_with('>')
        })
        .then_some(top)
}

/// How far above Codex's selected row the rest of its list can start: the
/// options before it, each with a description that may wrap.
const CODEX_LIST_REACH: usize = 16;

/// Whether `text` opens the way a numbered list option does: `2. Skip`.
fn numbered_option(text: &str) -> bool {
    let text = text.trim_start();
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && text[digits..].starts_with(". ")
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
/// The background the agent paints the screen row `line` in: the colour most
/// of its cells have — an inverse cell's being its foreground — or `None` for
/// the terminal's own background.
fn row_backdrop<T: EventListener>(
    term: &Term<T>,
    line: usize,
    palette: &[Rgb; 256],
) -> Option<Rgb> {
    let grid = term.grid();
    if line >= grid.screen_lines() {
        return None;
    }
    let row = &grid[Line(line as i32)];
    let mut tally: Vec<(Option<Rgb>, usize)> = Vec::new();
    for c in 0..grid.columns() {
        let cell = &row[Column(c)];
        let color = match cell.flags.contains(Flags::INVERSE) {
            true => cell.fg,
            false => cell.bg,
        };
        let rgb = match color {
            AnsiColor::Spec(rgb) => Some(rgb),
            AnsiColor::Indexed(i) => Some(palette[i as usize]),
            AnsiColor::Named(named) => palette.get(named as usize).copied(),
        };
        match tally.iter_mut().find(|(seen, _)| *seen == rgb) {
            Some((_, n)) => *n += 1,
            None => tally.push((rgb, 1)),
        }
    }
    tally
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .and_then(|(rgb, _)| rgb)
}

fn rgb_to_hsla(c: Rgb) -> gpui::Hsla {
    gpui::Rgba {
        r: c.r as f32 / 255.,
        g: c.g as f32 / 255.,
        b: c.b as f32 / 255.,
        a: 1.,
    }
    .into()
}

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
            ("/effort", "Set reasoning effort"),
            ("/init", "Create a CLAUDE.md for this repo"),
            ("/context", "Show context usage"),
            ("/usage", "Show cost and plan usage"),
            ("/resume", "Resume a previous conversation"),
            ("/rewind", "Rewind the conversation or code"),
            ("/memory", "Edit memory files"),
            ("/permissions", "Manage tool permissions"),
            ("/mcp", "Manage MCP servers"),
            ("/config", "Open settings"),
            ("/help", "Show help"),
        ],
        CLIAgent::Codex => &[
            ("/model", "Choose model and reasoning effort"),
            ("/permissions", "Choose what Codex is allowed to do"),
            ("/review", "Review the current changes"),
            ("/new", "Start a new chat"),
            ("/resume", "Resume a saved chat"),
            ("/compact", "Summarize to free up context"),
            ("/init", "Create an AGENTS.md for this repo"),
            ("/diff", "Show the git diff"),
            ("/mention", "Mention a file"),
            ("/status", "Show session configuration and usage"),
            ("/mcp", "List MCP tools"),
        ],
        CLIAgent::Gemini => &[
            ("/compress", "Summarize to free up context"),
            ("/clear", "Clear the screen and start a new session"),
            ("/model", "Choose the model"),
            ("/resume", "Browse and resume saved conversations"),
            ("/init", "Create a GEMINI.md for this repo"),
            ("/memory", "Manage memory"),
            ("/tools", "List available tools"),
            ("/mcp", "List MCP servers"),
            ("/stats", "Show session statistics"),
            ("/help", "Show help"),
        ],
        CLIAgent::OpenCode => &[
            ("/new", "New session"),
            ("/models", "Switch model"),
            ("/agents", "Switch agent"),
            ("/sessions", "Switch session"),
            ("/compact", "Compact session"),
            ("/undo", "Undo previous message"),
            ("/review", "Review changes"),
            ("/init", "Guided AGENTS.md setup"),
            ("/share", "Share session"),
            ("/status", "View status"),
            ("/mcps", "Toggle MCPs"),
            ("/help", "Help"),
        ],
        CLIAgent::Copilot => &[
            ("/model", "Switch the model"),
            ("/clear", "Abandon this session and start fresh"),
            ("/compact", "Summarize the conversation to free up context"),
            ("/context", "Show context window usage"),
            ("/resume", "Switch to another session"),
            ("/review", "Review the current changes"),
            ("/diff", "Review the changes in the current directory"),
            ("/init", "Create Copilot instructions for this repo"),
            ("/usage", "Show session usage metrics"),
            ("/permissions", "Choose how requests are approved"),
            ("/agent", "Browse and select agents"),
            ("/mcp", "Manage MCP servers"),
            ("/help", "Show help for interactive commands"),
        ],
        CLIAgent::Qwen => &[
            ("/compress", "Summarize to free up context"),
            ("/clear", "Clear conversation history and free up context"),
            ("/model", "Switch the model"),
            ("/resume", "Resume a previous session"),
            ("/init", "Create a QWEN.md for this repo"),
            ("/approval-mode", "Change the approval mode for tools"),
            ("/tools", "List available tools"),
            ("/mcp", "Manage MCP servers"),
            ("/stats", "Show usage statistics"),
            ("/help", "Show help"),
        ],
        CLIAgent::CodeBuddy => &[
            ("/compact", "Compress context"),
            ("/clear", "Start a new conversation"),
            ("/model", "Switch or view the main model"),
            ("/resume", "Resume a previous session"),
            ("/rewind", "Rewind the conversation or code"),
            ("/init", "Create a CODEBUDDY.md for this repo"),
            ("/context", "Show context usage"),
            ("/cost", "Show session cost and token usage"),
            ("/memory", "Manage long-term memory"),
            ("/permissions", "Manage tool permissions"),
            ("/mcp", "Manage MCP connections"),
            ("/status", "Show repository and session status"),
            ("/help", "Show help"),
        ],
        CLIAgent::Kimi => &[
            ("/new", "Start a fresh session"),
            ("/compact", "Compact the conversation context"),
            ("/model", "Switch LLM model"),
            ("/effort", "Switch thinking effort"),
            ("/sessions", "Browse and resume sessions"),
            ("/undo", "Withdraw the last prompt"),
            ("/init", "Analyze the codebase and generate AGENTS.md"),
            ("/plan", "Toggle plan mode"),
            ("/permission", "Select permission mode"),
            ("/usage", "Show session tokens and context window"),
            ("/status", "Show session and runtime status"),
            ("/mcp", "Show MCP server status"),
            ("/help", "Show available commands and shortcuts"),
        ],
        CLIAgent::Pi => &[
            ("/model", "Select model"),
            ("/thinking", "Set thinking level"),
            ("/compact", "Compact the session context"),
            ("/new", "Start a new session"),
            ("/resume", "Resume a different session"),
            ("/tree", "Navigate the session tree"),
            ("/fork", "Fork from a previous message"),
            ("/session", "Show session info and stats"),
            ("/copy", "Copy the last agent message"),
            ("/export", "Export the session"),
            ("/settings", "Open settings"),
            ("/reload", "Reload extensions, skills and prompts"),
            ("/hotkeys", "Show keyboard shortcuts"),
            ("/quit", "Quit Pi"),
        ],
        CLIAgent::OhMyPi => &[
            ("/model", "Switch model"),
            ("/plan", "Toggle plan mode"),
            ("/compact", "Compact the session context"),
            ("/new", "Start a new session"),
            ("/resume", "Resume a different session"),
            ("/retry", "Retry the last failed turn"),
            ("/tree", "Navigate the session tree"),
            ("/fork", "Fork from a previous message"),
            ("/context", "Show context usage"),
            ("/usage", "Show provider usage and limits"),
            ("/todo", "View or change the todo list"),
            ("/copy", "Copy text from the conversation"),
            ("/settings", "Open settings"),
            ("/exit", "Exit Oh My Pi"),
        ],
        CLIAgent::PrimeAgent => &[
            ("/model", "Switch model"),
            ("/effort", "Set reasoning effort"),
            ("/compact", "Compact the session context"),
            ("/new", "Start a new session"),
            ("/resume", "Resume a session"),
            ("/tree", "Jump to any point in the session"),
            ("/fork", "Fork from a previous message"),
            ("/usage", "Show token, cost and context usage"),
            ("/btw", "Ask a side question"),
            ("/copy", "Copy the last assistant message"),
            ("/export", "Export the session to HTML"),
            ("/settings", "Open settings"),
            ("/reload", "Reload extensions, skills and prompts"),
            ("/quit", "Quit Prime Agent"),
        ],
        CLIAgent::Grok => &[
            ("/model", "Switch the active model"),
            ("/effort", "Set reasoning effort"),
            ("/plan", "Enter plan mode"),
            ("/compact", "Compact conversation history"),
            ("/new", "Start a new session"),
            ("/resume", "Resume a previous session"),
            ("/context", "View context usage"),
            ("/usage", "View usage"),
            ("/btw", "Ask a side question"),
            ("/recap", "Summarize the session so far"),
            ("/copy", "Copy the last response"),
            ("/mcps", "Show MCP server status"),
            ("/settings", "Open settings"),
            ("/quit", "Quit Grok Build"),
        ],
        CLIAgent::Goose => &[
            ("/model", "Show or switch the model"),
            ("/mode", "Set the mode: auto, approve, smart_approve, chat"),
            ("/compact", "Compact the conversation"),
            ("/clear", "Clear the chat history"),
            ("/new", "Start a fresh session"),
            ("/status", "Show model, provider, mode and token usage"),
            ("/skills", "List or enable skills"),
            ("/prompts", "List available prompts"),
            ("/goal", "Set a goal to satisfy before finishing"),
            ("/doctor", "Check the Goose setup"),
            ("/help", "Show the commands"),
            ("/exit", "Exit the session"),
        ],
        CLIAgent::QoderCLI | CLIAgent::QoderCLICn => &[
            ("/compact", "Replace the context with a summary"),
            ("/clear", "Start a new conversation"),
            ("/model", "Set or manage the model"),
            ("/effort", "Set reasoning effort"),
            ("/resume", "Resume a previous session"),
            ("/rewind", "Rewind the conversation"),
            ("/init", "Create a context file for this repo"),
            ("/context", "Show context usage"),
            ("/plan", "Toggle plan mode"),
            ("/permissions", "Manage permissions"),
            ("/review", "Review code changes"),
            ("/memory", "Manage memory"),
            ("/mcp", "Manage MCP servers"),
            ("/status", "Show account and session status"),
            ("/help", "Show help"),
        ],
        // Amp and Crush have no typed commands: `/` on an empty input opens
        // their command palette, which a message cannot drive.
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

/// `path` as an `@` mention `agent` reads back as that one file — spelled the
/// way the agent's own `@` list would have put it in.
///
/// - **Claude Code** ends a bare mention at the first whitespace; a path with
///   a space in it goes in double quotes, `@"my notes.md"`. **Kimi Code**'s
///   list spells one the same way, and so do Pi's and its forks', Oh My Pi
///   and Prime Agent.
/// - **Codex** hands its mentions to the model as they are, and its list puts
///   in the path alone — without the `@`, in double quotes when it has a
///   space in it.
/// - **Gemini CLI** ends a mention at the first unescaped space, and reads it
///   back through its own `escapePath`: a backslash before each character a
///   shell would take, or — on Windows — the whole path in double quotes.
///   **Qwen Code** and **Qoder** keep that spelling, and Qwen Code escapes
///   a comma too: it ends a mention at one.
///
/// The others take the path as it is.
pub(super) fn mention(agent: Option<CLIAgent>, path: &str, shell: Option<&str>) -> String {
    let spaced = path.chars().any(char::is_whitespace);
    match agent {
        Some(a) if reads_like_gemini(a) => {}
        Some(
            CLIAgent::Claude
            | CLIAgent::Kimi
            | CLIAgent::Pi
            | CLIAgent::OhMyPi
            | CLIAgent::PrimeAgent,
        ) if spaced => return format!("@\"{path}\""),
        Some(CLIAgent::Codex) if spaced && !path.contains('"') => return format!("\"{path}\""),
        Some(CLIAgent::Codex) => return path.to_string(),
        _ => return format!("@{path}"),
    }
    let spelled = match quoting_for(shell) {
        Quoting::Posix => {
            let mut out = String::with_capacity(path.len());
            for c in path.chars() {
                if " \t()[]{};|*?$`'\"#&<>!~\\".contains(c)
                    || (c == ',' && agent == Some(CLIAgent::Qwen))
                {
                    out.push('\\');
                }
                out.push(c);
            }
            out
        }
        _ if path
            .chars()
            .any(|c| c.is_whitespace() || "&()[]{}^=;!'+,`~%$@#".contains(c)) =>
        {
            format!("\"{path}\"")
        }
        _ => path.to_string(),
    };
    format!("@{spelled}")
}

/// The `@` menu for `query`. The mention is spelled relative to `cwd` — which
/// is what the agent resolves it against — and in full for a file outside it.
fn file_items(
    index: &FileIndex,
    query: &str,
    cwd: Option<&Path>,
    agent: Option<CLIAgent>,
    shell: Option<&str>,
) -> Vec<MenuItem> {
    let spell = |path: &Path| -> String {
        cwd.and_then(|c| path.strip_prefix(c).ok())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    };
    let item = |f: &IndexedFile| MenuItem {
        label: f.name().to_string(),
        detail: f.dir().to_string(),
        insert: mention(agent, &spell(&f.path), shell),
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
    /// The effort level last picked from the toolbar, with how many turns the
    /// agent had finished then. The next finished turn reports the level it
    /// ran at — the pick, or whatever the agent made of it, declined included
    /// — and from then on the report is the truth again.
    effort: Option<(&'static str, u64)>,
    /// The model alias last picked from the toolbar, and the turns finished
    /// then, the same way.
    model: Option<(&'static str, u64)>,
    picker: Option<Picker>,
    /// The agent session the picks above were made in. A pick says nothing
    /// about the next session, which starts from its own settings.
    session: Option<String>,
    /// The box's height as last drawn empty — nothing typed, nothing
    /// attached — which is what it needs of the pane at the least.
    frame_height: Rc<Cell<Option<Pixels>>>,
    /// The colour the agent paints the screen in around its input, when it
    /// paints one of its own — the layer covering the input takes it, so it
    /// is not a strip of another colour around the box — and how wide the
    /// grid's columns paint it.
    backdrop: Option<(Rgb, Pixels)>,
    _subs: Vec<Subscription>,
}

/// The toolbar's pop-up lists, each over its own button.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Picker {
    Model,
    Effort,
}

/// Which of [`CLAUDE_MODELS`] a reported model is: its family, and the 1M
/// variant when its id says so.
fn model_alias(model: &str) -> Option<&'static str> {
    let long = model.contains("[1m]");
    let pair = if model.contains("opus") {
        ["opus", "opus[1m]"]
    } else if model.contains("sonnet") {
        ["sonnet", "sonnet[1m]"]
    } else if model.contains("fable") {
        return Some("fable");
    } else if model.contains("haiku") {
        return Some("haiku");
    } else {
        return None;
    };
    Some(pair[long as usize])
}

/// How the toolbar spells an effort level: capitalized, as the model names
/// and modes are.
fn effort_label(level: &str) -> String {
    match level {
        "xhigh" => "Extra high".into(),
        _ => level[..1].to_uppercase() + &level[1..],
    }
}

/// How the model list spells an alias.
fn alias_label(alias: &str) -> String {
    if alias == "default" {
        return t(L10nKey::ComposerModelDefault).to_string();
    }
    let (family, long) = match alias.strip_suffix("[1m]") {
        Some(family) => (family, true),
        None => (alias, false),
    };
    let mut name = family[..1].to_uppercase() + &family[1..];
    if long {
        name.push_str(" · 1M");
    }
    name
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
            effort: None,
            model: None,
            picker: None,
            session: None,
            frame_height: Rc::new(Cell::new(None)),
            backdrop: None,
            _subs: subs,
        });
    }

    /// Rows the grid leaves empty at its bottom so that the box, covering an
    /// input shorter than itself, covers nothing above it: Oh My Pi's input
    /// is a status line and a prompt line, where the box is some five rows
    /// tall, and would otherwise hide the last lines of the reply. The agent
    /// is told the pane is that much shorter and draws its input higher.
    ///
    /// Only for an agent whose input is that short ([`makes_room`]): the
    /// others' inputs, with their rules and status rows, about hold the box,
    /// and resizing their pane each time the box comes and goes would have
    /// them redraw for a row of padding.
    pub(super) fn composer_reserved_rows(&self) -> usize {
        let Presence::Covering(area) = self.presence() else {
            return 0;
        };
        if !self.agent().is_some_and(makes_room) {
            return 0;
        }
        let Some(frame) = self.composer.as_ref().and_then(|c| c.frame_height.get()) else {
            return 0;
        };
        rows_short(
            frame + px(BOX_INSET),
            self.line_height,
            area,
            self.grid_slack + px(GRID_PAD_Y),
        )
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
                // Focused here and now. `refocus` is for a box coming back
                // from stepping aside; set here it would outlive this focus
                // and pull the keyboard back from the next click elsewhere.
                c.open = true;
                c.refocus = false;
                self.focus_composer(window, cx);
            }
        }
        self.remember_composer(cx);
        cx.notify();
    }

    /// Esc in the box. Over the agent's input it is the agent's Esc — the one
    /// that interrupts a turn — since the box is standing in for that input;
    /// for an agent whose Esc does not, its own interrupt key
    /// ([`interrupt_keys`]).
    /// Docked, it hands the keyboard back to the terminal and leaves the box
    /// where it is, so the next Esc is the agent's.
    pub(super) fn composer_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.presence() {
            Presence::Covering(_) => {
                let Some(agent) = self.agent() else { return };
                if let Some(c) = self.composer.as_mut() {
                    c.queue.extend(interrupt_keys(Some(agent)));
                }
                self.pump_composer(agent, cx);
            }
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

    /// Whether files pasted now become the composer's attachments: it has
    /// the keyboard, or covers the input they would be pasted into.
    pub(super) fn composer_takes_files(&self) -> bool {
        let covering = matches!(self.presence(), Presence::Covering(_));
        self.composer_focused && self.composer_shown() || covering
    }

    /// Files arriving by way of the terminal's paste path — dropped on the
    /// box, a copied file or a screenshot pasted into it, an upload to a
    /// remote pane landing — become attachments while the box has the
    /// keyboard, or covers the input they would otherwise be pasted into
    /// (taking the keyboard with them, as typing there does). Hands them
    /// back otherwise, for the terminal to paste.
    pub(super) fn composer_takes_paths(
        &mut self,
        spelled: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Option<Vec<String>> {
        if !self.composer_takes_files() {
            return Some(spelled);
        }
        let focused = self.composer_focused;
        let c = self.composer.as_mut()?;
        c.grab |= !focused;
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

    fn toggle_picker(&mut self, picker: Picker, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(c) = self.composer.as_mut() {
            c.picker = (c.picker != Some(picker)).then_some(picker);
        }
        self.focus_composer(window, cx);
        cx.notify();
    }

    /// Pick from a toolbar list the way the agent takes it: its `/model` or
    /// `/effort` command, sent like any message.
    fn pick(
        &mut self,
        picker: Picker,
        value: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(agent) = self.agent() else {
            return;
        };
        let bracketed = self
            .terminal
            .term
            .lock()
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        let turns = self.agent_session().map_or(0, |s| s.turns);
        let Some(c) = self.composer.as_mut() else {
            return;
        };
        let command = match picker {
            Picker::Model => {
                c.model = Some((value, turns));
                format!("/model {value}")
            }
            Picker::Effort => {
                c.effort = Some((value, turns));
                format!("/effort {value}")
            }
        };
        c.picker = None;
        c.queue.extend(submit_plan(agent, &command, bracketed));
        self.pump_composer(agent, cx);
        self.focus_composer(window, cx);
        cx.notify();
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
        let session = agent
            .and_then(|_| self.agent_session())
            .and_then(|s| s.session_id);
        let mut palette = self.terminal.palette;
        if let Some(active) = cx.try_global::<super::palette::ActivePalette>() {
            palette[..16].copy_from_slice(&active.ansi16);
        }
        let (found, backdrop, screen_lines, offset) = {
            let term = self.terminal.term.lock();
            let found = agent
                .filter(|a| covers(*a))
                .and_then(|a| input_area(a, &screen_rows(&term), term.columns()));
            // The row over the input is the screen around it: the input's own
            // rows can be shaded apart from it.
            let backdrop = found
                .as_ref()
                .and_then(|area| row_backdrop(&term, area.top.saturating_sub(1), &palette))
                .map(|rgb| (rgb, self.cell_width * term.columns() as f32));
            (
                found,
                backdrop,
                term.screen_lines(),
                term.grid().display_offset(),
            )
        };
        let Some(c) = self.composer.as_mut() else {
            return;
        };
        if c.session != session {
            c.session = session;
            c.model = None;
            c.effort = None;
        }

        // The area, with a grace period before it counts as gone.
        let now = Instant::now();
        let mut recheck = None;
        match found {
            Some(area) => {
                c.area = Some(area);
                c.backdrop = backdrop;
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
                Files::Ready(index, _) => file_items(
                    index,
                    &trigger.query,
                    self.files_cwd().as_deref(),
                    self.agent(),
                    self.shell_program().as_deref(),
                ),
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
    ///
    /// Only a hook can say so. An agent without them — Amp — is marked
    /// waiting by any desktop notification it raises, "Agent is ready" when
    /// a turn ends included, and stays so; read as a question, that kept the
    /// box aside, with nothing sendable, from the first finished turn on.
    /// Its own dialogs it puts in its input's place, and the box steps aside
    /// for those off the screen.
    fn agent_is_asking(&self) -> bool {
        self.agent_session()
            .is_some_and(|s| s.rich && s.status == AgentStatus::Waiting)
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
        // Where OpenCode's mentions are looked up, to be picked from its list.
        let mentions_from = match agent == CLIAgent::OpenCode && self.host_id().is_local() {
            true => self.files_cwd(),
            false => None,
        };
        let Some(c) = self.composer.as_mut() else {
            return;
        };
        let bracketed = self
            .terminal
            .term
            .lock()
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        let (held, shell_mode) = match c.presence {
            Presence::Covering(_) => {
                let term = self.terminal.term.lock();
                let rows = screen_rows(&term);
                (
                    held_lines(agent, &rows, term.columns()),
                    in_shell_mode(agent, &rows, term.columns()),
                )
            }
            _ => (0, false),
        };
        let typed = c.input.read(cx).value().to_string();
        // Already in its shell mode, the `!` would take the agent out of it.
        let message = match shell_mode {
            true => typed.strip_prefix('!').unwrap_or(&typed),
            false => &typed,
        };
        let picked = mentions_from.as_deref().and_then(|cwd| {
            let exists = |p: &str| Path::new(p).is_relative() && cwd.join(p).exists();
            opencode_mention_plan(&mention_pieces(message, exists), &c.attached, bracketed)
        });
        let mut steps = match picked {
            Some(steps) => steps,
            None => submit_message(agent, message, &c.attached, shell.as_deref(), bracketed),
        };
        let mut before = Vec::new();
        // Text left in the agent's own input under the box would run into the
        // message: clear it first.
        if let Some(clear) = clear_input(agent, held) {
            before.extend(clear);
        }
        // The shell mode a `!` message left on would take this one for a
        // command: leave it first.
        if shell_mode && !typed.starts_with('!') {
            before.push(Step {
                delay: match before.is_empty() {
                    true => Duration::ZERO,
                    false => SETTLE,
                },
                bytes: b"\x1b".to_vec(),
            });
        }
        if !before.is_empty()
            && let Some(first) = steps.first_mut()
        {
            first.delay = first.delay.max(SETTLE);
            steps.splice(0..0, before);
        }
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
        // Measured while empty: the least the box needs of the pane, for
        // [`Self::composer_reserved_rows`].
        let measure = self.composer.as_ref().and_then(|c| {
            (covering.is_some() && c.attached.is_empty() && c.input.read(cx).value().is_empty())
                .then(|| c.frame_height.clone())
        });
        let frame = div()
            .w_full()
            .when_some(measure, |d, cell| {
                d.on_children_prepainted(move |bounds, window, _| {
                    let Some(b) = bounds.first() else { return };
                    if cell.get() != Some(b.size.height) {
                        cell.set(Some(b.size.height));
                        window.refresh();
                    }
                })
            })
            .child(frame);
        let reserved = self.composer_reserved_rows();
        let element = match covering {
            // Over the input area, painted in the background around it — the
            // grid's own, or the one the agent paints — so what is under it
            // is gone rather than showing through, and the layer is not seen. At least
            // as tall as the area; taller when the box needs it. The box sits
            // at the bottom of the pane, where a chat's input sits.
            Some(rows) => div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                // Measured up from the pane's bottom edge: its padding, the
                // part of a row the grid's height left over, the rows the
                // grid gave up for the box, then the input's rows.
                .min_h(
                    self.line_height * (rows + reserved) as f32 + self.grid_slack + px(GRID_PAD_Y),
                )
                .flex()
                .flex_col()
                .justify_end()
                .px(px(BOX_INSET))
                .pb(px(BOX_INSET))
                .bg(cx.theme().background)
                // Over the grid's own cells, the colour the agent paints
                // them in; the pane's padding around them keeps the theme's.
                .when_some(
                    self.composer.as_ref().and_then(|c| c.backdrop),
                    |d, (backdrop, width)| {
                        d.child(
                            div()
                                .absolute()
                                .top_0()
                                .left(px(GRID_PAD_X))
                                .w(width)
                                .bottom(
                                    self.line_height * reserved as f32
                                        + self.grid_slack
                                        + px(GRID_PAD_Y),
                                )
                                .bg(rgb_to_hsla(backdrop)),
                        )
                    },
                )
                .occlude()
                // Around the box is the terminal as far as a click goes: it
                // takes the keyboard out of the box, as a click on the rows
                // above does. The layer only hides what is under it.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, window, cx| {
                        window.focus(&this.focus_handle, cx);
                    }),
                )
                .child(frame)
                .into_any_element(),
            None => div()
                .flex_none()
                .w_full()
                .pt(px(8.))
                // The pane's own padding is already between it and the edges.
                .px(px(BOX_INSET - GRID_PAD_X))
                .pb(px(BOX_INSET - GRID_PAD_Y))
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
        let (readout, turns) = self
            .agent_session()
            .map(|s| (s.readout, s.turns))
            .unwrap_or_default();
        let theme = cx.theme();
        let focused = c.input.read(cx).focus_handle(cx).is_focused(window);
        let can_send = self.composer_can_send(cx);
        let ink = theme.foreground;
        let muted = theme.muted_foreground;
        let amber = theme.warning;
        let hairline = crate::ui::theme::hairline(window);
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
                .w(crate::ui::theme::hairline(window))
                .h(px(14.))
                .mx(px(4.))
                .bg(ink.opacity(0.15))
        });
        // A toolbar list, opened over its own button.
        let popup = |picker: Picker,
                     title: L10nKey,
                     rows: Vec<(&'static str, String, bool)>,
                     cx: &Context<Self>| {
            crate::ui::theme::floating_surface(div(), cx)
                .rounded(px(10.))
                .absolute()
                .left_0()
                .bottom_full()
                .mb(px(8.))
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .min_w(px(180.))
                .p(px(5.))
                .flex()
                .flex_col()
                .gap(px(1.))
                .text_size(px(13.))
                .text_color(ink)
                .occlude()
                .child(
                    div()
                        .h(px(24.))
                        .px(px(9.))
                        .flex()
                        .items_center()
                        .text_size(px(11.5))
                        .text_color(ink.opacity(0.4))
                        .child(t(title)),
                )
                .children(rows.into_iter().enumerate().map(|(i, (value, label, on))| {
                    h_flex()
                        .id(("composer-picker-row", i))
                        .h(px(30.))
                        .px(px(9.))
                        .gap(px(16.))
                        .rounded(px(6.))
                        .hover(move |s| s.bg(ink.opacity(0.05)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                window.prevent_default();
                                cx.stop_propagation();
                                this.pick(picker, value, window, cx);
                            }),
                        )
                        .child(div().flex_1().whitespace_nowrap().child(label))
                        .child(div().w(px(12.)).when(on, |s| {
                            s.child(
                                Icon::new(IconName::Check)
                                    .size(px(12.))
                                    .text_color(ink.opacity(0.6)),
                            )
                        }))
                }))
        };
        // A toolbar button that opens a list over itself.
        let picker_button = |picker: Picker,
                             id: &'static str,
                             label: gpui::AnyElement,
                             tip: L10nKey,
                             title: L10nKey,
                             rows: Vec<(&'static str, String, bool)>,
                             cx: &Context<Self>| {
            let open = c.picker == Some(picker);
            div()
                .relative()
                .flex_none()
                // A click anywhere else closes the list, as a menu's would.
                // The button and the list stop their own clicks short of this.
                .when(open, |s| {
                    s.on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _w, cx| {
                        if let Some(c) = this.composer.as_mut() {
                            c.picker = None;
                        }
                        cx.notify();
                    }))
                })
                .child(
                    tool(id)
                        .gap(px(5.))
                        .when(open, |s| s.bg(ink.opacity(0.05)))
                        .when(!open, |s| {
                            s.tooltip(move |window, cx| Tooltip::new(t(tip)).build(window, cx))
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.toggle_picker(picker, window, cx)
                        }))
                        .child(label),
                )
                // Painted after everything else, so the box's ring and whatever
                // else is under the list stays under it.
                .when(open, |s| {
                    s.child(gpui::deferred(popup(picker, title, rows, cx)).with_priority(1))
                })
        };

        let model = claude.then(|| {
            let picked = c
                .model
                .as_ref()
                .filter(|(_, then)| *then == turns)
                .map(|(alias, _)| *alias);
            let current = picked.or_else(|| readout.model.as_deref().and_then(model_alias));
            let name = match (picked, readout.model.as_deref()) {
                (Some(alias), _) => alias_label(alias),
                (None, Some(model)) => model_label(model),
                (None, None) => t(L10nKey::ComposerModel).to_string(),
            };
            let label = h_flex()
                .gap(px(5.))
                .child(name)
                .child(
                    Icon::new(IconName::ChevronDown)
                        .size(px(8.))
                        .text_color(ink.opacity(0.4)),
                )
                .into_any_element();
            let rows = CLAUDE_MODELS
                .iter()
                .map(|&alias| (alias, alias_label(alias), current == Some(alias)))
                .collect();
            picker_button(
                Picker::Model,
                "composer-model",
                label,
                L10nKey::ComposerModelTip,
                L10nKey::ComposerModel,
                rows,
                cx,
            )
        });
        let current_effort = c
            .effort
            .as_ref()
            .filter(|(_, then)| *then == turns)
            .map(|(level, _)| level.to_string())
            .or_else(|| readout.effort.clone())
            // A model that takes no effort level has none to show.
            .filter(|level| !level.is_empty());
        let effort = claude.then(|| {
            // The level alone, as the model button shows the model alone;
            // the tooltip and the list's title say what it is.
            let name = match current_effort.as_deref() {
                Some(level) => effort_label(level),
                None => t(L10nKey::ComposerEffort).to_string(),
            };
            let label = h_flex()
                .gap(px(5.))
                .child(name)
                .child(
                    Icon::new(IconName::ChevronDown)
                        .size(px(8.))
                        .text_color(ink.opacity(0.4)),
                )
                .into_any_element();
            let rows = EFFORT_LEVELS
                .iter()
                .map(|&level| {
                    let on = current_effort.as_deref() == Some(level);
                    (level, effort_label(level), on)
                })
                .collect();
            picker_button(
                Picker::Effort,
                "composer-effort",
                label,
                L10nKey::ComposerEffortTip,
                L10nKey::ComposerEffort,
                rows,
                cx,
            )
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
            crate::ui::theme::floating_surface(div(), cx)
                .rounded(px(10.))
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
                        // Not the layer under it, which takes a click as one
                        // on the terminal.
                        cx.stop_propagation();
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
                .when(c.picker.is_some(), |el| {
                    el.capture_action(cx.listener(|this, _: &input::Escape, _w, cx| {
                        cx.stop_propagation();
                        if let Some(c) = this.composer.as_mut() {
                            c.picker = None;
                        }
                        cx.notify();
                    }))
                })
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
                .children(popup.map(|p| gpui::deferred(p).with_priority(1)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .rounded(px(12.))
                        // Opaque: under it can be whatever colour the agent
                        // paints its screen in.
                        .bg(theme.background.blend(ink.opacity(0.035)))
                        .relative()
                        .border_t(hairline)
                        .border_b(hairline)
                        .border_l(hairline)
                        .border_r(hairline)
                        .border_color(ink.opacity(0.15))
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
                                .children(effort)
                                .children(asking)
                                .child(div().flex_1())
                                .child(send),
                        )
                        // Focus and a drag over the box thicken the ring. Drawn
                        // as a layer over the hairline, not as a wider border,
                        // so the box's contents stay where they are.
                        .child(
                            // Laid out inside the hairline, so pulled out over it.
                            div()
                                .absolute()
                                .top(-hairline)
                                .bottom(-hairline)
                                .left(-hairline)
                                .right(-hairline)
                                .rounded(px(12.))
                                .border_1()
                                .border_color(match focused {
                                    true => ink.opacity(0.22),
                                    false => gpui::transparent_black(),
                                })
                                .drag_over::<ExternalPaths>(move |s, _, _, _| {
                                    s.border_color(ink.opacity(0.48))
                                }),
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

    /// The writes [`clear_input`] makes, one per step.
    fn clear_keys(agent: CLIAgent, lines: usize) -> Option<Vec<Vec<u8>>> {
        clear_input(agent, lines).map(|steps| steps.into_iter().map(|s| s.bytes).collect())
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
    fn a_leading_bang_reaches_claude_and_gemini_as_a_key_of_its_own() {
        for agent in [CLIAgent::Claude, CLIAgent::CodeBuddy] {
            let steps = submit_plan(agent, "!git status", true);
            assert_eq!(
                bytes(&steps),
                [&b"!"[..], b"git status", b"\r"],
                "{agent:?}"
            );
            assert_eq!(steps[1].delay, SETTLE, "{agent:?}");
        }
        let steps = submit_plan(CLIAgent::Gemini, "!git status", true);
        assert_eq!(
            bytes(&steps),
            [&b"!"[..], b"\x1b[200~git status\x1b[201~", b"\r"]
        );
        // Anyone else gets the line as written.
        let steps = submit_plan(CLIAgent::Pi, "!git status", true);
        assert_eq!(bytes(&steps), [&b"!git status"[..], b"\r"]);
    }

    #[test]
    fn kimi_is_always_pasted() {
        // Typed, the Enter behind the line is one of its newlines.
        for text in ["fix it", "?why", "!ls", "/usage"] {
            let steps = submit_plan(CLIAgent::Kimi, text, true);
            let pasted = format!("\x1b[200~{text}\x1b[201~");
            assert_eq!(bytes(&steps), [pasted.as_bytes(), b"\r"], "{text}");
            assert_eq!(steps[1].delay, SETTLE, "{text}");
        }
    }

    #[test]
    fn copilot_is_always_pasted() {
        // Typed, the leading `?` would open its help and be lost; its shell
        // mode takes the `!` off the paste.
        for text in ["?why", "!git status", "hi"] {
            let steps = submit_plan(CLIAgent::Copilot, text, true);
            let pasted = format!("\x1b[200~{text}\x1b[201~");
            assert_eq!(bytes(&steps), [pasted.as_bytes(), b"\r"], "{text}");
            assert_eq!(steps[1].delay, SETTLE_AFTER_PASTE_SLOW, "{text}");
        }
    }

    #[test]
    fn gemini_is_always_pasted() {
        // Typed, the `!` would switch it into shell mode and the `?` would
        // open its shortcuts: each reads the input as empty.
        let steps = submit_plan(CLIAgent::Gemini, "hi! ok?", true);
        assert_eq!(bytes(&steps), [&b"\x1b[200~hi! ok?\x1b[201~"[..], b"\r"]);
    }

    #[test]
    fn opencode_and_amp_are_always_pasted() {
        // Typed, OpenCode would take the `!` for its shell mode, and Amp the
        // leading `?` for its shortcuts and a leading `/` for its palette.
        for (agent, text) in [
            (CLIAgent::OpenCode, "PONG! ok?"),
            (CLIAgent::Amp, "?why"),
            (CLIAgent::Amp, "/new"),
        ] {
            let steps = submit_plan(agent, text, true);
            let pasted = format!("\x1b[200~{text}\x1b[201~");
            assert_eq!(bytes(&steps), [pasted.as_bytes(), b"\r"], "{agent:?}");
            assert_eq!(steps[1].delay, SETTLE, "{agent:?}");
        }
        // A leading `!` is still OpenCode's shell-mode key; Amp's `$` works
        // pasted.
        let steps = submit_plan(CLIAgent::OpenCode, "!ls", true);
        assert_eq!(bytes(&steps), [&b"!"[..], b"\x1b[200~ls\x1b[201~", b"\r"]);
        let steps = submit_plan(CLIAgent::Amp, "$ls", true);
        assert_eq!(bytes(&steps), [&b"\x1b[200~$ls\x1b[201~"[..], b"\r"]);
    }

    #[test]
    fn escapes_cannot_close_the_paste_early() {
        let steps = submit_plan(CLIAgent::Claude, "a\n\x1b[201~b", true);
        assert_eq!(bytes(&steps)[0], b"\x1b[200~a\n[201~b\x1b[201~");
    }

    #[test]
    fn copilot_and_gemini_wait_longer_after_a_paste() {
        for agent in [CLIAgent::Copilot, CLIAgent::Gemini] {
            let steps = submit_plan(agent, "a\nb", true);
            assert_eq!(steps[1].delay, SETTLE_AFTER_PASTE_SLOW, "{agent:?}");
        }
        let steps = submit_plan(CLIAgent::Copilot, "ab", false);
        assert_eq!(steps[1].delay, SETTLE);
    }

    #[test]
    fn attachments_follow_the_text_as_shell_words() {
        let attached = vec!["/tmp/a b.png".to_string(), "/src/x.rs".to_string()];
        let claude = CLIAgent::Claude;
        assert_eq!(
            compose_message(claude, "look at these\n", &attached, Some("zsh")),
            "look at these '/tmp/a b.png' /src/x.rs"
        );
        assert_eq!(
            compose_message(claude, "", &attached[1..], Some("zsh")),
            "/src/x.rs"
        );
        assert_eq!(
            compose_message(claude, "just text  ", &[], Some("zsh")),
            "just text"
        );
    }

    #[test]
    fn opencode_and_amp_get_each_attachment_as_a_paste_of_its_own() {
        let attached = vec!["/tmp/a b.png".to_string(), "/src/x.rs".to_string()];
        for agent in [CLIAgent::OpenCode, CLIAgent::Amp] {
            let steps = submit_message(agent, "look\n", &attached, Some("zsh"), true);
            assert_eq!(
                bytes(&steps),
                [
                    &b"\x1b[200~look\x1b[201~"[..],
                    b" ",
                    b"\x1b[200~/tmp/a b.png\x1b[201~",
                    b" ",
                    b"\x1b[200~/src/x.rs\x1b[201~",
                    b"\r",
                ],
                "{agent:?}"
            );
            assert!(steps[1..].iter().all(|s| s.delay >= SETTLE));
            assert_eq!(steps.last().unwrap().delay, SETTLE_AFTER_PASTE_SLOW);
            // Attachments alone: nothing to space them from.
            let steps = submit_message(agent, "", &attached[..1], None, true);
            assert_eq!(
                bytes(&steps),
                [&b"\x1b[200~/tmp/a b.png\x1b[201~"[..], b"\r"]
            );
            assert_eq!(steps[0].delay, Duration::ZERO);
        }
        // Everyone else still gets them as words after the text.
        let steps = submit_message(CLIAgent::Claude, "look", &attached, Some("zsh"), true);
        assert_eq!(
            bytes(&steps),
            [&b"look '/tmp/a b.png' /src/x.rs"[..], b"\r"]
        );
    }

    #[test]
    fn a_closing_mention_is_left_behind_before_gemini_gets_enter() {
        let steps = submit_plan(CLIAgent::Gemini, r"read @my\ notes.txt", false);
        assert_eq!(bytes(&steps), [&br"read @my\ notes.txt "[..], b"\r"]);
        let steps = submit_plan(CLIAgent::Gemini, r#"read @"C:\My Files\a.rs""#, false);
        assert_eq!(bytes(&steps)[0], br#"read @"C:\My Files\a.rs" "#);
        // Nothing to close without a mention, and a command's space would
        // open its arguments.
        let steps = submit_plan(CLIAgent::Gemini, "read it", false);
        assert_eq!(bytes(&steps), [&b"read it"[..], b"\r"]);
        let steps = submit_plan(CLIAgent::Gemini, "/memory", false);
        assert_eq!(bytes(&steps), [&b"/memory"[..], b"\r"]);
        let steps = submit_plan(CLIAgent::Claude, "read @a.rs", true);
        assert_eq!(bytes(&steps), [&b"read @a.rs"[..], b"\r"]);
        // Copilot's list takes that Enter as well.
        let steps = submit_plan(CLIAgent::Copilot, "read @a.rs", false);
        assert_eq!(bytes(&steps), [&b"read @a.rs "[..], b"\r"]);
    }

    #[test]
    fn gemini_gets_its_attachments_as_mentions() {
        let attached = vec!["/tmp/a b.png".to_string(), "/src/x(1).rs".to_string()];
        assert_eq!(
            compose_message(CLIAgent::Gemini, "look at these", &attached, Some("zsh")),
            r"look at these @/tmp/a\ b.png @/src/x\(1\).rs"
        );
        assert_eq!(
            mention(Some(CLIAgent::Gemini), r"C:\My Files\a.png", Some("pwsh")),
            r#"@"C:\My Files\a.png""#
        );
        assert_eq!(
            mention(Some(CLIAgent::Gemini), r"C:\src\a.rs", Some("pwsh")),
            r"@C:\src\a.rs"
        );
    }

    #[test]
    fn copilot_and_codebuddy_get_their_attachments_as_mentions_where_they_can() {
        let attached = vec!["/tmp/red.png".to_string(), "/tmp/a b.png".to_string()];
        for agent in [CLIAgent::Copilot, CLIAgent::CodeBuddy] {
            assert_eq!(
                compose_message(agent, "look", &attached, Some("zsh")),
                "look @/tmp/red.png '/tmp/a b.png'",
                "{agent:?}"
            );
        }
        // Copilot's list would take the Enter after a closing mention.
        let message = compose_message(CLIAgent::Copilot, "look", &attached[..1], Some("zsh"));
        let steps = submit_plan(CLIAgent::Copilot, &message, true);
        assert_eq!(bytes(&steps)[0], b"\x1b[200~look @/tmp/red.png \x1b[201~");
    }

    #[test]
    fn mentions_close_before_enter_for_oh_my_pi_and_grok_and_prime_is_pasted() {
        // Their `@` list would take the Enter after a closing mention.
        for agent in [CLIAgent::OhMyPi, CLIAgent::Grok] {
            let steps = submit_plan(agent, "read @plain.txt", true);
            assert_eq!(
                bytes(&steps),
                [&b"read @plain.txt "[..], b"\r"],
                "{agent:?}"
            );
            let steps = submit_plan(agent, "/model", true);
            assert_eq!(bytes(&steps), [&b"/model"[..], b"\r"], "{agent:?}");
        }
        // Typed, Prime Agent's list stays open past the space and takes the
        // Enter; pasted, it reads the line whole.
        for text in ["read @plain.txt", "!ls", "/session", "hi"] {
            let steps = submit_plan(CLIAgent::PrimeAgent, text, true);
            let pasted = format!("\x1b[200~{text}\x1b[201~");
            assert_eq!(bytes(&steps), [pasted.as_bytes(), b"\r"], "{text}");
        }
    }

    /// Oh My Pi reads a mentioned file, an image too, into the turn; a
    /// mention with a space goes in its quotes.
    #[test]
    fn oh_my_pi_gets_its_attachments_as_mentions() {
        let attached = vec!["/tmp/red.png".to_string(), "/tmp/a b.png".to_string()];
        assert_eq!(
            compose_message(CLIAgent::OhMyPi, "look", &attached, Some("zsh")),
            "look @/tmp/red.png @\"/tmp/a b.png\""
        );
        assert_eq!(
            compose_message(CLIAgent::Pi, "look", &attached, Some("zsh")),
            "look /tmp/red.png '/tmp/a b.png'"
        );
    }

    #[test]
    fn qwen_is_spoken_to_the_way_gemini_is() {
        // Typed, its leading `?` opens the shortcuts and is lost.
        let steps = submit_plan(CLIAgent::Qwen, "?why", true);
        assert_eq!(bytes(&steps), [&b"\x1b[200~?why\x1b[201~"[..], b"\r"]);
        assert_eq!(steps[1].delay, SETTLE_AFTER_PASTE_SLOW);
        let steps = submit_plan(CLIAgent::Qwen, "!ls", true);
        assert_eq!(bytes(&steps), [&b"!"[..], b"\x1b[200~ls\x1b[201~", b"\r"]);
        // Its `@` list takes an Enter that comes right after a mention.
        let steps = submit_plan(CLIAgent::Qwen, "read @a.rs", false);
        assert_eq!(bytes(&steps), [&b"read @a.rs "[..], b"\r"]);
        // Mentions as Gemini spells them, with the comma Qwen ends one at.
        assert_eq!(
            mention(Some(CLIAgent::Qwen), "my notes, v2.md", Some("zsh")),
            r"@my\ notes\,\ v2.md"
        );
        assert_eq!(
            mention(Some(CLIAgent::Gemini), "a,b.md", Some("zsh")),
            "@a,b.md"
        );
        let attached = vec!["/tmp/a b.png".to_string()];
        assert_eq!(
            compose_message(CLIAgent::Qwen, "look", &attached, Some("zsh")),
            r"look @/tmp/a\ b.png"
        );
    }

    #[test]
    fn mentions_are_cut_out_where_they_name_a_path() {
        let exists = |p: &str| ["my notes.md", "plain.md", "src/a b/c.rs"].contains(&p);
        assert_eq!(
            mention_pieces("read @my notes.md and @plain.md.", exists),
            [
                Piece::Text("read "),
                Piece::Mention("my notes.md"),
                Piece::Text(" and "),
                Piece::Mention("plain.md"),
                Piece::Text("."),
            ]
        );
        assert_eq!(
            mention_pieces("@src/a b/c.rs", exists),
            [Piece::Mention("src/a b/c.rs")]
        );
        // An `@` inside a word, or naming nothing, stays words.
        for text in ["mail me@plain.md", "ask @someone about it", "@ plain.md"] {
            assert_eq!(mention_pieces(text, exists), [Piece::Text(text)]);
        }
        assert_eq!(mention_pieces("", exists), []);
    }

    /// OpenCode attaches only what is picked from its `@` list, so each
    /// mention is picked there: `@`, the path as the query, Enter.
    #[test]
    fn opencode_picks_each_mention_from_its_own_list() {
        let pieces = [
            Piece::Text("read "),
            Piece::Mention("my notes.md"),
            Piece::Text(" now, please"),
        ];
        let steps = opencode_mention_plan(&pieces, &[], true).unwrap();
        assert_eq!(
            bytes(&steps),
            [
                &b"\x1b[200~read \x1b[201~"[..],
                b"@",
                b"\x1b[200~mynotes.md\x1b[201~",
                b"\r",
                b"\x1b[200~now, please\x1b[201~",
                b"\r",
            ]
        );
        assert_eq!(steps[0].delay, Duration::ZERO);
        assert_eq!(steps[3].delay, OPENCODE_LIST_WAIT);
        assert_eq!(steps[5].delay, SETTLE_AFTER_PASTE_SLOW);
        // With no space in what follows, one of its own closes the list.
        let steps =
            opencode_mention_plan(&[Piece::Mention("a.md"), Piece::Text(".")], &[], true).unwrap();
        assert_eq!(
            bytes(&steps),
            [
                &b"@"[..],
                b"\x1b[200~a.md\x1b[201~",
                b"\r",
                b" ",
                b"\x1b[200~.\x1b[201~",
                b"\r",
            ]
        );
        // Ending on one, a space closes a list that found nothing, and
        // attachments still follow as pastes of their own.
        let attached = vec!["/tmp/a.png".to_string()];
        let steps = opencode_mention_plan(&[Piece::Mention("plain.md")], &attached, true).unwrap();
        assert_eq!(
            bytes(&steps),
            [
                &b"@"[..],
                b"\x1b[200~plain.md\x1b[201~",
                b"\r",
                b" ",
                b" ",
                b"\x1b[200~/tmp/a.png\x1b[201~",
                b"\r",
            ]
        );
        // Without a mention, or as a command or shell line, it goes as usual.
        assert_eq!(opencode_mention_plan(&[Piece::Text("hi")], &[], true), None);
        assert_eq!(opencode_mention_plan(&pieces, &[], false), None);
        let command = [Piece::Text("/review "), Piece::Mention("plain.md")];
        assert_eq!(opencode_mention_plan(&command, &[], true), None);
    }

    #[test]
    fn qoder_is_spoken_to_the_way_gemini_is() {
        for agent in [CLIAgent::QoderCLI, CLIAgent::QoderCLICn] {
            // Typed, its leading `?` opens the shortcuts and is lost, and an
            // Enter right behind a paste is a newline.
            let steps = submit_plan(agent, "?why", true);
            assert_eq!(bytes(&steps), [&b"\x1b[200~?why\x1b[201~"[..], b"\r"]);
            assert_eq!(steps[1].delay, SETTLE_AFTER_PASTE_SLOW);
            let steps = submit_plan(agent, "!ls", true);
            assert_eq!(bytes(&steps), [&b"!"[..], b"\x1b[200~ls\x1b[201~", b"\r"]);
            // Its `@` list takes an Enter that comes right after a mention.
            let steps = submit_plan(agent, "read @a.rs", true);
            assert_eq!(
                bytes(&steps),
                [&b"\x1b[200~read @a.rs \x1b[201~"[..], b"\r"]
            );
            // Its list puts a path with a space in as `@my\ notes.md`.
            assert_eq!(
                mention(Some(agent), "my notes.md", Some("zsh")),
                r"@my\ notes.md"
            );
        }
    }

    #[test]
    fn a_mention_with_a_space_is_spelled_the_way_each_agent_reads_it() {
        let zsh = Some("zsh");
        assert_eq!(
            mention(Some(CLIAgent::Claude), "my notes.md", zsh),
            r#"@"my notes.md""#
        );
        assert_eq!(
            mention(Some(CLIAgent::Claude), "src/a.rs", zsh),
            "@src/a.rs"
        );
        assert_eq!(
            mention(Some(CLIAgent::Codex), "my notes.md", zsh),
            r#""my notes.md""#
        );
        assert_eq!(mention(Some(CLIAgent::Codex), "src/a.rs", zsh), "src/a.rs");
        assert_eq!(
            mention(Some(CLIAgent::Gemini), "my notes.md", zsh),
            r"@my\ notes.md"
        );
        assert_eq!(
            mention(Some(CLIAgent::Kimi), "my notes.md", zsh),
            r#"@"my notes.md""#
        );
        assert_eq!(mention(Some(CLIAgent::Kimi), "src/a.rs", zsh), "@src/a.rs");
        for agent in [CLIAgent::Pi, CLIAgent::OhMyPi, CLIAgent::PrimeAgent] {
            assert_eq!(
                mention(Some(agent), "my notes.md", zsh),
                "@\"my notes.md\"",
                "{agent:?}"
            );
            assert_eq!(mention(Some(agent), "src/a.rs", zsh), "@src/a.rs");
        }
        // Grok Build's own list puts the path in as it is.
        assert_eq!(
            mention(Some(CLIAgent::Grok), "my notes.md", zsh),
            "@my notes.md"
        );
        assert_eq!(mention(None, "my notes.md", zsh), "@my notes.md");
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
    fn codex_lists_are_not_its_input() {
        let lists = [
            // The hook review at launch.
            &[
                "  Hooks need review",
                "",
                "› 1. Review hooks",
                "  2. Trust all and continue",
                "  3. Continue without trusting (hooks won't run)",
                "",
                "  Press enter to confirm or esc to go back",
            ][..],
            // `/permissions`, its descriptions wrapping, the last one picked.
            &[
                "  Update Model Permissions",
                "  1. Ask for approval (current)  Read and edit workspace files",
                "                                 required for internet access",
                "  2. Approve for me              Only ask for actions",
                "› 3. Full Access                 Use with caution: Codex can",
                "                                 and access the internet",
                "  enter select · esc back",
            ][..],
            // A command waiting for approval.
            &[
                "  Would you like to run the following command?",
                "  $ touch a.txt",
                "› 1. Yes, proceed (y)",
                "  2. Yes, and don't ask again for these files (p)",
                "  3. No, and tell Codex what to do differently (esc)",
            ][..],
        ];
        for rows in lists {
            assert_eq!(input_area(CLIAgent::Codex, &screen(rows), 40), None);
        }
        // A message of the user's own that happens to open like an option.
        let rows = screen(&["• Done.", "", "› 1. fix the build", "", "  ⏎ send"]);
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

    const LOWER_HALVES: &str = "▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄";
    const UPPER_HALVES: &str = "▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀";

    #[test]
    fn geminis_input_is_its_shaded_block_over_the_footer() {
        let rows = screen(&[
            LOWER_HALVES,
            " > say pong",
            UPPER_HALVES,
            "",
            "✦ pong",
            "",
            "? for shortcuts",
            RULE,
            " Shift+Tab to accept edits",
            LOWER_HALVES,
            " >   Type your message or @path/to/file",
            UPPER_HALVES,
            " workspace (/directory)   sandbox   /model",
            " ~/proj                   no sandbox gpt-5",
        ]);
        // The box covers its mode line, the rule over it and the hint too.
        assert_eq!(
            input_area(CLIAgent::Gemini, &rows, 40),
            Some(InputArea { top: 6, mode: None })
        );
        // Shell mode and YOLO mode change only the prompt's mark.
        for mark in [" ! ls", " * fix it"] {
            let mut rows = rows.clone();
            rows[10] = mark.to_string();
            assert_eq!(
                input_area(CLIAgent::Gemini, &rows, 40).map(|a| a.top),
                Some(6)
            );
        }
        // Without a footer, too.
        assert_eq!(
            input_area(CLIAgent::Gemini, &rows[..12], 40).map(|a| a.top),
            Some(6)
        );
        // Without a mode line, from the shaded block's top edge.
        let mut bare = rows.clone();
        bare.drain(6..9);
        assert_eq!(
            input_area(CLIAgent::Gemini, &bare, 40).map(|a| a.top),
            Some(6)
        );
        assert_eq!(bare[6], LOWER_HALVES);
    }

    #[test]
    fn a_sent_gemini_message_is_not_its_input() {
        // The input gives its place to a permission prompt; the last message
        // sent, shaded like the input, is still on screen above it.
        let rows = screen(&[
            LOWER_HALVES,
            " > make hello.txt",
            UPPER_HALVES,
            "",
            "╭──────────────────────────────────────╮",
            "│ ?  WriteFile Writing to hello.txt     │",
            "│ Allow this change?                    │",
            "│ ● 1. Allow once                       │",
            "│   2. Allow for this session           │",
            "│   3. No, suggest changes (esc)        │",
            "╰──────────────────────────────────────╯",
        ]);
        assert_eq!(input_area(CLIAgent::Gemini, &rows, 40), None);
    }

    #[test]
    fn geminis_unshaded_input_is_the_prompt_under_its_rule() {
        let rows = screen(&[
            "✦ pong",
            "",
            "? for shortcuts",
            RULE,
            " Shift+Tab to accept edits",
            RULE,
            " >   Type your message or @path/to/file",
            " workspace (/directory)   sandbox   /model",
        ]);
        // From the hint over its mode line's rule.
        assert_eq!(
            input_area(CLIAgent::Gemini, &rows, 40),
            Some(InputArea { top: 2, mode: None })
        );
    }

    const PI_FOOTER: [&str; 2] = [
        "/private/tmp/t7b5/proj (main)",
        "↑29k ↓52 1.9%/200k (auto)          claude-sonnet-4-6",
    ];

    fn pi_screen(lines: &[&str]) -> Vec<String> {
        let mut rows = screen(lines);
        rows.extend(PI_FOOTER.iter().map(|r| r.to_string()));
        rows
    }

    #[test]
    fn pis_input_is_its_lowest_ruled_editor() {
        // Empty, idle.
        let rows = pi_screen(&[" 只回复两个字：好的", "", " 好的", "", RULE, "", RULE]);
        assert_eq!(
            input_area(CLIAgent::Pi, &rows, 40),
            Some(InputArea { top: 4, mode: None })
        );
        // Several lines in it, while a turn runs and the spinner rides the
        // upper rule.
        let working = "── ⠼ Working ──────────────────────────────";
        let rows = pi_screen(&["", working, "line one", "line two", RULE]);
        assert_eq!(
            input_area(CLIAgent::Pi, &rows, 40),
            Some(InputArea { top: 1, mode: None })
        );
        // Its completion list opens between the editor and the status rows.
        let rows = pi_screen(&[
            RULE,
            "/mod",
            RULE,
            "→ model          <provider/model> — Select model",
            "  scoped-models  Enable/disable models for Ctrl+P cycling",
        ]);
        assert_eq!(
            input_area(CLIAgent::Pi, &rows, 40),
            Some(InputArea { top: 0, mode: None })
        );
    }

    /// Pi's selectors take the editor's place between its rules: none of
    /// them is the input.
    #[test]
    fn pis_selectors_are_not_its_input() {
        let model = pi_screen(&[
            " 好的",
            "",
            RULE,
            "",
            "Only showing models from configured providers. Use /login to add providers.",
            "",
            ">",
            "",
            "→ ✓ claude-sonnet-4-6 [relay] · default",
            "    gpt-5.5 [relay]",
            "",
            "  Enter to select · Ctrl+S to set as default · Escape/Ctrl+C to cancel",
            RULE,
        ]);
        assert_eq!(input_area(CLIAgent::Pi, &model, 40), None);
        let settings = pi_screen(&[
            " Operation aborted",
            "",
            RULE,
            ">",
            "",
            "→ Auto-compact                      true",
            "  Auto-resize images                true",
            "  (1/32)",
            "",
            "  Type to search · Enter/Space to change · Esc to cancel",
            RULE,
        ]);
        assert_eq!(input_area(CLIAgent::Pi, &settings, 40), None);
        let resume = pi_screen(&[
            RULE,
            "",
            "Resume Session (Current Folder)     ◉ Current Folder | ○ All",
            "tab scope · re:<pattern> regex · \"phrase\" exact",
            "",
            ">",
            "",
            "› Reply with just: ok1                         18 now",
            RULE,
        ]);
        assert_eq!(input_area(CLIAgent::Pi, &resume, 40), None);
        // A ruled block far above the bottom is the transcript's.
        let mut scrolled = screen(&[RULE, "quoted", RULE]);
        scrolled.extend((0..12).map(|i| format!(" reply line {i}")));
        assert_eq!(input_area(CLIAgent::Pi, &scrolled, 40), None);
    }

    const OMP_BAR: &str =
        " π > ◒ claude-sonnet-4-6 (relay) > 🗑 t7b5/proj > ⑂ main ?15 ▶─4%─────┃────200K─";

    #[test]
    fn omps_input_hangs_under_its_status_line() {
        let rows = screen(&[
            " DONE",
            "",
            OMP_BAR,
            "╰─                                  ⇧⇥ to change thinking effort",
        ]);
        assert_eq!(
            input_area(CLIAgent::OhMyPi, &rows, 40),
            Some(InputArea { top: 2, mode: None })
        );
        // Several lines, and its command list open under them.
        let rows = screen(&[
            RULE,
            "",
            OMP_BAR,
            "╰─ /model",
            "   second line",
            "❯ ⬢  model         Model: relay/claude-sonnet-4-6                █",
            "  ⬢  modelpreset   Presets: none saved                           █",
        ]);
        assert_eq!(
            input_area(CLIAgent::OhMyPi, &rows, 40),
            Some(InputArea { top: 2, mode: None })
        );
        // The rounded box shape.
        let rows = screen(&[
            "╭── π > ⬢ Sonnet 4.5 · ◒ high > 🗺 Plan ▶────────┃──◀ ◫ 62.0%/200K ⟲ ──╮",
            "╰─ Ask anything, edit files, run tools                               ─╯",
        ]);
        assert_eq!(
            input_area(CLIAgent::OhMyPi, &rows, 40),
            Some(InputArea { top: 0, mode: None })
        );
        // The Claude Code shape, ruled like Pi's.
        let rows = screen(&[
            "───────────────────────────── gallery · ◫ 62.0%/200K ⟲ ─",
            "❯ Ask anything, edit files, run tools",
            RULE,
            " π · ⬢ Sonnet 4.5 · ◒ high · 🗺 Plan",
        ]);
        assert_eq!(
            input_area(CLIAgent::OhMyPi, &rows, 40),
            Some(InputArea { top: 0, mode: None })
        );
    }

    /// A tool approval and the model picker take the input's place, framed;
    /// a frame's bottom edge is not the input's first line.
    #[test]
    fn omps_dialogs_are_not_its_input() {
        let approval = screen(&[
            "╭──────────────────────────────────────╮",
            "│ $ touch omp2.txt                     │",
            "╰──────────────────────────────────────╯",
            "  ⎋ Creating omp2.txt",
            "╭─ Allow tool: bash ───────────────────╮",
            "│ Command: touch omp2.txt              │",
            "│  ❯ Approve                           │",
            "│    Deny                              │",
            "│ ↑/↓ navigate  ⏎ select  ⎋ cancel     │",
            "╰──────────────────────────────────────╯",
        ]);
        assert_eq!(input_area(CLIAgent::OhMyPi, &approval, 40), None);
        let picker = screen(&[
            "│   ○ anthropic   │   local/qwen2.5-1.5b          free█ │",
            "│   ○ gmi-cloud   │   ● default ◒ · ○ smol · ○ slow   │",
            "├─────────────────┴──────────────────────────────────┤",
            "│ ⏎/→ models · ↑/↓ providers · type to search · ⎋ close │",
            "╰────────────────────────────────────────────────────╯",
        ]);
        assert_eq!(input_area(CLIAgent::OhMyPi, &picker, 40), None);
    }

    const PRIME_FOOTER: &str = "← manage                           claude-sonnet-4-6 · 0 (0%)";

    #[test]
    fn prime_agents_input_is_its_shaded_block_over_the_footer() {
        let rows = screen(&[
            "",
            "                      Details mode (Ctrl+O to expand)",
            "",
            " >",
            "",
            PRIME_FOOTER,
        ]);
        assert_eq!(
            input_area(CLIAgent::PrimeAgent, &rows, 40),
            Some(InputArea { top: 2, mode: None })
        );
        // Several lines, while a turn runs, its `/` list open above.
        let rows = screen(&[
            " ⠙ Writing · 4s · ↓ 99 tokens",
            "    › model          [search]",
            "      scoped-models",
            "",
            " >  /model",
            "    second line",
            "",
            "Press Ctrl+C again to exit       claude-sonnet-4-6 · 0 (0%)",
        ]);
        assert_eq!(
            input_area(CLIAgent::PrimeAgent, &rows, 40),
            Some(InputArea { top: 3, mode: None })
        );
    }

    #[test]
    fn prime_agents_model_picker_is_not_its_input() {
        let rows = screen(&[
            "                      Details mode (Ctrl+O to expand)",
            RULE,
            " >  Search models",
            RULE,
            "› claude-sonnet-4-6 (relay)               current · relay",
            "  gpt-5.5 (relay)                                  relay",
            "  (1/1317)",
            "",
            " Input          Cached input          Output",
            " $0             $0                    $0",
            "",
            " ↑/↓ model · ←/→ effort · Enter select · Esc close",
        ]);
        assert_eq!(input_area(CLIAgent::PrimeAgent, &rows, 40), None);
        // A transcript's last words are not a footer with a block over it.
        let rows = screen(&["  Reply with just the word PONG.", "", " PONG"]);
        assert_eq!(input_area(CLIAgent::PrimeAgent, &rows, 40), None);
    }

    const GROK_TOP: &str = "  ╭──────────────────────────────────────╮";
    const GROK_BOTTOM: &str = "  ╰─────────────────── gpt-5.5 (relay) ─╯";

    #[test]
    fn grok_builds_input_is_its_rounded_frame() {
        let rows = screen(&[
            "  Update: v1.0.46 available, press ctrl+u to restart",
            "",
            GROK_TOP,
            "  │ ❯                                    │",
            GROK_BOTTOM,
            "",
            "                 Logged in with API key  │  [stable]",
        ]);
        assert_eq!(
            input_area(CLIAgent::Grok, &rows, 40),
            Some(InputArea { top: 2, mode: None })
        );
        // Several lines, while a turn runs.
        let rows = screen(&[
            "    ⠼ Waiting for response… 3.1s        3.1s ⇣2.26k [stop]",
            "",
            GROK_TOP,
            "  │ ❯ line one                           │",
            "  │   line two                           │",
            GROK_BOTTOM,
            "",
            "  Shift+Tab:mode  │  Ctrl+c:cancel  │  Ctrl+x:shortcuts",
        ]);
        assert_eq!(
            input_area(CLIAgent::Grok, &rows, 40),
            Some(InputArea { top: 2, mode: None })
        );
    }

    /// A list open over the frame takes the keys the box would send; a framed
    /// block in the transcript is not the input.
    #[test]
    fn grok_builds_lists_and_transcript_frames_are_not_its_input() {
        let rows = screen(&[
            "  ───────────────────────────────────────4─",
            "    ❯ Grok 4.6                  SpaceXAI's latest",
            "      gpt-5.5 (relay) (current)",
            "  ────────────────────────────────────────",
            GROK_TOP,
            "  │ ❯ /model <model> [window] [effort]   │",
            GROK_BOTTOM,
            "",
            "  Enter:send  │  Opt+Enter:newline  │  Ctrl+x:shortcuts",
        ]);
        assert_eq!(input_area(CLIAgent::Grok, &rows, 40), None);
        // The release notes, drawn over the frame.
        let rows = screen(&[
            "  Update: │                                   │",
            "  ╭───────│        ↑/↓ scroll  |  Esc back    │──╮",
            "  │ ❯     └───────────────────────────────────┘  │",
            GROK_BOTTOM,
            "",
            "                 Logged in with API key  │  [stable]",
        ]);
        assert_eq!(input_area(CLIAgent::Grok, &rows, 40), None);
        let rows = screen(&[
            GROK_TOP,
            "  │ ❯ earlier                            │",
            GROK_BOTTOM,
            " reply line 1",
            " reply line 2",
            " reply line 3",
            " reply line 4",
        ]);
        assert_eq!(input_area(CLIAgent::Grok, &rows, 40), None);
    }

    /// A dismissed `/model` stays in Grok Build's input under the box, and is
    /// cleared before the next message rather than run into it.
    #[test]
    fn a_command_left_in_grok_builds_input_is_cleared_first() {
        let frame = |line: &str| {
            screen(&[
                GROK_TOP,
                line,
                GROK_BOTTOM,
                "",
                "  Enter:send  │  Opt+Enter:newline",
            ])
        };
        let left = frame("  │ ❯ /model                             │");
        let empty = frame("  │ ❯                                    │");
        assert_eq!(held_lines(CLIAgent::Grok, &left, 40), 1);
        assert_eq!(held_lines(CLIAgent::Grok, &empty, 40), 0);
        assert_eq!(held_lines(CLIAgent::Pi, &left, 40), 0);
        assert_eq!(clear_keys(CLIAgent::Grok, 1), Some(vec![b"\x15".to_vec()]));
        assert_eq!(clear_keys(CLIAgent::Grok, 0), None);
        assert_eq!(clear_keys(CLIAgent::Claude, 1), None);
        // Its Esc does not stop a turn; Ctrl+C does.
        assert_eq!(bytes(&interrupt_keys(Some(CLIAgent::Grok))), [b"\x03"]);
        assert_eq!(bytes(&interrupt_keys(Some(CLIAgent::Pi))), [b"\x1b"]);
        // Crush asks for its Esc twice, in separate reads.
        let crush = interrupt_keys(Some(CLIAgent::Crush));
        assert_eq!(bytes(&crush), [b"\x1b", b"\x1b"]);
        assert!(crush[1].delay >= SETTLE);
    }

    const CRUSH_HELP: &str =
        " tab focus chat • shift+tab mode • / or ctrl+p commands • ctrl+m models";

    #[test]
    fn crushs_input_is_its_editor_over_the_key_help() {
        let rows = screen(&[
            " │ Reply with just PONG",
            "",
            "   PONG",
            "",
            "   ◇ claude-sonnet-4-6 (relay) via Kalowave relay in 3s ──────────",
            "",
            "",
            "   > Ready...",
            " :::",
            " :::",
            "",
            CRUSH_HELP,
            "",
        ]);
        assert_eq!(
            input_area(CLIAgent::Crush, &rows, 80),
            Some(InputArea { top: 7, mode: None })
        );
        // Several lines, yolo mode's prompt, the chat holding the keys.
        for (first, top) in [
            ("   > line one", 4),
            ("  !  line one", 4),
            (" ::: Ready...", 4),
        ] {
            let rows = screen(&[
                "   PONG",
                "",
                "",
                "",
                first,
                " ::: line two",
                " ::: line three",
                " ::: line four",
                "",
                " esc cancel • tab focus chat • shift+tab mode",
                "",
            ]);
            assert_eq!(
                input_area(CLIAgent::Crush, &rows, 80),
                Some(InputArea { top, mode: None }),
                "{first}"
            );
        }
    }

    /// Crush's dialogs are framed over the screen with its editor left under
    /// them, and take the keys while open.
    #[test]
    fn crushs_dialogs_are_not_its_input() {
        let rows = screen(&[
            "   PONG                                    main",
            "        ╭──────────────────────────────╮  /tmp/t7b6/proj",
            "        │ Commands ╱╱╱╱╱╱╱╱╱╱╱╱╱╱╱╱╱╱╱ │",
            "        │ > Type to filter             │",
            "        │ New Session          ctrl+n  │",
            "        ╰──────────────────────────────╯",
            "",
            "   > Ready...",
            " :::",
            " :::",
            "",
            CRUSH_HELP,
        ]);
        assert_eq!(input_area(CLIAgent::Crush, &rows, 80), None);
        // A permission request runs down over the editor's first line.
        let rows = screen(&[
            "     ╭──────────────────────────────────────╮",
            " │ Us│  Permission Required ╱╱╱╱╱╱╱╱╱╱╱╱╱╱╱  │",
            "     │ Tool write                           │",
            "     │        Allow   Allow for Session  Deny│",
            "   > ╰──────────────────────────────────────╯",
            " :::",
            " :::",
            "",
            CRUSH_HELP,
        ]);
        assert_eq!(input_area(CLIAgent::Crush, &rows, 80), None);
        // Its splash, before the editor is drawn.
        let rows = screen(&[
            "  Would you like to initialize now?",
            "",
            "    Yep!     Nope",
            "",
            " ctrl+c quit • ctrl+g more",
        ]);
        assert_eq!(input_area(CLIAgent::Crush, &rows, 80), None);
    }

    const GOOSE_GAUGE: &str = "  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌ 1% 5k/1.0M";

    #[test]
    fn gooses_input_is_its_prompt_under_the_gauge_at_any_height() {
        let rows = screen(&[
            "    __( O)>  ● new session · anthropic claude-sonnet-4-6",
            "     L L     goose is ready",
            "  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌ 0% 0/1.0M",
            "> Enter to send · Ctrl+J newline",
        ]);
        let mut tall = rows.clone();
        tall.extend(std::iter::repeat_n(String::new(), 60));
        for rows in [rows, tall] {
            assert_eq!(
                input_area(CLIAgent::Goose, &rows, 80),
                Some(InputArea { top: 2, mode: None })
            );
        }
        let rows = screen(&["PONG", "  ⏱ 4.60s", GOOSE_GAUGE, "> ", ""]);
        assert_eq!(
            input_area(CLIAgent::Goose, &rows, 80),
            Some(InputArea { top: 2, mode: None })
        );
    }

    /// While a turn runs, its spinner and output are printed under the line
    /// sent; a tool approval is printed there too.
    #[test]
    fn a_running_goose_turn_is_not_its_input() {
        let spinner = screen(&[
            GOOSE_GAUGE,
            "> Reply with just PONG",
            "",
            "◓  Webbing connections...  (Ctrl+C to interrupt)",
        ]);
        let output = screen(&[
            GOOSE_GAUGE,
            "> Run the shell command: ls",
            "  ────────────────────────────────────────",
            "  ▸ shell",
            "    command: ls",
        ]);
        let transcript = screen(&[GOOSE_GAUGE, "> earlier", "PONG", "  ⏱ 4.60s"]);
        for rows in [spinner, output, transcript] {
            assert_eq!(input_area(CLIAgent::Goose, &rows, 80), None);
        }
    }

    const AMP_TOP: &str = "╭─────────────────────────────── medium ─╮";
    const AMP_BOTTOM: &str = "╰──────────────── /tmp/t7b6/proj (main) ─╯";
    const AMP_EMPTY: &str = "│                                      │";

    #[test]
    fn amps_input_is_the_frame_at_the_bottom() {
        let rows = screen(&[
            "                    •●●●●••     Welcome to Amp",
            "",
            AMP_TOP,
            AMP_EMPTY,
            AMP_EMPTY,
            AMP_EMPTY,
            AMP_BOTTOM,
            "",
        ]);
        assert_eq!(
            input_area(CLIAgent::Amp, &rows, 40),
            Some(InputArea { top: 2, mode: None })
        );
        // Sending, with the thread above.
        let rows = screen(&[
            " ┃ Reply with just PONG",
            "╭──────────────────────── $···· ─ medium ─╮",
            "│ line one                               │",
            "│ line two                               │",
            "╰ ∼ Sending ───────── /tmp/t7b6/proj (main) ─╯",
        ]);
        assert_eq!(
            input_area(CLIAgent::Amp, &rows, 40),
            Some(InputArea { top: 1, mode: None })
        );
    }

    /// Amp's palette opens framed over the screen; a notice in the input's
    /// place is a frame with nothing in its bottom edge.
    #[test]
    fn amps_palette_and_notices_are_not_its_input() {
        let rows = screen(&[
            "      ╭─ Command Palette ────────────╮",
            "      │ >                            │",
            "   ●● │    thread  new in orb        │",
            "      ╰──────────────────────────────╯",
            "",
            AMP_TOP,
            AMP_EMPTY,
            AMP_BOTTOM,
        ]);
        assert_eq!(input_area(CLIAgent::Amp, &rows, 40), None);
        let rows = screen(&[
            " ┃ Reply with just PONG",
            "╭──────────────────────────────────────╮",
            "│ Out of Credits                       │",
            "│ ‣ Add Paid Credits                   │",
            "│   Dismiss                            │",
            "╰──────────────────────────────────────╯",
        ]);
        assert_eq!(input_area(CLIAgent::Amp, &rows, 40), None);
    }

    /// What was typed into Crush's, Goose's or Amp's own input before the box
    /// was laid over it is cleared, line by line, before the next message.
    #[test]
    fn text_left_in_crush_goose_and_amp_inputs_is_cleared_first() {
        let crush = |lines: &[&str]| {
            let mut rows = vec!["   PONG".to_string(), String::new()];
            rows.extend(lines.iter().map(|l| l.to_string()));
            rows.extend(["", CRUSH_HELP].map(String::from));
            rows
        };
        assert_eq!(
            held_lines(
                CLIAgent::Crush,
                &crush(&["   > Ready...", " :::", " :::"]),
                80
            ),
            0
        );
        assert_eq!(
            held_lines(
                CLIAgent::Crush,
                &crush(&["  !  Go crazy", " :::", " :::"]),
                80
            ),
            0
        );
        assert_eq!(
            held_lines(CLIAgent::Crush, &crush(&["   > left", " :::", " :::"]), 80),
            1
        );
        assert_eq!(
            held_lines(
                CLIAgent::Crush,
                &crush(&["   > one", " ::: two", " ::: three", " :::"]),
                80
            ),
            3
        );

        let goose = |line: &str| screen(&[GOOSE_GAUGE, line, ""]);
        assert_eq!(
            held_lines(
                CLIAgent::Goose,
                &goose("> Enter to send · Ctrl+J newline"),
                80
            ),
            0
        );
        assert_eq!(held_lines(CLIAgent::Goose, &goose("> "), 80), 0);
        assert_eq!(
            held_lines(CLIAgent::Goose, &goose("> leftover words"), 80),
            1
        );

        let amp = |inside: &[&str]| {
            let mut rows = vec![AMP_TOP.to_string()];
            rows.extend(inside.iter().map(|l| l.to_string()));
            rows.push(AMP_BOTTOM.to_string());
            rows
        };
        assert_eq!(
            held_lines(CLIAgent::Amp, &amp(&[AMP_EMPTY, AMP_EMPTY]), 40),
            0
        );
        assert_eq!(
            held_lines(
                CLIAgent::Amp,
                &amp(&[
                    "│ x @my notes.md                       │",
                    "│ line two                             │",
                    AMP_EMPTY,
                ]),
                40
            ),
            2
        );

        assert_eq!(
            clear_keys(CLIAgent::Crush, 2),
            Some(vec![b"\x15\x7f\x15\x7f".to_vec()])
        );
        assert_eq!(clear_keys(CLIAgent::Amp, 0), None);
    }

    #[test]
    fn copilots_input_is_ruled_off_over_its_status_rows() {
        let rows = screen(&[
            " ● MCP Servers reloaded: 0 servers connected",
            "",
            "",
            " /private/tmp/t7dev/agents/smoke",
            RULE,
            "❯",
            RULE,
            " ← open sidebar · Interactive · Manual Approval",
            " claude-sonnet-4-6",
        ]);
        assert_eq!(
            input_area(CLIAgent::Copilot, &rows, 40),
            Some(InputArea { top: 4, mode: None })
        );
        // Several lines, with the status rows wrapped in a narrow pane.
        let rows = screen(&[
            " /private/tmp/t7dev/agents/smoke",
            RULE,
            "❯ line one",
            "  line two",
            "  line three",
            RULE,
            " ← open    · Interactive · Manual ·/ commands",
            " sidebar     Approval              tab next",
            " claude-sonnet-4-6",
        ]);
        assert_eq!(
            input_area(CLIAgent::Copilot, &rows, 40),
            Some(InputArea { top: 1, mode: None })
        );
        assert_eq!(held_lines(CLIAgent::Copilot, &rows, 40), 3);
        assert_eq!(
            clear_keys(CLIAgent::Copilot, 3),
            Some(vec![b"\x15\x7f\x15\x7f\x15\x7f".to_vec()])
        );
    }

    const COPILOT_SHADE_TOP: &str = "╻▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄▄";
    const COPILOT_SHADE_BOTTOM: &str = "╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀";

    /// With true colour Copilot shades its input instead of ruling it off.
    #[test]
    fn copilots_shaded_input_is_the_block_over_its_status_rows() {
        let at = |inside: &[&str]| {
            let mut rows = vec![
                String::new(),
                " /private/tmp/t7b7/proj".to_string(),
                COPILOT_SHADE_TOP.to_string(),
            ];
            rows.extend(inside.iter().map(|l| l.to_string()));
            rows.push(COPILOT_SHADE_BOTTOM.to_string());
            rows.push(" ← open sidebar · Interactive · Manual Approval".to_string());
            rows.push(" claude-sonnet-4-6".to_string());
            rows
        };
        for (inside, held) in [(&["┃"][..], 0), (&["┃ line one", "┃ line two"][..], 2)] {
            let rows = at(inside);
            assert_eq!(
                input_area(CLIAgent::Copilot, &rows, 40),
                Some(InputArea { top: 2, mode: None })
            );
            assert_eq!(held_lines(CLIAgent::Copilot, &rows, 40), held);
        }
        // Its `/` list over the block, drawn shaded too, with blank rows
        // under a short one; a command sent before is no list.
        let rows = screen(&[
            "  ❯ /move                Move your uncommitted changes",
            "    /memory              Show memory status",
            "",
            "",
            COPILOT_SHADE_TOP,
            "┃ /move",
            COPILOT_SHADE_BOTTOM,
            " Interactive · Manual Approval · /help show help",
        ]);
        assert_eq!(input_area(CLIAgent::Copilot, &rows, 40), None);
        let rows = screen(&[
            "  ❯ /model                                    13:31",
            "",
            " ● Model set to claude-sonnet-4-6",
            "",
            COPILOT_SHADE_TOP,
            "┃",
            COPILOT_SHADE_BOTTOM,
            " Interactive · Manual Approval · / commands",
        ]);
        assert_eq!(
            input_area(CLIAgent::Copilot, &rows, 40),
            Some(InputArea { top: 4, mode: None })
        );
        // On a bar right over the block.
        let rows = screen(&[
            "┃ ❯ /add-dir           Allow file access to a directory",
            "┃   /agent             Browse and select agents",
            COPILOT_SHADE_TOP,
            "┃ /add-dir",
            COPILOT_SHADE_BOTTOM,
            " Interactive · Manual Approval · /help show help",
        ]);
        assert_eq!(input_area(CLIAgent::Copilot, &rows, 40), None);
    }

    /// Copilot's lists open on a bar right over its input; its approvals and
    /// pickers take the input's place, framed or between rules of their own.
    #[test]
    fn copilots_lists_and_dialogs_are_not_its_input() {
        let slash = screen(&[
            "┃ ❯ /add-dir           Allow file access to a directory",
            "┃   /agent             Browse and select agents",
            RULE,
            "❯ /add-dir",
            RULE,
            " Interactive · Manual Approval · /help show help",
        ]);
        let mention = screen(&[
            "┃   @cop.txt",
            "┃ ❯ @/private/tmp/t7dev/agents/smoke/",
            RULE,
            "❯ @",
            RULE,
            " Interactive · Manual Approval · @ files · # issues",
        ]);
        let approval = screen(&[
            " $ Shell Create cop.txt file",
            "   touch /tmp/t7dev/agents/smoke/cop.txt",
            "",
            "╭──────────────────────────────────────╮",
            "│ Create cop.txt file                  │",
            "│ ──────────────────────────────────── │",
            "│ Do you want to run this command?     │",
            "│                                      │",
            "│ ❯ 1. Yes                             │",
            "│   2. Yes, and don't ask again        │",
            "│   3. No, and tell Copilot (Esc)      │",
            "│                                      │",
            "│ ↑/↓ to navigate · enter to select    │",
            "╰──────────────────────────────────────╯",
        ]);
        let picker = screen(&[
            RULE,
            "",
            " Subagent Configuration",
            "",
            " ❯ explore   built-in   gpt-5.6-luna",
            "   task      built-in   gpt-5.6-luna",
            "",
            " ↑/↓ to navigate · enter to select",
            "",
            RULE,
        ]);
        for rows in [slash, mention, approval, picker] {
            assert_eq!(input_area(CLIAgent::Copilot, &rows, 40), None, "{rows:#?}");
        }
    }

    const QWEN_FOOTER: [&str; 2] = [
        "  ➜ smoke · claude-sonnet-4-6",
        "  Auto mode (shift + tab to cycle)",
    ];

    #[test]
    fn qwens_input_is_ruled_off_over_its_status_rows() {
        let at = |lines: &[&str]| {
            let mut rows = vec![
                "  Tips: You can run any shell commands from Qwen Code.".to_string(),
                String::new(),
                RULE.to_string(),
            ];
            rows.extend(lines.iter().map(|l| l.to_string()));
            rows.push(RULE.to_string());
            rows.extend(QWEN_FOOTER.map(String::from));
            // Drawn after the transcript, with the rest of the screen empty.
            rows.extend(std::iter::repeat_n(String::new(), 14));
            rows
        };
        for (lines, held) in [
            (&[">   Type your message or @path/to/file"][..], 0),
            (&["*   Type your message or @path/to/file"][..], 0),
            (&["!   Type your message or @path/to/file"][..], 0),
            (&["> line one", "  line two \u{200b}"][..], 2),
            (&["> / \u{200b}"][..], 1),
        ] {
            let rows = at(lines);
            assert_eq!(
                input_area(CLIAgent::Qwen, &rows, 40),
                Some(InputArea { top: 2, mode: None }),
                "{lines:?}"
            );
            assert_eq!(held_lines(CLIAgent::Qwen, &rows, 40), held, "{lines:?}");
        }
    }

    /// Qwen Code stays in its shell mode after a `!` command, its prompt a
    /// `!`; the next message leaves it first, and a `!` one stays in it.
    #[test]
    fn qwens_shell_mode_is_read_off_its_prompt() {
        let at = |line: &str| {
            screen(&[
                RULE,
                line,
                RULE,
                QWEN_FOOTER[0],
                "  shell mode enabled (esc to disable)",
            ])
        };
        assert!(in_shell_mode(CLIAgent::Qwen, &at("! commit this"), 40));
        assert!(!in_shell_mode(
            CLIAgent::Qwen,
            &at(">   Type your message or @path/to/file"),
            40
        ));
        assert!(!in_shell_mode(CLIAgent::Gemini, &at("! commit this"), 40));
        // CodeBuddy's bash mode, the same way.
        let bash = screen(&[RULE, "!", RULE, "! for bash mode  ·  shift+Tab for local"]);
        assert!(in_shell_mode(CLIAgent::CodeBuddy, &bash, 40));
        assert_eq!(
            input_area(CLIAgent::CodeBuddy, &bash, 40),
            Some(InputArea { top: 0, mode: None })
        );
    }

    /// Qwen Code's `/` list opens under its input, a tool approval and its
    /// dialogs take the input's place.
    #[test]
    fn qwens_lists_and_dialogs_are_not_its_input() {
        let slash = screen(&[
            RULE,
            "> / \u{200b}",
            RULE,
            "  > model [--fast|--voice]   Switch the model for this session",
            "    cd <path>                Move this session to a new directory",
            "    btw                      Ask a quick side question",
            "    bug <description>        submit a bug report",
            "    ide                      manage IDE integration",
            "    vim                      toggle vim mode on/off",
            "  ▼",
            "  (1/86)",
        ]);
        let approval = screen(&[
            RULE,
            "  > Use the shell to run: touch /tmp/t7dev/agents/smoke/qwen.txt",
            "",
            "  ? Shell touch /tmp/t7dev/agents/smoke/qwen.txt",
            "",
            "   Allow execution of: 'touch'?",
            "",
            "   › 1. Yes, allow once",
            "     2. Always allow run 'touch *' commands in this project",
            "     4. No, suggest changes (esc)",
            "",
            "  ⠏ Waiting for user confirmation...",
        ]);
        let model = screen(&[
            "  ╭──────────────────────────────────────╮",
            "  │ Select Model                         │",
            "  │ › 1. [openai] claude-sonnet-4-6      │",
            "  │ ──────────────────────────────────── │",
            "  │ Enter to select, ↑↓ to navigate      │",
            "  ╰──────────────────────────────────────╯",
        ]);
        for rows in [slash, approval, model] {
            assert_eq!(input_area(CLIAgent::Qwen, &rows, 40), None, "{rows:#?}");
        }
    }

    #[test]
    fn codebuddys_input_is_ruled_off_over_its_key_help() {
        let rows = screen(&[
            "│      ████            ████      │ claude-sonnet-4-6      │",
            "╰────────────────────────────────────────────────────────╯",
            "",
            RULE,
            "> line one",
            "  line two",
            RULE,
            "  ← for agents",
            "",
            "",
            "",
        ]);
        assert_eq!(
            input_area(CLIAgent::CodeBuddy, &rows, 40),
            Some(InputArea { top: 3, mode: None })
        );
        assert_eq!(held_lines(CLIAgent::CodeBuddy, &rows, 40), 2);
        // An empty input, or one offering a suggestion to send as it is.
        for line in [">", "> verify file exists with ls command    ↵ send"] {
            let rows = screen(&[
                "✔ Worked for 22s",
                RULE,
                line,
                RULE,
                "? for shortcuts  ← for agents",
            ]);
            assert_eq!(
                input_area(CLIAgent::CodeBuddy, &rows, 40),
                Some(InputArea { top: 1, mode: None })
            );
            assert_eq!(held_lines(CLIAgent::CodeBuddy, &rows, 40), 0);
        }
        // Keys that arrive together it takes for a paste: one at a time.
        assert_eq!(
            clear_keys(CLIAgent::CodeBuddy, 1),
            Some(vec![b"\x15".to_vec(), b"\x7f".to_vec()])
        );
        assert!(clear_input(CLIAgent::CodeBuddy, 1).unwrap()[1].delay >= KEY_GAP);
    }

    /// CodeBuddy's `/` list opens under its input; a tool approval takes
    /// the input's place between rules of its own.
    #[test]
    fn codebuddys_lists_and_approvals_are_not_its_input() {
        let slash = screen(&[
            RULE,
            "> /",
            RULE,
            "  /commit              Create a git commit",
            "  /commit-push-pr      Commit, push, and open a PR",
            "  /loop                Run a prompt on a recurring interval",
            "  /security-review     Complete a security review",
            "  /add-dir             Add a new working directory",
            "  /agent-mode          Switch the main agent mode",
            "",
        ]);
        let approval = screen(&[
            "● Bash(touch /tmp/t7dev/agents/smoke/cb.txt)",
            RULE,
            " Bash command",
            "",
            "   touch /tmp/t7dev/agents/smoke/cb.txt",
            "   Create cb.txt file",
            "",
            RULE,
            "",
            " Do you want to proceed?",
            "",
            " > 1. Yes",
            "   2. Yes, and don't ask again for session (shift + tab)",
            "   3. No, and tell CodeBuddy what to do differently (escape)",
        ]);
        for rows in [slash, approval] {
            assert_eq!(
                input_area(CLIAgent::CodeBuddy, &rows, 40),
                None,
                "{rows:#?}"
            );
        }
    }

    const KIMI_FOOTER: [&str; 2] = [
        " claude-sonnet-4-6 thinking: high  …/t7dev/agents/smoke",
        "                                   context: 0% (0/195k)",
    ];

    #[test]
    fn kimis_input_is_the_frame_over_its_model_line() {
        let at = |inside: &[&str]| {
            let mut rows = vec![
                " ╭──────────────────────────────────────╮".to_string(),
                " │  ▐█▛█▛█▌  Welcome to Kimi Code!       │".to_string(),
                " ╰──────────────────────────────────────╯".to_string(),
                String::new(),
                "   No session yet — one will be created.".to_string(),
                String::new(),
                " ╭──────────────────────────────────────╮".to_string(),
            ];
            rows.extend(inside.iter().map(|l| l.to_string()));
            rows.push(" ╰──────────────────────────────────────╯".to_string());
            rows.extend(KIMI_FOOTER.map(String::from));
            rows.extend(std::iter::repeat_n(String::new(), 5));
            rows
        };
        for (inside, held) in [
            (&[" │ >                                    │"][..], 0),
            (
                &[
                    " │ > line one                           │",
                    " │   line two                           │",
                    " │   line three                         │",
                ][..],
                3,
            ),
        ] {
            let rows = at(inside);
            assert_eq!(
                input_area(CLIAgent::Kimi, &rows, 40),
                Some(InputArea { top: 6, mode: None })
            );
            assert_eq!(held_lines(CLIAgent::Kimi, &rows, 40), held);
        }
    }

    /// Kimi Code's lists open under its input's frame; a tool approval takes
    /// the frame's place, with only its welcome banner framed far above.
    #[test]
    fn kimis_lists_and_approvals_are_not_its_input() {
        let slash = screen(&[
            " ╭──────────────────────────────────────╮",
            " │ > /                                  │",
            " ╰──────────────────────────────────────╯",
            " │   → yolo           Ask When Needed    │",
            " │     model          Switch LLM model   │",
            " │     permission     Select permission  │",
            " │     plan           Toggle plan mode   │",
            " │     (1/51)                            │",
            KIMI_FOOTER[0],
            KIMI_FOOTER[1],
        ]);
        let approval = screen(&[
            " ╰──────────────────────────────────────╯",
            "",
            " ✨ Use the shell to run: touch kimi.txt",
            "",
            " ● Running a command · $ touch kimi.txt",
            " ──────────────────────────────────────",
            "   ▶ Run this command?",
            "",
            "   ▶ 1. Approve once",
            "     2. Approve for this session",
            "     3. Reject",
            "",
            "   ↑/↓ select · 1/2/3/4 choose · ↵ confirm",
            " ──────────────────────────────────────",
            KIMI_FOOTER[0],
            KIMI_FOOTER[1],
        ]);
        for rows in [slash, approval] {
            assert_eq!(input_area(CLIAgent::Kimi, &rows, 40), None, "{rows:#?}");
        }
    }

    const OPENCODE_EDGE: &str = "  ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀";

    #[test]
    fn opencodes_input_is_the_bar_over_its_key_hints() {
        // Its home screen, the input in the middle over the directory.
        let rows = screen(&[
            "        █▀▀█ █▀▀█ █▀▀█ █▀▀▄ █▀▀▀ █▀▀█",
            "",
            "    ┃",
            "    ┃  Ask anything... \"Fix broken tests\"",
            "    ┃",
            "    ┃  Build · claude-sonnet-4-6 Relay",
            "    ╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀",
            "    tab agents  ctrl+p commands",
            "",
            "",
            "  /private/tmp/t7dev/agents/smoke            1.18.4",
        ]);
        assert_eq!(
            input_area(CLIAgent::OpenCode, &rows, 40),
            Some(InputArea { top: 2, mode: None })
        );
        assert_eq!(held_lines(CLIAgent::OpenCode, &rows, 40), 0);
        // A session, a turn running, two lines typed.
        let rows = screen(&[
            "  ┃  Use the shell to run: touch oc.txt",
            "  ┃",
            "",
            "     ▣  Build · claude-sonnet-4-6",
            "",
            "  ┃",
            "  ┃  line one",
            "  ┃  line two",
            "  ┃",
            "  ┃  Build · claude-sonnet-4-6 Relay",
            OPENCODE_EDGE,
            "   ⬝⬝⬝⬝⬝⬝⬝⬝  esc interrupt         ctrl+p commands",
            "",
        ]);
        assert_eq!(
            input_area(CLIAgent::OpenCode, &rows, 40),
            Some(InputArea { top: 5, mode: None })
        );
        assert_eq!(held_lines(CLIAgent::OpenCode, &rows, 40), 3);
    }

    /// OpenCode's lists open on its input's bar, closed by a bar at the
    /// right; a permission request takes the input's place, with no edge.
    #[test]
    fn opencodes_lists_and_requests_are_not_its_input() {
        let slash = screen(&[
            "  ┃ /agents     Switch agent                ┃",
            "  ┃ /connect    Connect provider            ┃",
            "  ┃",
            "  ┃  /",
            "  ┃",
            "  ┃  Build · claude-sonnet-4-6 Relay",
            OPENCODE_EDGE,
            "   tab agents  ctrl+p commands",
        ]);
        let request = screen(&[
            "     $ touch /tmp/t7dev/agents/smoke/oc.txt",
            "",
            "  ┃",
            "  ┃  △ Permission required",
            "  ┃    ← Access external directory",
            "  ┃",
            "  ┃   Allow once   Allow always   Reject",
            "  ┃",
            "",
        ]);
        let sent = screen(&["  ┃", "  ┃  Reply with just PONG", "  ┃", "", "     PONG"]);
        for rows in [slash, request, sent] {
            assert_eq!(input_area(CLIAgent::OpenCode, &rows, 40), None, "{rows:#?}");
        }
    }

    const QODER_HINT: &str = "                          ? for shortcuts";
    const QODER_MODEL: &str = "  Model · ctx ░░░░░░░░░░ 0% · /private/tmp/t7b8/proj";

    /// Qoder's screen from its banner down, `over` between the transcript and
    /// the mode line's rule, `lines` between the input's rules.
    fn qoder_screen(over: &[&str], mode: &[&str], lines: &[&str]) -> Vec<String> {
        let mut rows = screen(&[
            "   ████  ██  Not Login Please Auth   │ 4. Be specific for the best results │",
            "                                     ╰─────────────────────────────────────╯",
            "",
            " > hello there",
            "",
            " x Qoder authentication is not ready. Please sign in again (e.g. via /login)",
            "",
        ]);
        rows.extend(over.iter().map(|l| l.to_string()));
        rows.push(RULE.to_string());
        rows.extend(mode.iter().map(|l| l.to_string()));
        rows.push(RULE.to_string());
        rows.extend(lines.iter().map(|l| l.to_string()));
        rows.push(RULE.to_string());
        rows.push(QODER_MODEL.to_string());
        rows
    }

    #[test]
    fn qoders_input_is_ruled_off_under_its_mode_line() {
        let mode = [" Shift+Tab to Accept Edits          1 MCP server · 20 skills"];
        // Idle, with the hint over the mode line, the box covers from there.
        for (lines, held) in [
            (&[" >   Type your message or @path/to/file"][..], 0),
            (&[" *   Type your message or @path/to/file"][..], 0),
            (&[" !   Type your shell command"][..], 0),
        ] {
            let rows = qoder_screen(&[QODER_HINT], &mode, lines);
            for agent in [CLIAgent::QoderCLI, CLIAgent::QoderCLICn] {
                assert_eq!(
                    input_area(agent, &rows, 40),
                    Some(InputArea { top: 7, mode: None }),
                    "{lines:?}"
                );
                assert_eq!(held_lines(agent, &rows, 40), held, "{lines:?}");
            }
        }
        // Typing, the hint goes; a newline typed last leaves a blank line.
        for (lines, held) in [
            (&[" > hello there"][..], 1),
            (&[" > hello there", "   second line"][..], 2),
            (&[" > line one", "   line two", ""][..], 2),
            (&[" ! echo pasted"][..], 1),
        ] {
            let rows = qoder_screen(&[""], &mode, lines);
            assert_eq!(
                input_area(CLIAgent::QoderCLI, &rows, 40),
                Some(InputArea { top: 8, mode: None }),
                "{lines:?}"
            );
            assert_eq!(held_lines(CLIAgent::QoderCLI, &rows, 40), held, "{lines:?}");
        }
        // In a narrow pane the mode line breaks in two around a blank row.
        let narrow = [
            " Shift+Tab to Accept Edits",
            "",
            "  1 MCP server · 20 skills",
        ];
        let rows = qoder_screen(&[QODER_HINT], &narrow, &[" > x"]);
        assert_eq!(
            input_area(CLIAgent::QoderCLI, &rows, 40),
            Some(InputArea { top: 7, mode: None })
        );
    }

    /// Qoder stays in its shell mode after a `!` command, its prompt a `!`.
    #[test]
    fn qoders_shell_mode_is_read_off_its_prompt() {
        let shell = [" Shell mode enabled (esc to disable)  1 MCP server · 20 skills"];
        let rows = qoder_screen(&[QODER_HINT], &shell, &[" !   Type your shell command"]);
        assert!(in_shell_mode(CLIAgent::QoderCLI, &rows, 40));
        assert!(in_shell_mode(CLIAgent::QoderCLICn, &rows, 40));
        let mode = [" Shift+Tab to Accept Edits          1 MCP server · 20 skills"];
        let rows = qoder_screen(&[QODER_HINT], &mode, &[" > ! not a command"]);
        assert!(!in_shell_mode(CLIAgent::QoderCLI, &rows, 40));
    }

    /// Qoder's lists open under its input, its dialogs in its place.
    #[test]
    fn qoders_lists_and_dialogs_are_not_its_input() {
        let mode = [" Shift+Tab to Accept Edits          1 MCP server · 20 skills"];
        let mut slash = qoder_screen(&[""], &mode, &[" > /"]);
        slash.pop();
        slash.extend(screen(&[
            " ❯ about                      Show version info",
            "   add-dir                    Add a directory to the workspace context",
            "   agents                     Manage agents",
            "   batch                      Apply a batch change across the codebase",
            "   branch                     Create a new session branch",
            "   btw                        Ask a quick side question",
            "   claim                      Check available campaigns",
            " ▼ clear                      Clear the screen and start a new conversation",
        ]));
        let mut mention = qoder_screen(&[""], &mode, &[" > read @my"]);
        mention.pop();
        mention.push(" ❯ my notes.md".to_string());
        let trust = screen(&[
            " Do you trust the files in this folder?",
            RULE,
            " Please confirm this is your own project or from a trusted source.",
            "",
            " /private/tmp/t7b8/proj",
            "",
            "  ❯ 1. Trust folder",
            "    2. Don't trust and exit",
            "",
            " ↑/↓ navigate · Enter select · Esc back",
        ]);
        let sign_in = screen(&[
            " Welcome to Qoder CLI",
            RULE,
            " Sign in to get started, or exit the application.",
            "",
            "  ❯ 1. Sign in to continue",
            "    2. Exit the application",
            "",
            " ↑/↓ navigate · Enter select · Esc back",
        ]);
        let model = screen(&[
            " > /model",
            "",
            " Model ·  Default (0)   New (0)   Custom (0)",
            RULE,
            " Overview:",
            "  · Current Model     : Auto",
            "",
            " ──────────────────────────────────────",
            "  Press / to search models",
            " ──────────────────────────────────────",
            "  No models in this group.",
            "",
            "",
            " Tab switch · r refresh · Esc back",
        ]);
        let sent = screen(&[
            " > hello there",
            "   second line",
            "",
            " x Qoder authentication is not ready.",
        ]);
        for rows in [slash, mention, trust, sign_in, model, sent] {
            for agent in [CLIAgent::QoderCLI, CLIAgent::QoderCLICn] {
                assert_eq!(input_area(agent, &rows, 40), None, "{rows:#?}");
            }
        }
    }

    /// The grid gives up rows only for the part of the box an input does not
    /// already hold, and whole rows of it.
    #[test]
    fn the_grid_makes_room_only_for_what_the_input_does_not_hold() {
        let (line, below) = (px(21.), px(9.));
        // Claude Code's five rows hold the box.
        assert_eq!(rows_short(px(101.), line, 5, below), 0);
        // Oh My Pi's two do not: 101 - 42 - 9 = 50, three rows' worth.
        assert_eq!(rows_short(px(101.), line, 2, below), 3);
        assert_eq!(rows_short(px(101.), line, 4, below), 1);
        assert_eq!(rows_short(px(101.), px(0.), 2, below), 0);
    }

    #[test]
    fn model_ids_read_the_way_people_say_them() {
        assert_eq!(model_label("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_label("claude-sonnet-5-5[1m]"), "Sonnet 5.5");
        assert_eq!(model_label("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(model_label("gpt-5-codex"), "gpt-5-codex");
    }

    #[test]
    fn a_reported_model_is_checked_under_its_alias() {
        assert_eq!(model_alias("claude-opus-5-5"), Some("opus"));
        assert_eq!(model_alias("claude-sonnet-5-5[1m]"), Some("sonnet[1m]"));
        assert_eq!(model_alias("claude-haiku-4-5-20251001"), Some("haiku"));
        assert_eq!(model_alias("claude-fable-5-1"), Some("fable"));
        assert_eq!(model_alias("gpt-5-codex"), None);
        assert_eq!(alias_label("sonnet[1m]"), "Sonnet · 1M");
        assert_eq!(alias_label("haiku"), "Haiku");
        assert_eq!(effort_label("medium"), "Medium");
        assert_eq!(effort_label("xhigh"), "Extra high");
    }

    #[test]
    fn each_agents_builtin_commands_are_distinct_slash_words() {
        for agent in [
            CLIAgent::Claude,
            CLIAgent::Codex,
            CLIAgent::Gemini,
            CLIAgent::OpenCode,
            CLIAgent::Copilot,
            CLIAgent::Qwen,
            CLIAgent::CodeBuddy,
            CLIAgent::Kimi,
            CLIAgent::Goose,
            CLIAgent::QoderCLI,
        ] {
            let names: Vec<&str> = builtin_commands(agent).iter().map(|(n, _)| *n).collect();
            for (i, name) in names.iter().enumerate() {
                assert!(name.starts_with('/') && !name.contains(' '), "{name}");
                assert!(!names[..i].contains(name), "{agent:?} lists {name} twice");
            }
        }
        // The toolbar's effort picker types it; the menu should offer it too.
        assert!(
            builtin_commands(CLIAgent::Claude)
                .iter()
                .any(|(n, _)| *n == "/effort")
        );
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
        let items = file_items(&index, "main", Some(Path::new("/repo/app")), None, None);
        assert_eq!(items[0].insert, "@src/main.rs");
        let items = file_items(&index, "readme", Some(Path::new("/repo/app")), None, None);
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

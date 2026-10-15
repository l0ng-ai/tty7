//! A waiting agent's question, read off its pane, for the phone to answer
//! from the tree.
//!
//! An agent's hook says that it waits, and often what for ("Claude needs your
//! permission to use Bash"), but not the answers it will take: those are only
//! on its screen, as a menu. So the gateway reads the pane's screen the way a
//! pane stream does (`mirror`) and picks the menu out of it, the same way the
//! app's in-pane quick answers do (`readChoices` in mobile/src/main.ts, which
//! reads the phone's own copy of the screen and has to keep doing so for
//! older desktops): keep the two in step.
//!
//! An answer is never typed on the strength of what the tree said. The phone
//! names the [`AgentPrompt`] it saw and the option; the gateway reads the
//! screen again and types only if the same question is still there, working
//! out the keys from where the menu's cursor is now.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};
use tty7_core::core::cli_agent::{AgentSessionState, AgentStatus as CoreStatus};
use tty7_core::daemon::control::PaneAgentState;
use tty7_core::daemon::protocol::DaemonMsg;
use tty7_mobile_proto::{AgentPrompt, AgentStatus, PaneView, PromptOption, WorkspaceView};

use crate::mirror::Mirror;
use crate::serve::PaneFeed;

/// One of the answers on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    /// What typing it takes: its digit, or arrows to it and Enter.
    pub keys: Vec<u8>,
    /// The screen row it is on.
    row: usize,
}

/// The answers a menu on screen offers, top to bottom; none when there is no
/// menu this recognises. Two shapes:
///
/// - a menu picked with the arrows and no numbers (a "trust this folder?",
///   a picker), told by the "Enter to confirm/select" hint under it: its
///   options are the lines above the hint at the cursor line's indent;
/// - otherwise the last run of lines numbered from 1, as Claude Code and
///   Codex draw them ("❯ 1. Yes", "2. No, and tell Claude…"), box edges and
///   the cursor mark stripped. Typing the digit picks one.
pub fn choices(rows: &[String]) -> Vec<Choice> {
    let arrows = arrow_menu(rows);
    if !arrows.is_empty() {
        return arrows;
    }
    numbered(rows)
}

fn arrow_menu(rows: &[String]) -> Vec<Choice> {
    let Some(hint) = rows.iter().rposition(|row| {
        let row = row.to_lowercase();
        row.contains("enter to confirm") || row.contains("enter to select")
    }) else {
        return Vec::new();
    };
    let mut end = hint;
    while end > 0 && rows[end - 1].trim().is_empty() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && !rows[start - 1].trim().is_empty() {
        start -= 1;
    }
    let block: Vec<Vec<char>> = rows[start..end]
        .iter()
        .map(|r| r.chars().collect())
        .collect();
    let cursor = |line: &[char]| {
        let at = line.iter().position(|c| !c.is_whitespace())?;
        (line[at] == '❯' && line.get(at + 1).is_some_and(|c| c.is_whitespace())).then_some(at)
    };
    let Some((sel, col)) = block
        .iter()
        .enumerate()
        .find_map(|(i, line)| cursor(line).map(|col| (i, col)))
    else {
        return Vec::new();
    };
    // A numbered menu with the cursor on it is the numbered kind.
    let after: String = block[sel][col + 1..].iter().collect();
    if numbered_line(after.trim_start()).is_some() {
        return Vec::new();
    }
    let options: Vec<usize> = (0..block.len())
        .filter(|&i| {
            let line = &block[i];
            i == sel
                || (line.len() > col + 2
                    && line[..col + 2].iter().all(|c| c.is_whitespace())
                    && line[col + 2] != ' ')
        })
        .collect();
    if !(2..=9).contains(&options.len()) {
        return Vec::new();
    }
    let at = options.iter().position(|&i| i == sel).unwrap_or(0);
    options
        .iter()
        .enumerate()
        .map(|(k, &i)| {
            let line: String = block[i].iter().collect();
            Choice {
                label: line
                    .trim_start()
                    .trim_start_matches('❯')
                    .trim_start()
                    .to_string(),
                keys: walk(at, k),
                row: start + i,
            }
        })
        .collect()
}

/// The keys that move a menu's cursor from option `at` to option `to` and
/// pick it.
fn walk(at: usize, to: usize) -> Vec<u8> {
    let step = if to > at { "\x1b[B" } else { "\x1b[A" };
    format!("{}\r", step.repeat(to.abs_diff(at))).into_bytes()
}

fn numbered(rows: &[String]) -> Vec<Choice> {
    let mut run: Vec<Choice> = Vec::new();
    // Which of the run the cursor mark is on, if any.
    let mut cursor = None;
    let mut found = (Vec::new(), None);
    for (y, row) in rows.iter().enumerate() {
        let text = strip_edges(row);
        let (marked, text) = match text.chars().next() {
            Some(c @ ('❯' | '›' | '>' | '▸' | '●' | '◉' | '○')) => (
                matches!(c, '❯' | '›' | '>' | '▸'),
                text[c.len_utf8()..].trim_start(),
            ),
            _ => (false, text),
        };
        let Some((digits, label)) = numbered_line(text) else {
            continue;
        };
        if digits == "1" {
            run.clear();
            cursor = None;
        }
        if digits.parse::<usize>().ok() == Some(run.len() + 1) {
            if marked {
                cursor = Some(run.len());
            }
            run.push(Choice {
                label: label.to_string(),
                keys: digits.as_bytes().to_vec(),
                row: y,
            });
        }
        if run.len() >= 2 {
            found = (run.clone(), cursor);
        }
    }
    let (mut found, cursor) = found;
    // Past 9 a number is two keys, and the menu takes the first, "1", as
    // its first option. Those are walked to with the arrows from the
    // cursor, or left off when the cursor cannot be seen.
    if found.len() > 9 {
        match cursor {
            Some(at) => {
                for (k, choice) in found.iter_mut().enumerate().skip(9) {
                    choice.keys = walk(at, k);
                }
            }
            None => found.truncate(9),
        }
    }
    found
}

/// `12. label` or `3) label`: the number and the label.
fn numbered_line(text: &str) -> Option<(&str, &str)> {
    let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if !(1..=2).contains(&digits) {
        return None;
    }
    let rest = text[digits..].strip_prefix(['.', ')'])?;
    let label = rest.trim_start();
    if label.len() == rest.len() || label.is_empty() {
        return None;
    }
    Some((&text[..digits], label))
}

/// A row without the box it may be drawn in, or the space around it.
fn strip_edges(row: &str) -> &str {
    row.trim_matches(|c: char| c.is_whitespace() || matches!(c, '│' | '┃' | '|'))
}

/// The line over the options that asks the question: the nearest one with
/// words in it, past blank lines and rules.
fn question_above(rows: &[String], first: usize) -> Option<String> {
    rows[..first]
        .iter()
        .rev()
        .map(|row| strip_edges(row))
        .find(|text| text.chars().any(char::is_alphanumeric))
        .map(str::to_string)
}

/// The question a waiting agent's screen asks, with the keys that give each
/// of its answers, in order. `None` unless the agent waits and a menu is on
/// screen.
pub fn read(
    pane_id: u64,
    state: &AgentSessionState,
    rows: &[String],
) -> Option<(AgentPrompt, Vec<Vec<u8>>)> {
    if state.status != CoreStatus::Waiting {
        return None;
    }
    let choices = choices(rows);
    let first = choices.first()?.row;
    let above = question_above(rows, first);
    let hook = state.message.clone().filter(|m| !m.trim().is_empty());
    // The hook says what kind of thing is asked ("Claude needs your
    // permission to use Bash"), the screen what about: both are shown, and
    // both name it.
    let detail = hook
        .as_ref()
        .and(above.clone())
        .filter(|a| hook.as_ref() != Some(a));
    let question = hook.or_else(|| above.clone());
    // Where in the agent's session this question came: the turns it has
    // finished and the tools it has run. A question worded the same later
    // on is a different one, and an answer meant for this one must not
    // reach it. Not the cursor's place: moving it on the desktop leaves the
    // question as it was, and the keys are worked out afresh anyway.
    let mut hash = Sha256::new()
        .chain_update(b"tty7 agent prompt\0")
        .chain_update(pane_id.to_le_bytes())
        .chain_update(state.turns.to_le_bytes())
        .chain_update(state.activity.to_le_bytes())
        .chain_update(state.session_id.as_deref().unwrap_or("").as_bytes())
        .chain_update(b"\0")
        .chain_update(question.as_deref().unwrap_or("").as_bytes())
        .chain_update(b"\0")
        .chain_update(above.as_deref().unwrap_or("").as_bytes());
    for choice in &choices {
        hash = hash
            .chain_update(b"\0")
            .chain_update(choice.label.as_bytes());
    }
    let id: String = hash.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let prompt = AgentPrompt {
        id,
        question,
        detail,
        options: choices
            .iter()
            .map(|c| PromptOption {
                label: c.label.clone(),
            })
            .collect(),
    };
    Some((prompt, choices.into_iter().map(|c| c.keys).collect()))
}

/// How long a pane's replay may take to read, and how long a quiet feed is
/// taken to mean it is over.
const SCREEN_LIMIT: Duration = Duration::from_millis(1500);
const SCREEN_QUIET: Duration = Duration::from_millis(120);

/// A pane's screen as text, read through an observer the way a pane stream
/// starts: its size, then its replay, played through the desktop's emulator.
pub fn read_screen(mut feed: Box<dyn PaneFeed>) -> io::Result<Vec<String>> {
    let until = Instant::now() + SCREEN_LIMIT;
    let mut mirror: Option<Mirror> = None;
    let mut size = (80, 24);
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match feed.recv(SCREEN_QUIET.min(left))? {
            Some(DaemonMsg::Size(new)) => {
                size = (new.cols, new.rows);
                match &mut mirror {
                    Some(mirror) => mirror.resize(new.cols, new.rows),
                    None => mirror = Some(Mirror::new(new.cols, new.rows)),
                }
            }
            Some(DaemonMsg::Snapshot(bytes) | DaemonMsg::Output(bytes)) => {
                mirror
                    .get_or_insert_with(|| Mirror::new(size.0, size.1))
                    .feed(&bytes);
            }
            Some(DaemonMsg::Exited { .. }) => break,
            Some(_) => {}
            // Gone quiet after the replay: it is all in.
            None if mirror.is_some() => break,
            None => {}
        }
    }
    mirror
        .map(|m| m.screen())
        .ok_or_else(|| io::Error::other("the pane sent nothing to read"))
}

/// After a read finds no menu, when to look again: the hook can come a
/// moment before the menu is drawn. After these, every [`REREAD`].
const RETRIES: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(2)];

/// How often a waiting agent's screen is read again while it waits on the
/// same thing. A menu read before it was all drawn (a numbered list in the
/// reply over it taken for it), redrawn at another width, or answered on
/// the desktop before the hook says so, is put right within this.
const REREAD: Duration = Duration::from_secs(5);

/// A pane, by the machine it is on (`None` for this one) and its id.
type PaneKey = (Option<String>, u64);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The answers the phones give, shared by every stream: one at a time in a
/// pane, never the same prompt answered twice (two taps, or two phones),
/// and a refused one has the trees read that pane again, as the prompt they
/// showed is not the one on screen.
#[derive(Default)]
pub struct Answers {
    /// Each pane's lock, held while an answer is checked and typed, over
    /// the prompt last answered there.
    panes: Mutex<HashMap<PaneKey, Arc<Mutex<Option<String>>>>>,
    /// When an answer in a pane was last refused.
    refused: Mutex<HashMap<PaneKey, Instant>>,
}

impl Answers {
    /// Answers prompt `id` in the pane with `typed`, which checks it is
    /// still on screen and types the keys, unless it was answered already.
    /// Answers to the same pane wait for each other.
    pub fn give(
        &self,
        machine: Option<&str>,
        pane_id: u64,
        id: &str,
        typed: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        let key = (machine.map(str::to_string), pane_id);
        let pane = lock(&self.panes).entry(key.clone()).or_default().clone();
        let mut last = lock(&pane);
        let given = if last.as_deref() == Some(id) {
            Err(io::Error::other("that question was already answered"))
        } else {
            typed()
        };
        match &given {
            Ok(()) => *last = Some(id.to_string()),
            Err(_) => {
                lock(&self.refused).insert(key, Instant::now());
            }
        }
        given
    }

    /// Whether an answer in the pane was refused after `since`.
    fn refused_since(&self, key: &PaneKey, since: Instant) -> bool {
        lock(&self.refused).get(key).is_some_and(|at| *at > since)
    }
}

/// The prompts the tree shows, kept between its reads: a pane's screen is
/// read when its agent starts waiting on something new, then every few
/// seconds while it waits, not every second.
pub struct Prompts {
    known: HashMap<PaneKey, Known>,
    answers: Arc<Answers>,
}

impl Default for Prompts {
    fn default() -> Self {
        Self::new(Arc::default())
    }
}

struct Known {
    /// What the agent was waiting on when this was read.
    episode: (u64, u64, Option<String>, Option<String>),
    prompt: Option<AgentPrompt>,
    misses: usize,
    /// When the screen was last read, and when to read it next.
    read: Option<Instant>,
    next: Instant,
}

impl Known {
    fn new(episode: (u64, u64, Option<String>, Option<String>), now: Instant) -> Self {
        Self {
            episode,
            prompt: None,
            misses: 0,
            read: None,
            next: now,
        }
    }
}

fn panes(workspaces: &mut [WorkspaceView]) -> impl Iterator<Item = &mut PaneView> {
    workspaces
        .iter_mut()
        .flat_map(|ws| ws.tabs.iter_mut())
        .flat_map(|tab| tab.panes.iter_mut())
}

impl Prompts {
    /// Prompts that read a pane again at once when an answer to it is
    /// refused in `answers`.
    pub fn new(answers: Arc<Answers>) -> Self {
        Self {
            known: HashMap::new(),
            answers,
        }
    }

    /// Gives each waiting agent in `workspaces` (on `machine`, `None` for
    /// this one) the prompt on its screen, reading the screens due with
    /// `screen`, side by side: when an agent waits on something new, while
    /// its menu is not drawn yet, every [`REREAD`] after, and at once when
    /// an answer to it was refused.
    pub fn fill(
        &mut self,
        machine: Option<&str>,
        workspaces: &mut [WorkspaceView],
        agents: &[PaneAgentState],
        screen: impl Fn(u64) -> io::Result<Vec<String>> + Sync,
        now: Instant,
    ) {
        let mut waiting = HashSet::new();
        let mut due = Vec::new();
        for pane in panes(workspaces) {
            if pane.stopped {
                continue;
            }
            let Some(view) = pane.agent.as_ref() else {
                continue;
            };
            let live = agents
                .iter()
                .find(|a| a.pane_id == pane.id && a.state.status == CoreStatus::Waiting);
            let (Some(live), AgentStatus::Waiting) = (live, view.status) else {
                continue;
            };
            let state = &live.state;
            let key = (machine.map(str::to_string), pane.id);
            waiting.insert(key.clone());
            let episode = (
                state.turns,
                state.activity,
                state.message.clone(),
                state.session_id.clone(),
            );
            let known = self
                .known
                .entry(key.clone())
                .or_insert_with(|| Known::new(episode.clone(), now));
            if known.episode != episode {
                *known = Known::new(episode, now);
            }
            let refused = known
                .read
                .is_some_and(|at| self.answers.refused_since(&key, at));
            if known.next <= now || refused {
                due.push((key, state));
            }
        }

        // Each read can take a while to go quiet: read them side by side.
        let screen = &screen;
        let found: Vec<Option<AgentPrompt>> = std::thread::scope(|scope| {
            let reads: Vec<_> = due
                .iter()
                .map(|(key, state)| {
                    let pane_id = key.1;
                    scope.spawn(move || {
                        let rows = screen(pane_id)
                            .map_err(|e| log::debug!("mobile gateway: reading pane {pane_id}: {e}"))
                            .ok()?;
                        read(pane_id, state, &rows).map(|(prompt, _)| prompt)
                    })
                })
                .collect();
            reads
                .into_iter()
                .map(|read| read.join().ok().flatten())
                .collect()
        });
        for ((key, _), found) in due.iter().zip(found) {
            let Some(known) = self.known.get_mut(key) else {
                continue;
            };
            known.read = Some(now);
            // No menu now is no prompt: one there before was answered, or
            // was not one.
            known.next = now
                + match found {
                    Some(_) => REREAD,
                    None => RETRIES.get(known.misses).copied().unwrap_or(REREAD),
                };
            known.misses = if found.is_some() { 0 } else { known.misses + 1 };
            known.prompt = found;
        }

        for pane in panes(workspaces) {
            let key = (machine.map(str::to_string), pane.id);
            if let (Some(view), Some(known)) = (pane.agent.as_mut(), self.known.get(&key))
                && waiting.contains(&key)
            {
                view.prompt = known.prompt.clone();
            }
        }
        self.known
            .retain(|key, _| key.0.as_deref() != machine || waiting.contains(key));
    }

    /// Forgets the linked machines not among `remotes`: one whose link went
    /// away has no tree for [`Prompts::fill`] to tidy up after.
    pub fn only_on<'a>(&mut self, remotes: impl Iterator<Item = &'a str>) {
        let remotes: HashSet<&str> = remotes.collect();
        self.known
            .retain(|(machine, _), _| machine.as_deref().is_none_or(|m| remotes.contains(m)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    use tty7_mobile_proto::{AgentView, TabView};

    fn screen(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    fn picked(rows: &[String]) -> Vec<(String, String)> {
        choices(rows)
            .into_iter()
            .map(|c| (c.label, String::from_utf8(c.keys).unwrap()))
            .collect()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(l, k)| (l.to_string(), k.to_string()))
            .collect()
    }

    const RULE: &str = "────────────────────────────────────────────────────────────";

    /// Claude Code asking to run a command, as it draws it today.
    fn claude_bash() -> Vec<String> {
        screen(&[
            "⏺ Bash(cargo test -p tty7-gateway)",
            "  ⎿  Running…",
            "",
            RULE,
            " Bash command",
            "",
            "   cargo test -p tty7-gateway",
            "   Run the gateway's tests",
            "",
            " Do you want to proceed?",
            " ❯ 1. Yes",
            "   2. Yes, and don't ask again for cargo test commands in /src/tty7",
            "   3. No, and tell Claude what to do differently (esc)",
            "",
        ])
    }

    fn waiting(message: Option<&str>) -> AgentSessionState {
        AgentSessionState {
            status: CoreStatus::Waiting,
            message: message.map(str::to_string),
            ..AgentSessionState::default()
        }
    }

    #[test]
    fn claude_codes_permission_prompt_is_answered_by_its_digits() {
        assert_eq!(
            picked(&claude_bash()),
            pairs(&[
                ("Yes", "1"),
                (
                    "Yes, and don't ask again for cargo test commands in /src/tty7",
                    "2"
                ),
                ("No, and tell Claude what to do differently (esc)", "3"),
            ])
        );
    }

    #[test]
    fn claude_codes_boxed_prompt_loses_its_box() {
        let rows = screen(&[
            "╭──────────────────────────────────────────────────╮",
            "│ Edit file                                        │",
            "│ Do you want to make this edit to route.py?       │",
            "│ ❯ 1. Yes                                         │",
            "│   2. Yes, and don't ask again this session       │",
            "│   3. No, and tell Claude what to do differently  │",
            "╰──────────────────────────────────────────────────╯",
        ]);
        assert_eq!(
            picked(&rows),
            pairs(&[
                ("Yes", "1"),
                ("Yes, and don't ask again this session", "2"),
                ("No, and tell Claude what to do differently", "3"),
            ])
        );
        let (prompt, _) = read(3, &waiting(None), &rows).unwrap();
        assert_eq!(
            prompt.question.as_deref(),
            Some("Do you want to make this edit to route.py?")
        );
    }

    /// Claude Code's "trust this folder?" is numbered and says Enter under
    /// it; the numbers win, as typing one answers at once.
    #[test]
    fn claude_codes_trust_prompt_is_numbered_not_arrowed() {
        let rows = screen(&[
            " Do you trust the files in this folder?",
            "",
            " /Users/me/src/tty7",
            "",
            " ❯ 1. Yes, proceed",
            "   2. No, exit",
            "",
            " Enter to confirm · Esc to exit",
        ]);
        assert_eq!(
            picked(&rows),
            pairs(&[("Yes, proceed", "1"), ("No, exit", "2")])
        );
    }

    #[test]
    fn codexs_approval_is_answered_by_its_digits() {
        let rows = screen(&[
            "  Would you like to run the following command?",
            "",
            "  $ touch a.txt",
            "",
            "› 1. Yes, proceed (y)",
            "  2. Yes, and don't ask again for these files (p)",
            "  3. No, and tell Codex what to do differently (esc)",
            "",
            "  Press enter to confirm or esc to cancel",
        ]);
        assert_eq!(
            picked(&rows),
            pairs(&[
                ("Yes, proceed (y)", "1"),
                ("Yes, and don't ask again for these files (p)", "2"),
                ("No, and tell Codex what to do differently (esc)", "3"),
            ])
        );
        let (prompt, _) = read(1, &waiting(Some("")), &rows).unwrap();
        assert_eq!(prompt.question.as_deref(), Some("$ touch a.txt"));
    }

    /// Options that wrap carry on at a deeper indent, and are not options.
    #[test]
    fn a_menu_without_numbers_is_walked_to_with_the_arrows() {
        let rows = screen(&[
            " Subagent Configuration",
            "",
            "   explore   built-in   gpt-5.6-luna",
            " ❯ task      built-in   gpt-5.6-luna",
            "     runs a task to the end",
            "   review    built-in   gpt-5.6-luna",
            "",
            " ↑/↓ to navigate · enter to select",
        ]);
        assert_eq!(
            picked(&rows),
            pairs(&[
                ("explore   built-in   gpt-5.6-luna", "\x1b[A\r"),
                ("task      built-in   gpt-5.6-luna", "\r"),
                ("review    built-in   gpt-5.6-luna", "\x1b[B\r"),
            ])
        );
    }

    #[test]
    fn the_last_numbered_run_is_the_menu() {
        let rows = screen(&[
            "⏺ Two ways to go:",
            "  1. Patch the parser",
            "  2. Rewrite it",
            "",
            " Which one?",
            " ❯ 1. Patch",
            "   2. Rewrite",
        ]);
        assert_eq!(picked(&rows), pairs(&[("Patch", "1"), ("Rewrite", "2")]));
    }

    #[test]
    fn a_screen_with_no_menu_offers_nothing() {
        for rows in [
            screen(&["$ ls", "1. not a menu", "$ "]),
            screen(&["10. starts late", "11. and goes on", "$ "]),
            screen(&[RULE, "❯ ", RULE, "  ? for shortcuts"]),
            screen(&[" ❯ only", "", " enter to select"]),
        ] {
            assert_eq!(picked(&rows), [], "{rows:#?}");
        }
    }

    #[test]
    fn only_a_waiting_agent_has_a_prompt() {
        let working = AgentSessionState {
            status: CoreStatus::Working,
            ..AgentSessionState::default()
        };
        assert!(read(1, &working, &claude_bash()).is_none());
        let (prompt, keys) = read(
            1,
            &waiting(Some("Claude needs your permission to use Bash")),
            &claude_bash(),
        )
        .unwrap();
        // The hook's words come first; the screen's are for when it said none.
        assert_eq!(
            prompt.question.as_deref(),
            Some("Claude needs your permission to use Bash")
        );
        assert_eq!(prompt.options.len(), 3);
        assert_eq!(keys, [b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]);
        let (prompt, _) = read(1, &waiting(None), &claude_bash()).unwrap();
        assert_eq!(prompt.question.as_deref(), Some("Do you want to proceed?"));
    }

    /// The same question is the same prompt while it stands; the next one,
    /// worded the same, is another - so a late answer meant for the first
    /// is not taken for an answer to it.
    #[test]
    fn a_prompt_is_named_by_where_it_came_not_only_its_words() {
        let state = waiting(Some("needs permission"));
        let id =
            |state: &AgentSessionState, pane: u64| read(pane, state, &claude_bash()).unwrap().0.id;
        assert_eq!(id(&state, 1), id(&state, 1));
        assert_ne!(id(&state, 1), id(&state, 2));
        let later = AgentSessionState {
            activity: 1,
            ..state.clone()
        };
        assert_ne!(id(&state, 1), id(&later, 1));
        // Moving the cursor on the desktop changes nothing.
        let mut moved = claude_bash();
        moved[10] = "   1. Yes".into();
        moved[11] = " ❯ 2. Yes, and don't ask again for cargo test commands in /src/tty7".into();
        assert_eq!(read(1, &state, &moved).unwrap().0.id, id(&state, 1));
    }

    /// The hook says what kind of thing is asked, the screen what about:
    /// both are shown, and two questions the hook words the same are told
    /// apart by the screen's.
    #[test]
    fn the_hooks_question_comes_with_the_screens() {
        let hook = waiting(Some("Claude needs your permission to use Edit"));
        let edit = |file: &str| {
            screen(&[
                " Edit file",
                &format!(" Do you want to make this edit to {file}?"),
                " ❯ 1. Yes",
                "   2. No",
            ])
        };
        let (a, _) = read(1, &hook, &edit("a.rs")).unwrap();
        assert_eq!(
            a.question.as_deref(),
            Some("Claude needs your permission to use Edit")
        );
        assert_eq!(
            a.detail.as_deref(),
            Some("Do you want to make this edit to a.rs?")
        );
        let (b, _) = read(1, &hook, &edit("b.rs")).unwrap();
        assert_ne!(a.id, b.id);
        // Without the hook's words the screen's are the question, said once.
        let (c, _) = read(1, &waiting(None), &edit("a.rs")).unwrap();
        assert_eq!(
            (c.question.as_deref(), c.detail),
            (Some("Do you want to make this edit to a.rs?"), None)
        );
    }

    /// Typing "1" of "10" picks the first: past nine the arrows are used,
    /// from the cursor, and without one to walk from those are left off.
    #[test]
    fn options_past_nine_are_walked_to_with_the_arrows() {
        let mut rows = vec![" Pick a model".to_string()];
        rows.extend((1..=12).map(|n| {
            let mark = if n == 2 { "❯" } else { " " };
            format!(" {mark} {n}. model {n}")
        }));
        let got = picked(&rows);
        assert_eq!(got.len(), 12);
        assert_eq!(got[0], ("model 1".into(), "1".into()));
        assert_eq!(got[8], ("model 9".into(), "9".into()));
        assert_eq!(
            got[9],
            ("model 10".into(), format!("{}\r", "\x1b[B".repeat(8)))
        );
        assert_eq!(got[11].1, format!("{}\r", "\x1b[B".repeat(10)));

        let unmarked: Vec<String> = rows.iter().map(|r| r.replace('❯', " ")).collect();
        let got = picked(&unmarked);
        assert_eq!(got.len(), 9);
        assert_eq!(got[8], ("model 9".into(), "9".into()));
    }

    /// Read through the desktop's emulator from the bytes Claude Code
    /// writes: colours, cursor moves and a wide mark in front.
    #[test]
    fn a_screen_read_through_the_mirror_finds_the_menu() {
        struct Replay(Vec<DaemonMsg>);
        impl PaneFeed for Replay {
            fn recv(&mut self, _wait: Duration) -> io::Result<Option<DaemonMsg>> {
                Ok((!self.0.is_empty()).then(|| self.0.remove(0)))
            }
        }
        let bytes = "\x1b[2J\x1b[H⏺ Bash(ls)\r\n\x1b[1m Do you want to proceed?\x1b[0m\r\n \x1b[36m❯ 1. Yes\x1b[0m\r\n   2. No, and tell Claude what to do differently \x1b[2m(esc)\x1b[0m\r\n";
        let feed = Replay(vec![
            DaemonMsg::Size(tty7_core::daemon::protocol::WinSize {
                cols: 60,
                rows: 8,
                cell_w: 8,
                cell_h: 16,
            }),
            DaemonMsg::Snapshot(bytes.as_bytes().to_vec()),
        ]);
        let rows = read_screen(Box::new(feed)).unwrap();
        assert_eq!(rows.len(), 8);
        assert_eq!(
            picked(&rows),
            pairs(&[
                ("Yes", "1"),
                ("No, and tell Claude what to do differently (esc)", "2")
            ])
        );
    }

    fn tree_with(pane: u64, status: AgentStatus) -> Vec<WorkspaceView> {
        vec![WorkspaceView {
            id: "w".into(),
            name: "w".into(),
            tabs: vec![TabView {
                id: "t".into(),
                name: "t".into(),
                hibernated: false,
                panes: vec![PaneView {
                    id: pane,
                    title: "claude".into(),
                    cwd: None,
                    agent: Some(AgentView {
                        kind: "claude".into(),
                        status,
                        message: None,
                        prompt: None,
                    }),
                    stopped: false,
                }],
            }],
            groups: Vec::new(),
            active_tab: None,
        }]
    }

    fn prompt_of(tree: &[WorkspaceView]) -> Option<AgentPrompt> {
        tree[0].tabs[0].panes[0].agent.as_ref()?.prompt.clone()
    }

    /// A pane's screen as a test sets it, and how often it was read.
    #[derive(Default)]
    struct Pane {
        rows: Mutex<Vec<String>>,
        reads: AtomicUsize,
    }

    impl Pane {
        fn show(&self, rows: Vec<String>) {
            *self.rows.lock().unwrap() = rows;
        }

        fn reads(&self) -> usize {
            self.reads.load(SeqCst)
        }

        fn read(&self, _pane: u64) -> io::Result<Vec<String>> {
            self.reads.fetch_add(1, SeqCst);
            Ok(self.rows.lock().unwrap().clone())
        }
    }

    fn asked(state: AgentSessionState) -> Vec<PaneAgentState> {
        vec![PaneAgentState {
            pane_id: 5,
            agent: None,
            state,
        }]
    }

    /// The prompt a tree read `secs` in gives pane 5, waiting.
    fn fill_at(
        prompts: &mut Prompts,
        pane: &Pane,
        agents: &[PaneAgentState],
        t0: Instant,
        secs: f64,
    ) -> Option<AgentPrompt> {
        let mut tree = tree_with(5, AgentStatus::Waiting);
        let now = t0 + Duration::from_secs_f64(secs);
        prompts.fill(None, &mut tree, agents, |p| pane.read(p), now);
        prompt_of(&tree)
    }

    #[test]
    fn a_screen_is_read_when_its_agent_starts_waiting_and_while_its_menu_is_missing() {
        let pane = Pane::default();
        pane.show(screen(&["⏺ Bash(cargo test)"]));
        let mut prompts = Prompts::default();
        let t0 = Instant::now();
        let asking = asked(waiting(Some("needs permission")));

        // The hook came before the menu: nothing yet, and a look again soon.
        assert_eq!(fill_at(&mut prompts, &pane, &asking, t0, 0.0), None);
        assert_eq!(pane.reads(), 1);
        fill_at(&mut prompts, &pane, &asking, t0, 0.5);
        assert_eq!(pane.reads(), 1, "not before the retry is due");

        pane.show(claude_bash());
        let first = fill_at(&mut prompts, &pane, &asking, t0, 1.0).expect("the menu, once drawn");
        assert_eq!(pane.reads(), 2);
        assert_eq!(first.options.len(), 3);

        // The same question: kept between reads, not read every second.
        assert_eq!(
            fill_at(&mut prompts, &pane, &asking, t0, 3.0),
            Some(first.clone())
        );
        assert_eq!(pane.reads(), 2);

        // The next question, worded the same: read at once, named anew.
        let next = asked(AgentSessionState {
            activity: 1,
            ..waiting(Some("needs permission"))
        });
        let second = fill_at(&mut prompts, &pane, &next, t0, 4.0).unwrap();
        assert_eq!(pane.reads(), 3);
        assert_ne!(second.id, first.id);

        // Working again: no prompt, and nothing kept for it.
        let mut tree = tree_with(5, AgentStatus::Working);
        let working = asked(AgentSessionState {
            status: CoreStatus::Working,
            ..AgentSessionState::default()
        });
        let now = t0 + Duration::from_secs(5);
        prompts.fill(None, &mut tree, &working, |p| pane.read(p), now);
        assert_eq!((pane.reads(), prompt_of(&tree)), (3, None));
        assert!(prompts.known.is_empty());
    }

    /// A numbered list in the reply, read before the menu under it was
    /// drawn, is taken for the menu; the next read puts it right, and one
    /// that finds no menu takes the buttons away.
    #[test]
    fn a_waiting_agents_screen_is_read_again_while_it_waits() {
        let pane = Pane::default();
        pane.show(screen(&[
            "⏺ Two ways to go:",
            "  1. Patch the parser",
            "  2. Rewrite it",
        ]));
        let mut prompts = Prompts::default();
        let t0 = Instant::now();
        let asking = asked(waiting(Some("needs permission")));
        let wrong = fill_at(&mut prompts, &pane, &asking, t0, 0.0).unwrap();
        assert_eq!(wrong.options[0].label, "Patch the parser");

        pane.show(claude_bash());
        assert_eq!(
            fill_at(&mut prompts, &pane, &asking, t0, 4.0),
            Some(wrong),
            "kept until the next read is due"
        );
        let reread = REREAD.as_secs_f64();
        let right = fill_at(&mut prompts, &pane, &asking, t0, reread).unwrap();
        assert_eq!(right.options[0].label, "Yes");

        // Answered on the desktop while the hook still says it waits.
        pane.show(screen(&["⏺ Bash(cargo test)", "  ⎿  Running…"]));
        assert_eq!(
            fill_at(&mut prompts, &pane, &asking, t0, 2.0 * reread),
            None
        );
        assert_eq!(pane.reads(), 3);

        // A question the screen has no menu for is still looked at now and
        // then: soon at first, then every few seconds.
        for s in 11..=60 {
            fill_at(&mut prompts, &pane, &asking, t0, s as f64);
        }
        // After the miss at 10: 11, 13, then every 5 from 18 to 58.
        assert_eq!(pane.reads(), 3 + 2 + 9);
    }

    #[test]
    fn a_refused_answer_has_the_pane_read_again_at_once() {
        let answers = Arc::new(Answers::default());
        let pane = Pane::default();
        pane.show(claude_bash());
        let mut prompts = Prompts::new(answers.clone());
        let t0 = Instant::now();
        let asking = asked(waiting(Some("needs permission")));
        let shown = fill_at(&mut prompts, &pane, &asking, t0, 0.0).unwrap();
        fill_at(&mut prompts, &pane, &asking, t0, 1.0);
        assert_eq!(pane.reads(), 1);

        let refused = answers.give(None, 5, &shown.id, || Err(io::Error::other("moved on")));
        assert_eq!(refused.unwrap_err().to_string(), "moved on");
        // Another machine's pane 5 is another pane.
        let mut other = tree_with(5, AgentStatus::Waiting);
        let now = t0 + Duration::from_secs_f64(1.5);
        prompts.fill(Some("mac"), &mut other, &asking, |p| pane.read(p), now);
        assert_eq!(pane.reads(), 2);
        prompts.fill(Some("mac"), &mut other, &asking, |p| pane.read(p), now);
        assert_eq!(pane.reads(), 2);

        pane.show(screen(&["⏺ Bash(cargo test)"]));
        assert_eq!(fill_at(&mut prompts, &pane, &asking, t0, 1.5), None);
        assert_eq!(pane.reads(), 3);
        fill_at(&mut prompts, &pane, &asking, t0, 2.0);
        assert_eq!(pane.reads(), 3, "once per refusal");
    }

    /// Two taps, or two phones: the prompt is answered once, and the
    /// second waits for the first rather than reading the screen under it.
    #[test]
    fn a_prompt_is_answered_once_and_a_pane_one_answer_at_a_time() {
        let answers = Answers::default();
        let typed = AtomicUsize::new(0);
        let busy = AtomicBool::new(false);
        let type_it = || {
            assert!(!busy.swap(true, SeqCst), "two answers at once");
            std::thread::sleep(Duration::from_millis(50));
            typed.fetch_add(1, SeqCst);
            busy.store(false, SeqCst);
            Ok(())
        };
        let given: Vec<bool> = std::thread::scope(|scope| {
            let taps: Vec<_> = (0..2)
                .map(|_| scope.spawn(|| answers.give(None, 5, "abc", type_it).is_ok()))
                .collect();
            taps.into_iter().map(|tap| tap.join().unwrap()).collect()
        });
        assert_eq!(given.iter().filter(|ok| **ok).count(), 1, "{given:?}");
        assert_eq!(typed.load(SeqCst), 1);
        let again = answers.give(None, 5, "abc", type_it).unwrap_err();
        assert_eq!(again.to_string(), "that question was already answered");

        // The next question there, and the same one on another machine,
        // are answered.
        answers.give(None, 5, "def", type_it).unwrap();
        answers.give(Some("mac"), 5, "abc", type_it).unwrap();
        assert_eq!(typed.load(SeqCst), 3);
    }
}

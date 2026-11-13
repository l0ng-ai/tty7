//! Handing a task from one coding agent to another.
//!
//! An agent whose context is full, or that has gone round in circles, is
//! usually best replaced rather than argued with — by a different agent, or by
//! a fresh session of the same one. What the replacement needs is what the
//! first one knew, and tty7 already holds most of that: the session's first
//! prompt (in the agent's own transcript), the agent's last status message,
//! the repository's state, and the pane's scrollback.
//!
//! This module puts those into one markdown document. It is assembled, never
//! summarised: no model is asked anything, so the same inputs always give the
//! same file, and whatever it says can be traced back to where it came from.

use std::path::{Path, PathBuf};

use crate::core::cli_agent::{AgentStatus, CLIAgent};
use crate::host::Host;

/// How many lines of the pane's scrollback the document carries.
pub const SCROLLBACK_LINES: usize = 80;

/// The most of that scrollback kept, whatever the line count: an agent's TUI
/// can draw very wide lines, and the document is read by an agent with a
/// context budget of its own.
const MAX_SCROLLBACK_BYTES: usize = 16 * 1024;

/// The most of the original prompt kept. A pasted spec can run long; past
/// this the transcript itself is where to look.
const MAX_GOAL_CHARS: usize = 4000;

/// Same bound for each git section: a branch with thousands of changed files
/// would otherwise bury everything else.
const MAX_GIT_BYTES: usize = 8 * 1024;

/// Where the repository keeps handoffs, when it already ignores `.tty7/`.
pub const REPO_DIR: &str = ".tty7/handoffs";

/// Where the original goal came from, said in the document so the reader
/// knows how much to trust it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalSource {
    /// The first prompt in the agent's own session transcript.
    Transcript,
    /// The transcript could not be read; this is the tab's title.
    TabTitle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Goal {
    pub text: String,
    pub source: GoalSource,
}

/// The repository as `git` describes it, at the moment of the handoff.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitSnapshot {
    pub root: PathBuf,
    pub branch: Option<String>,
    /// `git status --short`.
    pub status: String,
    /// `git diff --stat HEAD` — staged and unstaged together.
    pub diff_stat: String,
}

/// Everything the document is built from.
#[derive(Clone, Debug)]
pub struct Input<'a> {
    pub from: CLIAgent,
    pub to: CLIAgent,
    /// The source tab's label, as the tab strip shows it.
    pub source_tab: &'a str,
    pub session_id: Option<&'a str>,
    pub goal: Option<Goal>,
    pub status: Option<AgentStatus>,
    pub status_message: Option<&'a str>,
    pub cwd: Option<&'a Path>,
    pub git: Option<&'a GitSnapshot>,
    /// Already trimmed with [`trim_scrollback`].
    pub scrollback: &'a str,
    pub written: Stamp,
}

/// A moment in UTC, broken into fields. Its own type rather than a date
/// library: two formats is all this needs, and both have to be stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl Stamp {
    /// `secs` since the Unix epoch, in UTC.
    pub fn from_unix(secs: u64) -> Stamp {
        let days = (secs / 86_400) as i64;
        let rem = secs % 86_400;
        let (year, month, day) = civil_from_days(days);
        Stamp {
            year,
            month,
            day,
            hour: (rem / 3600) as u32,
            minute: (rem % 3600 / 60) as u32,
            second: (rem % 60) as u32,
        }
    }

    pub fn now() -> Stamp {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Stamp::from_unix(secs)
    }

    /// `20261113-142530`: sorts by time, and has nothing a filesystem minds.
    pub fn compact(&self) -> String {
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }

    /// `2026-11-13 14:25:30 UTC`.
    pub fn readable(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `20261113-142530-claude-to-codex.md`.
pub fn file_name(written: Stamp, from: CLIAgent, to: CLIAgent) -> String {
    format!("{}-{}-to-{}.md", written.compact(), from.slug(), to.slug())
}

/// What the new agent is told first.
pub fn takeover_prompt(from: CLIAgent, document: &Path) -> String {
    format!(
        "You're taking over a task from {}. Read {} first, then continue.",
        from.display_name(),
        document.display()
    )
}

/// Where a handoff document goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Destination {
    /// `<repo>/.tty7/handoffs`, because the repository already ignores
    /// `.tty7/` — the file cannot end up in a commit.
    Repo(PathBuf),
    /// Under tty7's own config directory, because the repository would
    /// otherwise show it as an untracked file (or there is no repository).
    AppData(PathBuf),
}

impl Destination {
    pub fn dir(&self) -> &Path {
        match self {
            Destination::Repo(dir) | Destination::AppData(dir) => dir,
        }
    }
}

/// Choose where the document goes. Never edits a `.gitignore`: a repository
/// that does not already ignore `.tty7/` gets nothing written into it.
///
/// `app_dir` is tty7's handoff directory (`<config dir>/handoffs`); the
/// document lands in a folder named after the repository (or the directory)
/// under it, so handoffs from different projects stay apart.
pub fn choose_destination(
    repo_root: Option<&Path>,
    repo_ignores_tty7: bool,
    cwd: &Path,
    app_dir: Option<&Path>,
) -> Option<Destination> {
    if let Some(root) = repo_root
        && repo_ignores_tty7
    {
        return Some(Destination::Repo(root.join(REPO_DIR)));
    }
    let named = repo_root.unwrap_or(cwd);
    let folder = named
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "root".to_string());
    app_dir.map(|dir| Destination::AppData(dir.join(folder)))
}

/// Whether the repository at `root` already ignores `.tty7/handoffs/`.
/// Asked of `git check-ignore`, so `.gitignore`, `.git/info/exclude` and the
/// global excludes file all count, whatever pattern spelled it.
pub fn repo_ignores_tty7(host: &dyn Host, root: &Path) -> bool {
    host.git(
        root,
        &[
            "check-ignore",
            "-q",
            "--no-index",
            ".tty7/handoffs/probe.md",
        ],
    )
    .is_ok_and(|out| out.success())
}

/// The repository `cwd` is in, or `None` outside one.
pub fn git_snapshot(host: &dyn Host, cwd: &Path) -> Option<GitSnapshot> {
    let root = host.repo_root(cwd).ok().flatten()?;
    let run = |args: &[&str]| {
        host.git(&root, args)
            .ok()
            .filter(|o| o.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
    };
    let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| !b.is_empty());
    let status = run(&["-c", "core.quotePath=false", "status", "--short"]).unwrap_or_default();
    // `HEAD` covers what is staged too; a repository with no commit yet has
    // no `HEAD`, and the plain form is the best left to ask.
    let diff_stat = run(&["diff", "--stat", "HEAD"])
        .or_else(|| run(&["diff", "--stat"]))
        .unwrap_or_default();
    Some(GitSnapshot {
        root,
        branch,
        status,
        diff_stat,
    })
}

/// Write `document` as `name` under `dest`, creating the directory.
pub fn write(
    host: &dyn Host,
    dest: &Destination,
    name: &str,
    document: &str,
) -> std::io::Result<PathBuf> {
    let dir = dest.dir();
    host.create_dir(dir, true)?;
    let path = host.join(dir, name);
    host.write_file(&path, document.as_bytes())?;
    Ok(path)
}

/// The last `lines` lines of `text` worth reading: trailing whitespace off
/// each line, the empty rows a screen ends on dropped, and the whole capped
/// at [`MAX_SCROLLBACK_BYTES`] from the end.
pub fn trim_scrollback(text: &str, lines: usize) -> String {
    let mut kept: Vec<&str> = text.lines().map(str::trim_end).collect();
    while kept.last().is_some_and(|l| l.is_empty()) {
        kept.pop();
    }
    let start = kept.len().saturating_sub(lines);
    let mut kept = &kept[start..];
    while kept.first().is_some_and(|l| l.is_empty()) {
        kept = &kept[1..];
    }
    let joined = kept.join("\n");
    if joined.len() <= MAX_SCROLLBACK_BYTES {
        return joined;
    }
    let mut cut = joined.len() - MAX_SCROLLBACK_BYTES;
    while !joined.is_char_boundary(cut) {
        cut += 1;
    }
    // Start on a whole line, so the first one is not half of something.
    let from = joined[cut..].find('\n').map_or(cut, |i| cut + i + 1);
    joined[from..].to_string()
}

/// The goal as it goes in the document: whole, unless it is very long.
pub fn cap_goal(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_GOAL_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX_GOAL_CHARS).collect();
    out.push_str("\n[… cut by tty7 — the rest is in the original session's transcript]");
    out
}

fn cap_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[… cut by tty7]", &text[..end])
}

/// A code fence longer than any run of backticks inside `body`, so the
/// block cannot be closed early by what it quotes.
fn fenced(body: &str, lang: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in body.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}{lang}\n{body}\n{fence}\n")
}

fn status_word(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Idle => "idle",
        AgentStatus::Working => "working",
        AgentStatus::Waiting => "waiting for input",
        AgentStatus::Done => "done with its turn",
    }
}

/// The handoff document.
pub fn build_document(input: &Input<'_>) -> String {
    let from = input.from.display_name();
    let to = input.to.display_name();
    let mut doc = String::new();

    doc.push_str(&format!("# Handoff: {from} → {to}\n\n"));
    doc.push_str(
        "> You are taking over this task. tty7 assembled this file from what it could \
         see when the handoff was made — nothing here was summarised by a model, so \
         check it against the repository before relying on it.\n\n",
    );

    doc.push_str("| | |\n|---|---|\n");
    doc.push_str(&format!(
        "| From | {from} — tab \"{}\" |\n",
        table_cell(input.source_tab)
    ));
    doc.push_str(&format!("| To | {to} |\n"));
    doc.push_str(&format!("| Written | {} |\n", input.written.readable()));
    if let Some(cwd) = input.cwd {
        doc.push_str(&format!(
            "| Directory | `{}` |\n",
            table_cell(&cwd.display().to_string())
        ));
    }
    if let Some(branch) = input.git.and_then(|g| g.branch.as_deref()) {
        doc.push_str(&format!("| Branch | `{}` |\n", table_cell(branch)));
    }
    if let Some(id) = input.session_id {
        doc.push_str(&format!(
            "| Previous session | `{}` ({}) |\n",
            table_cell(id),
            input.from.slug()
        ));
    }
    doc.push('\n');

    doc.push_str("## Original goal\n\n");
    match &input.goal {
        Some(goal) => {
            doc.push_str(&fenced(&cap_goal(&goal.text), "text"));
            doc.push_str(match goal.source {
                GoalSource::Transcript => "\n_The first prompt of the previous session._\n\n",
                GoalSource::TabTitle => {
                    "\n_The previous session's transcript could not be read, so this is \
                     only the tab's title._\n\n"
                }
            });
        }
        None => doc.push_str("_Not known — neither the transcript nor the tab said._\n\n"),
    }

    doc.push_str("## Last status\n\n");
    match (input.status, input.status_message.map(str::trim)) {
        (Some(status), Some(message)) if !message.is_empty() => {
            doc.push_str(&format!("{from} was **{}**:\n\n", status_word(status)));
            doc.push_str(&fenced(message, "text"));
            doc.push('\n');
        }
        (Some(status), _) => doc.push_str(&format!(
            "{from} was **{}**, with no message.\n\n",
            status_word(status)
        )),
        (None, Some(message)) if !message.is_empty() => {
            doc.push_str(&fenced(message, "text"));
            doc.push('\n');
        }
        (None, _) => doc.push_str(&format!(
            "{from} had not reported a status (its tty7 hooks may not be installed).\n\n"
        )),
    }

    doc.push_str("## Repository\n\n");
    match input.git {
        Some(git) => {
            doc.push_str(&format!(
                "Root: `{}` · branch `{}`\n\n",
                git.root.display(),
                git.branch.as_deref().unwrap_or("(detached or unborn)")
            ));
            doc.push_str("### `git status --short`\n\n");
            match git.status.trim().is_empty() {
                true => doc.push_str("Clean.\n\n"),
                false => {
                    doc.push_str(&fenced(&cap_bytes(&git.status, MAX_GIT_BYTES), "text"));
                    doc.push('\n');
                }
            }
            doc.push_str("### `git diff --stat HEAD`\n\n");
            match git.diff_stat.trim().is_empty() {
                true => doc.push_str("No changes against `HEAD`.\n\n"),
                false => {
                    doc.push_str(&fenced(&cap_bytes(&git.diff_stat, MAX_GIT_BYTES), "text"));
                    doc.push('\n');
                }
            }
        }
        None => doc.push_str("Not a git repository.\n\n"),
    }

    doc.push_str("## Recent terminal output\n\n");
    match input.scrollback.trim().is_empty() {
        true => doc.push_str("_The pane had nothing on screen._\n\n"),
        false => {
            doc.push_str(&format!("The last lines of {from}'s pane, as text:\n\n"));
            doc.push_str(&fenced(input.scrollback, "text"));
            doc.push('\n');
        }
    }

    doc.push_str("## Next steps\n\n");
    doc.push_str(
        "<!-- Write what is left to do here before the new agent reads this, or leave it \
         and let the agent work it out from the sections above. -->\n\n- [ ] \n",
    );
    doc
}

/// A value that can sit in a one-line table cell.
fn table_cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const WRITTEN: Stamp = Stamp {
        year: 2026,
        month: 11,
        day: 13,
        hour: 14,
        minute: 25,
        second: 30,
    };

    fn git() -> GitSnapshot {
        GitSnapshot {
            root: PathBuf::from("/work/parser"),
            branch: Some("fix/tokens".into()),
            status: " M src/lex.rs\n?? tests/new.rs".into(),
            diff_stat:
                " src/lex.rs | 12 ++++++------\n 1 file changed, 6 insertions(+), 6 deletions(-)"
                    .into(),
        }
    }

    fn input<'a>(git: Option<&'a GitSnapshot>) -> Input<'a> {
        Input {
            from: CLIAgent::Claude,
            to: CLIAgent::Codex,
            source_tab: "fix the lexer",
            session_id: Some("8f3c-11"),
            goal: Some(Goal {
                text: "Make the lexer accept unicode identifiers".into(),
                source: GoalSource::Transcript,
            }),
            status: Some(AgentStatus::Waiting),
            status_message: Some("Claude needs your permission to use Bash"),
            cwd: Some(Path::new("/work/parser")),
            git,
            scrollback: "running 4 tests\ntest lex::ident ... FAILED",
            written: WRITTEN,
        }
    }

    #[test]
    fn the_document_carries_every_section_in_order() {
        let snapshot = git();
        let doc = build_document(&input(Some(&snapshot)));
        let order = [
            "# Handoff: Claude Code → Codex",
            "| From | Claude Code — tab \"fix the lexer\" |",
            "| Written | 2026-11-13 14:25:30 UTC |",
            "| Branch | `fix/tokens` |",
            "| Previous session | `8f3c-11` (claude) |",
            "## Original goal",
            "Make the lexer accept unicode identifiers",
            "_The first prompt of the previous session._",
            "## Last status",
            "Claude Code was **waiting for input**:",
            "Claude needs your permission to use Bash",
            "## Repository",
            "### `git status --short`",
            " M src/lex.rs",
            "### `git diff --stat HEAD`",
            "1 file changed",
            "## Recent terminal output",
            "test lex::ident ... FAILED",
            "## Next steps",
            "- [ ] ",
        ];
        let mut at = 0;
        for needle in order {
            let found = doc[at..]
                .find(needle)
                .unwrap_or_else(|| panic!("{needle:?} missing or out of order in:\n{doc}"));
            at += found + needle.len();
        }
    }

    #[test]
    fn the_same_inputs_give_the_same_document() {
        let snapshot = git();
        assert_eq!(
            build_document(&input(Some(&snapshot))),
            build_document(&input(Some(&snapshot)))
        );
    }

    #[test]
    fn missing_facts_are_said_rather_than_left_out() {
        let doc = build_document(&Input {
            session_id: None,
            goal: Some(Goal {
                text: "claude".into(),
                source: GoalSource::TabTitle,
            }),
            status: None,
            status_message: None,
            cwd: None,
            scrollback: "",
            ..input(None)
        });
        assert!(doc.contains("only the tab's title"), "{doc}");
        assert!(doc.contains("had not reported a status"), "{doc}");
        assert!(doc.contains("Not a git repository."), "{doc}");
        assert!(doc.contains("nothing on screen"), "{doc}");
        assert!(!doc.contains("Previous session"), "{doc}");

        let doc = build_document(&Input {
            goal: None,
            ..input(None)
        });
        assert!(doc.contains("_Not known"), "{doc}");
    }

    #[test]
    fn a_clean_tree_says_so() {
        let snapshot = GitSnapshot {
            status: String::new(),
            diff_stat: String::new(),
            ..git()
        };
        let doc = build_document(&input(Some(&snapshot)));
        assert!(doc.contains("Clean."));
        assert!(doc.contains("No changes against `HEAD`."));
    }

    #[test]
    fn quoted_text_cannot_close_its_own_fence() {
        let tricky = "here is code:\n```rust\nfn main() {}\n```\nand more";
        let doc = build_document(&Input {
            scrollback: tricky,
            ..input(None)
        });
        assert!(doc.contains("````text\nhere is code:"), "{doc}");
        assert!(doc.contains("and more\n````\n"), "{doc}");
    }

    #[test]
    fn a_tab_label_cannot_break_the_table() {
        let doc = build_document(&Input {
            source_tab: "a | b\nc",
            ..input(None)
        });
        assert!(doc.contains("tab \"a \\| b c\""), "{doc}");
    }

    #[test]
    fn a_long_goal_is_cut_with_a_note() {
        let long = "x".repeat(MAX_GOAL_CHARS + 10);
        let capped = cap_goal(&long);
        assert!(capped.starts_with(&"x".repeat(MAX_GOAL_CHARS)));
        assert!(capped.contains("cut by tty7"));
        assert_eq!(cap_goal("  short  "), "short");
    }

    #[test]
    fn scrollback_keeps_the_last_lines_without_trailing_blanks() {
        let text = "one\ntwo   \nthree\n\n\n   \n";
        assert_eq!(trim_scrollback(text, 2), "two\nthree");
        assert_eq!(trim_scrollback(text, 80), "one\ntwo\nthree");
        assert_eq!(trim_scrollback("\n\nonly\n", 5), "only");
        assert_eq!(trim_scrollback("", 5), "");
    }

    #[test]
    fn scrollback_is_capped_from_the_end_on_a_line_boundary() {
        let line = "y".repeat(1000);
        let text = (0..40)
            .map(|i| format!("{i:02}{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let kept = trim_scrollback(&text, 40);
        assert!(kept.len() <= MAX_SCROLLBACK_BYTES);
        assert!(kept.ends_with(&format!("39{line}")));
        assert!(
            kept.lines().all(|l| l.len() == line.len() + 2),
            "no half lines"
        );
    }

    #[test]
    fn file_names_sort_by_time_and_name_both_agents() {
        assert_eq!(
            file_name(WRITTEN, CLIAgent::Claude, CLIAgent::Codex),
            "20261113-142530-claude-to-codex.md"
        );
        // A fresh session of the same agent is a handoff too.
        assert_eq!(
            file_name(WRITTEN, CLIAgent::Codex, CLIAgent::Codex),
            "20261113-142530-codex-to-codex.md"
        );
    }

    #[test]
    fn unix_time_becomes_a_utc_date() {
        assert_eq!(
            Stamp::from_unix(0),
            Stamp {
                year: 1970,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0
            }
        );
        // 2024-02-29 23:59:59 UTC — a leap day, one second before March.
        assert_eq!(Stamp::from_unix(1_709_251_199).compact(), "20240229-235959");
        assert_eq!(
            Stamp::from_unix(1_709_251_200).readable(),
            "2024-03-01 00:00:00 UTC"
        );
    }

    #[test]
    fn the_takeover_prompt_names_the_agent_and_the_file() {
        assert_eq!(
            takeover_prompt(
                CLIAgent::Claude,
                Path::new("/r/.tty7/handoffs/20261113-142530-claude-to-codex.md")
            ),
            "You're taking over a task from Claude Code. Read \
             /r/.tty7/handoffs/20261113-142530-claude-to-codex.md first, then continue."
        );
    }

    #[test]
    fn the_repo_holds_the_handoff_only_when_it_already_ignores_tty7() {
        let root = Path::new("/work/parser");
        let app = Path::new("/home/me/.config/tty7/handoffs");
        assert_eq!(
            choose_destination(Some(root), true, root, Some(app)),
            Some(Destination::Repo(PathBuf::from(
                "/work/parser/.tty7/handoffs"
            )))
        );
        assert_eq!(
            choose_destination(Some(root), false, Path::new("/work/parser/src"), Some(app)),
            Some(Destination::AppData(PathBuf::from(
                "/home/me/.config/tty7/handoffs/parser"
            )))
        );
        // No repository: named after the directory instead.
        assert_eq!(
            choose_destination(None, false, Path::new("/tmp/scratch"), Some(app)),
            Some(Destination::AppData(PathBuf::from(
                "/home/me/.config/tty7/handoffs/scratch"
            )))
        );
        assert_eq!(choose_destination(None, false, Path::new("/x"), None), None);
    }
}

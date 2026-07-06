//! A small, self-contained completion engine for the command editor — tty7's own
//! engine, not the shell's `compsys`.
//!
//! It offers two sources, each candidate carrying the exact char range it
//! replaces:
//!   - **command** — builtins + `$PATH` executables, in command position;
//!   - **path** — files / directories, elsewhere (replace just the word).
//!
//! History deliberately does *not* feed the menu:
//! whole-line recall belongs to the inline ghost text (frecency-ranked, cwd
//! aware — accepted with → / Ctrl+F) and Ctrl+R search. Mixing recalled lines
//! into the Tab menu buried the precise completions under near-duplicate path
//! variants of past commands.
//!
//! Pure and side-effect-free apart from reading the filesystem / `$PATH`, so the
//! word-parsing and path logic are unit-tested directly.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::signature::{self, Arg, CmdNode, Signature};

/// A word candidate before it's placed at a range. Signature-derived candidates
/// carry a `description` and possibly an `icon` (a raw Fig icon string — emoji or
/// `fig://…`); `$PATH` and path candidates carry neither.
struct WordCand {
    text: String,
    kind: CandidateKind,
    description: Option<String>,
    icon: Option<String>,
}

impl WordCand {
    /// A candidate with no signature metadata — the command and path sources.
    fn plain(text: String, kind: CandidateKind) -> Self {
        Self {
            text,
            kind,
            description: None,
            icon: None,
        }
    }
}

/// What a completion candidate refers to — drives both the trailing `/` for
/// directories and the menu's leading icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    /// A command name (builtin or `$PATH` executable).
    Command,
    /// A directory.
    Dir,
    /// A regular file.
    File,
    /// A command flag / option (e.g. `--message`), from a command signature.
    Flag,
    /// A subcommand or argument value, from a command signature.
    Value,
}

/// A single completion candidate: the replacement text, its kind, and the char
/// range `[start, end)` in the original line that it replaces — just the word
/// under the cursor (`word_start..cursor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub text: String,
    pub kind: CandidateKind,
    pub start: usize,
    pub end: usize,
    /// A one-line hint shown in a second column — the flag/subcommand's
    /// description from its command signature; `None` for path/command candidates.
    pub description: Option<String>,
    /// Raw Fig icon string (emoji or `fig://…`) for signature candidates; the
    /// view interprets it, falling back to a per-kind glyph. `None` otherwise.
    pub icon: Option<String>,
}

impl Candidate {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, CandidateKind::Dir)
    }
}

/// The result of completing at a cursor: the word candidates, each with its own
/// replacement range.
#[derive(Debug)]
pub struct Completion {
    pub candidates: Vec<Candidate>,
}

/// Common shell builtins / keywords, offered in command position. Not exhaustive,
/// but covers what `$PATH` scanning misses (builtins aren't files).
const BUILTINS: &[&str] = &[
    "cd", "echo", "exit", "export", "pwd", "alias", "unalias", "source", "set", "unset", "history",
    "jobs", "fg", "bg", "kill", "which", "type", "read", "local", "return", "eval", "exec", "test",
    "true", "false", "printf", "let", "declare", "typeset", "shift", "trap", "wait", "umask",
];

/// Cap on candidates returned, so a bare prefix that matches thousands of files
/// (or `$PATH` entries) can't blow up the UI or the cycle.
const MAX_CANDIDATES: usize = 400;

/// Compute completions for `line` at char position `cursor`, resolving relative
/// paths against `cwd`: command names in command position, filesystem paths
/// elsewhere. Returns `None` when there's nothing to offer.
pub fn complete(line: &str, cursor: usize, cwd: &Path) -> Option<Completion> {
    let chars: Vec<char> = line.chars().collect();
    let cursor = cursor.min(chars.len());

    // The word under completion is the run of non-whitespace ending at the cursor.
    let mut word_start = cursor;
    while word_start > 0 && !chars[word_start - 1].is_whitespace() {
        word_start -= 1;
    }
    let word: String = chars[word_start..cursor].iter().collect();

    let is_command = chars[..word_start].iter().all(|c| c.is_whitespace());
    let word_cands = if is_command && !word.contains('/') {
        complete_command(&word)
    } else {
        // In argument position, prefer a per-command signature (flags,
        // subcommands, typed args) when the command has one; otherwise fall
        // back to filesystem paths.
        complete_signature(&chars, word_start, &word, cwd)
            .unwrap_or_else(|| complete_path(&word, cwd))
    };
    let candidates: Vec<Candidate> = word_cands
        .into_iter()
        .take(MAX_CANDIDATES)
        .map(|wc| Candidate {
            text: wc.text,
            kind: wc.kind,
            start: word_start,
            end: cursor,
            description: wc.description,
            icon: wc.icon,
        })
        .collect();
    if candidates.is_empty() {
        None
    } else {
        Some(Completion { candidates })
    }
}

/// Command-name completion: builtins plus `$PATH` executables starting with
/// `word`. An empty word returns nothing (we don't dump every command on a bare
/// Tab in command position). Ordered by closeness.
fn complete_command(word: &str) -> Vec<WordCand> {
    if word.is_empty() {
        return Vec::new();
    }
    let mut set = BTreeSet::new();
    for b in BUILTINS {
        if b.starts_with(word) {
            set.insert((*b).to_string());
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(word) {
                    set.insert(name);
                    if set.len() >= MAX_CANDIDATES {
                        break;
                    }
                }
            }
        }
    }
    let mut out: Vec<String> = set.into_iter().take(MAX_CANDIDATES).collect();
    sort_by_closeness(&mut out);
    out.into_iter()
        .map(|t| WordCand::plain(t, CandidateKind::Command))
        .collect()
}

/// Order strings by closeness to what the user typed: since every candidate
/// shares the typed prefix, the edit distance is just the length still to fill
/// in — so shorter completions come first, ties broken alphabetically.
fn sort_by_closeness(items: &mut [String]) {
    items.sort_by(|a, b| {
        a.chars()
            .count()
            .cmp(&b.chars().count())
            .then_with(|| a.cmp(b))
    });
}

/// Filesystem path completion. Splits `word` into the directory part (kept
/// verbatim in each candidate so the typed path prefix is preserved) and the
/// final-segment prefix to match in that directory. Ordered by closeness.
fn complete_path(word: &str, cwd: &Path) -> Vec<WordCand> {
    // Split on the last path separator. `is_separator` is `/` on Unix and both
    // `/` and `\` on Windows, so a `C:\Users\me\f`-style word splits correctly
    // under the (future) Windows line editor; separators are ASCII so the byte
    // slice boundaries are valid.
    let (dir_part, prefix) = match word.rfind(std::path::is_separator) {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };
    let base = resolve_dir(dir_part, cwd);

    let Ok(rd) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut out: Vec<WordCand> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Hidden entries only when the prefix explicitly starts with a dot.
        if name.starts_with('.') && !prefix.starts_with('.') {
            continue;
        }
        if !name.starts_with(prefix) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let kind = if is_dir {
            CandidateKind::Dir
        } else {
            CandidateKind::File
        };
        out.push(WordCand::plain(format!("{dir_part}{name}"), kind));
        if out.len() >= MAX_CANDIDATES {
            break;
        }
    }
    out.sort_by(|a, b| {
        a.text
            .chars()
            .count()
            .cmp(&b.text.chars().count())
            .then_with(|| a.text.cmp(&b.text))
    });
    out
}

/// Signature-driven completion in argument position. Tokenizes the text before
/// the word into an argv, and — if the current command has a signature — offers
/// flags, subcommands, or typed-argument suggestions for the cursor's position.
///
/// Returns `None` (so the caller falls back to path completion) when the command
/// has no signature, or when the position yields nothing useful and isn't a flag
/// or value slot (so a bare argument still lists files).
fn complete_signature(
    chars: &[char],
    word_start: usize,
    word: &str,
    cwd: &Path,
) -> Option<Vec<WordCand>> {
    // Only the current simple command matters: start after the last shell
    // separator so `foo | git <tab>` completes `git`, not `foo`.
    let prefix: String = chars[..word_start].iter().collect();
    let seg_start = prefix
        .rfind(['|', '&', ';', '\n', '('])
        .map(|i| i + 1)
        .unwrap_or(0);
    let tokens: Vec<&str> = prefix[seg_start..].split_whitespace().collect();
    let cmd = tokens.first()?;
    let sig = signature::signature(cmd)?;

    let (node, pending_value) = walk_signature(&sig, &tokens[1..]);

    // Flag position: options of the current node whose spelling extends `word`.
    if word.starts_with('-') {
        let mut out = Vec::new();
        for opt in node.options() {
            if opt.hidden {
                continue;
            }
            for name in &opt.names {
                if name.starts_with(word) {
                    out.push(WordCand {
                        text: name.clone(),
                        kind: CandidateKind::Flag,
                        description: opt.description.clone(),
                        icon: opt.icon.clone(),
                    });
                }
            }
        }
        return Some(finish(out));
    }

    // Value position: the previous token was an option taking an argument.
    if let Some(arg) = pending_value {
        let mut out = Vec::new();
        push_arg_suggestions(&mut out, arg, word);
        if arg.wants_paths() {
            out.extend(complete_path(word, cwd));
        }
        return if out.is_empty() { None } else { Some(out) };
    }

    // Fresh token: subcommands of the current node plus its first positional arg.
    let mut out = Vec::new();
    for sub in node.subcommands() {
        if sub.hidden {
            continue;
        }
        for name in &sub.names {
            if name.starts_with(word) {
                out.push(WordCand {
                    text: name.clone(),
                    kind: CandidateKind::Value,
                    description: sub.description.clone(),
                    icon: sub.icon.clone(),
                });
            }
        }
    }
    if let Some(arg) = node.args().first() {
        push_arg_suggestions(&mut out, arg, word);
        if arg.wants_paths() {
            out.extend(complete_path(word, cwd));
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(finish(out))
    }
}

/// Walk the argv after the command name, descending into matched subcommands and
/// skipping options (and the value tokens of value-taking ones). Returns the
/// deepest node reached, and — when the final prior token is a value-taking
/// option — the argument the cursor is now positioned to complete.
fn walk_signature<'a>(sig: &'a Signature, rest: &[&str]) -> (&'a dyn CmdNode, Option<&'a Arg>) {
    let mut node: &dyn CmdNode = sig;
    let mut i = 0;
    while i < rest.len() {
        let tok = rest[i];
        if tok.starts_with('-') {
            // Skip the flag, and its value token when it takes one inline-`=`-free.
            if node.find_option(tok).is_some_and(|o| o.takes_arg()) && !tok.contains('=') {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(sub) = node.find_subcommand(tok) {
            node = sub;
        }
        // A non-matching bare token is a positional arg; the node is unchanged.
        i += 1;
    }

    // Is the cursor sitting on a value-taking option's value?
    let pending = rest.last().and_then(|last| {
        (last.starts_with('-') && !last.contains('='))
            .then(|| node.find_option(last))
            .flatten()
            .filter(|o| o.takes_arg())
            .and_then(|o| o.args.first())
    });
    (node, pending)
}

/// Append an argument's static value suggestions matching `word`.
fn push_arg_suggestions(out: &mut Vec<WordCand>, arg: &Arg, word: &str) {
    for sug in &arg.suggestions {
        for name in &sug.names {
            if name.starts_with(word) {
                out.push(WordCand {
                    text: name.clone(),
                    kind: CandidateKind::Value,
                    description: sug.description.clone(),
                    icon: sug.icon.clone(),
                });
            }
        }
    }
}

/// Dedupe by replacement text and order by closeness (shorter first, then
/// alphabetical) — the same ordering path completion uses.
fn finish(mut out: Vec<WordCand>) -> Vec<WordCand> {
    out.sort_by(|a, b| {
        a.text
            .chars()
            .count()
            .cmp(&b.text.chars().count())
            .then_with(|| a.text.cmp(&b.text))
    });
    out.dedup_by(|a, b| a.text == b.text);
    out
}

/// Resolve the directory portion of a path word to an absolute directory to list:
/// handles `~` expansion, absolute paths, and paths relative to `cwd`.
fn resolve_dir(dir_part: &str, cwd: &Path) -> PathBuf {
    if dir_part.is_empty() {
        return cwd.to_path_buf();
    }
    if dir_part == "~" || dir_part == "~/" {
        if let Some(home) = home_dir() {
            return home;
        }
    }
    if let Some(rest) = dir_part.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    let p = PathBuf::from(dir_part);
    if p.is_absolute() { p } else { cwd.join(p) }
}

/// The user's home directory: `$HOME` on Unix, falling back to `%USERPROFILE%`
/// on Windows (where `HOME` is usually unset).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// One open completion menu: a *picker* over the candidates gathered
/// when it opened. Moving the highlight (Tab / ↑ / ↓) never touches the editor
/// line — the line changes only when a candidate is accepted (Enter) or when Tab
/// fills the candidates' common prefix. Typing re-filters the same candidate set
/// via [`CompletionSession::refilter`]; the session ends once the word stops
/// extending the one it opened on. Fields are `pub(super)` so the terminal view
/// can render the menu.
pub(super) struct CompletionSession {
    /// Char index where the word under completion starts — the fixed left edge
    /// of the range an accept replaces (the right edge is the live caret).
    pub(super) word_start: usize,
    /// The word as typed when the menu opened (before any common-prefix fill).
    /// Backspacing below it closes the menu.
    pub(super) open_word: String,
    /// Every candidate from open time; `filtered` holds indices into this.
    pub(super) all: Vec<Candidate>,
    /// Indices into `all` still prefix-matching the live word, in order.
    pub(super) filtered: Vec<usize>,
    /// Highlighted row (an index into `filtered`).
    pub(super) index: Option<usize>,
}

/// A splice to apply to the command editor: replace chars `[start, end)` of
/// `orig` with `text`. Used by the view's accept / prefix-fill paths; kept
/// separate so the pure string edit is testable without a live editor.
pub(super) struct Replacement {
    pub(super) orig: String,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) text: String,
}

impl Replacement {
    /// Perform the splice: returns the new line and the caret position (just after
    /// the inserted text). Char-indexed and clamped, so out-of-range candidate
    /// offsets can never panic.
    pub(super) fn apply(&self) -> (String, usize) {
        let mut chars: Vec<char> = self.orig.chars().collect();
        let start = self.start.min(chars.len());
        let end = self.end.min(chars.len()).max(start);
        let ins: Vec<char> = self.text.chars().collect();
        let new_cursor = start + ins.len();
        chars.splice(start..end, ins);
        (chars.into_iter().collect(), new_cursor)
    }
}

impl CompletionSession {
    /// Open a menu over `all` with the first row highlighted (a default
    /// preselection, so a bare Enter accepts the top pick).
    pub(super) fn new(word_start: usize, open_word: String, all: Vec<Candidate>) -> Self {
        let filtered = (0..all.len()).collect();
        Self {
            word_start,
            open_word,
            all,
            filtered,
            index: Some(0),
        }
    }

    /// The highlighted candidate, if any.
    pub(super) fn selected(&self) -> Option<&Candidate> {
        self.index
            .and_then(|i| self.filtered.get(i))
            .map(|&i| &self.all[i])
    }

    /// Move the highlight to the next (`forward`) or previous row, wrapping.
    /// Selection is visual only — the editor line changes on accept.
    pub(super) fn select(&mut self, forward: bool) {
        let n = self.filtered.len();
        if n == 0 {
            return;
        }
        self.index = Some(match self.index {
            None if forward => 0,
            None => n - 1,
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
        });
    }

    /// Re-filter for the live `word`. Returns `false` when the menu should
    /// close: the word no longer extends the one it opened on (backspaced past
    /// it) or nothing matches any more. A highlighted candidate that survives
    /// the filter keeps its highlight; one filtered away falls back to the top.
    pub(super) fn refilter(&mut self, word: &str) -> bool {
        if !word.starts_with(self.open_word.as_str()) {
            return false;
        }
        let selected_all = self.index.and_then(|i| self.filtered.get(i)).copied();
        self.filtered = (0..self.all.len())
            .filter(|&i| self.all[i].text.starts_with(word))
            .collect();
        if self.filtered.is_empty() {
            return false;
        }
        let kept = selected_all.and_then(|a| self.filtered.iter().position(|&i| i == a));
        self.index = Some(kept.unwrap_or(0));
        true
    }

    /// Longest common prefix (in chars) of the filtered candidates — what Tab
    /// fills before it starts moving the highlight.
    pub(super) fn common_prefix(&self) -> Option<String> {
        let mut texts = self.filtered.iter().map(|&i| self.all[i].text.as_str());
        let mut lcp: Vec<char> = texts.next()?.chars().collect();
        for t in texts {
            let shared = lcp
                .iter()
                .zip(t.chars())
                .take_while(|(a, b)| **a == *b)
                .count();
            lcp.truncate(shared);
            if lcp.is_empty() {
                break;
            }
        }
        Some(lcp.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(text: &str, kind: CandidateKind, start: usize, end: usize) -> Candidate {
        Candidate {
            text: text.into(),
            kind,
            start,
            end,
            description: None,
            icon: None,
        }
    }

    /// The candidate texts `complete` returns for `line` with the cursor at the
    /// end, or an empty vec when it offers nothing.
    fn texts(line: &str) -> Vec<String> {
        complete(line, line.chars().count(), Path::new("/"))
            .map(|c| c.candidates.into_iter().map(|c| c.text).collect())
            .unwrap_or_default()
    }

    #[test]
    fn signature_offers_subcommands_after_command() {
        let t = texts("git ");
        assert!(t.iter().any(|s| s == "commit"), "git subcommands: {t:?}");
        assert!(t.iter().any(|s| s == "status"));
        // Descriptions ride along for the menu's second column.
        let c = complete("git ", 4, Path::new("/")).unwrap();
        let commit = c.candidates.iter().find(|c| c.text == "commit").unwrap();
        assert_eq!(commit.kind, CandidateKind::Value);
        assert!(commit.description.is_some());
    }

    #[test]
    fn signature_narrows_subcommands_by_prefix() {
        let t = texts("git comm");
        assert!(t.iter().any(|s| s == "commit"));
        assert!(t.iter().all(|s| s.starts_with("comm")), "only comm*: {t:?}");
    }

    #[test]
    fn signature_offers_flags_for_the_active_subcommand() {
        let c = complete("git commit --", 13, Path::new("/")).unwrap();
        let msg = c.candidates.iter().find(|c| c.text == "--message").unwrap();
        assert_eq!(msg.kind, CandidateKind::Flag);
        assert_eq!(
            msg.description.as_deref(),
            Some("Use the given message as the commit message")
        );
    }

    #[test]
    fn signature_resolves_nested_subcommands() {
        // docker compose was grafted in via loadSpec; its subcommands complete.
        let t = texts("docker compose ");
        assert!(
            t.iter().any(|s| s == "up"),
            "docker compose subcommands: {t:?}"
        );
    }

    #[test]
    fn unknown_command_falls_back_to_paths() {
        // A command with no signature still path-completes (no panic, no menu here).
        let dir = temp_tree("fallback", &[("readme.md", false)]);
        let c = complete("frobnicate read", "frobnicate read".chars().count(), &dir).unwrap();
        assert_eq!(c.candidates[0].text, "readme.md");
    }

    #[test]
    fn replacement_splices_over_the_char_range() {
        let (line, cursor) = Replacement {
            orig: "cd sr".into(),
            start: 3,
            end: 5,
            text: "src/".into(),
        }
        .apply();
        assert_eq!(line, "cd src/");
        assert_eq!(cursor, 7);
    }

    #[test]
    fn replacement_clamps_out_of_range_offsets_without_panicking() {
        let (line, cursor) = Replacement {
            orig: "ab".into(),
            start: 5,
            end: 9,
            text: "X".into(),
        }
        .apply();
        assert_eq!(line, "abX");
        assert_eq!(cursor, 3);
    }

    fn session(words: &[&str]) -> CompletionSession {
        let cands = words
            .iter()
            .map(|w| cand(w, CandidateKind::Command, 0, 1))
            .collect();
        CompletionSession::new(0, "a".into(), cands)
    }

    #[test]
    fn select_moves_the_highlight_and_wraps_without_touching_candidates() {
        let mut s = session(&["aa", "ab", "ac"]);
        assert_eq!(s.index, Some(0)); // first row preselected on open
        s.select(true);
        assert_eq!(s.index, Some(1));
        s.select(true);
        s.select(true);
        assert_eq!(s.index, Some(0)); // wraps forward
        s.select(false);
        assert_eq!(s.index, Some(2)); // wraps backward
        assert_eq!(s.selected().unwrap().text, "ac");
    }

    #[test]
    fn refilter_narrows_keeps_surviving_highlight_and_closes_when_stale() {
        let mut s = session(&["aa", "ab", "abc"]);
        s.select(true); // highlight "ab"
        assert!(s.refilter("ab"));
        // "aa" filtered out; the highlighted "ab" survives and keeps its highlight.
        assert_eq!(s.filtered.len(), 2);
        assert_eq!(s.selected().unwrap().text, "ab");
        // A word that no longer extends the open word closes the menu…
        assert!(!s.refilter(""));
        // …as does one nothing matches.
        let mut s = session(&["aa", "ab"]);
        assert!(!s.refilter("az"));
    }

    #[test]
    fn refilter_falls_back_to_the_top_when_the_highlight_is_filtered_away() {
        let mut s = session(&["aa", "ab", "abc"]);
        // Highlight "aa", then type "ab" — "aa" drops out, top row takes over.
        assert_eq!(s.selected().unwrap().text, "aa");
        assert!(s.refilter("ab"));
        assert_eq!(s.selected().unwrap().text, "ab");
    }

    #[test]
    fn common_prefix_spans_the_filtered_candidates() {
        let mut s = session(&["apple.txt", "apply.sh", "apricot"]);
        assert_eq!(s.common_prefix().unwrap(), "ap");
        assert!(s.refilter("app"));
        assert_eq!(s.common_prefix().unwrap(), "appl");
    }

    fn temp_tree(tag: &str, entries: &[(&str, bool)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-comp-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, is_dir) in entries {
            if *is_dir {
                std::fs::create_dir_all(dir.join(name)).unwrap();
            } else {
                std::fs::write(dir.join(name), b"").unwrap();
            }
        }
        dir
    }

    #[test]
    fn command_position_offers_builtins_with_word_range() {
        let c = complete("ech", 3, Path::new("/")).unwrap();
        let echo = c.candidates.iter().find(|c| c.text == "echo").unwrap();
        assert_eq!(echo.kind, CandidateKind::Command);
        assert_eq!((echo.start, echo.end), (0, 3)); // replaces the word "ech"
    }

    #[test]
    fn path_completion_matches_prefix_and_flags_dirs() {
        let dir = temp_tree(
            "paths",
            &[("apple.txt", false), ("apply.sh", false), ("assets", true)],
        );
        let line = "cat a";
        let c = complete(line, line.chars().count(), &dir).unwrap();
        let names: Vec<&str> = c.candidates.iter().map(|c| c.text.as_str()).collect();
        // Closeness order: assets(6) < apply.sh(8) < apple.txt(9).
        assert_eq!(names, vec!["assets", "apply.sh", "apple.txt"]);
        let assets = c.candidates.iter().find(|c| c.text == "assets").unwrap();
        assert!(assets.is_dir());
        assert_eq!((assets.start, assets.end), (4, 5)); // the "a" word
    }

    #[test]
    fn path_completion_keeps_dir_prefix_in_candidate() {
        let dir = temp_tree("nested", &[("sub", true)]);
        std::fs::write(dir.join("sub/file.rs"), b"").unwrap();
        let line = "cat sub/f";
        let c = complete(line, line.chars().count(), &dir).unwrap();
        assert_eq!(c.candidates[0].text, "sub/file.rs");
        assert_eq!(c.candidates[0].start, 4);
    }

    #[test]
    fn hidden_files_only_with_dot_prefix() {
        let dir = temp_tree("hidden", &[(".secret", false), ("visible", false)]);
        let c = complete("ls v", 4, &dir).unwrap();
        assert!(c.candidates.iter().all(|c| !c.text.starts_with('.')));
        let c = complete("ls .", 4, &dir).unwrap();
        assert!(c.candidates.iter().any(|c| c.text == ".secret"));
    }

    #[test]
    fn candidates_ordered_by_closeness_then_alpha() {
        let dir = temp_tree(
            "closeness",
            &[
                ("xy", false),
                ("xyz", false),
                ("xa", false),
                ("xyzzy", false),
            ],
        );
        let line = "cat x";
        let c = complete(line, line.chars().count(), &dir).unwrap();
        let names: Vec<&str> = c.candidates.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(names, vec!["xa", "xy", "xyz", "xyzzy"]);
    }

    #[test]
    fn no_candidates_returns_none() {
        let dir = temp_tree("empty", &[("zzz", false)]);
        assert!(complete("cat q", 5, &dir).is_none());
        // A blank line offers nothing (no dump of every command on bare Tab).
        assert!(complete("", 0, &dir).is_none());
        assert!(complete("   ", 3, &dir).is_none());
    }

    #[test]
    fn mid_line_cursor_completes_only_the_word_before_it() {
        // Caret sits right after "ap" with more text following; the candidate
        // replaces only `word_start..cursor`, leaving the tail untouched.
        let dir = temp_tree("midline", &[("apple.txt", false)]);
        let c = complete("cat ap x.log", 6, &dir).unwrap();
        let apple = c.candidates.iter().find(|c| c.text == "apple.txt").unwrap();
        assert_eq!((apple.start, apple.end), (4, 6));
        // Applying it splices over just that range.
        let (line, cursor) = Replacement {
            orig: "cat ap x.log".into(),
            start: apple.start,
            end: apple.end,
            text: apple.text.clone(),
        }
        .apply();
        assert_eq!(line, "cat apple.txt x.log");
        assert_eq!(cursor, 13);
    }

    #[test]
    fn sort_by_closeness_orders_by_length_then_alpha() {
        let mut items = vec![
            "xyzzy".to_string(),
            "xa".to_string(),
            "xyz".to_string(),
            "xb".to_string(),
        ];
        sort_by_closeness(&mut items);
        // Shorter first; equal-length ties broken alphabetically.
        assert_eq!(items, vec!["xa", "xb", "xyz", "xyzzy"]);
    }

    #[test]
    fn resolve_dir_handles_empty_absolute_and_relative() {
        let cwd = Path::new("/work/proj");
        // Empty dir part → the cwd itself.
        assert_eq!(resolve_dir("", cwd), PathBuf::from("/work/proj"));
        // An absolute dir part is taken verbatim.
        assert_eq!(resolve_dir("/etc/", cwd), PathBuf::from("/etc/"));
        // A relative dir part is joined onto the cwd.
        assert_eq!(resolve_dir("src/", cwd), PathBuf::from("/work/proj/src/"));
    }

    #[test]
    fn resolve_dir_expands_tilde_to_home() {
        // Read the real home (no env mutation, so parallel tests aren't disturbed);
        // the `~` branches must resolve against it.
        if let Some(home) = home_dir() {
            let cwd = Path::new("/work");
            assert_eq!(resolve_dir("~", cwd), home);
            assert_eq!(resolve_dir("~/", cwd), home.clone());
            assert_eq!(resolve_dir("~/dev/", cwd), home.join("dev/"));
        }
    }
}

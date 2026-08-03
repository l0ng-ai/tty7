use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::signature::{self, Arg, CmdNode, Signature};

struct WordCand {
    text: String,
    kind: CandidateKind,
    description: Option<String>,
    icon: Option<String>,
}

impl WordCand {
    fn plain(text: String, kind: CandidateKind) -> Self {
        Self {
            text,
            kind,
            description: None,
            icon: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    Command,
    Dir,
    File,
    Flag,
    Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub text: String,
    pub kind: CandidateKind,
    pub start: usize,
    pub end: usize,
    pub description: Option<String>,
    pub icon: Option<String>,
}

impl Candidate {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, CandidateKind::Dir)
    }
}

#[derive(Debug)]
pub struct Completion {
    pub candidates: Vec<Candidate>,
    pub pending: Vec<PendingGenerator>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingGenerator {
    pub script: String,
}

const BUILTINS: &[&str] = &[
    "cd", "echo", "exit", "export", "pwd", "alias", "unalias", "source", "set", "unset", "history",
    "jobs", "fg", "bg", "kill", "which", "type", "read", "local", "return", "eval", "exec", "test",
    "true", "false", "printf", "let", "declare", "typeset", "shift", "trap", "wait", "umask",
];

const MAX_CANDIDATES: usize = 400;

const DIR_ONLY_COMMANDS: &[&str] = &["cd", "pushd", "popd", "rmdir"];

fn current_command(chars: &[char], word_start: usize) -> Option<String> {
    let prefix: String = chars[..word_start].iter().collect();
    let seg_start = prefix
        .rfind(['|', '&', ';', '\n', '('])
        .map(|i| i + 1)
        .unwrap_or(0);
    let cmd = prefix[seg_start..].split_whitespace().next()?;
    let base = cmd
        .rfind(std::path::is_separator)
        .map(|i| &cmd[i + 1..])
        .unwrap_or(cmd);
    (!base.is_empty()).then(|| base.to_string())
}

pub fn complete(line: &str, cursor: usize, cwd: Option<&Path>) -> Option<Completion> {
    let chars: Vec<char> = line.chars().collect();
    let cursor = cursor.min(chars.len());
    let word_start = shell_word_start(&chars, cursor);
    let word: String = chars[word_start..cursor].iter().collect();

    let is_command = chars[..word_start].iter().all(|c| c.is_whitespace());
    let (word_cands, pending) = if is_command && !word.contains('/') {
        (complete_command(&word, cwd.is_some()), Vec::new())
    } else {
        match complete_signature(&chars, word_start, &word, cwd) {
            Some(sig) => (sig.cands, sig.pending),
            None => match cwd {
                None => (Vec::new(), Vec::new()),
                Some(cwd) => {
                    let dirs_only = current_command(&chars, word_start)
                        .is_some_and(|c| DIR_ONLY_COMMANDS.contains(&c.as_str()));
                    (complete_path(&word, cwd, dirs_only), Vec::new())
                }
            },
        }
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
    if candidates.is_empty() && pending.is_empty() {
        None
    } else {
        Some(Completion {
            candidates,
            pending,
        })
    }
}

/// Finds the start of the shell word at `cursor`. A whitespace character
/// preceded by an odd-length run of backslashes belongs to the word, as in
/// `My\ Documents/` after a path candidate has been inserted.
fn shell_word_start(chars: &[char], cursor: usize) -> usize {
    let mut start = cursor.min(chars.len());
    while start > 0 {
        if !chars[start - 1].is_whitespace() {
            start -= 1;
            continue;
        }
        let escapes = chars[..start - 1]
            .iter()
            .rev()
            .take_while(|&&c| c == '\\')
            .count();
        if escapes % 2 == 0 {
            break;
        }
        start -= 1;
    }
    start
}

fn complete_command(word: &str, local: bool) -> Vec<WordCand> {
    if word.is_empty() {
        return Vec::new();
    }
    let mut set = BTreeSet::new();
    for b in BUILTINS {
        if b.starts_with(word) {
            set.insert((*b).to_string());
        }
    }
    if let Some(path) = std::env::var_os("PATH").filter(|_| local) {
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

fn sort_by_closeness(items: &mut [String]) {
    items.sort_by(|a, b| {
        a.chars()
            .count()
            .cmp(&b.chars().count())
            .then_with(|| a.cmp(b))
    });
}

fn sort_candidates_by_closeness(cands: &mut [Candidate]) {
    cands.sort_by(|a, b| {
        a.text
            .chars()
            .count()
            .cmp(&b.text.chars().count())
            .then_with(|| a.text.cmp(&b.text))
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePathRequest {
    pub dir: String,
    pub prefix: String,
    pub dir_part: String,
    pub word_start: usize,
    pub cursor: usize,
    pub dirs_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
}

pub fn remote_path_request(
    line: &str,
    cursor: usize,
    remote_cwd: &str,
) -> Option<RemotePathRequest> {
    if !remote_cwd.starts_with('/') {
        return None;
    }
    let chars: Vec<char> = line.chars().collect();
    let cursor = cursor.min(chars.len());
    let word_start = shell_word_start(&chars, cursor);
    let word: String = chars[word_start..cursor].iter().collect();

    let is_command = chars[..word_start].iter().all(|c| c.is_whitespace());
    if is_command && !word.contains('/') {
        return None;
    }
    if word.starts_with('~') {
        return None;
    }

    let word = unescape_path_word(&word);
    let (dir_part, prefix) = match word.rfind('/') {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word.as_str()),
    };
    let dir = if dir_part.starts_with('/') {
        dir_part.to_string()
    } else if dir_part.is_empty() {
        remote_cwd.to_string()
    } else {
        format!("{}/{dir_part}", remote_cwd.trim_end_matches('/'))
    };

    Some(RemotePathRequest {
        dir,
        prefix: prefix.to_string(),
        dir_part: dir_part.to_string(),
        word_start,
        cursor,
        dirs_only: current_command(&chars, word_start)
            .is_some_and(|c| DIR_ONLY_COMMANDS.contains(&c.as_str())),
    })
}

pub fn remote_path_candidates(req: &RemotePathRequest, entries: &[RemoteEntry]) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for entry in entries {
        let name = entry.name.as_str();
        if name == "." || name == ".." {
            continue;
        }
        if name.starts_with('.') && !req.prefix.starts_with('.') {
            continue;
        }
        if !name.starts_with(&req.prefix) {
            continue;
        }
        if req.dirs_only && !entry.is_dir {
            continue;
        }
        out.push(Candidate {
            text: format!("{}{name}", req.dir_part),
            kind: if entry.is_dir {
                CandidateKind::Dir
            } else {
                CandidateKind::File
            },
            start: req.word_start,
            end: req.cursor,
            description: None,
            icon: None,
        });
        if out.len() >= MAX_CANDIDATES {
            break;
        }
    }
    sort_candidates_by_closeness(&mut out);
    out
}

fn complete_path(word: &str, cwd: &Path, dirs_only: bool) -> Vec<WordCand> {
    // Candidates are emitted unescaped and escaped at insertion time. Undo the
    // corresponding backslash quoting for lookup, so a second Tab after
    // inserting `My\ Documents/` still enters the real directory.
    let word = unescape_path_word(word);
    let (dir_part, prefix) = match word.rfind(std::path::is_separator) {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word.as_str()),
    };
    let base = resolve_dir(dir_part, cwd);

    let Ok(rd) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut out: Vec<WordCand> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !prefix.starts_with('.') {
            continue;
        }
        if !name.starts_with(prefix) {
            continue;
        }
        let is_dir = entry
            .file_type()
            .is_ok_and(|t| t.is_dir() || (t.is_symlink() && entry.path().is_dir()));
        if dirs_only && !is_dir {
            continue;
        }
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

fn unescape_path_word(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut escaped = false;
    for ch in word.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    if escaped {
        out.push('\\');
    }
    out
}

struct SigResult {
    cands: Vec<WordCand>,
    pending: Vec<PendingGenerator>,
}

fn complete_signature(
    chars: &[char],
    word_start: usize,
    word: &str,
    cwd: Option<&Path>,
) -> Option<SigResult> {
    let prefix: String = chars[..word_start].iter().collect();
    let seg_start = prefix
        .rfind(['|', '&', ';', '\n', '('])
        .map(|i| i + 1)
        .unwrap_or(0);
    let tokens: Vec<&str> = prefix[seg_start..].split_whitespace().collect();
    let cmd = tokens.first()?;
    let sig = signature::signature(cmd)?;

    let (node, pending_value) = walk_signature(&sig, &tokens[1..]);

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
        return Some(SigResult {
            cands: finish(out),
            pending: Vec::new(),
        });
    }

    if let Some(arg) = pending_value {
        let mut out = Vec::new();
        push_arg_suggestions(&mut out, arg, word);
        if let Some(cwd) = cwd {
            if arg.wants_paths() {
                out.extend(complete_path(word, cwd, arg.wants_dirs_only()));
            }
        }
        let pending = match cwd {
            Some(_) => collect_generators(arg),
            None => Vec::new(),
        };
        if out.is_empty() && pending.is_empty() && arg.suggestions.is_empty() {
            return None;
        }
        return Some(SigResult {
            cands: out,
            pending,
        });
    }

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
    let mut pending = Vec::new();
    let mut claims_slot = false;
    if let Some(arg) = node.args().first() {
        push_arg_suggestions(&mut out, arg, word);
        if let Some(cwd) = cwd {
            if arg.wants_paths() {
                out.extend(complete_path(word, cwd, arg.wants_dirs_only()));
            }
        }
        if cwd.is_some() {
            pending = collect_generators(arg);
        }
        claims_slot = !arg.suggestions.is_empty() || !pending.is_empty();
    }
    if out.is_empty() && !claims_slot {
        None
    } else {
        Some(SigResult {
            cands: finish(out),
            pending,
        })
    }
}

fn collect_generators(arg: &Arg) -> Vec<PendingGenerator> {
    arg.generators
        .iter()
        .filter(|g| !g.script.is_empty())
        .map(|g| PendingGenerator {
            script: g.script.join(" "),
        })
        .collect()
}

fn walk_signature<'a>(sig: &'a Signature, rest: &[&str]) -> (&'a dyn CmdNode, Option<&'a Arg>) {
    let mut node: &dyn CmdNode = sig;
    let mut i = 0;
    while i < rest.len() {
        let tok = rest[i];
        if tok.starts_with('-') {
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
        i += 1;
    }

    let pending = rest.last().and_then(|last| {
        (last.starts_with('-') && !last.contains('='))
            .then(|| node.find_option(last))
            .flatten()
            .filter(|o| o.takes_arg())
            .and_then(|o| o.args.first())
    });
    (node, pending)
}

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

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

pub(super) struct CompletionSession {
    pub(super) word_start: usize,
    pub(super) open_word: String,
    pub(super) all: Vec<Candidate>,
    pub(super) filtered: Vec<usize>,
    pub(super) index: Option<usize>,
    pub(super) pending_generators: usize,
}

pub(super) struct Replacement {
    pub(super) orig: String,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) text: String,
}

impl Replacement {
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
    pub(super) fn new(
        word_start: usize,
        open_word: String,
        all: Vec<Candidate>,
        pending_generators: usize,
    ) -> Self {
        let filtered = (0..all.len()).collect();
        Self {
            word_start,
            open_word,
            all,
            filtered,
            index: Some(0),
            pending_generators,
        }
    }

    pub(super) fn generator_answered(&mut self) {
        self.pending_generators = self.pending_generators.saturating_sub(1);
    }

    pub(super) fn is_spent(&self) -> bool {
        self.pending_generators == 0 && self.filtered.is_empty()
    }

    pub(super) fn selected(&self) -> Option<&Candidate> {
        self.index
            .and_then(|i| self.filtered.get(i))
            .map(|&i| &self.all[i])
    }

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

    pub(super) fn merge(&mut self, new: Vec<Candidate>, live_word: &str) {
        let selected_text = self
            .index
            .and_then(|i| self.filtered.get(i))
            .map(|&i| self.all[i].text.clone());

        let mut seen: BTreeSet<String> = self.all.iter().map(|c| c.text.clone()).collect();
        let fresh: Vec<Candidate> = new
            .into_iter()
            .filter(|c| seen.insert(c.text.clone()))
            .collect();
        self.all.extend(fresh);
        sort_candidates_by_closeness(&mut self.all);

        self.filtered = (0..self.all.len())
            .filter(|&i| self.all[i].text.starts_with(live_word))
            .collect();
        self.index = if self.filtered.is_empty() {
            None
        } else {
            let kept = selected_text
                .and_then(|t| self.filtered.iter().position(|&i| self.all[i].text == t));
            Some(kept.unwrap_or(0))
        };
    }

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

    fn texts(line: &str) -> Vec<String> {
        complete(line, line.chars().count(), Some(Path::new("/")))
            .map(|c| c.candidates.into_iter().map(|c| c.text).collect())
            .unwrap_or_default()
    }

    #[test]
    fn signature_offers_subcommands_after_command() {
        let t = texts("git ");
        assert!(t.iter().any(|s| s == "commit"), "git subcommands: {t:?}");
        assert!(t.iter().any(|s| s == "status"));
        let c = complete("git ", 4, Some(Path::new("/"))).unwrap();
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
        let c = complete("git commit --", 13, Some(Path::new("/"))).unwrap();
        let msg = c.candidates.iter().find(|c| c.text == "--message").unwrap();
        assert_eq!(msg.kind, CandidateKind::Flag);
        assert_eq!(
            msg.description.as_deref(),
            Some("Use the given message as the commit message")
        );
    }

    #[test]
    fn signature_resolves_nested_subcommands() {
        let t = texts("docker compose ");
        assert!(
            t.iter().any(|s| s == "up"),
            "docker compose subcommands: {t:?}"
        );
    }

    #[test]
    fn generator_arg_pends_scripts_and_suppresses_path_fallback() {
        let dir = temp_tree("gen-checkout", &[("sentinel.txt", false), ("subdir", true)]);
        let line = "git checkout ";
        let c = complete(line, line.chars().count(), Some(dir.as_path()))
            .expect("generator slot is a completion");
        assert!(
            !c.pending.is_empty(),
            "the branch/tag generators ride along as pending scripts"
        );
        assert!(
            c.pending
                .iter()
                .any(|p| p.script.contains("branch") && p.script.starts_with("git ")),
            "one pending script is the joined git-branch listing: {:?}",
            c.pending
        );
        assert!(
            c.candidates
                .iter()
                .all(|cand| cand.text != "sentinel.txt" && cand.text != "subdir"),
            "no path fallback: {:?}",
            c.candidates
        );
    }

    #[test]
    fn generator_script_tokens_join_with_single_spaces() {
        let c = complete("git checkout ", 13, Some(Path::new("/"))).unwrap();
        let branch = c
            .pending
            .iter()
            .find(|p| p.script.contains("branch"))
            .unwrap();
        assert_eq!(
            branch.script,
            "git --no-optional-locks branch -a --no-color --sort=-committerdate"
        );
    }

    #[test]
    fn merge_dedupes_resorts_and_refilters_to_live_word() {
        let mut s = CompletionSession::new(
            0,
            "f".into(),
            vec![cand("feature", CandidateKind::Value, 0, 1)],
            0,
        );
        let new = vec![
            cand("feature", CandidateKind::Value, 0, 1),
            cand("fix", CandidateKind::Value, 0, 1),
            cand("main", CandidateKind::Value, 0, 1),
        ];
        s.merge(new, "f");
        let texts: Vec<&str> = s.filtered.iter().map(|&i| s.all[i].text.as_str()).collect();
        assert_eq!(texts, vec!["fix", "feature"]);
        assert_eq!(s.selected().unwrap().text, "feature");
    }

    #[test]
    fn merge_preserves_a_surviving_highlight() {
        let mut s = CompletionSession::new(
            0,
            "b".into(),
            vec![
                cand("branch-a", CandidateKind::Value, 0, 1),
                cand("branch-b", CandidateKind::Value, 0, 1),
            ],
            0,
        );
        s.select(true);
        assert_eq!(s.selected().unwrap().text, "branch-b");
        s.merge(vec![cand("bugfix", CandidateKind::Value, 0, 1)], "b");
        assert_eq!(s.selected().unwrap().text, "branch-b");
    }

    #[test]
    fn dir_only_commands_complete_only_directories() {
        let dir = temp_tree("dironly", &[("target", true), ("tar.gz", false)]);
        let only_dirs = |line: &str| {
            complete(line, line.chars().count(), Some(dir.as_path()))
                .map(|c| c.candidates.into_iter().map(|c| c.text).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        assert_eq!(only_dirs("cd tar"), vec!["target"]);
        assert_eq!(only_dirs("pushd tar"), vec!["target"]);
        assert_eq!(only_dirs("/bin/rmdir tar"), vec!["target"]);
        assert_eq!(only_dirs("foo | cd tar"), vec!["target"]);
        assert_eq!(only_dirs("cd "), vec!["target"]);
        let both = only_dirs("frobnicate tar");
        assert!(both.contains(&"tar.gz".to_string()), "{both:?}");
        assert!(both.contains(&"target".to_string()), "{both:?}");
    }

    #[test]
    fn unknown_command_falls_back_to_paths() {
        let dir = temp_tree("fallback", &[("readme.md", false)]);
        let c = complete(
            "frobnicate read",
            "frobnicate read".chars().count(),
            Some(dir.as_path()),
        )
        .unwrap();
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
        CompletionSession::new(0, "a".into(), cands, 0)
    }

    #[test]
    fn select_moves_the_highlight_and_wraps_without_touching_candidates() {
        let mut s = session(&["aa", "ab", "ac"]);
        assert_eq!(s.index, Some(0));
        s.select(true);
        assert_eq!(s.index, Some(1));
        s.select(true);
        s.select(true);
        assert_eq!(s.index, Some(0));
        s.select(false);
        assert_eq!(s.index, Some(2));
        assert_eq!(s.selected().unwrap().text, "ac");
    }

    #[test]
    fn refilter_narrows_keeps_surviving_highlight_and_closes_when_stale() {
        let mut s = session(&["aa", "ab", "abc"]);
        s.select(true);
        assert!(s.refilter("ab"));
        assert_eq!(s.filtered.len(), 2);
        assert_eq!(s.selected().unwrap().text, "ab");
        assert!(!s.refilter(""));
        let mut s = session(&["aa", "ab"]);
        assert!(!s.refilter("az"));
    }

    #[test]
    fn refilter_falls_back_to_the_top_when_the_highlight_is_filtered_away() {
        let mut s = session(&["aa", "ab", "abc"]);
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
                let path = dir.join(name);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, b"").unwrap();
            }
        }
        dir
    }

    #[test]
    fn command_position_offers_builtins_with_word_range() {
        let c = complete("ech", 3, Some(Path::new("/"))).unwrap();
        let echo = c.candidates.iter().find(|c| c.text == "echo").unwrap();
        assert_eq!(echo.kind, CandidateKind::Command);
        assert_eq!((echo.start, echo.end), (0, 3));
    }

    #[test]
    fn path_completion_matches_prefix_and_flags_dirs() {
        let dir = temp_tree(
            "paths",
            &[("apple.txt", false), ("apply.sh", false), ("assets", true)],
        );
        let line = "cat a";
        let c = complete(line, line.chars().count(), Some(dir.as_path())).unwrap();
        let names: Vec<&str> = c.candidates.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(names, vec!["assets", "apply.sh", "apple.txt"]);
        let assets = c.candidates.iter().find(|c| c.text == "assets").unwrap();
        assert!(assets.is_dir());
        assert_eq!((assets.start, assets.end), (4, 5));
    }

    #[test]
    fn completion_reenters_a_directory_inserted_with_an_escaped_space() {
        let dir = temp_tree("escaped-path", &[("My Documents/notes.txt", false)]);
        let line = r"cat My\ Documents/no";
        let c = complete(line, line.chars().count(), Some(dir.as_path())).unwrap();
        assert_eq!(c.candidates[0].text, "My Documents/notes.txt");
        assert_eq!(
            (c.candidates[0].start, c.candidates[0].end),
            (4, line.chars().count())
        );
    }

    #[test]
    fn path_completion_keeps_dir_prefix_in_candidate() {
        let dir = temp_tree("nested", &[("sub", true)]);
        std::fs::write(dir.join("sub/file.rs"), b"").unwrap();
        let line = "cat sub/f";
        let c = complete(line, line.chars().count(), Some(dir.as_path())).unwrap();
        assert_eq!(c.candidates[0].text, "sub/file.rs");
        assert_eq!(c.candidates[0].start, 4);
    }

    #[test]
    fn hidden_files_only_with_dot_prefix() {
        let dir = temp_tree("hidden", &[(".secret", false), ("visible", false)]);
        let c = complete("ls v", 4, Some(dir.as_path())).unwrap();
        assert!(c.candidates.iter().all(|c| !c.text.starts_with('.')));
        let c = complete("ls .", 4, Some(dir.as_path())).unwrap();
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
        let c = complete(line, line.chars().count(), Some(dir.as_path())).unwrap();
        let names: Vec<&str> = c.candidates.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(names, vec!["xa", "xy", "xyz", "xyzzy"]);
    }

    #[test]
    fn a_remote_pane_completes_commands_but_never_local_paths() {
        let dir = temp_tree("remote", &[("only-here.txt", false), ("subdir", true)]);

        let c = complete("cat only", 8, Some(dir.as_path())).expect("local pane completes paths");
        assert!(c.candidates.iter().any(|c| c.text.starts_with("only-here")));

        assert!(complete("cat only", 8, None).is_none());
        assert!(complete("cat ", 4, None).is_none());

        let c = complete("ech", 3, None).expect("command completion needs no cwd");
        assert!(c.candidates.iter().any(|c| c.text == "echo"));
    }

    #[test]
    fn a_remote_pane_offers_builtins_but_never_this_machines_binaries() {
        let remote: Vec<String> = complete("l", 1, None)
            .map(|c| c.candidates.into_iter().map(|c| c.text).collect())
            .unwrap_or_default();
        assert!(
            remote.iter().all(|n| BUILTINS.contains(&n.as_str())),
            "a builtin is true on any POSIX shell, but a PATH scan reads *this* machine \
             and its names do not exist on the remote: {remote:?}"
        );

        #[cfg(unix)]
        {
            let local: Vec<String> = complete("l", 1, Some(Path::new("/")))
                .map(|c| c.candidates.into_iter().map(|c| c.text).collect())
                .unwrap_or_default();
            assert!(
                local.iter().any(|n| n == "ls"),
                "a local pane still scans PATH: {local:?}"
            );
            assert!(!remote.iter().any(|n| n == "ls"));
        }
    }

    #[test]
    fn a_remote_pane_still_gets_a_signatures_static_candidates() {
        let c = complete("git ", 4, None).expect("subcommands need no filesystem");
        assert!(c.candidates.iter().any(|c| c.text == "commit"));
        assert!(c.candidates.iter().any(|c| c.text == "push"));

        let c = complete("git ch", 6, None).expect("subcommands need no filesystem");
        assert!(c.candidates.iter().any(|c| c.text == "checkout"));
        assert!(!c.candidates.iter().any(|c| c.text == "commit"));

        let c = complete("git commit --", 13, None).expect("flags need no filesystem");
        assert!(c.candidates.iter().any(|c| c.text == "--message"));
    }

    #[test]
    fn a_remote_pane_never_runs_a_generator() {
        let local = complete("git checkout ", 13, Some(Path::new("/"))).unwrap();
        assert!(
            !local.pending.is_empty(),
            "expected the local branch generator to still be declared"
        );

        if let Some(remote) = complete("git checkout ", 13, None) {
            assert!(
                remote.pending.is_empty(),
                "a remote pane scheduled local generators: {:?}",
                remote.pending
            );
        }
    }

    #[test]
    fn remote_path_request_splits_the_word_and_resolves_against_the_remote_cwd() {
        let r = remote_path_request("cat fi", 6, "/home/me").unwrap();
        assert_eq!(
            (r.dir.as_str(), r.prefix.as_str(), r.dir_part.as_str()),
            ("/home/me", "fi", "")
        );
        assert_eq!((r.word_start, r.cursor), (4, 6));
        assert!(!r.dirs_only);

        let r = remote_path_request("cat sub/fi", 10, "/home/me").unwrap();
        assert_eq!(r.dir, "/home/me/sub/");
        assert_eq!((r.prefix.as_str(), r.dir_part.as_str()), ("fi", "sub/"));

        let r = remote_path_request("cat /etc/pa", 11, "/home/me").unwrap();
        assert_eq!(r.dir, "/etc/");
        assert_eq!(r.prefix, "pa");

        let r = remote_path_request("cat sub/", 8, "/").unwrap();
        assert_eq!(r.dir, "/sub/");

        assert!(
            remote_path_request("cd pro", 6, "/home/me")
                .unwrap()
                .dirs_only
        );

        let r = remote_path_request(r"cat a\b", 7, "/home/me").unwrap();
        assert_eq!((r.dir.as_str(), r.prefix.as_str()), ("/home/me", "ab"));
    }

    #[test]
    fn remote_path_request_declines_where_a_listing_cannot_help() {
        assert!(remote_path_request("ls", 2, "/home/me").is_none());
        assert!(remote_path_request("", 0, "/home/me").is_none());
        assert!(remote_path_request("./scr", 5, "/home/me").is_some());

        assert!(remote_path_request("cat ~/pro", 9, "/home/me").is_none());

        assert!(remote_path_request("cat fi", 6, "").is_none());
        assert!(remote_path_request("cat fi", 6, "relative/dir").is_none());
    }

    #[test]
    fn remote_path_candidates_mirror_the_local_path_rules() {
        let entries = |names: &[(&str, bool)]| -> Vec<RemoteEntry> {
            names
                .iter()
                .map(|(n, d)| RemoteEntry {
                    name: (*n).to_string(),
                    is_dir: *d,
                })
                .collect()
        };
        let all = entries(&[
            (".", true),
            ("..", true),
            (".hidden", false),
            ("src", true),
            ("setup.py", false),
            ("s", false),
            ("other", false),
        ]);
        let texts =
            |cands: Vec<Candidate>| -> Vec<String> { cands.into_iter().map(|c| c.text).collect() };

        let req = remote_path_request("cat s", 5, "/home/me").unwrap();
        let got = texts(remote_path_candidates(&req, &all));
        assert_eq!(
            got,
            vec!["s", "src", "setup.py"],
            "prefix-matched, shortest first; `.`/`..`/hidden/non-matching dropped"
        );

        let cands = remote_path_candidates(&req, &all);
        assert!(cands.iter().find(|c| c.text == "src").unwrap().is_dir());
        assert!(!cands.iter().find(|c| c.text == "s").unwrap().is_dir());

        let req = remote_path_request("cat sub/s", 9, "/home/me").unwrap();
        let c = &remote_path_candidates(&req, &all)[0];
        assert_eq!(c.text, "sub/s");
        assert_eq!((c.start, c.end), (4, 9));

        let req = remote_path_request("cat .h", 6, "/home/me").unwrap();
        let got = texts(remote_path_candidates(&req, &all));
        assert_eq!(got, vec![".hidden"]);

        let req = remote_path_request("cd s", 4, "/home/me").unwrap();
        let got = texts(remote_path_candidates(&req, &all));
        assert_eq!(got, vec!["src"]);
    }

    #[test]
    fn no_candidates_returns_none() {
        let dir = temp_tree("empty", &[("zzz", false)]);
        assert!(complete("cat q", 5, Some(dir.as_path())).is_none());
        assert!(complete("", 0, Some(dir.as_path())).is_none());
        assert!(complete("   ", 3, Some(dir.as_path())).is_none());
    }

    #[test]
    fn mid_line_cursor_completes_only_the_word_before_it() {
        let dir = temp_tree("midline", &[("apple.txt", false)]);
        let c = complete("cat ap x.log", 6, Some(dir.as_path())).unwrap();
        let apple = c.candidates.iter().find(|c| c.text == "apple.txt").unwrap();
        assert_eq!((apple.start, apple.end), (4, 6));
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
        assert_eq!(items, vec!["xa", "xb", "xyz", "xyzzy"]);
    }

    #[test]
    fn resolve_dir_handles_empty_absolute_and_relative() {
        let cwd = Path::new("/work/proj");
        assert_eq!(resolve_dir("", cwd), PathBuf::from("/work/proj"));
        assert_eq!(resolve_dir("/etc/", cwd), PathBuf::from("/etc/"));
        assert_eq!(resolve_dir("src/", cwd), PathBuf::from("/work/proj/src/"));
    }

    #[test]
    fn resolve_dir_expands_tilde_to_home() {
        if let Some(home) = home_dir() {
            let cwd = Path::new("/work");
            assert_eq!(resolve_dir("~", cwd), home);
            assert_eq!(resolve_dir("~/", cwd), home.clone());
            assert_eq!(resolve_dir("~/dev/", cwd), home.join("dev/"));
        }
    }
}

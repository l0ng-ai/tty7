use crate::core::config::config_path;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_ENTRIES: usize = 5000;

const FREQ_WEIGHT: f64 = 0.6;

const CWD_BONUS: f64 = 1.2;

struct Raw {
    cmd: String,
    cwd: Option<String>,
    ts: Option<u64>,
    exit: Option<i32>,
}

impl Raw {
    fn bare(cmd: String) -> Self {
        Self {
            cmd,
            cwd: None,
            ts: None,
            exit: None,
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct EntryMeta {
    pub ts: Option<u64>,
    pub exit: Option<i32>,
}

pub struct History {
    pub entries: Vec<String>,
    pub counts: HashMap<String, u32>,
    pub cwds: HashMap<String, HashSet<String>>,
    pub meta: HashMap<String, EntryMeta>,
}

#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub enum Scope {
    #[default]
    Local,
    Remote(String),
}

impl Scope {
    pub fn remote(label: &str) -> Scope {
        let label = label.trim();
        if label.is_empty() {
            Scope::Local
        } else {
            Scope::Remote(label.to_string())
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, Scope::Local)
    }

    fn file(&self) -> Option<PathBuf> {
        match self {
            Scope::Local => config_path("history"),
            Scope::Remote(label) => config_path("history.d").map(|d| d.join(file_stem(label))),
        }
    }
}

fn file_stem(label: &str) -> String {
    let mut safe: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    safe.truncate(48);
    format!("{safe}-{:016x}", tty7_core::host::fnv1a64(label.as_bytes()))
}

pub fn load(scope: &Scope) -> History {
    load_with_shell_files(scope, Vec::new())
}

/// `shell_files` are the far end's own history files, each paired with the file
/// name it was read from — the name is what picks the reader, so fetching a
/// file the bash reader cannot parse is not enough to corrupt the list.
pub fn load_with_shell_files(scope: &Scope, shell_files: Vec<(String, Vec<u8>)>) -> History {
    let mut raw: Vec<Raw> = if scope.is_local() {
        load_shell_history()
    } else {
        let mut out = Vec::new();
        for (name, bytes) in &shell_files {
            parse_history_file(name, &String::from_utf8_lossy(bytes), &mut out);
        }
        out
    };
    if let Some(path) = scope.file()
        && let Ok(content) = std::fs::read_to_string(&path)
    {
        raw.extend(content.lines().map(parse_own_line));
    }
    normalize(raw)
}

const FISH_HISTORY_FILE: &str = "fish_history";

/// fish's history relative to the XDG data dir, and relative to `$HOME` when
/// that dir is the default `~/.local/share`. `shell_history_names` returns the
/// second, so a test pins the two spellings to each other.
const FISH_HISTORY_UNDER_DATA_DIR: &str = "fish/fish_history";
const FISH_HISTORY_UNDER_HOME: &str = ".local/share/fish/fish_history";

/// History files worth fetching from a remote host, relative to its home dir.
/// `load_with_shell_files` reads the file name back off these to pick a parser,
/// so a name added here needs a reader in `parse_history_file`.
pub fn shell_history_names() -> [&'static str; 3] {
    [".zsh_history", ".bash_history", FISH_HISTORY_UNDER_HOME]
}

fn looks_absolute(p: &str) -> bool {
    match p.as_bytes() {
        [b'/' | b'\\', ..] => true,
        [d, b':', ..] => d.is_ascii_alphabetic(),
        _ => false,
    }
}

fn parse_own_line(line: &str) -> Raw {
    let mut f = line.splitn(4, '\t');
    if let (Some(ts), Some(exit), Some(cwd), Some(cmd)) = (f.next(), f.next(), f.next(), f.next())
        && !ts.is_empty()
        && ts.bytes().all(|b| b.is_ascii_digit())
        && (exit.is_empty() || exit.parse::<i32>().is_ok())
        && (cwd.is_empty() || looks_absolute(cwd))
    {
        return Raw {
            cmd: cmd.to_string(),
            cwd: (!cwd.is_empty()).then(|| cwd.to_string()),
            ts: ts.parse().ok(),
            exit: exit.parse().ok(),
        };
    }
    if let Some((cwd, cmd)) = line.split_once('\t')
        && looks_absolute(cwd)
    {
        return Raw {
            cmd: cmd.to_string(),
            cwd: Some(cwd.to_string()),
            ts: None,
            exit: None,
        };
    }
    Raw::bare(line.to_string())
}

pub fn frecency_scores(
    entries: &[String],
    counts: &HashMap<String, u32>,
    cwds: &HashMap<String, HashSet<String>>,
    cwd: Option<&str>,
) -> Vec<f64> {
    let n = entries.len();
    entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let recency = if n <= 1 {
                1.0
            } else {
                i as f64 / (n - 1) as f64
            };
            let count = f64::from(*counts.get(e).unwrap_or(&1));
            let mut score = recency + FREQ_WEIGHT * (1.0 + count).ln();
            if let Some(cwd) = cwd
                && cwds.get(e).is_some_and(|dirs| dirs.contains(cwd))
            {
                score += CWD_BONUS;
            }
            score
        })
        .collect()
}

pub fn rank_by_frecency(
    entries: &[String],
    counts: &HashMap<String, u32>,
    cwds: &HashMap<String, HashSet<String>>,
    cwd: Option<&str>,
) -> Vec<String> {
    let scores = frecency_scores(entries, counts, cwds, cwd);
    let mut idx: Vec<usize> = (0..entries.len()).collect();
    idx.sort_by(|&a, &b| {
        scores[b]
            .partial_cmp(&scores[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.cmp(&a))
    });
    idx.into_iter().map(|i| entries[i].clone()).collect()
}

pub fn format_ago(now: u64, ts: u64) -> String {
    let s = now.saturating_sub(ts);
    let (n, unit) = if s < 60 {
        return "now".to_string();
    } else if s < 3600 {
        (s / 60, "m")
    } else if s < 86_400 {
        (s / 3600, "h")
    } else if s < 7 * 86_400 {
        (s / 86_400, "d")
    } else if s < 30 * 86_400 {
        (s / (7 * 86_400), "w")
    } else if s < 365 * 86_400 {
        (s / (30 * 86_400), "mo")
    } else {
        (s / (365 * 86_400), "y")
    };
    format!("{n}{unit}")
}

pub fn append(scope: &Scope, cmd: &str, cwd: Option<&Path>, ts: u64, exit: Option<i32>) {
    if cmd.contains('\n') {
        return;
    }
    let Some(path) = scope.file() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let cwd = match cwd.and_then(Path::to_str) {
        Some(c) if looks_absolute(c) && !c.contains(['\t', '\n', '\r']) => c,
        _ => "",
    };
    let exit = exit.map(|e| e.to_string()).unwrap_or_default();
    let line = format!("{ts}\t{exit}\t{cwd}\t{cmd}");
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(format!("{line}\n").as_bytes());
    }
}

fn normalize(raw: Vec<Raw>) -> History {
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut cwds: HashMap<String, HashSet<String>> = HashMap::new();
    let mut meta: HashMap<String, EntryMeta> = HashMap::new();
    let mut seen = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for r in raw.into_iter().rev() {
        let line = r.cmd.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        *counts.entry(line.to_string()).or_insert(0) += 1;
        if let Some(cwd) = r.cwd {
            cwds.entry(line.to_string()).or_default().insert(cwd);
        }
        if (r.ts.is_some() || r.exit.is_some()) && !meta.contains_key(line) {
            meta.insert(
                line.to_string(),
                EntryMeta {
                    ts: r.ts,
                    exit: r.exit,
                },
            );
        }
        if seen.insert(line.to_string()) {
            out.push(line.to_string());
        }
    }
    out.reverse();
    if out.len() > MAX_ENTRIES {
        let cut = out.len() - MAX_ENTRIES;
        for r in out.drain(0..cut) {
            counts.remove(&r);
            cwds.remove(&r);
            meta.remove(&r);
        }
    }
    History {
        entries: out,
        counts,
        cwds,
        meta,
    }
}

fn load_shell_history() -> Vec<Raw> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |p: PathBuf| {
        if p.is_file() && seen.insert(p.clone()) {
            files.push(p);
        }
    };
    if let Some(hf) = std::env::var_os("HISTFILE") {
        add(PathBuf::from(hf));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        add(home.join(".zsh_history"));
        add(home.join(".bash_history"));
    }
    // fish keeps its history under the XDG data dir, which is set independently
    // of HOME, so ~/.local/share is only the fallback.
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        add(PathBuf::from(xdg).join(FISH_HISTORY_UNDER_DATA_DIR));
    } else if let Some(home) = std::env::var_os("HOME") {
        add(PathBuf::from(home).join(FISH_HISTORY_UNDER_HOME));
    }
    files.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    });

    let mut out = Vec::new();
    for path in files {
        if let Ok(bytes) = std::fs::read(&path) {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            parse_history_file(&name, &String::from_utf8_lossy(&bytes), &mut out);
        }
    }
    out
}

/// Which reader a history file gets, by file name. The remote side has only
/// bytes and a name, so the choice has to hang off the name in both paths.
fn parse_history_file(name: &str, content: &str, out: &mut Vec<Raw>) {
    if name == FISH_HISTORY_FILE {
        parse_fish_history(content, out);
    } else {
        parse_shell_history(content, out);
    }
}

/// `fish_history` looks like YAML and is not.
///
/// fish writes a command with exactly two characters escaped — a literal
/// backslash becomes `\\` and a newline becomes `\n` — and nothing else. It
/// does not quote, so `git commit -m "fix: crash"` goes to disk verbatim, and
/// a YAML parser rejects that record outright ("mapping values are not allowed
/// here"). `echo a: b`, any `[ ... ]` test, and any command with a ` #` in it
/// fail or truncate the same way, silently, because a record that fails to
/// parse is a record that vanishes from history search.
///
/// So read it the way fish's own reader does: a record starts at `- cmd:` in
/// column 0, its keys are indented under it, and everything after `- cmd:` on
/// that line is the command.
fn parse_fish_history(content: &str, out: &mut Vec<Raw>) {
    let mut pending: Option<Raw> = None;
    for raw in content.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(cmd) = line.strip_prefix("- cmd:") {
            out.extend(pending.take());
            let cmd = unescape_fish(cmd.trim());
            // Everything downstream treats an entry as one line: `append`
            // refuses embedded newlines, handing a line off to the shell bails
            // on them, and the reverse-search menu draws one row per entry. A
            // multiline command is skipped rather than half-supported — the
            // same place bash and zsh multiline entries already land.
            if !cmd.is_empty() && !cmd.contains('\n') {
                pending = Some(Raw::bare(cmd));
            }
            continue;
        }
        if pending.is_none() {
            continue;
        }
        // Keys are indented under their record; anything else ends it.
        let Some(key) = line.strip_prefix("  ") else {
            out.extend(pending.take());
            continue;
        };
        if let Some(when) = key.strip_prefix("when:")
            && let Ok(ts) = when.trim().trim_matches('\'').parse::<u64>()
            && let Some(entry) = pending.as_mut()
        {
            entry.ts = Some(ts);
        }
    }
    out.extend(pending);
}

/// Reverse `escape_yaml_fish_2_0`: `\\` is a backslash and `\n` is a newline.
/// A backslash before anything else is not an escape and stays as written.
fn unescape_fish(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let mut tail = rest[at..].chars();
        tail.next();
        match tail.next() {
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            _ => {
                out.push('\\');
                rest = &rest[at + 1..];
                continue;
            }
        }
        rest = &rest[at + 2..];
    }
    out.push_str(rest);
    out
}

fn parse_shell_history(content: &str, out: &mut Vec<Raw>) {
    let mut pending_ts: Option<u64> = None;
    for raw in content.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(ts) = bash_timestamp(line) {
            pending_ts = Some(ts);
            continue;
        }
        if let Some((cmd, zsh_ts)) = start_of_command(line) {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
                out.push(Raw {
                    cmd: cmd.to_string(),
                    cwd: None,
                    ts: zsh_ts.or(pending_ts),
                    exit: None,
                });
            }
        }
        pending_ts = None;
    }
}

fn bash_timestamp(line: &str) -> Option<u64> {
    let rest = line.strip_prefix('#')?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

fn start_of_command(line: &str) -> Option<(&str, Option<u64>)> {
    if line.is_empty() {
        return None;
    }
    if let Some(rest) = line.strip_prefix(": ")
        && let Some(semi) = rest.find(';')
    {
        let ts = &rest[..semi];
        if ts.bytes().any(|b| b.is_ascii_digit())
            && ts.bytes().all(|b| b.is_ascii_digit() || b == b':')
        {
            let start = ts.split(':').next().and_then(|t| t.parse().ok());
            return Some((&rest[semi + 1..], start));
        }
    }
    Some((line, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> Vec<String> {
        let mut out = Vec::new();
        parse_shell_history(content, &mut out);
        out.into_iter().map(|r| r.cmd).collect()
    }

    fn parse_ts(content: &str) -> Vec<(String, Option<u64>)> {
        let mut out = Vec::new();
        parse_shell_history(content, &mut out);
        out.into_iter().map(|r| (r.cmd, r.ts)).collect()
    }

    fn parse_fish(content: &str) -> Vec<(String, Option<u64>)> {
        let mut out = Vec::new();
        parse_fish_history(content, &mut out);
        out.into_iter().map(|r| (r.cmd, r.ts)).collect()
    }

    #[test]
    fn fish_history_entries_carry_cmd_and_timestamp() {
        let content =
            "- cmd: git status\n  when: 1700000000\n- cmd: cargo build\n  when: 1700000005\n";
        assert_eq!(
            parse_fish(content),
            [
                ("git status".to_string(), Some(1_700_000_000)),
                ("cargo build".to_string(), Some(1_700_000_005)),
            ]
        );
    }

    #[test]
    fn fish_writes_commands_unquoted_so_yaml_punctuation_is_just_text() {
        // Every one of these is what fish actually puts on disk, and every one
        // of them is a YAML parse error. Read as YAML they vanish from history
        // search without a trace; read as fish reads them they are commands.
        let content = concat!(
            "- cmd: git commit -m \"fix: crash on start\"\n  when: 1700000000\n",
            "- cmd: echo foo: bar\n  when: 1700000001\n",
            "- cmd: [ -f x ]; and echo y\n  when: 1700000002\n",
            "- cmd: rg --files # every file\n  when: 1700000003\n",
            "- cmd: this: is: not: broken\n  when: 1700000004\n",
        );
        assert_eq!(
            parse_fish(content),
            [
                (
                    "git commit -m \"fix: crash on start\"".to_string(),
                    Some(1_700_000_000)
                ),
                ("echo foo: bar".to_string(), Some(1_700_000_001)),
                ("[ -f x ]; and echo y".to_string(), Some(1_700_000_002)),
                // A YAML reader drops everything from ` #` on; fish has no
                // comments in its history file.
                ("rg --files # every file".to_string(), Some(1_700_000_003)),
                ("this: is: not: broken".to_string(), Some(1_700_000_004)),
            ]
        );
    }

    #[test]
    fn fish_escapes_only_backslash_and_newline() {
        // `escape_yaml_fish_2_0` doubles a backslash and turns a newline into
        // `\n`. Leaving that undone hands back a command the user never ran.
        assert_eq!(
            parse_fish("- cmd: grep \"a\\\\b\" f\n  when: 1700000000\n"),
            [("grep \"a\\b\" f".to_string(), Some(1_700_000_000))]
        );
        // `\t` is not an escape fish writes, so it stays two characters.
        assert_eq!(
            parse_fish("- cmd: printf 'a\\tb'\n"),
            [("printf 'a\\tb'".to_string(), None)]
        );
    }

    #[test]
    fn a_multiline_fish_command_is_skipped_not_half_recalled() {
        // fish stores this as one line with an escaped newline. Every consumer
        // here is single-line — `append` refuses embedded newlines — so the
        // entry is dropped rather than offered as something that cannot be run.
        let content = "- cmd: for f in *\\necho $f\\nend\n  when: 1700000000\n- cmd: ls\n  when: 1700000001\n";
        assert_eq!(
            parse_fish(content),
            [("ls".to_string(), Some(1_700_000_001))]
        );
    }

    #[test]
    fn fish_history_missing_or_quoted_when_is_tolerated() {
        let content = "- cmd: ls\n- cmd: pwd\n  when: '1700000000'\n";
        assert_eq!(
            parse_fish(content),
            [
                ("ls".to_string(), None),
                ("pwd".to_string(), Some(1_700_000_000)),
            ]
        );
    }

    #[test]
    fn fish_paths_and_stray_lines_never_become_commands() {
        // fish 3.x writes a `paths:` block under a record; nothing in it is a
        // command, and a truncated tail must not resurrect the record either.
        let content = concat!(
            "- cmd: vim src/main.rs\n",
            "  when: 1700000000\n",
            "  paths:\n",
            "    - src/main.rs\n",
            "\n",
            "  when: 9999999999\n",
        );
        assert_eq!(
            parse_fish(content),
            [("vim src/main.rs".to_string(), Some(1_700_000_000))]
        );
    }

    #[test]
    fn fish_history_garbage_is_skipped() {
        assert_eq!(parse_fish("not a record at all\n- cmd:\n"), []);
        assert_eq!(parse_fish(""), []);
    }

    #[test]
    fn the_fish_history_path_is_spelled_the_same_way_everywhere() {
        assert_eq!(
            FISH_HISTORY_UNDER_HOME,
            format!(".local/share/{FISH_HISTORY_UNDER_DATA_DIR}"),
            "the XDG-relative and HOME-relative spellings have drifted apart"
        );
        assert!(FISH_HISTORY_UNDER_HOME.ends_with(FISH_HISTORY_FILE));
        assert!(
            shell_history_names().contains(&FISH_HISTORY_UNDER_HOME),
            "a remote host must be asked for the same file the local side reads"
        );
    }

    #[test]
    fn plain_bash_lines() {
        assert_eq!(
            parse("ls\ncd /tmp\ngit status\n"),
            ["ls", "cd /tmp", "git status"]
        );
    }

    #[test]
    fn zsh_extended_prefix_is_stripped_and_timestamp_kept() {
        let content = ": 1700000000:0;git status\n: 1700000005:2;cargo build\n";
        assert_eq!(
            parse_ts(content),
            [
                ("git status".to_string(), Some(1_700_000_000)),
                ("cargo build".to_string(), Some(1_700_000_005)),
            ]
        );
    }

    #[test]
    fn bash_timestamp_comments_stamp_the_next_command() {
        let content = "#1700000000\nls -la\n#1700000005\ncd ..\nuntimed\n";
        assert_eq!(
            parse_ts(content),
            [
                ("ls -la".to_string(), Some(1_700_000_000)),
                ("cd ..".to_string(), Some(1_700_000_005)),
                ("untimed".to_string(), None),
            ]
        );
    }

    #[test]
    fn multiline_commands_are_split_not_joined() {
        let content = ": 1700000000:0;for f in *; do\\\necho $f\\\ndone\n";
        let got = parse(content);
        assert_eq!(got, ["for f in *; do\\", "echo $f\\", "done"]);
        assert!(got.iter().all(|e| !e.contains('\n')));
    }

    fn pair(cmd: &str, cwd: Option<&str>) -> Raw {
        Raw {
            cmd: cmd.to_string(),
            cwd: cwd.map(str::to_string),
            ts: None,
            exit: None,
        }
    }

    #[test]
    fn parse_own_line_reads_all_generations() {
        let r = parse_own_line("1700000000\t0\t/home/me\tgit status");
        assert_eq!(r.cmd, "git status");
        assert_eq!(r.cwd.as_deref(), Some("/home/me"));
        assert_eq!(r.ts, Some(1_700_000_000));
        assert_eq!(r.exit, Some(0));
        let r = parse_own_line("1700000000\t\t\tmake");
        assert_eq!(
            (r.cmd.as_str(), r.cwd, r.ts, r.exit),
            ("make", None, Some(1_700_000_000), None)
        );
        let r = parse_own_line("1700000000\t1\t/a\techo\tfoo");
        assert_eq!(r.cmd, "echo\tfoo");
        assert_eq!(r.exit, Some(1));
        let r = parse_own_line("/home/me\tgit status");
        assert_eq!(
            (r.cmd.as_str(), r.cwd.as_deref(), r.ts),
            ("git status", Some("/home/me"), None)
        );
        let r = parse_own_line("C:\\Users\\me\tgit status");
        assert_eq!(r.cwd.as_deref(), Some("C:\\Users\\me"));
        let r = parse_own_line("ls -la");
        assert_eq!(
            (r.cmd.as_str(), r.cwd, r.ts, r.exit),
            ("ls -la", None, None, None)
        );
        assert_eq!(parse_own_line("echo\tfoo").cmd, "echo\tfoo");
    }

    #[test]
    fn normalize_dedups_keeping_latest_and_drops_blanks() {
        let raw = vec![
            pair("ls", None),
            pair("", None),
            pair("cd /tmp", None),
            pair("ls", None),
        ];
        let h = normalize(raw);
        assert_eq!(h.entries, ["cd /tmp", "ls"]);
        assert_eq!(h.counts.get("ls"), Some(&2));
        assert_eq!(h.counts.get("cd /tmp"), Some(&1));
    }

    #[test]
    fn normalize_collects_directories_per_command() {
        let raw = vec![
            pair("make", Some("/a")),
            pair("make", Some("/b")),
            pair("make", Some("/a")),
        ];
        let h = normalize(raw);
        let dirs = h.cwds.get("make").unwrap();
        assert!(dirs.contains("/a") && dirs.contains("/b"));
        assert_eq!(dirs.len(), 2);
    }

    #[test]
    fn normalize_keeps_the_most_recent_runs_metadata() {
        let with_meta = |cmd: &str, ts: u64, exit: Option<i32>| Raw {
            cmd: cmd.to_string(),
            cwd: None,
            ts: Some(ts),
            exit,
        };
        let raw = vec![
            with_meta("make", 100, Some(2)),
            pair("ls", None),
            with_meta("make", 200, Some(0)),
            pair("make", None),
        ];
        let h = normalize(raw);
        assert_eq!(
            h.meta.get("make"),
            Some(&EntryMeta {
                ts: Some(200),
                exit: Some(0)
            })
        );
        assert_eq!(h.meta.get("ls"), None);
    }

    #[test]
    fn frecency_ranks_frequent_over_merely_recent() {
        let entries = vec![
            "git status".to_string(),
            "ls".to_string(),
            "oops typo".to_string(),
        ];
        let mut counts = HashMap::new();
        counts.insert("git status".to_string(), 40);
        counts.insert("ls".to_string(), 5);
        counts.insert("oops typo".to_string(), 1);
        let ranked = rank_by_frecency(&entries, &counts, &HashMap::new(), None);
        assert_eq!(ranked[0], "git status");
        assert!(
            ranked.iter().position(|e| e == "git status").unwrap()
                < ranked.iter().position(|e| e == "oops typo").unwrap()
        );
    }

    #[test]
    fn frecency_favours_commands_run_in_the_current_directory() {
        let entries = vec!["npm test".to_string(), "cargo build".to_string()];
        let counts = HashMap::new();
        let mut cwds: HashMap<String, HashSet<String>> = HashMap::new();
        cwds.entry("cargo build".to_string())
            .or_default()
            .insert("/work/proj".to_string());
        let ranked = rank_by_frecency(&entries, &counts, &cwds, Some("/work/proj"));
        assert_eq!(ranked[0], "cargo build");
        let neutral = rank_by_frecency(&entries, &counts, &cwds, None);
        assert_eq!(neutral[0], "cargo build");
        assert_eq!(neutral[1], "npm test");
    }

    #[test]
    fn frecency_scores_align_with_the_ranking() {
        let entries = vec!["a".to_string(), "b".to_string()];
        let scores = frecency_scores(&entries, &HashMap::new(), &HashMap::new(), None);
        assert_eq!(scores.len(), 2);
        assert!(scores[1] > scores[0]);
    }

    #[test]
    fn format_ago_picks_readable_units() {
        let now = 1_700_000_000;
        assert_eq!(format_ago(now, now - 5), "now");
        assert_eq!(format_ago(now, now - 300), "5m");
        assert_eq!(format_ago(now, now - 2 * 3600), "2h");
        assert_eq!(format_ago(now, now - 3 * 86_400), "3d");
        assert_eq!(format_ago(now, now - 20 * 86_400), "2w");
        assert_eq!(format_ago(now, now - 90 * 86_400), "3mo");
        assert_eq!(format_ago(now, now - 800 * 86_400), "2y");
        assert_eq!(format_ago(now, now + 100), "now");
    }

    #[test]
    fn looks_absolute_recognizes_unix_and_windows_roots() {
        assert!(looks_absolute("/home/me"));
        assert!(looks_absolute("\\\\server\\share"));
        assert!(looks_absolute("C:\\Users"));
        assert!(looks_absolute("D:/data"));
        assert!(looks_absolute("Z:"));
        assert!(!looks_absolute("relative/path"));
        assert!(!looks_absolute("1:no"));
        assert!(!looks_absolute(""));
    }

    #[test]
    fn start_of_command_strips_prefixes_and_keeps_timestamps() {
        assert_eq!(
            start_of_command(": 1700000000:0;git status"),
            Some(("git status", Some(1_700_000_000)))
        );
        assert_eq!(
            start_of_command(": not-a-ts;cmd"),
            Some((": not-a-ts;cmd", None))
        );
        assert_eq!(start_of_command(": ;echo hi"), Some((": ;echo hi", None)));
        assert_eq!(start_of_command(": :::;cmd"), Some((": :::;cmd", None)));
        assert_eq!(start_of_command(""), None);
        assert_eq!(start_of_command("ls -la"), Some(("ls -la", None)));
    }

    #[test]
    fn bash_timestamp_recognizes_only_all_digit_comments() {
        assert_eq!(bash_timestamp("#1700000000"), Some(1_700_000_000));
        assert_eq!(bash_timestamp("#notdigits"), None);
        assert_eq!(bash_timestamp("#"), None);
        assert_eq!(bash_timestamp("ls"), None);
    }

    #[test]
    fn normalize_dedups_counts_and_caps_entries() {
        let raw = vec![
            pair("ls", Some("/a")),
            pair("git", None),
            pair("", None),
            pair("ls", Some("/b")),
        ];
        let h = normalize(raw);
        assert_eq!(h.entries, vec!["git".to_string(), "ls".to_string()]);
        assert_eq!(h.counts.get("ls"), Some(&2));
        let dirs = h.cwds.get("ls").unwrap();
        assert!(dirs.contains("/a") && dirs.contains("/b"));

        let big: Vec<Raw> = (0..MAX_ENTRIES + 50)
            .map(|i| pair(&format!("cmd{i}"), None))
            .collect();
        let capped = normalize(big);
        assert_eq!(capped.entries.len(), MAX_ENTRIES);
        assert_eq!(
            capped.entries.last().unwrap(),
            &format!("cmd{}", MAX_ENTRIES + 49)
        );
    }

    #[test]
    fn append_then_load_recovers_the_command_and_metadata() {
        crate::core::config::pin_test_config_dir();

        append(&Scope::Local, "bad\ncmd", None, 1_700_000_000, None);

        let unique = format!("tty7_cov_marker_{}", std::process::id());
        append(
            &Scope::Local,
            &unique,
            Some(Path::new("/tmp")),
            1_700_000_123,
            Some(1),
        );
        let loaded = load(&Scope::Local);
        assert!(
            loaded.entries.iter().any(|e| e == &unique),
            "appended command should be recalled by load()"
        );
        assert_eq!(
            loaded.meta.get(&unique),
            Some(&EntryMeta {
                ts: Some(1_700_000_123),
                exit: Some(1)
            })
        );
        assert!(
            loaded.cwds.get(&unique).is_some_and(|d| d.contains("/tmp")),
            "cwd association should round-trip"
        );
        assert!(
            !loaded.entries.iter().any(|e| e.contains('\n')),
            "newline command was never written"
        );
    }

    #[test]
    fn concurrent_appends_never_interleave_records() {
        crate::core::config::pin_test_config_dir();

        let tag = format!("tty7_race_{}", std::process::id());
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let tag = tag.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        append(
                            &Scope::Local,
                            &format!("{tag}_{t}_{i}"),
                            Some(Path::new("/tmp")),
                            1_700_000_000,
                            Some(0),
                        );
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let loaded = load(&Scope::Local);
        for t in 0..8 {
            for i in 0..25 {
                let cmd = format!("{tag}_{t}_{i}");
                assert!(
                    loaded.entries.iter().any(|e| e == &cmd),
                    "record {cmd} was lost or fused with a concurrent one"
                );
            }
        }
    }

    #[test]
    fn a_remote_scope_never_serves_the_local_machine_s_history() {
        crate::core::config::pin_test_config_dir();

        let tag = format!("tty7_scope_{}", std::process::id());
        let here = Scope::Local;
        let there = Scope::remote("me@box");
        append(&here, &format!("{tag}_local"), None, 1_700_000_000, Some(0));
        append(
            &there,
            &format!("{tag}_remote"),
            None,
            1_700_000_001,
            Some(0),
        );

        let local = load(&here);
        let remote = load(&there);
        assert!(local.entries.iter().any(|e| e == &format!("{tag}_local")));
        assert!(remote.entries.iter().any(|e| e == &format!("{tag}_remote")));
        assert!(
            !remote.entries.iter().any(|e| e == &format!("{tag}_local")),
            "a remote pane must not be offered commands from this machine"
        );
        assert!(
            !local.entries.iter().any(|e| e == &format!("{tag}_remote")),
            "the local pane must not be offered commands from the far end"
        );
    }

    #[test]
    fn two_remotes_keep_their_own_stores() {
        crate::core::config::pin_test_config_dir();

        let tag = format!("tty7_twohosts_{}", std::process::id());
        let a = Scope::remote("me@alpha");
        let b = Scope::remote("me@beta");
        append(&a, &format!("{tag}_a"), None, 1_700_000_000, Some(0));
        append(&b, &format!("{tag}_b"), None, 1_700_000_001, Some(0));

        assert!(
            !load(&a).entries.iter().any(|e| e == &format!("{tag}_b")),
            "one host's history must not leak into another's"
        );
        assert!(!load(&b).entries.iter().any(|e| e == &format!("{tag}_a")));
    }

    #[test]
    fn a_remote_scope_reads_the_far_end_s_own_shell_history() {
        crate::core::config::pin_test_config_dir();

        let scope = Scope::remote("me@readfile");
        let zsh = b": 1700000000:0;systemctl status nginx\n".to_vec();
        let bash = b"journalctl -u nginx\n".to_vec();
        let loaded = load_with_shell_files(
            &scope,
            vec![
                (".zsh_history".to_string(), zsh),
                (".bash_history".to_string(), bash),
            ],
        );

        assert!(
            loaded.entries.iter().any(|e| e == "systemctl status nginx"),
            "the far end's zsh history should be searchable"
        );
        assert!(loaded.entries.iter().any(|e| e == "journalctl -u nginx"));
        assert_eq!(
            loaded.meta.get("systemctl status nginx"),
            Some(&EntryMeta {
                ts: Some(1_700_000_000),
                exit: None,
            }),
            "zsh's second field is elapsed seconds, not an exit code"
        );
    }

    #[test]
    fn a_remote_fish_history_is_read_as_fish_not_as_bash_lines() {
        crate::core::config::pin_test_config_dir();

        let scope = Scope::remote("me@fishbox");
        let fish = b"- cmd: systemctl restart nginx\n  when: 1700000000\n".to_vec();
        let loaded = load_with_shell_files(&scope, vec![(FISH_HISTORY_FILE.to_string(), fish)]);

        assert!(
            loaded
                .entries
                .iter()
                .any(|e| e == "systemctl restart nginx"),
            "the far end's fish history should be searchable: {:?}",
            loaded.entries
        );
        assert!(
            !loaded.entries.iter().any(|e| e.starts_with("- cmd:")),
            "fish records must not reach the menu as raw file lines: {:?}",
            loaded.entries
        );
        assert!(
            !loaded.entries.iter().any(|e| e.starts_with("when:")),
            "a record's keys are not commands: {:?}",
            loaded.entries
        );
        assert_eq!(
            loaded.meta.get("systemctl restart nginx"),
            Some(&EntryMeta {
                ts: Some(1_700_000_000),
                exit: None,
            })
        );
    }

    #[test]
    fn a_label_that_is_not_a_filename_still_gets_its_own_file() {
        let slashes = file_stem("me@box:/srv/../weird");
        assert!(
            !slashes.contains(['/', '\\', ':', '@']),
            "a scope file name must not escape its directory: {slashes}"
        );
        assert_ne!(file_stem("me@alpha"), file_stem("me@beta"));
        assert_eq!(file_stem("me@alpha"), file_stem("me@alpha"));
        assert!(file_stem(&"x".repeat(400)).len() < 80);
    }

    #[test]
    fn an_empty_label_falls_back_to_local_rather_than_a_nameless_file() {
        assert_eq!(Scope::remote(""), Scope::Local);
        assert_eq!(Scope::remote("   "), Scope::Local);
    }

    #[test]
    fn append_rejects_a_cwd_that_would_break_the_line_format() {
        crate::core::config::pin_test_config_dir();

        let unique = format!("tty7_nlcwd_marker_{}", std::process::id());
        append(
            &Scope::Local,
            &unique,
            Some(Path::new("/tmp/evil\n/tmp/tail")),
            1_700_000_000,
            None,
        );
        let loaded = load(&Scope::Local);
        assert!(loaded.entries.iter().any(|e| e == &unique));
        assert!(loaded.cwds.get(&unique).is_none_or(|d| d.is_empty()));
        assert!(!loaded.entries.iter().any(|e| e == "/tmp/evil"));
    }
}

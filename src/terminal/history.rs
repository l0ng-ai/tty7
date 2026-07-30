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

pub fn load() -> History {
    let mut raw: Vec<Raw> = load_shell_history();
    if let Some(path) = config_path("history")
        && let Ok(content) = std::fs::read_to_string(&path)
    {
        raw.extend(content.lines().map(parse_own_line));
    }
    normalize(raw)
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

pub fn append(cmd: &str, cwd: Option<&Path>, ts: u64, exit: Option<i32>) {
    if cmd.contains('\n') {
        return;
    }
    let Some(path) = config_path("history") else {
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
    files.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    });

    let mut out = Vec::new();
    for path in files {
        if let Ok(bytes) = std::fs::read(&path) {
            parse_shell_history(&String::from_utf8_lossy(&bytes), &mut out);
        }
    }
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

        append("bad\ncmd", None, 1_700_000_000, None);

        let unique = format!("tty7_cov_marker_{}", std::process::id());
        append(&unique, Some(Path::new("/tmp")), 1_700_000_123, Some(1));
        let loaded = load();
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

        let loaded = load();
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
    fn append_rejects_a_cwd_that_would_break_the_line_format() {
        crate::core::config::pin_test_config_dir();

        let unique = format!("tty7_nlcwd_marker_{}", std::process::id());
        append(
            &unique,
            Some(Path::new("/tmp/evil\n/tmp/tail")),
            1_700_000_000,
            None,
        );
        let loaded = load();
        assert!(loaded.entries.iter().any(|e| e == &unique));
        assert!(loaded.cwds.get(&unique).is_none_or(|d| d.is_empty()));
        assert!(!loaded.entries.iter().any(|e| e == "/tmp/evil"));
    }
}

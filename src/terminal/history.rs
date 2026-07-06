//! Persistent command history, shared across sessions.
//!
//! Stored as a newline-delimited file at `~/.config/tty7/history` (the same config
//! dir as `config.json`), oldest first — simple, greppable, and good enough for
//! ↑/↓ recall and Ctrl+R search without pulling in a database. Each terminal loads
//! a snapshot on creation and appends as commands are submitted.
//!
//! Each new line is `<cwd>\t<command>` — the working directory the command ran in,
//! a tab, then the command. That cwd feeds the frecency ranking: commands you've
//! run *in this directory* float to the top of completion and ghost text. Legacy
//! plain-command lines (no tab) still parse fine, just without a cwd association.
//!
//! On load we also seed from the user's real shell histories (`~/.zsh_history`,
//! `~/.bash_history`, and `$HISTFILE`), so recall and completion work from the
//! very first launch — before tty7 has accumulated a history of its own. Those
//! files are read-only inputs; tty7 only ever writes its own file.

use crate::core::config::config_path;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Keep at most this many entries when loading, so the file can't grow without
/// bound across months of use (and so a huge shell history can't flood recall).
const MAX_ENTRIES: usize = 5000;

/// Frequency weight in the frecency score: how much a command's repeat count
/// matters relative to its recency. Recency contributes a normalized `0..1`
/// (oldest..newest); `FREQ_WEIGHT * ln(1 + count)` adds the frequency boost on
/// top, so a command run dozens of times outranks a once-typed recent line.
const FREQ_WEIGHT: f64 = 0.6;

/// Bonus added when a command was previously run in the *current* working
/// directory. Larger than recency's `0..1` range and on par with a ~7× frequency
/// boost, so directory-local commands float up strongly without wholly drowning a
/// very frequent global one (`git status`, `ls`, …).
const CWD_BONUS: f64 = 1.2;

/// Loaded history: the unique command lines (oldest-first, the source for ↑/↓
/// recall and Ctrl+R search), plus the two extra dimensions the frecency ranking
/// needs — per-line run `counts` (frequency) and, for each line, the set of
/// directories it was run in (`cwds`), so we can favour commands used *here*.
pub struct History {
    pub entries: Vec<String>,
    pub counts: HashMap<String, u32>,
    pub cwds: HashMap<String, HashSet<String>>,
}

/// Load history (oldest first), seeding from the user's shell histories and then
/// tty7's own file. Blanks are dropped and duplicates collapsed (keeping the most
/// recent occurrence), while occurrence counts and per-directory associations are
/// tallied for frecency ranking. Returns empty when nothing is readable.
pub fn load() -> History {
    // Shell history first (older, so it sits at a lower completion priority than
    // commands actually run in tty7), then tty7's own file last (most recent).
    // Shell-history lines carry no cwd; tty7's own `<cwd>\t<cmd>` lines do.
    let mut raw: Vec<(String, Option<String>)> = load_shell_history()
        .into_iter()
        .map(|cmd| (cmd, None))
        .collect();
    if let Some(path) = config_path("history") {
        if let Ok(content) = std::fs::read_to_string(&path) {
            raw.extend(content.lines().map(parse_own_line));
        }
    }
    normalize(raw)
}

/// Whether `p` looks like an absolute path, recognizing **both** Unix (`/…`) and
/// Windows (`C:\…`, `\\server\…`) forms regardless of the host platform. The
/// std `Path::is_absolute` is host-specific (it rejects `/home/me` on Windows and
/// `C:\…` on Unix), but a history file could have been written on either OS, and
/// the `\t` tag separator can't appear in a path, so this lenient check is safe.
fn looks_absolute(p: &str) -> bool {
    match p.as_bytes() {
        // Unix absolute, or a Windows rooted / UNC path.
        [b'/' | b'\\', ..] => true,
        // Windows drive path: `C:\`, `C:/`, or bare `C:`.
        [d, b':', ..] => d.is_ascii_alphabetic(),
        _ => false,
    }
}

/// Parse one line of tty7's own history file into `(command, cwd)`. New lines are
/// `<cwd>\t<command>` with an absolute cwd; legacy plain lines (and anything whose
/// pre-tab part isn't an absolute path) carry just the command, no cwd.
fn parse_own_line(line: &str) -> (String, Option<String>) {
    if let Some((cwd, cmd)) = line.split_once('\t')
        && looks_absolute(cwd)
    {
        return (cmd.to_string(), Some(cwd.to_string()));
    }
    (line.to_string(), None)
}

/// Order unique history entries by *frecency* (frequency × recency, plus a
/// current-directory bonus), most relevant first — the ranking that drives
/// ghost-text autosuggestion and the completion menu's history recalls, so neither
/// surfaces stale junk just because it was typed once, recently. `entries` is
/// oldest-first as from [`load`]; `counts` and `cwds` are its companions; `cwd` is
/// the directory to favour (none → no directory bonus).
pub fn rank_by_frecency(
    entries: &[String],
    counts: &HashMap<String, u32>,
    cwds: &HashMap<String, HashSet<String>>,
    cwd: Option<&str>,
) -> Vec<String> {
    let n = entries.len();
    let mut scored: Vec<(f64, usize)> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            // Recency: 0 for the oldest entry, 1 for the newest (position in the
            // oldest-first list). Frequency: a diminishing-returns boost on count.
            let recency = if n <= 1 {
                1.0
            } else {
                i as f64 / (n - 1) as f64
            };
            let count = f64::from(*counts.get(e).unwrap_or(&1));
            let mut score = recency + FREQ_WEIGHT * (1.0 + count).ln();
            // Directory bonus: this command has been run here before.
            if let Some(cwd) = cwd
                && cwds.get(e).is_some_and(|dirs| dirs.contains(cwd))
            {
                score += CWD_BONUS;
            }
            (score, i)
        })
        .collect();
    // Higher score first; ties broken toward the more recent entry.
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.1.cmp(&a.1))
    });
    scored
        .into_iter()
        .map(|(_, i)| entries[i].clone())
        .collect()
}

/// Append one command to the history file (best effort), tagged with the `cwd` it
/// ran in when that's a usable absolute path. Commands containing a newline are
/// skipped, since the format is one-per-line.
pub fn append(cmd: &str, cwd: Option<&Path>) {
    if cmd.contains('\n') {
        return;
    }
    let Some(path) = config_path("history") else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // `<cwd>\t<cmd>` when we have an absolute cwd — one that can't confuse the
    // one-line format: no tab (the field separator) and no newline/CR (a cwd
    // containing one would split the record across lines, and the stray tail
    // would load back as a bogus command). Otherwise the legacy bare form.
    let line = match cwd.and_then(Path::to_str) {
        Some(c) if looks_absolute(c) && !c.contains(['\t', '\n', '\r']) => {
            format!("{c}\t{cmd}")
        }
        _ => cmd.to_string(),
    };
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        // One `write_all` of the fully formatted record: `writeln!` on an
        // unbuffered `File` can issue the text and the trailing newline as
        // separate writes, and concurrent appenders (several panes, or several
        // tty7 processes sharing the file) then interleave half-records even
        // though O_APPEND keeps each individual write atomic.
        let _ = f.write_all(format!("{line}\n").as_bytes());
    }
}

/// Drop blanks and de-duplicate (keeping the most recent occurrence, so recall
/// and completion stay clean when shell history and tty7's own file overlap),
/// tallying how many times each line appears and which directories it ran in, then
/// cap to the most recent `MAX_ENTRIES`. Input is `(command, cwd)` pairs; output
/// entries are oldest-first.
fn normalize(raw: Vec<(String, Option<String>)>) -> History {
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut cwds: HashMap<String, HashSet<String>> = HashMap::new();
    let mut seen = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for (line, cwd) in raw.into_iter().rev() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        *counts.entry(line.to_string()).or_insert(0) += 1;
        if let Some(cwd) = cwd {
            cwds.entry(line.to_string()).or_default().insert(cwd);
        }
        if seen.insert(line.to_string()) {
            out.push(line.to_string());
        }
    }
    out.reverse(); // back to oldest-first
    if out.len() > MAX_ENTRIES {
        let cut = out.len() - MAX_ENTRIES;
        // Drop the over-cap entries from the companion maps too, keeping them bounded.
        for r in out.drain(0..cut) {
            counts.remove(&r);
            cwds.remove(&r);
        }
    }
    History {
        entries: out,
        counts,
        cwds,
    }
}

/// Read the user's bash/zsh histories (best effort), returning command lines
/// oldest-first. Reads the standard `~/.zsh_history` and `~/.bash_history` plus
/// `$HISTFILE` if set, and orders the files by modification time so the
/// most-recently-used shell's entries end up with the highest completion
/// priority.
fn load_shell_history() -> Vec<String> {
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
    // Oldest-modified file first → newest last (highest recall/completion priority).
    files.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    });

    let mut out = Vec::new();
    for path in files {
        if let Ok(bytes) = std::fs::read(&path) {
            // History files can hold non-UTF-8 bytes (zsh metafies some); lossy
            // decoding keeps the rest usable.
            parse_shell_history(&String::from_utf8_lossy(&bytes), &mut out);
        }
    }
    out
}

/// Parse one shell-history file into command lines, appending to `out`. Strips
/// zsh's extended-format prefix (`: <start>:<elapsed>;cmd`) and skips bash
/// `HISTTIMEFORMAT` timestamp comments.
///
/// Each physical line becomes its own entry — we deliberately do *not* stitch
/// backslash-continued multi-line commands back together. bash stores multi-line
/// commands as separate lines anyway, and joining them would (a) embed newlines
/// that wreck the single-line completion menu's layout and (b) on bash, wrongly
/// swallow the following command. A few stray fragments from a zsh here-doc are a
/// fair price for robustness.
fn parse_shell_history(content: &str, out: &mut Vec<String>) {
    for raw in content.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(cmd) = start_of_command(line) {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
                out.push(cmd.to_string());
            }
        }
    }
}

/// The command text at the start of a history line, or `None` for lines that
/// carry no command (blank lines, bash timestamp comments). Strips the zsh
/// extended-history `": <start>:<elapsed>;"` prefix when present.
fn start_of_command(line: &str) -> Option<&str> {
    if line.is_empty() {
        return None;
    }
    // zsh extended history: ": 1700000000:0;the command". The timestamp field
    // must hold at least one digit — an empty/colon-only prefix would otherwise
    // match a *real* command like `: ;echo hi` and wrongly strip its head.
    if let Some(rest) = line.strip_prefix(": ") {
        if let Some(semi) = rest.find(';') {
            let ts = &rest[..semi];
            if ts.bytes().any(|b| b.is_ascii_digit())
                && ts.bytes().all(|b| b.is_ascii_digit() || b == b':')
            {
                return Some(&rest[semi + 1..]);
            }
        }
    }
    // bash HISTTIMEFORMAT timestamp comment: "#1700000000".
    if let Some(rest) = line.strip_prefix('#') {
        if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> Vec<String> {
        let mut out = Vec::new();
        parse_shell_history(content, &mut out);
        out
    }

    #[test]
    fn plain_bash_lines() {
        assert_eq!(
            parse("ls\ncd /tmp\ngit status\n"),
            ["ls", "cd /tmp", "git status"]
        );
    }

    #[test]
    fn zsh_extended_prefix_is_stripped() {
        let content = ": 1700000000:0;git status\n: 1700000005:2;cargo build\n";
        assert_eq!(parse(content), ["git status", "cargo build"]);
    }

    #[test]
    fn bash_timestamp_comments_are_skipped() {
        let content = "#1700000000\nls -la\n#1700000005\ncd ..\n";
        assert_eq!(parse(content), ["ls -la", "cd .."]);
    }

    #[test]
    fn multiline_commands_are_split_not_joined() {
        // We never stitch continuation lines together — each physical line is its
        // own entry, so no entry can carry an embedded newline (which would wreck
        // the single-line completion menu's layout).
        let content = ": 1700000000:0;for f in *; do\\\necho $f\\\ndone\n";
        let got = parse(content);
        assert_eq!(got, ["for f in *; do\\", "echo $f\\", "done"]);
        assert!(got.iter().all(|e| !e.contains('\n')));
    }

    fn pair(cmd: &str, cwd: Option<&str>) -> (String, Option<String>) {
        (cmd.to_string(), cwd.map(str::to_string))
    }

    #[test]
    fn parse_own_line_reads_cwd_prefix_and_legacy_lines() {
        assert_eq!(
            parse_own_line("/home/me\tgit status"),
            ("git status".to_string(), Some("/home/me".to_string()))
        );
        // Windows absolute cwd is recognized too (cross-platform, host-independent).
        // `\\` are literal backslashes; `\t` is the real tab separator.
        assert_eq!(
            parse_own_line("C:\\Users\\me\tgit status"),
            ("git status".to_string(), Some("C:\\Users\\me".to_string()))
        );
        // Legacy bare command — no tab, no cwd.
        assert_eq!(parse_own_line("ls -la"), ("ls -la".to_string(), None));
        // A tab whose pre-part isn't an absolute path is not treated as a cwd.
        assert_eq!(parse_own_line("echo\tfoo"), ("echo\tfoo".to_string(), None));
    }

    #[test]
    fn normalize_dedups_keeping_latest_and_drops_blanks() {
        let raw = vec![
            pair("ls", None),
            pair("", None),
            pair("cd /tmp", None),
            pair("ls", None), // later duplicate wins its (later) position
        ];
        let h = normalize(raw);
        assert_eq!(h.entries, ["cd /tmp", "ls"]);
        // Both occurrences of "ls" are counted, even though it appears once.
        assert_eq!(h.counts.get("ls"), Some(&2));
        assert_eq!(h.counts.get("cd /tmp"), Some(&1));
    }

    #[test]
    fn normalize_collects_directories_per_command() {
        let raw = vec![
            pair("make", Some("/a")),
            pair("make", Some("/b")),
            pair("make", Some("/a")), // same dir again — still just the set {/a, /b}
        ];
        let h = normalize(raw);
        let dirs = h.cwds.get("make").unwrap();
        assert!(dirs.contains("/a") && dirs.contains("/b"));
        assert_eq!(dirs.len(), 2);
    }

    #[test]
    fn frecency_ranks_frequent_over_merely_recent() {
        // `git status` is old but run many times; `oops typo` is the newest line
        // but a one-off. Frecency should float the frequent command above it.
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
        // Two equally rare, equally old commands; only `cargo build` has been run
        // in the current directory, so the cwd bonus lifts it above `npm test`.
        let entries = vec!["npm test".to_string(), "cargo build".to_string()];
        let counts = HashMap::new(); // both default to count 1
        let mut cwds: HashMap<String, HashSet<String>> = HashMap::new();
        cwds.entry("cargo build".to_string())
            .or_default()
            .insert("/work/proj".to_string());
        let ranked = rank_by_frecency(&entries, &counts, &cwds, Some("/work/proj"));
        assert_eq!(ranked[0], "cargo build");
        // Without the directory context, recency tie-break favours the newer entry.
        let neutral = rank_by_frecency(&entries, &counts, &cwds, None);
        assert_eq!(neutral[0], "cargo build"); // newest wins the tie either way
        assert_eq!(neutral[1], "npm test");
    }

    #[test]
    fn looks_absolute_recognizes_unix_and_windows_roots() {
        assert!(looks_absolute("/home/me"));
        assert!(looks_absolute("\\\\server\\share")); // UNC
        assert!(looks_absolute("C:\\Users")); // drive + backslash
        assert!(looks_absolute("D:/data")); // drive + forward slash
        assert!(looks_absolute("Z:")); // bare drive
        // Not absolute.
        assert!(!looks_absolute("relative/path"));
        assert!(!looks_absolute("1:no")); // non-alpha "drive"
        assert!(!looks_absolute(""));
    }

    #[test]
    fn start_of_command_strips_prefixes_and_skips_timestamps() {
        // zsh extended-history prefix is stripped.
        assert_eq!(
            start_of_command(": 1700000000:0;git status"),
            Some("git status")
        );
        // A colon-prefixed line whose middle isn't numeric is taken verbatim.
        assert_eq!(start_of_command(": not-a-ts;cmd"), Some(": not-a-ts;cmd"));
        // Regression: an empty or colon-only "timestamp" is not the zsh format —
        // the line is a real command (`: ;echo hi` runs the colon builtin, then
        // echo) and must NOT have its head stripped.
        assert_eq!(start_of_command(": ;echo hi"), Some(": ;echo hi"));
        assert_eq!(start_of_command(": :::;cmd"), Some(": :::;cmd"));
        // A bash timestamp comment carries no command.
        assert_eq!(start_of_command("#1700000000"), None);
        // A real comment-looking line with non-digits is a command.
        assert_eq!(start_of_command("#notdigits"), Some("#notdigits"));
        // Blank → None.
        assert_eq!(start_of_command(""), None);
        // Plain command passes through.
        assert_eq!(start_of_command("ls -la"), Some("ls -la"));
    }

    #[test]
    fn normalize_dedups_counts_and_caps_entries() {
        // Duplicates collapse to the most recent position, with a run count tallied.
        let raw = vec![
            ("ls".to_string(), Some("/a".to_string())),
            ("git".to_string(), None),
            ("".to_string(), None), // blank dropped
            ("ls".to_string(), Some("/b".to_string())),
        ];
        let h = normalize(raw);
        // "ls" moved to the end (most recent) and "git" stayed; blank gone.
        assert_eq!(h.entries, vec!["git".to_string(), "ls".to_string()]);
        assert_eq!(h.counts.get("ls"), Some(&2));
        // Both directories "ls" ran in are recorded.
        let dirs = h.cwds.get("ls").unwrap();
        assert!(dirs.contains("/a") && dirs.contains("/b"));

        // The cap keeps only the most recent MAX_ENTRIES unique lines.
        let big: Vec<(String, Option<String>)> = (0..MAX_ENTRIES + 50)
            .map(|i| (format!("cmd{i}"), None))
            .collect();
        let capped = normalize(big);
        assert_eq!(capped.entries.len(), MAX_ENTRIES);
        // The oldest were dropped; the newest survives.
        assert_eq!(
            capped.entries.last().unwrap(),
            &format!("cmd{}", MAX_ENTRIES + 49)
        );
    }

    #[test]
    fn append_then_load_recovers_the_command() {
        // Pin the config dir so history writes to a temp file, not the real one.
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::core::config::set_config_dir(dir);

        // A command with an embedded newline is rejected (one-per-line format).
        append("bad\ncmd", None);

        // A unique command tagged with an absolute cwd round-trips through load().
        let unique = format!("tty7_cov_marker_{}", std::process::id());
        append(&unique, Some(Path::new("/tmp")));
        let loaded = load();
        assert!(
            loaded.entries.iter().any(|e| e == &unique),
            "appended command should be recalled by load()"
        );
        assert!(
            !loaded.entries.iter().any(|e| e.contains('\n')),
            "newline command was never written"
        );
    }

    #[test]
    fn concurrent_appends_never_interleave_records() {
        // Regression: `writeln!` on an unbuffered File could split one record
        // into two write syscalls (text, then newline), so two panes appending
        // at once produced fused half-lines ("cmdAcmdB\n\n") that loaded back
        // as garbage commands. Each record must land as one atomic write.
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::core::config::set_config_dir(dir);

        let tag = format!("tty7_race_{}", std::process::id());
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let tag = tag.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        append(&format!("{tag}_{t}_{i}"), Some(Path::new("/tmp")));
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
        // Regression: a cwd containing a newline used to be written verbatim into
        // the `<cwd>\t<cmd>` line, splitting the record — the pre-newline half
        // loaded back as a bogus command and the real command gained a wrong cwd.
        // Such a cwd is dropped (legacy bare form) so the record stays one line.
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::core::config::set_config_dir(dir);

        let unique = format!("tty7_nlcwd_marker_{}", std::process::id());
        append(&unique, Some(Path::new("/tmp/evil\n/tmp/tail")));
        let loaded = load();
        // The command itself survives, as a bare entry…
        assert!(loaded.entries.iter().any(|e| e == &unique));
        // …with no cwd association (the unusable path was dropped, not split)…
        assert!(loaded.cwds.get(&unique).is_none_or(|d| d.is_empty()));
        // …and no half-a-path entry leaked in as a phantom command.
        assert!(!loaded.entries.iter().any(|e| e == "/tmp/evil"));
    }
}

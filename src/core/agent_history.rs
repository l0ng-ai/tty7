//! Past coding-agent sessions on this computer, read off the agents' own
//! history files, so one can be picked and resumed.
//!
//! Only the ends of each file are read: the head carries where the session
//! ran and what it was first asked, the tail the title the agent last gave
//! it. A transcript runs to megabytes, and reading the whole of hundreds of
//! them to list their names would make opening the list the slow part.
//!
//! Parsed files are remembered by path, size and modification time, so after
//! the first scan only the sessions that moved are read again.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::core::cli_agent::CLIAgent;

/// How many sessions are listed, newest first. Past this the list is a
/// search target, not something to scroll, and older sessions are rarely
/// worth resuming.
const MAX_SESSIONS: usize = 500;

/// How much of a file's head and tail is read.
const HEAD_BYTES: u64 = 64 * 1024;
const TAIL_BYTES: u64 = 128 * 1024;

/// How much of a prompt a title keeps.
const TITLE_CHARS: usize = 120;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PastSession {
    pub agent: CLIAgent,
    pub id: String,
    /// Where it ran — where it has to be resumed, since agents key their
    /// history by directory.
    pub cwd: Option<PathBuf>,
    pub title: String,
    pub branch: Option<String>,
    /// Last written, in Unix seconds.
    pub updated: u64,
}

/// Every past session found under `home`, most recently used first.
///
/// `codex_home` is `$CODEX_HOME` when set; Codex keeps its sessions under
/// `~/.codex` otherwise.
pub fn scan(home: &Path, codex_home: Option<&Path>) -> Vec<PastSession> {
    let codex_home = codex_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".codex"));
    let mut files = claude_files(home);
    files.extend(codex_files(&codex_home));
    files.sort_by_key(|f| std::cmp::Reverse(f.modified));
    files.truncate(MAX_SESSIONS);
    // Codex names a thread after the fact, in an index beside the sessions
    // rather than in the session's own file.
    let codex_titles = codex_thread_names(&codex_home);

    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    let mut seen = HashMap::with_capacity(files.len());
    for file in files {
        let hit = cache
            .get(&file.path)
            .filter(|c| c.len == file.len && c.modified == file.modified)
            .map(|c| c.session.clone());
        let session = match hit {
            Some(session) => session,
            None => match file.agent {
                CLIAgent::Codex => read_codex(&file.path, unix(file.modified)),
                _ => read_claude(&file.path, file.len, unix(file.modified)),
            },
        };
        seen.insert(
            file.path.clone(),
            Cached {
                len: file.len,
                modified: file.modified,
                session: session.clone(),
            },
        );
        out.extend(session.map(|mut s| {
            if let Some(name) = codex_titles
                .get(&s.id)
                .filter(|_| s.agent == CLIAgent::Codex)
            {
                s.title = name.clone();
            }
            s
        }));
    }
    // What was not seen this time was deleted or fell off the end.
    *cache = seen;
    out
}

/// What the last [`scan`] found, without touching the disk — what the list
/// shows while a new scan runs.
pub fn cached() -> Vec<PastSession> {
    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<PastSession> = cache.values().filter_map(|c| c.session.clone()).collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.updated));
    out
}

/// `$CODEX_HOME`, if set.
pub fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// This user's home, where the agents keep their history.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

struct Cached {
    len: u64,
    modified: SystemTime,
    /// `None` for a file that holds no session worth listing — remembered
    /// too, so it is not read again until it changes.
    session: Option<PastSession>,
}

static CACHE: LazyLock<Mutex<HashMap<PathBuf, Cached>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct Found {
    agent: CLIAgent,
    path: PathBuf,
    len: u64,
    modified: SystemTime,
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Claude Code keeps one `<session id>.jsonl` per session under
/// `~/.claude/projects/<cwd with separators as dashes>/`. The directories
/// beside those files hold a session's subagent transcripts, which are not
/// sessions of their own.
fn claude_files(home: &Path) -> Vec<Found> {
    let root = home.join(".claude").join("projects");
    let Ok(projects) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for project in projects.flatten() {
        let Ok(entries) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            out.push(Found {
                agent: CLIAgent::Claude,
                path,
                len: meta.len(),
                modified: meta.modified().unwrap_or(UNIX_EPOCH),
            });
        }
    }
    out
}

/// Codex writes `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`.
fn codex_files(codex_home: &Path) -> Vec<Found> {
    let mut out = Vec::new();
    let mut dirs = vec![codex_home.join("sessions")];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            let path = entry.path();
            if meta.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                out.push(Found {
                    agent: CLIAgent::Codex,
                    path,
                    len: meta.len(),
                    modified: meta.modified().unwrap_or(UNIX_EPOCH),
                });
            }
        }
    }
    out
}

/// `session_index.jsonl`: one `{id, thread_name}` line per naming, the
/// last one for an id being its name now.
fn codex_thread_names(codex_home: &Path) -> HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(codex_home.join("session_index.jsonl")) else {
        return HashMap::new();
    };
    records(&text)
        .filter_map(|r| {
            let id = str_field(&r, "id")?.to_string();
            let name = str_field(&r, "thread_name")?.to_string();
            Some((id, name))
        })
        .collect()
}

/// How far into a rollout the first prompt is looked for. The first record
/// alone can run to hundreds of kilobytes — it carries the instructions
/// Codex started with — so this reads whole lines rather than a fixed head.
const CODEX_HEAD_BYTES: u64 = 2 * 1024 * 1024;

fn read_codex(path: &Path, updated: u64) -> Option<PastSession> {
    use std::io::BufRead;
    // Everything Codex says about the session is at its start.
    let file = File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file.take(CODEX_HEAD_BYTES));
    let mut head = String::new();
    let mut line = String::new();
    while reader.read_line(&mut line).ok()? > 0 {
        let prompt = line.contains("\"user_message\"");
        head.push_str(&line);
        line.clear();
        // The prompt comes after the metadata, so once one is in, both are.
        if prompt {
            break;
        }
    }
    parse_codex(&head, updated)
}

pub(crate) fn parse_codex(head: &str, updated: u64) -> Option<PastSession> {
    let mut id = None;
    let mut cwd = None;
    let mut branch = None;
    let mut title = None;
    let mut first_prompt = None;
    for record in records(head) {
        let Some(payload) = record.get("payload") else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                // A review or a subagent Codex ran for itself is not a
                // session anyone started, and there is nothing to go back to.
                if codex_not_the_users(payload) {
                    return None;
                }
                id = str_field(payload, "id").map(str::to_string);
                cwd = str_field(payload, "cwd").map(PathBuf::from);
                branch = payload.get("git").and_then(|git| {
                    str_field(git, "branch")
                        .or_else(|| str_field(git, "current_branch"))
                        .map(str::to_string)
                });
                title = str_field(payload, "title")
                    .or_else(|| str_field(payload, "thread_name"))
                    .or_else(|| str_field(payload, "threadName"))
                    .map(str::to_string);
            }
            Some("turn_context") if cwd.is_none() => {
                cwd = str_field(payload, "cwd").map(PathBuf::from);
            }
            // What the user typed, as the event stream records it. The
            // `response_item` of the same turn also carries the context
            // Codex injects around it (`<environment_context>`, AGENTS.md).
            Some("event_msg")
                if first_prompt.is_none()
                    && payload.get("type").and_then(Value::as_str) == Some("user_message") =>
            {
                first_prompt = str_field(payload, "message").and_then(prompt_title);
            }
            _ => {}
        }
        if id.is_some() && first_prompt.is_some() {
            break;
        }
    }
    let title = title.or(first_prompt)?;
    Some(PastSession {
        agent: CLIAgent::Codex,
        id: id?,
        cwd,
        title,
        branch,
        updated,
    })
}

fn codex_not_the_users(meta: &Value) -> bool {
    let source = match meta.get("source") {
        Some(Value::String(s)) => matches!(s.as_str(), "subagent" | "internal"),
        // `{"subagent": …}`: a spawned one, described.
        Some(Value::Object(_)) => true,
        _ => false,
    };
    let thread = str_field(meta, "thread_source").is_some_and(|s| s != "user");
    source || thread
}

fn read_claude(path: &Path, len: u64, updated: u64) -> Option<PastSession> {
    let id = path.file_stem()?.to_str()?.to_string();
    let (head, tail) = ends(path, len).ok()?;
    parse_claude(id, &head, &tail, updated)
}

/// The file's first [`HEAD_BYTES`] and last [`TAIL_BYTES`], as text. The two
/// overlap on a short file, which is harmless: the head is read for the first
/// of things and the tail for the last.
fn ends(path: &Path, len: u64) -> std::io::Result<(String, String)> {
    let mut file = File::open(path)?;
    let mut head = Vec::new();
    (&mut file).take(HEAD_BYTES).read_to_end(&mut head)?;
    let mut tail = Vec::new();
    if len > HEAD_BYTES {
        file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))?;
        file.take(TAIL_BYTES).read_to_end(&mut tail)?;
    }
    Ok((
        String::from_utf8_lossy(&head).into_owned(),
        String::from_utf8_lossy(&tail).into_owned(),
    ))
}

/// Complete JSON records in `text`. A cut at either end leaves a partial
/// line, which does not parse and is skipped.
fn records(text: &str) -> impl DoubleEndedIterator<Item = Value> + '_ {
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub(crate) fn parse_claude(
    id: String,
    head: &str,
    tail: &str,
    updated: u64,
) -> Option<PastSession> {
    let mut cwd = None;
    let mut branch = None;
    let mut first_prompt = None;
    for record in records(head) {
        if cwd.is_none() {
            cwd = str_field(&record, "cwd").map(PathBuf::from);
        }
        if branch.is_none() {
            branch = str_field(&record, "gitBranch")
                .filter(|b| *b != "HEAD")
                .map(str::to_string);
        }
        if first_prompt.is_none() {
            first_prompt = claude_user_prompt(&record);
        }
        if cwd.is_some() && branch.is_some() && first_prompt.is_some() {
            break;
        }
    }

    // A name the user gave it outranks the one the agent made up, which
    // outranks what it was first asked. The last of each wins: a session is
    // renamed, and retitled as it goes.
    let mut named = None;
    let mut titled = None;
    for record in records(tail).rev().chain(records(head).rev()) {
        match record.get("type").and_then(Value::as_str) {
            Some("custom-title") if named.is_none() => {
                named = str_field(&record, "customTitle").map(str::to_string);
            }
            Some("ai-title") if titled.is_none() => {
                titled = str_field(&record, "aiTitle").map(str::to_string);
            }
            Some("summary") if titled.is_none() => {
                titled = str_field(&record, "summary").map(str::to_string);
            }
            _ => {}
        }
        if named.is_some() {
            break;
        }
    }
    // A session that was opened and never asked anything has nothing to
    // resume.
    let title = named.or(titled).or(first_prompt)?;
    Some(PastSession {
        agent: CLIAgent::Claude,
        id,
        cwd,
        title,
        branch,
        updated,
    })
}

/// What the user typed, from a user record — not a tool result, not the
/// harness's own injections (`<command-name>`, `<local-command-caveat>`, …).
fn claude_user_prompt(record: &Value) -> Option<String> {
    if record.get("type").and_then(Value::as_str) != Some("user")
        || record.get("isMeta").and_then(Value::as_bool) == Some(true)
        || record.get("isSidechain").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let content = record.get("message")?.get("content")?;
    let text = match content {
        Value::String(s) => s.as_str(),
        Value::Array(parts) => parts.iter().find_map(|p| {
            (p.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| p.get("text").and_then(Value::as_str))
                .flatten()
        })?,
        _ => return None,
    };
    prompt_title(text)
}

/// The first line of `text` worth reading as a title, or `None` for text
/// the harness wrote rather than the user.
fn prompt_title(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.starts_with('<') || text.starts_with("Caveat:") {
        return None;
    }
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut title: String = line.chars().take(TITLE_CHARS).collect();
    if line.chars().count() > TITLE_CHARS {
        title.push('…');
    }
    Some(title)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(v: serde_json::Value) -> String {
        format!("{v}\n")
    }

    fn user(text: &str) -> String {
        line(serde_json::json!({
            "type": "user",
            "cwd": "/repo",
            "gitBranch": "main",
            "message": {"role": "user", "content": text},
        }))
    }

    #[test]
    fn the_harness_is_not_the_user() {
        let head = [
            line(serde_json::json!({"type": "mode", "sessionId": "s"})),
            user("<command-name>/clear</command-name>"),
            user("Caveat: The messages below were generated by the user"),
            user("fix the flaky test\nit fails on CI"),
        ]
        .concat();
        let s = parse_claude("s".into(), &head, "", 7).expect("a session");
        assert_eq!(s.title, "fix the flaky test");
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/repo")));
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.updated, 7);
    }

    #[test]
    fn a_name_beats_a_generated_title_beats_the_first_prompt() {
        let head = user("first thing asked");
        let titled = [
            line(serde_json::json!({"type": "ai-title", "aiTitle": "Old title"})),
            line(serde_json::json!({"type": "ai-title", "aiTitle": "Flaky test fix"})),
        ]
        .concat();
        let s = parse_claude("s".into(), &head, &titled, 0).unwrap();
        assert_eq!(s.title, "Flaky test fix", "the last title wins");

        let named = format!(
            "{}{titled}",
            line(serde_json::json!({"type": "custom-title", "customTitle": "ci work"}))
        );
        let s = parse_claude("s".into(), &head, &named, 0).unwrap();
        assert_eq!(s.title, "ci work");

        let s = parse_claude("s".into(), &head, "", 0).unwrap();
        assert_eq!(s.title, "first thing asked");
    }

    #[test]
    fn a_session_never_asked_anything_is_not_listed() {
        let head = user("<command-name>/clear</command-name>");
        assert!(parse_claude("s".into(), &head, "", 0).is_none());
    }

    #[test]
    fn a_line_cut_by_the_read_is_skipped_not_fatal() {
        let head = format!("{}{{\"type\":\"user\",\"mess", user("real prompt"));
        let tail = format!("t\":1}}\n{}", user("later"));
        let s = parse_claude("s".into(), &head, &tail, 0).unwrap();
        assert_eq!(s.title, "real prompt");
    }

    #[test]
    fn prompt_parts_and_tool_results() {
        let tool = line(serde_json::json!({
            "type": "user",
            "message": {"content": [{"type": "tool_result", "content": "ok"}]},
        }));
        let parts = line(serde_json::json!({
            "type": "user",
            "message": {"content": [{"type": "text", "text": "look at this"}]},
        }));
        let s = parse_claude("s".into(), &format!("{tool}{parts}"), "", 0).unwrap();
        assert_eq!(s.title, "look at this");
    }

    #[test]
    fn a_long_prompt_is_cut_to_a_title() {
        let long = "x".repeat(TITLE_CHARS + 10);
        let s = parse_claude("s".into(), &user(&long), "", 0).unwrap();
        assert_eq!(s.title.chars().count(), TITLE_CHARS + 1);
        assert!(s.title.ends_with('…'));
    }

    #[test]
    fn scan_reads_sessions_not_subagent_transcripts() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".claude/projects/-repo");
        std::fs::create_dir_all(project.join("abc/subagents")).unwrap();
        std::fs::write(project.join("abc.jsonl"), user("hello")).unwrap();
        std::fs::write(project.join("abc/subagents/agent-1.jsonl"), user("sub")).unwrap();
        std::fs::write(project.join("empty.jsonl"), "").unwrap();
        let found = scan(home.path(), None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "abc");
        assert_eq!(found[0].title, "hello");
    }

    fn codex_meta(extra: serde_json::Value) -> String {
        let mut payload = serde_json::json!({
            "id": "0199-abc",
            "cwd": "/work",
            "git": {"branch": "feature"},
        });
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        line(serde_json::json!({"type": "session_meta", "payload": payload}))
    }

    fn codex_user(text: &str) -> String {
        line(serde_json::json!({
            "type": "event_msg",
            "payload": {"type": "user_message", "message": text},
        }))
    }

    #[test]
    fn a_codex_rollout_is_named_by_what_was_typed_not_the_injected_context() {
        let injected = line(serde_json::json!({
            "type": "response_item",
            "payload": {"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "<environment_context>…"}]},
        }));
        let head = [
            codex_meta(serde_json::json!({})),
            injected,
            codex_user("add a retry"),
        ]
        .concat();
        let s = parse_codex(&head, 3).expect("a session");
        assert_eq!(s.agent, CLIAgent::Codex);
        assert_eq!(s.id, "0199-abc");
        assert_eq!(s.title, "add a retry");
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/work")));
        assert_eq!(s.branch.as_deref(), Some("feature"));
    }

    #[test]
    fn a_rollout_codex_ran_for_itself_is_not_listed() {
        for extra in [
            serde_json::json!({"source": "subagent"}),
            serde_json::json!({"source": {"subagent": "review"}}),
            serde_json::json!({"thread_source": "automation"}),
        ] {
            let head = format!("{}{}", codex_meta(extra.clone()), codex_user("x"));
            assert!(parse_codex(&head, 0).is_none(), "{extra}");
        }
        let head = format!(
            "{}{}",
            codex_meta(serde_json::json!({"source": "cli"})),
            codex_user("x")
        );
        assert!(parse_codex(&head, 0).is_some());
    }

    #[test]
    fn codex_sessions_take_the_name_from_its_index() {
        let home = tempfile::tempdir().unwrap();
        let codex = home.path().join(".codex");
        let day = codex.join("sessions/2026/09/26");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            day.join("rollout-2026-09-26T10-00-00-0199-abc.jsonl"),
            format!("{}{}", codex_meta(serde_json::json!({})), codex_user("hi")),
        )
        .unwrap();
        std::fs::write(
            codex.join("session_index.jsonl"),
            [
                line(serde_json::json!({"id": "0199-abc", "thread_name": "Old"})),
                line(serde_json::json!({"id": "0199-abc", "thread_name": "Retry logic"})),
            ]
            .concat(),
        )
        .unwrap();
        let found = scan(home.path(), None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Retry logic");
    }
}

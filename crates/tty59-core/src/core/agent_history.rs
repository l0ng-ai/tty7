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

/// Crosses the control protocol: a remote workspace's server scans its own
/// machine and sends back what it found.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

/// Where each agent keeps its history on this computer.
#[derive(Clone, Debug)]
pub struct Roots {
    /// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
    pub claude: PathBuf,
    /// `$CODEX_HOME`, else `~/.codex`.
    pub codex: PathBuf,
    /// `$GEMINI_CLI_HOME/.gemini`, else `~/.gemini`.
    pub gemini: PathBuf,
    /// `$QWEN_HOME`, else `~/.qwen`.
    pub qwen: PathBuf,
    /// `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
    pub pi: PathBuf,
    /// `~/.omp/agent`.
    pub omp: PathBuf,
    /// `$KIMI_CODE_HOME`, else `~/.kimi-code`.
    pub kimi: PathBuf,
    /// `$COPILOT_HOME`, else `~/.copilot`.
    pub copilot: PathBuf,
    /// `$FACTORY_HOME_OVERRIDE/.factory`, else `~/.factory`.
    pub droid: PathBuf,
    /// `$QODER_CONFIG_DIR`, else `~/.qoder`.
    pub qoder: PathBuf,
    /// `$QODERCN_CONFIG_DIR`, else `~/.qoder-cn`.
    pub qoder_cn: PathBuf,
    /// `$CODEBUDDY_CONFIG_DIR`, else `~/.codebuddy`.
    pub codebuddy: PathBuf,
    /// `$CURSOR_CONFIG_DIR`, else `~/.cursor`.
    pub cursor: PathBuf,
    /// Where OpenCode's databases are: `$XDG_DATA_HOME/opencode`, else
    /// `~/.local/share/opencode` — on Windows too.
    pub opencode: PathBuf,
    /// `$OPENCODE_DB`: one database file, instead of those in `opencode`.
    pub opencode_db: Option<PathBuf>,
}

impl Roots {
    /// Every agent's default, under `home`.
    pub fn under(home: &Path) -> Self {
        Self {
            claude: home.join(".claude"),
            codex: home.join(".codex"),
            gemini: home.join(".gemini"),
            qwen: home.join(".qwen"),
            pi: home.join(".pi").join("agent"),
            omp: home.join(".omp").join("agent"),
            kimi: home.join(".kimi-code"),
            copilot: home.join(".copilot"),
            droid: home.join(".factory"),
            qoder: home.join(".qoder"),
            qoder_cn: home.join(".qoder-cn"),
            codebuddy: home.join(".codebuddy"),
            cursor: home.join(".cursor"),
            opencode: home.join(".local").join("share").join("opencode"),
            opencode_db: None,
        }
    }

    /// The defaults under `home`, moved wherever the environment says an
    /// agent keeps its history instead.
    pub fn from_env(home: &Path) -> Self {
        let var = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let mut roots = Self::under(home);
        // The same override the hooks installer honours
        // (`agent_hooks::claude_settings_path`): Claude Code keeps its
        // projects wherever it keeps its settings.
        if let Some(dir) = var("CLAUDE_CONFIG_DIR") {
            roots.claude = dir;
        }
        if let Some(dir) = var("CODEX_HOME") {
            roots.codex = dir;
        }
        // These two stand in for the home the agent's directory hangs off.
        if let Some(dir) = var("GEMINI_CLI_HOME") {
            roots.gemini = dir.join(".gemini");
        }
        if let Some(dir) = var("FACTORY_HOME_OVERRIDE") {
            roots.droid = dir.join(".factory");
        }
        if let Some(dir) = var("QWEN_HOME") {
            roots.qwen = dir;
        }
        if let Some(dir) = var("PI_CODING_AGENT_DIR") {
            roots.pi = dir;
        }
        if let Some(dir) = var("KIMI_CODE_HOME") {
            roots.kimi = dir;
        }
        if let Some(dir) = var("COPILOT_HOME") {
            roots.copilot = dir;
        }
        if let Some(dir) = var("QODER_CONFIG_DIR") {
            roots.qoder = dir;
        }
        if let Some(dir) = var("QODERCN_CONFIG_DIR") {
            roots.qoder_cn = dir;
        }
        if let Some(dir) = var("CODEBUDDY_CONFIG_DIR") {
            roots.codebuddy = dir;
        }
        if let Some(dir) = var("CURSOR_CONFIG_DIR") {
            roots.cursor = dir;
        }
        if let Some(dir) = var("XDG_DATA_HOME") {
            roots.opencode = dir.join("opencode");
        }
        roots.opencode_db = var("OPENCODE_DB");
        roots
    }

    /// This computer's agents, under the current user's home.
    pub fn local() -> Option<Self> {
        home().map(|home| Self::from_env(&home))
    }
}

/// Every past session found under `roots`, most recently used first.
///
/// `known_dirs` are directories the caller knows of — open tabs'. Cursor
/// records where a chat ran only as a hash of the directory, which one of
/// these, or a directory another agent's session ran in, may answer.
pub fn scan(roots: &Roots, known_dirs: &[PathBuf]) -> Vec<PastSession> {
    let mut files = claude_files(&roots.claude.join("projects"), CLIAgent::Claude);
    files.extend(codex_files(&roots.codex));
    files.extend(claude_files(
        &roots.qoder.join("projects"),
        CLIAgent::QoderCLI,
    ));
    files.extend(claude_files(
        &roots.qoder_cn.join("projects"),
        CLIAgent::QoderCLICn,
    ));
    files.extend(claude_files(
        &roots.codebuddy.join("projects"),
        CLIAgent::CodeBuddy,
    ));
    files.extend(qwen_files(&roots.qwen));
    files.extend(gemini_files(&roots.gemini));
    files.extend(pi_files(&roots.pi.join("sessions"), CLIAgent::Pi));
    files.extend(pi_files(&roots.omp.join("sessions"), CLIAgent::OhMyPi));
    files.extend(kimi_files(&roots.kimi));
    files.extend(copilot_files(&roots.copilot));
    files.extend(droid_files(&roots.droid));
    files.extend(cursor_files(&roots.cursor));
    files.sort_by_key(|f| std::cmp::Reverse(f.modified));
    files.truncate(MAX_SESSIONS);
    // Codex names a thread after the fact, in an index beside the sessions
    // rather than in the session's own file.
    let codex_titles = codex_thread_names(&roots.codex);

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
            None => read(&file),
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
    drop(cache);

    out.extend(opencode_sessions(roots));
    resolve_cursor_dirs(&mut out, known_dirs);
    out.sort_by_key(|s| std::cmp::Reverse(s.updated));
    out.truncate(MAX_SESSIONS);
    out
}

/// Fill in where each Cursor chat ran from the hash it is filed under, and
/// leave out those no known directory hashes to: resumed anywhere else, the
/// agent would not find them.
fn resolve_cursor_dirs(sessions: &mut Vec<PastSession>, known_dirs: &[PathBuf]) {
    if !sessions.iter().any(|s| s.agent == CLIAgent::Cursor) {
        return;
    }
    let mut by_hash: HashMap<String, PathBuf> = HashMap::new();
    let candidates = known_dirs
        .iter()
        .cloned()
        .chain(sessions.iter().filter_map(|s| s.cwd.clone()));
    for dir in candidates {
        by_hash.entry(cursor_dir_hash(&dir)).or_insert(dir);
    }
    sessions.retain_mut(|s| {
        if s.agent != CLIAgent::Cursor || s.cwd.is_some() {
            return true;
        }
        let Some(hash) = s.branch.take() else {
            return false;
        };
        s.cwd = by_hash.get(&hash).cloned();
        s.cwd.is_some()
    });
}

/// Cursor files a chat under the md5 of `path.resolve(cwd)`.
fn cursor_dir_hash(dir: &Path) -> String {
    format!("{:x}", md5::compute(dir.to_string_lossy().as_bytes()))
}

/// How a session is named in `hidden_agent_sessions`: ids are only unique
/// within one agent's history.
pub fn session_key(agent: CLIAgent, id: &str) -> String {
    format!("{}:{id}", agent.slug())
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

/// Parse the session `file` holds, whichever agent wrote it.
fn read(file: &Found) -> Option<PastSession> {
    let updated = unix(file.modified);
    match file.agent {
        CLIAgent::Codex => read_codex(&file.path, updated),
        CLIAgent::Claude | CLIAgent::QoderCLI | CLIAgent::QoderCLICn => {
            let id = file.path.file_stem()?.to_str()?.to_string();
            let (head, tail) = ends(&file.path, file.len).ok()?;
            let mut s = parse_claude(id, &head, &tail, updated)?;
            s.agent = file.agent;
            Some(s)
        }
        CLIAgent::CodeBuddy => {
            let id = file.path.file_stem()?.to_str()?.to_string();
            let (head, tail) = ends(&file.path, file.len).ok()?;
            parse_codebuddy(id, &head, &tail, updated)
        }
        CLIAgent::Qwen => {
            let id = file.path.file_stem()?.to_str()?.to_string();
            let (head, tail) = ends(&file.path, file.len).ok()?;
            parse_qwen(id, &head, &tail, updated)
        }
        CLIAgent::Gemini => {
            let (head, tail) = ends(&file.path, file.len).ok()?;
            // `tmp/<project>/chats/<file>`: the project's directory is
            // written beside its chats, not in them.
            let cwd = file
                .path
                .parent()
                .and_then(Path::parent)
                .and_then(|project| std::fs::read_to_string(project.join(".project_root")).ok())
                .map(|root| PathBuf::from(root.trim()))
                .filter(|root| !root.as_os_str().is_empty());
            parse_gemini(&head, &tail, cwd, updated)
        }
        CLIAgent::Pi | CLIAgent::OhMyPi => {
            let (head, tail) = ends(&file.path, file.len).ok()?;
            parse_pi(file.agent, &head, &tail, updated)
        }
        CLIAgent::Kimi => {
            let text = std::fs::read_to_string(&file.path).ok()?;
            let dir = file.path.parent()?.file_name()?.to_str()?;
            parse_kimi(dir, &text, updated)
        }
        CLIAgent::Copilot => {
            let (head, _) = ends(&file.path, file.len).ok()?;
            let workspace = file
                .path
                .parent()
                .and_then(|dir| std::fs::read_to_string(dir.join("workspace.yaml")).ok());
            parse_copilot(&head, workspace.as_deref(), updated)
        }
        CLIAgent::Droid => {
            let (head, _) = ends(&file.path, file.len).ok()?;
            parse_droid(&head, updated)
        }
        CLIAgent::Cursor => read_cursor(&file.path, updated),
        _ => None,
    }
}

fn found(agent: CLIAgent, path: PathBuf, meta: &std::fs::Metadata) -> Found {
    Found {
        agent,
        path,
        len: meta.len(),
        modified: meta.modified().unwrap_or(UNIX_EPOCH),
    }
}

/// The files directly in `dir` that `keep` accepts by name.
fn files_in(dir: &Path, keep: impl Fn(&str) -> bool) -> Vec<(PathBuf, std::fs::Metadata)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(&keep))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file().then(|| (e.path(), meta))
        })
        .collect()
}

/// The directories directly in `dir`.
fn dirs_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
}

fn is_jsonl(name: &str) -> bool {
    name.ends_with(".jsonl")
}

/// Qwen Code: `projects/<cwd as dashes>/chats/<id>.jsonl`. Archived
/// sessions are a directory further down, and stay out.
fn qwen_files(root: &Path) -> Vec<Found> {
    dirs_in(&root.join("projects"))
        .into_iter()
        .flat_map(|project| files_in(&project.join("chats"), is_jsonl))
        .map(|(path, meta)| found(CLIAgent::Qwen, path, &meta))
        .collect()
}

/// Gemini CLI: `tmp/<project>/chats/session-<time>-<id prefix>.jsonl`. A
/// subagent's chat sits a directory further down, under its parent's id.
fn gemini_files(root: &Path) -> Vec<Found> {
    dirs_in(&root.join("tmp"))
        .into_iter()
        .flat_map(|project| {
            files_in(&project.join("chats"), |n| {
                n.starts_with("session-") && is_jsonl(n)
            })
        })
        .map(|(path, meta)| found(CLIAgent::Gemini, path, &meta))
        .collect()
}

/// Pi and Oh My Pi: `sessions/<cwd>/<time>_<id>.jsonl`. Oh My Pi keeps a
/// session's subagent transcripts in a directory beside it.
fn pi_files(sessions: &Path, agent: CLIAgent) -> Vec<Found> {
    dirs_in(sessions)
        .into_iter()
        .flat_map(|project| files_in(&project, is_jsonl))
        .map(|(path, meta)| found(agent, path, &meta))
        .collect()
}

/// Kimi Code: `sessions/wd_<dir>_<hash>/<session>/state.json`.
fn kimi_files(root: &Path) -> Vec<Found> {
    dirs_in(&root.join("sessions"))
        .into_iter()
        .flat_map(|project| dirs_in(&project))
        .flat_map(|session| files_in(&session, |n| n == "state.json"))
        .map(|(path, meta)| found(CLIAgent::Kimi, path, &meta))
        .collect()
}

/// Copilot CLI: `session-state/<id>/events.jsonl`, with `workspace.yaml`
/// beside it.
fn copilot_files(root: &Path) -> Vec<Found> {
    dirs_in(&root.join("session-state"))
        .into_iter()
        .flat_map(|session| files_in(&session, |n| n == "events.jsonl"))
        .map(|(path, meta)| found(CLIAgent::Copilot, path, &meta))
        .collect()
}

/// Cursor CLI: `chats/<md5 of the directory>/<chat id>/meta.json`, with the
/// chat itself in `store.db` beside it.
fn cursor_files(root: &Path) -> Vec<Found> {
    dirs_in(&root.join("chats"))
        .into_iter()
        .flat_map(|project| dirs_in(&project))
        .flat_map(|chat| files_in(&chat, |n| n == "meta.json"))
        .map(|(path, meta)| found(CLIAgent::Cursor, path, &meta))
        .collect()
}

/// A Cursor chat from its `meta.json`, and its name from `store.db` when
/// the meta has none. A chat whose meta does not say where it ran carries
/// its directory's hash in `branch` until [`resolve_cursor_dirs`] turns it
/// into a path — Cursor records no branch.
fn read_cursor(meta_path: &Path, updated: u64) -> Option<PastSession> {
    let chat = meta_path.parent()?;
    let id = chat.file_name()?.to_str()?.to_string();
    let hash = chat.parent()?.file_name()?.to_str()?.to_string();
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(meta_path).ok()?).ok()?;
    let title = str_field(&meta, "title")
        .map(str::to_string)
        .or_else(|| cursor_store_name(&chat.join("store.db")));
    parse_cursor(id, hash, &meta, title, updated)
}

pub(crate) fn parse_cursor(
    id: String,
    hash: String,
    meta: &Value,
    title: Option<String>,
    updated: u64,
) -> Option<PastSession> {
    let flag = |key: &str| meta.get(key).and_then(Value::as_bool);
    if flag("isSubagent") == Some(true) || flag("hasConversation") == Some(false) {
        return None;
    }
    let cwd = str_field(meta, "cwd").map(PathBuf::from);
    let updated = meta
        .get("updatedAtMs")
        .and_then(Value::as_u64)
        .map_or(updated, |ms| ms / 1000);
    Some(PastSession {
        agent: CLIAgent::Cursor,
        id,
        branch: cwd.is_none().then_some(hash),
        cwd,
        title: real_title(title.as_deref())?,
        updated,
    })
}

/// The chat's name from `store.db`: its `meta` row `0`, hex-encoded JSON.
fn cursor_store_name(db: &Path) -> Option<String> {
    let conn = open_read_only(db)?;
    let value: String = conn
        .query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
            row.get(0)
        })
        .ok()?;
    let json: Value = serde_json::from_slice(&hex::decode(value.trim()).ok()?).ok()?;
    str_field(&json, "name").map(str::to_string)
}

/// `path` opened for reading only. An agent may be writing to it right now,
/// in WAL mode; when the read-only open cannot share its log, the file is
/// read as it stands on disk instead.
fn open_read_only(path: &Path) -> Option<rusqlite::Connection> {
    use rusqlite::{Connection, OpenFlags};
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let usable = |conn: Connection| {
        conn.busy_timeout(std::time::Duration::from_millis(500))
            .ok()?;
        conn.query_row("SELECT 1", [], |_| Ok(())).ok()?;
        Some(conn)
    };
    if let Some(conn) = Connection::open_with_flags(path, flags)
        .ok()
        .and_then(usable)
    {
        return Some(conn);
    }
    Connection::open_with_flags(immutable_uri(path), flags)
        .ok()
        .and_then(usable)
}

/// `path` as a SQLite URI that reads the file as it stands, taking no locks
/// and never looking for a log. What SQLite would read as URI syntax in the
/// path — `%`, `?`, `#` — is escaped.
fn immutable_uri(path: &Path) -> String {
    // `%` first: it is the escape the other two are spelled in.
    let path = path
        .to_string_lossy()
        .replace('\\', "/")
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    // An empty authority and an absolute path: `file:///C:/x` on Windows,
    // where SQLite drops the `/` before the drive letter; `file:C:/x` would
    // not name that file.
    let slash = if path.starts_with('/') { "" } else { "/" };
    format!("file://{slash}{path}?immutable=1")
}

/// OpenCode's databases: `$OPENCODE_DB`, else every `opencode*.db` in its
/// data directory (release and other channels).
fn opencode_dbs(roots: &Roots) -> Vec<PathBuf> {
    if let Some(db) = &roots.opencode_db {
        return vec![db.clone()];
    }
    files_in(&roots.opencode, |n| {
        n.starts_with("opencode") && n.ends_with(".db")
    })
    .into_iter()
    .map(|(path, _)| path)
    .collect()
}

/// A database's stamp: it and its write-ahead log's length and time. The
/// log is where recent writes land, so the database alone would read as
/// unchanged.
fn db_stamp(db: &Path) -> Option<(u64, SystemTime, u64, SystemTime)> {
    let meta = std::fs::metadata(db).ok()?;
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    let wal = std::fs::metadata(PathBuf::from(wal)).ok();
    Some((
        meta.len(),
        meta.modified().unwrap_or(UNIX_EPOCH),
        wal.as_ref().map_or(0, |m| m.len()),
        wal.and_then(|m| m.modified().ok()).unwrap_or(UNIX_EPOCH),
    ))
}

type DbStamp = (u64, SystemTime, u64, SystemTime);

static DB_CACHE: LazyLock<Mutex<HashMap<PathBuf, (DbStamp, Vec<PastSession>)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn opencode_sessions(roots: &Roots) -> Vec<PastSession> {
    let mut cache = DB_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    for db in opencode_dbs(roots) {
        let Some(stamp) = db_stamp(&db) else { continue };
        if let Some((seen, sessions)) = cache.get(&db)
            && *seen == stamp
        {
            out.extend(sessions.iter().cloned());
            continue;
        }
        let sessions = open_read_only(&db)
            .map(|conn| read_opencode(&conn))
            .unwrap_or_default();
        out.extend(sessions.iter().cloned());
        cache.insert(db, (stamp, sessions));
    }
    out
}

/// OpenCode's sessions, less subagents and archived ones. A session still
/// under its placeholder title (`New session - <time>`) is named by its
/// first prompt.
pub(crate) fn read_opencode(conn: &rusqlite::Connection) -> Vec<PastSession> {
    let rows = conn
        .prepare(
            "SELECT id, directory, title, time_updated FROM session \
             WHERE parent_id IS NULL AND time_archived IS NULL \
             ORDER BY time_updated DESC LIMIT ?1",
        )
        .and_then(|mut stmt| {
            stmt.query_map([MAX_SESSIONS as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()
        });
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => {
            log::warn!("reading OpenCode sessions: {e}");
            return Vec::new();
        }
    };
    rows.into_iter()
        .filter_map(|(id, dir, title, updated)| {
            let title = title
                .filter(|t| !t.starts_with("New session - ") && !t.starts_with("Child session - "))
                .and_then(|t| real_title(Some(t.trim())))
                .or_else(|| opencode_first_prompt(conn, &id))?;
            Some(PastSession {
                agent: CLIAgent::OpenCode,
                cwd: dir.filter(|d| !d.is_empty()).map(PathBuf::from),
                id,
                title,
                branch: None,
                updated: updated.map_or(0, |ms| (ms / 1000).max(0) as u64),
            })
        })
        .collect()
}

fn opencode_first_prompt(conn: &rusqlite::Connection, session: &str) -> Option<String> {
    let mut stmt = conn
        .prepare(
            "SELECT p.data FROM part p JOIN message m ON m.id = p.message_id \
             WHERE m.session_id = ?1 AND json_extract(m.data, '$.role') = 'user' \
             ORDER BY m.time_created, p.id LIMIT 20",
        )
        .ok()?;
    let parts = stmt
        .query_map([session], |row| row.get::<_, String>(0))
        .ok()?;
    parts.flatten().find_map(|data| {
        let part: Value = serde_json::from_str(&data).ok()?;
        if str_field(&part, "type") != Some("text")
            || part.get("synthetic").and_then(Value::as_bool) == Some(true)
        {
            return None;
        }
        str_field(&part, "text").and_then(prompt_title)
    })
}

/// Droid: `sessions/<cwd as dashes>/<id>.jsonl`, or directly in
/// `sessions/` for older ones.
fn droid_files(root: &Path) -> Vec<Found> {
    let sessions = root.join("sessions");
    let mut out: Vec<Found> = files_in(&sessions, is_jsonl)
        .into_iter()
        .map(|(path, meta)| found(CLIAgent::Droid, path, &meta))
        .collect();
    out.extend(
        dirs_in(&sessions)
            .into_iter()
            .flat_map(|project| files_in(&project, is_jsonl))
            .map(|(path, meta)| found(CLIAgent::Droid, path, &meta)),
    );
    out
}

/// Claude Code keeps one `<session id>.jsonl` per session under
/// `~/.claude/projects/<cwd with separators as dashes>/`, and Qoder and
/// CodeBuddy copied the layout. The directories beside those files hold a
/// session's subagent transcripts, which are not sessions of their own.
fn claude_files(root: &Path, agent: CLIAgent) -> Vec<Found> {
    let Ok(projects) = std::fs::read_dir(root) else {
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
                agent,
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
    let mut line = Vec::new();
    // Bytes, not `read_line`: the cap can cut a multi-byte character in
    // half, and `read_line` fails the whole read on that — losing a session
    // whose metadata was already in.
    while reader.read_until(b'\n', &mut line).ok()? > 0 {
        let line_text = String::from_utf8_lossy(&line);
        let prompt = line_text.contains("\"user_message\"");
        head.push_str(&line_text);
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
        // Qoder marks a compaction's summary and transcript-only lines.
        || record.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
        || record.get("isVisibleInTranscriptOnly").and_then(Value::as_bool) == Some(true)
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
    let text = strip_injected(text);
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

/// `text` without the context blocks agents put in front of what was typed
/// (`<system-reminder>…</system-reminder>` and its kin).
fn strip_injected(text: &str) -> &str {
    const TAGS: [&str; 5] = [
        "system-reminder",
        "system_reminder",
        "hook_context",
        "loaded_context",
        "session_context",
    ];
    let mut text = text.trim();
    'strip: loop {
        for tag in TAGS {
            let open = format!("<{tag}>");
            let close = format!("</{tag}>");
            if let Some(rest) = text.strip_prefix(open.as_str())
                && let Some(end) = rest.find(close.as_str())
            {
                text = rest[end + close.len()..].trim_start();
                continue 'strip;
            }
        }
        return text;
    }
}

/// The text of a message's `content`: a string, or the first text part of a
/// list (`{"type": "text" | "input_text", "text"}`, or a bare `{"text"}`).
fn content_text(content: &Value) -> Option<&str> {
    match content {
        Value::String(s) => Some(s),
        Value::Array(parts) => parts.iter().find_map(|p| {
            let kind = p.get("type").and_then(Value::as_str);
            matches!(kind, None | Some("text" | "input_text"))
                .then(|| p.get("text").and_then(Value::as_str))
                .flatten()
        }),
        _ => None,
    }
}

/// A title an agent wrote, or `None` for the placeholder it writes before
/// it has one.
fn real_title(title: Option<&str>) -> Option<String> {
    title
        .filter(|t| !matches!(*t, "New Session" | "Untitled"))
        .map(str::to_string)
}

/// Qwen Code: Claude Code's layout with its own record shapes — the prompt
/// in `message.parts`, a name in a `system`/`custom_title` record.
pub(crate) fn parse_qwen(id: String, head: &str, tail: &str, updated: u64) -> Option<PastSession> {
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
        // What was typed has no subtype; cron, goals and notifications do.
        if first_prompt.is_none()
            && str_field(&record, "type") == Some("user")
            && record.get("subtype").is_none()
        {
            let shown = record
                .get("systemPayload")
                .and_then(|p| str_field(p, "displayText"));
            let typed = record
                .get("message")
                .and_then(|m| m.get("parts"))
                .and_then(content_text);
            first_prompt = shown.or(typed).and_then(prompt_title);
        }
    }
    let named = records(tail)
        .rev()
        .chain(records(head).rev())
        .find_map(|r| {
            (str_field(&r, "type") == Some("system")
                && str_field(&r, "subtype") == Some("custom_title"))
            .then(|| {
                r.get("systemPayload")
                    .and_then(|p| str_field(p, "customTitle"))
            })
            .flatten()
            .map(str::to_string)
        });
    Some(PastSession {
        agent: CLIAgent::Qwen,
        id,
        cwd,
        title: named.or(first_prompt)?,
        branch,
        updated,
    })
}

/// CodeBuddy: Claude Code's directories, OpenAI-style items in them.
pub(crate) fn parse_codebuddy(
    id: String,
    head: &str,
    tail: &str,
    updated: u64,
) -> Option<PastSession> {
    let mut cwd = None;
    let mut first_prompt = None;
    for record in records(head) {
        if cwd.is_none() {
            cwd = str_field(&record, "cwd").map(PathBuf::from);
        }
        if first_prompt.is_some()
            || str_field(&record, "type") != Some("message")
            || str_field(&record, "role") != Some("user")
        {
            continue;
        }
        let injected = record.get("providerData").is_some_and(|d| {
            ["isMeta", "skipRun", "isCompactInternal"]
                .iter()
                .any(|k| d.get(*k).and_then(Value::as_bool) == Some(true))
                || str_field(d, "agent") == Some("compact")
                || d.get("teammateMessage").is_some()
        });
        if !injected {
            first_prompt = record
                .get("content")
                .and_then(content_text)
                .and_then(prompt_title);
        }
    }
    let mut named = None;
    let mut titled = None;
    for record in records(tail).rev().chain(records(head).rev()) {
        match str_field(&record, "type") {
            Some("custom-title") if named.is_none() => {
                named = str_field(&record, "customTitle").map(str::to_string);
            }
            Some("ai-title") if titled.is_none() => {
                titled = str_field(&record, "aiTitle").map(str::to_string);
            }
            Some("topic") if titled.is_none() => {
                titled = str_field(&record, "topic").map(str::to_string);
            }
            _ => {}
        }
    }
    Some(PastSession {
        agent: CLIAgent::CodeBuddy,
        id,
        cwd,
        title: named.or(titled).or(first_prompt)?,
        branch: None,
        updated,
    })
}

/// Gemini CLI: an event log. The first line carries the id, `$set` lines
/// update it and name the session, and the file does not say where it ran
/// — `cwd` comes from beside it.
pub(crate) fn parse_gemini(
    head: &str,
    tail: &str,
    cwd: Option<PathBuf>,
    updated: u64,
) -> Option<PastSession> {
    let mut id = None;
    let mut summary = None;
    let mut first_prompt = None;
    for record in records(head).chain(records(tail)) {
        let set = record.get("$set").unwrap_or(&record);
        if let Some(sid) = str_field(set, "sessionId") {
            id = Some(sid.to_string());
        }
        if let Some(s) = record.get("$set").and_then(|s| str_field(s, "summary")) {
            summary = Some(s.to_string());
        }
        if first_prompt.is_none() && str_field(&record, "type") == Some("user") {
            first_prompt = record
                .get("content")
                .and_then(content_text)
                // Slash commands and `?` help are talking to the CLI.
                .filter(|t| !t.trim_start().starts_with(['/', '?']))
                .and_then(prompt_title);
        }
    }
    Some(PastSession {
        agent: CLIAgent::Gemini,
        id: id?,
        cwd,
        title: summary.or(first_prompt)?,
        branch: None,
        updated,
    })
}

/// Pi and Oh My Pi: a `session` header (after Oh My Pi's fixed-width
/// `title` line, when it has one), then the conversation.
pub(crate) fn parse_pi(
    agent: CLIAgent,
    head: &str,
    tail: &str,
    updated: u64,
) -> Option<PastSession> {
    let mut id = None;
    let mut cwd = None;
    let mut titled = None;
    let mut first_prompt = None;
    for record in records(head) {
        match str_field(&record, "type") {
            Some("title") if titled.is_none() => titled = real_title(str_field(&record, "title")),
            Some("session") if id.is_none() => {
                id = str_field(&record, "id").map(str::to_string);
                cwd = str_field(&record, "cwd").map(PathBuf::from);
                if titled.is_none() {
                    titled = real_title(str_field(&record, "title"));
                }
            }
            Some("message") if first_prompt.is_none() => {
                let message = record.get("message");
                if message.and_then(|m| str_field(m, "role")) == Some("user") {
                    first_prompt = message
                        .and_then(|m| m.get("content"))
                        .and_then(content_text)
                        .and_then(prompt_title);
                }
            }
            _ => {}
        }
    }
    // A name given in the session outranks the title line, which Oh My Pi
    // rewrites in place and so is already the newest.
    let mut named = None;
    let mut summary = None;
    for record in records(tail).rev().chain(records(head).rev()) {
        match str_field(&record, "type") {
            Some("session_info") if named.is_none() => {
                named = str_field(&record, "name").map(str::to_string);
            }
            Some("compaction") if summary.is_none() => {
                summary = str_field(&record, "shortSummary").map(str::to_string);
            }
            _ => {}
        }
    }
    Some(PastSession {
        agent,
        id: id?,
        cwd,
        title: named.or(titled).or(summary).or(first_prompt)?,
        branch: None,
        updated,
    })
}

/// Kimi Code: a session is a directory, `state.json` its summary. Both
/// shapes the file has had are read.
pub(crate) fn parse_kimi(dir: &str, text: &str, updated: u64) -> Option<PastSession> {
    let state: Value = serde_json::from_str(text).ok()?;
    if state.get("archived").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let title = real_title(str_field(&state, "title"))
        .or_else(|| str_field(&state, "lastPrompt").and_then(prompt_title))?;
    Some(PastSession {
        agent: CLIAgent::Kimi,
        id: str_field(&state, "id").unwrap_or(dir).to_string(),
        cwd: str_field(&state, "cwd")
            .or_else(|| str_field(&state, "workDir"))
            .map(PathBuf::from),
        title,
        branch: None,
        updated,
    })
}

/// Copilot CLI: `session.start` says where and on what branch; a name, if
/// the session has one, is in `workspace.yaml`.
pub(crate) fn parse_copilot(
    head: &str,
    workspace: Option<&str>,
    updated: u64,
) -> Option<PastSession> {
    let workspace: Option<serde_yaml::Value> = workspace.and_then(|w| serde_yaml::from_str(w).ok());
    let yaml = |key: &str| {
        workspace
            .as_ref()
            .and_then(|w| w.get(key))
            .and_then(serde_yaml::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let mut id = yaml("id");
    let mut cwd = yaml("cwd").map(PathBuf::from);
    let mut branch = yaml("branch");
    let mut first_prompt = None;
    for record in records(head) {
        let Some(data) = record.get("data") else {
            continue;
        };
        match str_field(&record, "type") {
            Some("session.start") => {
                id = id.or_else(|| str_field(data, "sessionId").map(str::to_string));
                let context = data.get("context");
                cwd = cwd.or_else(|| context.and_then(|c| str_field(c, "cwd")).map(PathBuf::from));
                branch = branch.or_else(|| {
                    context
                        .and_then(|c| str_field(c, "branch"))
                        .map(str::to_string)
                });
            }
            // What was typed has no source; autopilot and subagents do.
            Some("user.message")
                if first_prompt.is_none()
                    && str_field(data, "source").is_none()
                    && data.get("isAutopilotContinuation").and_then(Value::as_bool)
                        != Some(true)
                    && record.get("agentId").is_none() =>
            {
                first_prompt = str_field(data, "content")
                    .map(|t| {
                        ["<system_reminder>", "<reminder>"]
                            .iter()
                            .filter_map(|tag| t.find(tag))
                            .min()
                            .map_or(t, |at| &t[..at])
                    })
                    .and_then(prompt_title);
            }
            _ => {}
        }
    }
    Some(PastSession {
        agent: CLIAgent::Copilot,
        id: id?,
        cwd,
        title: yaml("name").or(first_prompt)?,
        branch,
        updated,
    })
}

/// Droid: a `session_start` line with the id, title and directory, then the
/// conversation.
pub(crate) fn parse_droid(head: &str, updated: u64) -> Option<PastSession> {
    let mut start = None;
    let mut first_prompt = None;
    for record in records(head) {
        match str_field(&record, "type") {
            Some("session_start") if start.is_none() => start = Some(record),
            Some("message") if first_prompt.is_none() && record.get("hookEventName").is_none() => {
                let message = record.get("message");
                if message.and_then(|m| str_field(m, "role")) == Some("user") {
                    first_prompt = message
                        .and_then(|m| m.get("content"))
                        .and_then(content_text)
                        .and_then(prompt_title);
                }
            }
            _ => {}
        }
        if start.is_some() && first_prompt.is_some() {
            break;
        }
    }
    let start = start?;
    Some(PastSession {
        agent: CLIAgent::Droid,
        id: str_field(&start, "id")?.to_string(),
        cwd: str_field(&start, "cwd").map(PathBuf::from),
        title: real_title(str_field(&start, "title")).or(first_prompt)?,
        branch: None,
        updated,
    })
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
        let found = scan(&Roots::under(home.path()), &[]);
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
        let found = scan(&Roots::under(home.path()), &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Retry logic");
    }

    use serde_json::json;

    fn lines(records: &[serde_json::Value]) -> String {
        records.iter().map(|r| line(r.clone())).collect()
    }

    #[test]
    fn injected_context_in_front_of_a_prompt_is_not_its_title() {
        assert_eq!(
            prompt_title("<system-reminder>\ntoday is Monday\n</system-reminder>\nfix the build"),
            Some("fix the build".into())
        );
        assert_eq!(prompt_title("<command-name>/clear</command-name>"), None);
    }

    #[test]
    fn qwen_takes_its_name_from_a_custom_title_record_and_skips_cron_turns() {
        let head = lines(&[
            json!({"type": "user", "subtype": "cron", "cwd": "/q", "gitBranch": "dev",
                "message": {"role": "user", "parts": [{"text": "scheduled"}]}}),
            json!({"type": "user", "cwd": "/q",
                "message": {"role": "user", "parts": [{"text": "add tests"}]}}),
        ]);
        let s = parse_qwen("u1".into(), &head, "", 0).unwrap();
        assert_eq!(
            (s.agent, s.title.as_str(), s.branch.as_deref()),
            (CLIAgent::Qwen, "add tests", Some("dev"))
        );
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/q")));
        let tail = line(json!({"type": "system", "subtype": "custom_title",
            "systemPayload": {"customTitle": "Test work"}}));
        assert_eq!(
            parse_qwen("u1".into(), &head, &tail, 0).unwrap().title,
            "Test work"
        );
    }

    #[test]
    fn codebuddy_reads_openai_style_items() {
        let head = lines(&[
            json!({"type": "message", "role": "user", "cwd": "/cb",
                "content": [{"type": "input_text", "text": "internal"}],
                "providerData": {"isMeta": true}}),
            json!({"type": "message", "role": "user", "cwd": "/cb",
                "content": [{"type": "input_text", "text": "refactor auth"}]}),
        ]);
        let s = parse_codebuddy("c1".into(), &head, "", 0).unwrap();
        assert_eq!(
            (s.agent, s.title.as_str()),
            (CLIAgent::CodeBuddy, "refactor auth")
        );
        let tail = line(json!({"type": "ai-title", "aiTitle": "Auth refactor"}));
        assert_eq!(
            parse_codebuddy("c1".into(), &head, &tail, 0).unwrap().title,
            "Auth refactor"
        );
    }

    #[test]
    fn gemini_takes_the_id_from_the_log_and_a_summary_over_the_prompt() {
        let head = lines(&[
            json!({"sessionId": "g-full-id", "projectHash": "h", "startTime": "t"}),
            json!({"id": "m1", "type": "user", "content": [{"text": "/help"}]}),
            json!({"id": "m2", "type": "user", "content": [{"text": "explain the parser"}]}),
        ]);
        let s = parse_gemini(&head, "", Some("/g".into()), 0).unwrap();
        assert_eq!(
            (s.id.as_str(), s.title.as_str()),
            ("g-full-id", "explain the parser")
        );
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/g")));
        let tail = line(json!({"$set": {"summary": "Parser walkthrough"}}));
        assert_eq!(
            parse_gemini(&head, &tail, None, 0).unwrap().title,
            "Parser walkthrough"
        );
    }

    #[test]
    fn pi_and_oh_my_pi_read_the_header_after_an_optional_title_line() {
        let pi = lines(&[
            json!({"type": "session", "version": 3, "id": "p1", "cwd": "/pi"}),
            json!({"type": "custom_message", "content": "extension text"}),
            json!({"type": "message", "message": {"role": "user", "content": "write docs"}}),
        ]);
        let s = parse_pi(CLIAgent::Pi, &pi, "", 0).unwrap();
        assert_eq!((s.id.as_str(), s.title.as_str()), ("p1", "write docs"));
        let named = line(json!({"type": "session_info", "name": "Docs pass"}));
        assert_eq!(
            parse_pi(CLIAgent::Pi, &pi, &named, 0).unwrap().title,
            "Docs pass"
        );

        let omp = format!(
            "{}{pi}",
            line(json!({"type": "title", "v": 1, "title": "Auto title", "source": "auto"}))
        );
        let s = parse_pi(CLIAgent::OhMyPi, &omp, "", 0).unwrap();
        assert_eq!(
            (s.agent, s.title.as_str()),
            (CLIAgent::OhMyPi, "Auto title")
        );
    }

    #[test]
    fn kimi_reads_both_state_shapes_and_leaves_archived_ones_out() {
        let v2 = json!({"id": "session_k1", "cwd": "/k", "title": "New Session",
            "lastPrompt": "port the tests", "updatedAt": 1})
        .to_string();
        let s = parse_kimi("session_k1", &v2, 0).unwrap();
        assert_eq!(
            (s.id.as_str(), s.title.as_str()),
            ("session_k1", "port the tests")
        );
        let v0 = json!({"title": "Porting", "workDir": "/k0"}).to_string();
        let s = parse_kimi("dir-id", &v0, 0).unwrap();
        assert_eq!(
            (s.id.as_str(), s.cwd.as_deref()),
            ("dir-id", Some(Path::new("/k0")))
        );
        let archived = json!({"id": "x", "title": "t", "archived": true}).to_string();
        assert!(parse_kimi("x", &archived, 0).is_none());
    }

    #[test]
    fn copilot_prefers_the_workspace_name_and_skips_autopilot_turns() {
        let head = lines(&[
            json!({"type": "session.start", "data": {"sessionId": "cp1",
                "context": {"cwd": "/cp", "branch": "main"}}}),
            json!({"type": "user.message", "data": {"content": "go on",
                "isAutopilotContinuation": true}}),
            json!({"type": "user.message",
                "data": {"content": "bump deps<system_reminder>x</system_reminder>"}}),
        ]);
        let s = parse_copilot(&head, None, 0).unwrap();
        assert_eq!(
            (s.id.as_str(), s.title.as_str(), s.branch.as_deref()),
            ("cp1", "bump deps", Some("main"))
        );
        let s = parse_copilot(&head, Some("id: cp1\nname: Dependency bump\n"), 0).unwrap();
        assert_eq!(s.title, "Dependency bump");
    }

    #[test]
    fn droid_reads_its_session_start_line() {
        let head = lines(&[
            json!({"type": "session_start", "id": "d1", "title": "", "cwd": "/d", "version": 2}),
            json!({"type": "message", "message": {"role": "user",
                "content": [{"type": "text", "text": "triage the issue"}]}}),
        ]);
        let s = parse_droid(&head, 0).unwrap();
        assert_eq!(
            (s.id.as_str(), s.title.as_str()),
            ("d1", "triage the issue")
        );
    }

    fn opencode_db(path: &Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT,
                directory TEXT, title TEXT, time_created INTEGER, time_updated INTEGER,
                time_archived INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT,
                time_created INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
                data TEXT);
             INSERT INTO session VALUES
                ('ses_named', 'p', NULL, 'C:/oc', 'Fix login', 0, 2000, NULL),
                ('ses_default', 'p', NULL, '/oc', 'New session - 2026-09-27T10:00:00.000Z',
                    0, 5000, NULL),
                ('ses_child', 'p', 'ses_named', '/oc', 'sub', 0, 9000, NULL),
                ('ses_archived', 'p', NULL, '/oc', 'old', 0, 9000, 1);
             INSERT INTO message VALUES
                ('m1', 'ses_default', 1, '{\"role\":\"user\"}');
             INSERT INTO part VALUES
                ('a', 'm1', 'ses_default', '{\"type\":\"text\",\"text\":\"ctx\",\"synthetic\":true}'),
                ('b', 'm1', 'ses_default', '{\"type\":\"text\",\"text\":\"add dark mode\"}');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn opencode_lists_top_level_sessions_and_names_placeholders_by_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let conn = opencode_db(&dir.path().join("opencode.db"));
        let found: Vec<(String, String, u64)> = read_opencode(&conn)
            .into_iter()
            .map(|s| (s.id, s.title, s.updated))
            .collect();
        assert_eq!(
            found,
            vec![
                ("ses_default".into(), "add dark mode".into(), 5),
                ("ses_named".into(), "Fix login".into(), 2),
            ]
        );
    }

    #[test]
    fn cursor_chats_are_placed_by_the_hash_of_a_known_directory() {
        let home = tempfile::tempdir().unwrap();
        let known = home.path().join("repo");
        let hash = cursor_dir_hash(&known);
        let chat = home
            .path()
            .join(".cursor/chats")
            .join(&hash)
            .join("agent-1");
        std::fs::create_dir_all(&chat).unwrap();
        std::fs::write(
            chat.join("meta.json"),
            json!({"schemaVersion": 1, "hasConversation": true, "title": "Cursor chat",
                "updatedAtMs": 7000})
            .to_string(),
        )
        .unwrap();
        let other = home.path().join(".cursor/chats/ffff/agent-2");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join("meta.json"),
            json!({"title": "lost"}).to_string(),
        )
        .unwrap();

        let roots = Roots::under(home.path());
        assert!(scan(&roots, &[]).is_empty(), "nowhere to resume either");
        let found = scan(&roots, &[known.clone()]);
        assert_eq!(found.len(), 1);
        assert_eq!(
            (
                found[0].id.as_str(),
                found[0].cwd.as_deref(),
                found[0].branch.as_deref()
            ),
            ("agent-1", Some(known.as_path()), None)
        );
        assert_eq!(found[0].updated, 7);
    }

    #[test]
    fn the_immutable_fallback_opens_a_path_with_uri_characters_in_it() {
        let dir = tempfile::tempdir().unwrap();
        // `?` is not allowed in a Windows file name.
        let odd = dir.path().join(if cfg!(windows) {
            "50% #1 x"
        } else {
            "50% #1 ?x"
        });
        std::fs::create_dir_all(&odd).unwrap();
        let db = odd.join("opencode.db");
        drop(opencode_db(&db));
        let flags =
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI;
        let conn = rusqlite::Connection::open_with_flags(immutable_uri(&db), flags).unwrap();
        assert_eq!(read_opencode(&conn).len(), 2);
    }

    #[test]
    fn a_codex_rollout_cut_inside_a_character_still_lists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let meta = codex_meta(json!({"title": "named"}));
        // A second record long enough that the head cap lands one byte into
        // a two-byte character, with no prompt before it.
        let filler = CODEX_HEAD_BYTES as usize - meta.len() - 1;
        let long = format!("{}{}\n", "a".repeat(filler), "é".repeat(8));
        std::fs::write(&path, format!("{meta}{long}")).unwrap();
        let s = read_codex(&path, 0).expect("the metadata was read");
        assert_eq!((s.id.as_str(), s.title.as_str()), ("0199-abc", "named"));
    }

    #[test]
    fn a_cursor_chat_without_a_title_is_named_from_its_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("store.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        let value = hex::encode(json!({"agentId": "a", "name": "Stored name"}).to_string());
        conn.execute_batch(&format!(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO meta VALUES ('0', '{value}');"
        ))
        .unwrap();
        drop(conn);
        assert_eq!(cursor_store_name(&db).as_deref(), Some("Stored name"));
    }

    #[test]
    fn scan_finds_each_agents_sessions_where_it_keeps_them() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let write = |rel: &str, text: String| {
            let path = h.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        let claude_like = user("hello");
        write(".qoder/projects/-q/q1.jsonl", claude_like);
        write(".qoder-cn/projects/-q/q2.jsonl", user("hello cn"));
        write(
            ".qwen/projects/q/chats/w1.jsonl",
            line(json!({"type": "user", "message": {"parts": [{"text": "hi qwen"}]}})),
        );
        write(".qwen/projects/q/chats/archive/old.jsonl", String::new());
        write(
            ".gemini/tmp/proj/chats/session-2026-09-27T10-00-abcd1234.jsonl",
            lines(&[
                json!({"sessionId": "abcd1234-full"}),
                json!({"type": "user", "content": "hi gemini"}),
            ]),
        );
        write(".gemini/tmp/proj/.project_root", "/gem\n".into());
        write(
            ".pi/agent/sessions/--pi--/t_p1.jsonl",
            lines(&[
                json!({"type": "session", "id": "p1", "cwd": "/pi"}),
                json!({"type": "message", "message": {"role": "user", "content": "hi pi"}}),
            ]),
        );
        write(
            ".omp/agent/sessions/-o/t_o1/sub.jsonl",
            line(json!({"type": "session", "id": "sub"})),
        );
        write(
            ".kimi-code/sessions/wd_x_123/session_k/state.json",
            json!({"title": "hi kimi"}).to_string(),
        );
        write(
            ".copilot/session-state/cp/events.jsonl",
            line(json!({"type": "session.start", "data": {"sessionId": "cp"}})),
        );
        write(
            ".copilot/session-state/cp/workspace.yaml",
            "name: hi copilot\n".into(),
        );
        opencode_db(&{
            std::fs::create_dir_all(h.join(".local/share/opencode")).unwrap();
            h.join(".local/share/opencode/opencode.db")
        });
        write(
            ".factory/sessions/-d/d1.jsonl",
            line(json!({"type": "session_start", "id": "d1", "title": "hi droid"})),
        );

        let mut found: Vec<(CLIAgent, String, String)> = scan(&Roots::under(h), &[])
            .into_iter()
            .map(|s| (s.agent, s.id, s.title))
            .collect();
        found.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(
            found,
            vec![
                (CLIAgent::Gemini, "abcd1234-full".into(), "hi gemini".into()),
                (CLIAgent::Copilot, "cp".into(), "hi copilot".into()),
                (CLIAgent::Droid, "d1".into(), "hi droid".into()),
                (CLIAgent::Pi, "p1".into(), "hi pi".into()),
                (CLIAgent::QoderCLI, "q1".into(), "hello".into()),
                (CLIAgent::QoderCLICn, "q2".into(), "hello cn".into()),
                (
                    CLIAgent::OpenCode,
                    "ses_default".into(),
                    "add dark mode".into()
                ),
                (CLIAgent::OpenCode, "ses_named".into(), "Fix login".into()),
                (CLIAgent::Kimi, "session_k".into(), "hi kimi".into()),
                (CLIAgent::Qwen, "w1".into(), "hi qwen".into()),
            ]
        );
    }
}

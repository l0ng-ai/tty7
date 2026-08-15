//! Everything tty7 knows about git, split by question:
//!
//! - this file — how a `git` process is *run* (and its output re-assembled)
//! - [`status`] — what the working tree looks like right now
//! - [`diff`] — what a particular patch looks like
//! - [`log`] — what the history looks like, and how to lay it out in lanes
//! - [`ops`] — how to *change* the repository
//!
//! Nothing here depends on gpui: the headless `tty7-server` answers the same
//! questions for a remote workspace that `LocalHost` answers for this machine,
//! and the conformance suite holds the two to the same behaviour.
pub mod diff;
pub mod log;
pub mod ops;
pub mod status;

use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::host::{Host, Output};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitStatus {
    pub branch: String,
    pub added: u32,
    pub removed: u32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RepoSnapshot {
    pub root: PathBuf,
    pub home: PathBuf,
    pub branch: String,
    pub counts: Option<(u32, u32)>,
}

pub fn probe(host: &dyn Host, cwd: &Path) -> Option<RepoSnapshot> {
    let paths = git(
        host,
        cwd,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ],
    )?;
    let mut lines = paths.lines().map(|l| l.trim_end_matches(['\n', '\r']));
    let root = PathBuf::from(lines.next()?);
    let home = repo_home(&root, lines.next(), lines.next());
    let branch = branch_name(host, cwd)?;
    Some(RepoSnapshot {
        home,
        root,
        branch,
        counts: diff_numstat(host, cwd),
    })
}

pub(crate) fn repo_home(root: &Path, git_dir: Option<&str>, common_dir: Option<&str>) -> PathBuf {
    let (Some(git_dir), Some(common)) = (git_dir, common_dir) else {
        return root.to_path_buf();
    };
    if git_dir == common {
        return root.to_path_buf();
    }
    let common = Path::new(common);
    match (common.file_name(), common.parent()) {
        (Some(name), Some(parent)) if name == ".git" => parent.to_path_buf(),
        _ => common.to_path_buf(),
    }
}

pub fn branch_name(host: &dyn Host, cwd: &Path) -> Option<String> {
    if let Some(out) = git(host, cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        let name = out.trim();
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    let sha = git(host, cwd, &["rev-parse", "--short", "HEAD"])?;
    let sha = sha.trim();
    (!sha.is_empty()).then(|| sha.to_string())
}

fn diff_numstat(host: &dyn Host, cwd: &Path) -> Option<(u32, u32)> {
    let out = git(host, cwd, &["diff", "--numstat", "HEAD"])?;
    let mut added = 0u32;
    let mut removed = 0u32;
    for line in out.lines() {
        let mut fields = line.split('\t');
        if let Some(n) = fields.next().and_then(|s| s.parse::<u32>().ok()) {
            added += n;
        }
        if let Some(n) = fields.next().and_then(|s| s.parse::<u32>().ok()) {
            removed += n;
        }
    }
    Some((added, removed))
}

pub fn git(host: &dyn Host, cwd: &Path, args: &[&str]) -> Option<String> {
    let out = host.git(cwd, args).ok()?;
    if !out.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

pub fn git_output(cwd: &Path, args: &[&str]) -> io::Result<Output> {
    git_output_with_env(cwd, args, &[])
}

/// `git_output` plus extra environment.
///
/// What `LocalHost` passes is the no-prompt set: git and ssh have to fail
/// rather than block on a credential prompt nobody is watching. It goes on
/// every call, not just `fetch`/`pull`/`push` — a read path never prompts, so
/// carrying it there costs nothing, and a remote workspace only inherits it
/// because the far side's `LocalHost` applies the same rule to a request that
/// arrived over the wire with no "this one is a network op" bit on it.
///
/// A `None` value removes the variable instead of setting it.
pub(crate) fn git_output_with_env(
    cwd: &Path,
    args: &[&str],
    env: &[(&str, Option<&str>)],
) -> io::Result<Output> {
    if !cwd.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("git cwd does not exist: {}", cwd.display()),
        ));
    }
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(cwd)
        .args(args)
        // Keeps `git status` from refreshing and writing back `.git/index`.
        // Beyond the obvious (never dirty a repo just by looking at it), this is
        // what stops the SCM panel's own probes from waking the `.git` watcher
        // that schedules them — the read path is provably write-free. Do not
        // drop it.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        match value {
            Some(v) => cmd.env(key, v),
            None => cmd.env_remove(key),
        };
    }
    let out = crate::core::proc::hide_console(&mut cmd).output()?;
    Ok(Output {
        status: out.status.code(),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

pub(crate) fn git_stream(
    cwd: &Path,
    args: &[&str],
    mut on_chunk: impl FnMut(&[u8]) -> bool,
) -> io::Result<Option<i32>> {
    if !cwd.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("git cwd does not exist: {}", cwd.display()),
        ));
    }
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = crate::core::proc::hide_console(&mut cmd).spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("git stdout was not piped"))?;

    let mut buf = vec![0u8; 64 * 1024];
    let mut read_err = None;
    loop {
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if !on_chunk(&buf[..n]) {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                read_err = Some(e);
                break;
            }
        }
    }
    drop(stdout);
    let status = child.wait()?;
    match read_err {
        Some(e) => Err(e),
        None => Ok(status.code()),
    }
}

#[derive(Default)]
pub struct LineSplitter {
    tail: Vec<u8>,
    dropped: usize,
}

pub const MAX_LINE: usize = 1024 * 1024;

impl LineSplitter {
    pub fn push(&mut self, chunk: &[u8], mut on_line: impl FnMut(&str)) {
        let mut rest = chunk;
        while let Some(nl) = rest.iter().position(|b| *b == b'\n') {
            let (line, after) = rest.split_at(nl);
            if self.tail.is_empty() && self.dropped == 0 && line.len() <= MAX_LINE {
                on_line(&trim_cr(line));
            } else {
                self.keep(line);
                let joined = std::mem::take(&mut self.tail);
                let dropped = std::mem::take(&mut self.dropped);
                emit(&joined, dropped, &mut on_line);
            }
            rest = &after[1..];
        }
        self.keep(rest);
    }

    pub fn finish(self, mut on_line: impl FnMut(&str)) {
        if !self.tail.is_empty() || self.dropped > 0 {
            emit(&self.tail, self.dropped, &mut on_line);
        }
    }

    fn keep(&mut self, bytes: &[u8]) {
        let room = MAX_LINE.saturating_sub(self.tail.len());
        let take = room.min(bytes.len());
        self.tail.extend_from_slice(&bytes[..take]);
        self.dropped += bytes.len() - take;
    }
}

/// Splits a byte stream on an arbitrary separator, handing out whole records.
///
/// [`LineSplitter`]'s sibling, for the two git formats that are *not* newline
/// delimited: `--porcelain=v2 -z` (NUL) and `log --pretty` with an ASCII record
/// separator. Records come out as `&[u8]` rather than `&str` because a path in
/// a `-z` status is raw bytes and need not be UTF-8 at all — deciding what to
/// do about that belongs to the parser, not to the splitter.
pub struct RecordSplitter {
    sep: u8,
    tail: Vec<u8>,
    /// The record being assembled overran [`MAX_RECORD`] and is now being
    /// discarded up to its separator.
    discarding: bool,
    dropped: usize,
}

pub const MAX_RECORD: usize = 1024 * 1024;

impl RecordSplitter {
    pub fn new(sep: u8) -> RecordSplitter {
        RecordSplitter {
            sep,
            tail: Vec::new(),
            discarding: false,
            dropped: 0,
        }
    }

    pub fn push(&mut self, chunk: &[u8], mut on_record: impl FnMut(&[u8])) {
        let mut rest = chunk;
        while let Some(at) = rest.iter().position(|b| *b == self.sep) {
            let (record, after) = rest.split_at(at);
            // An overlong record is dropped whole, never delivered cut short:
            // a truncated record still parses — a commit body cut mid-way
            // reads as the real message — and a wrong record is worse than a
            // missing one the caller is told about.
            if self.discarding {
                self.discarding = false;
                self.dropped += 1;
            } else if self.tail.is_empty() {
                // The common case — a whole record inside one chunk — is
                // borrowed straight from the input, no copy.
                if record.len() <= MAX_RECORD {
                    on_record(record);
                } else {
                    self.dropped += 1;
                }
            } else {
                self.keep(record);
                if self.discarding {
                    self.discarding = false;
                    self.dropped += 1;
                } else {
                    let joined = std::mem::take(&mut self.tail);
                    on_record(&joined);
                }
            }
            rest = &after[1..];
        }
        self.keep(rest);
    }

    /// Emits a trailing record only if one was actually started — unlike
    /// lines, well-formed `-z` output ends *with* a separator, so the common
    /// case here is emitting nothing. Returns how many records were dropped
    /// whole for overrunning [`MAX_RECORD`]; non-zero means the parse is
    /// incomplete and the caller must not present it as the full answer.
    #[must_use]
    pub fn finish(mut self, mut on_record: impl FnMut(&[u8])) -> usize {
        if self.discarding {
            self.dropped += 1;
        } else if !self.tail.is_empty() {
            let joined = std::mem::take(&mut self.tail);
            on_record(&joined);
        }
        self.dropped
    }

    fn keep(&mut self, bytes: &[u8]) {
        if self.discarding {
            return;
        }
        if self.tail.len() + bytes.len() > MAX_RECORD {
            // Free what was buffered too — nobody will ever see this record.
            self.tail.clear();
            self.discarding = true;
            return;
        }
        self.tail.extend_from_slice(bytes);
    }
}

fn emit(line: &[u8], dropped: usize, on_line: &mut impl FnMut(&str)) {
    if dropped == 0 {
        on_line(&trim_cr(line));
        return;
    }
    let mut text = String::from_utf8_lossy(line).into_owned();
    text.push_str(&format!(
        " …[{dropped} more bytes on this line dropped: past tty7's {MAX_LINE}-byte line cap]"
    ));
    on_line(&text);
}

fn trim_cr(line: &[u8]) -> std::borrow::Cow<'_, str> {
    let line = match line.last() {
        Some(b'\r') => &line[..line.len() - 1],
        _ => line,
    };
    String::from_utf8_lossy(line)
}

/// What [`status`], [`diff`], [`log`] and [`ops`] all need to drive a scratch
/// repository the same way. One copy here rather than four that drift.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;

    /// The `-c` overrides a test's own git invocations carry.
    ///
    /// `user.name`/`user.email` because a CI runner has neither and git refuses
    /// to commit without them, and `commit.gpgsign` because a developer may
    /// have signing on globally. The two line-ending settings are here for the
    /// same reason: Git for Windows ships `core.autocrlf=true` in its *system*
    /// config, so a fixture built with LF and read back is a fixture whose
    /// bytes depend on which machine ran the test.
    pub(crate) const PINS: [&str; 10] = [
        "-c",
        "user.name=tty7 test",
        "-c",
        "user.email=test@tty7.invalid",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.autocrlf=false",
        "-c",
        "core.eol=lf",
    ];

    /// The same settings, written into `<repo>/.git/config`.
    ///
    /// [`PINS`] only reaches the commands the *test* runs. The code under test
    /// runs its own git — `run_op`'s `checkout --`, `probe_status`,
    /// `probe_diff` — with no overrides at all, and rightly so: it must obey
    /// the repository the user actually has. So the line-ending rules have to
    /// live in the repository, where every git that opens it will read them,
    /// and repository config outranks the system config that put
    /// `core.autocrlf=true` there.
    ///
    /// Call this straight after `git init`, before anything is written or
    /// checked out, so no blob is ever created under the other rules.
    pub(crate) fn pin_repo_config(repo: &Path) -> bool {
        PINS.chunks(2).all(|pair| {
            let Some((key, value)) = pair[1].split_once('=') else {
                return false;
            };
            super::git_output(repo, &["config", key, value]).is_ok_and(|out| out.success())
        })
    }

    /// A path in the one spelling two halves of an assertion can agree on.
    ///
    /// They do not naturally agree. Anything that came out of `git rev-parse`
    /// is in git's dialect — forward slashes, no extended-length prefix, even
    /// on Windows — while the expected side is usually built from
    /// `fs::canonicalize`, which on Windows answers `\\?\C:\…`. Both name the
    /// same directory and the Win32 file APIs take either, so the code under
    /// test is right to pass git's answer straight through; it is only the
    /// comparison that has to pick a spelling. On unix this is the identity.
    ///
    /// Note this normalises the *spelling*, not the path: equality stays exact.
    pub(crate) fn one_spelling(p: &Path) -> String {
        let slashed = p.to_string_lossy().replace('\\', "/");
        match slashed.strip_prefix("//?/") {
            Some(bare) => bare.to_string(),
            None => slashed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::one_spelling;
    use super::*;

    fn h() -> crate::host::SharedHost {
        crate::host::local::LocalHost::new()
    }

    /// The mapping [`test_support::one_spelling`] promises, proven on literals
    /// rather than on whatever this machine's temp directory happens to be —
    /// a developer on unix never sees either of the Windows shapes, which is
    /// exactly how a comparison against `fs::canonicalize` reached Windows CI
    /// unnoticed in the first place.
    #[test]
    fn one_spelling_folds_the_two_ways_windows_writes_a_path() {
        assert_eq!(
            one_spelling(Path::new(r"\\?\C:\Users\x\repo\.git")),
            "C:/Users/x/repo/.git",
            "the extended-length prefix goes, and the separators match git's"
        );
        assert_eq!(
            one_spelling(Path::new(r"C:\Users\x\repo\.git")),
            "C:/Users/x/repo/.git",
            "a plain Windows path lands on the same spelling"
        );
        assert_eq!(
            one_spelling(Path::new("C:/Users/x/repo/.git")),
            "C:/Users/x/repo/.git",
            "what git itself answers is already in that spelling"
        );
        assert_eq!(
            one_spelling(Path::new("/private/var/t/repo/.git")),
            "/private/var/t/repo/.git",
            "on unix it is the identity, which is why this never fired locally"
        );
    }

    #[test]
    fn line_splitter_rejoins_across_chunks() {
        let mut split = LineSplitter::default();
        let mut got = Vec::new();
        split.push(b"alpha\nbe", |l| got.push(l.to_string()));
        assert_eq!(got, ["alpha"], "only the complete line so far");
        split.push(b"ta\ngamma\n", |l| got.push(l.to_string()));
        split.finish(|l| got.push(l.to_string()));
        assert_eq!(got, ["alpha", "beta", "gamma"]);
    }

    #[test]
    fn line_splitter_handles_crlf_and_a_missing_final_newline() {
        let mut split = LineSplitter::default();
        let mut got = Vec::new();
        split.push(b"one\r\ntwo\r\nthree", |l| got.push(l.to_string()));
        split.finish(|l| got.push(l.to_string()));
        assert_eq!(got, ["one", "two", "three"]);
    }

    #[test]
    fn line_splitter_replaces_invalid_utf8() {
        let mut split = LineSplitter::default();
        let mut got = Vec::new();
        split.push(b"caf\xe9\n", |l| got.push(l.to_string()));
        split.finish(|l| got.push(l.to_string()));
        assert_eq!(got.len(), 1);
        assert!(got[0].starts_with("caf"), "{:?}", got[0]);
    }

    #[test]
    fn record_splitter_rejoins_across_chunks_and_emits_a_trailing_record() {
        let mut split = RecordSplitter::new(0);
        let mut got = Vec::new();
        split.push(b"alpha\0be", |r| got.push(r.to_vec()));
        assert_eq!(got, [b"alpha".to_vec()], "only the complete record so far");
        split.push(b"ta\0gamma", |r| got.push(r.to_vec()));
        // Well-formed `-z` output ends with a separator; a trailing record
        // without one still comes out at `finish`.
        assert_eq!(split.finish(|r| got.push(r.to_vec())), 0);
        assert_eq!(
            got,
            [b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()]
        );
    }

    /// An overlong record is dropped whole and *counted* — delivered cut
    /// short it would still parse, and a commit body cut mid-way reads as the
    /// real message.
    #[test]
    fn record_splitter_drops_an_absurd_record_whole_and_says_so() {
        let mut split = RecordSplitter::new(0);
        let mut got = Vec::new();
        let huge = vec![b'x'; MAX_RECORD + 5_000];
        split.push(b"before\0", |r| got.push(r.to_vec()));
        for piece in huge.chunks(64 * 1024) {
            split.push(piece, |r| got.push(r.to_vec()));
        }
        split.push(b"\0after\0", |r| got.push(r.to_vec()));
        let dropped = split.finish(|r| got.push(r.to_vec()));

        assert_eq!(dropped, 1);
        assert_eq!(
            got,
            [b"before".to_vec(), b"after".to_vec()],
            "no truncated ghost between the two, and the next record survives"
        );

        // A single-chunk oversized record takes the borrow fast path and must
        // be counted the same way.
        let mut split = RecordSplitter::new(0);
        let mut got: Vec<Vec<u8>> = Vec::new();
        let mut one = vec![b'y'; MAX_RECORD + 1];
        one.push(0);
        one.extend_from_slice(b"tail\0");
        split.push(&one, |r| got.push(r.to_vec()));
        assert_eq!(split.finish(|r| got.push(r.to_vec())), 1);
        assert_eq!(got, [b"tail".to_vec()]);
    }

    /// A stream that *ends* mid-way through an oversized record still reports
    /// the drop.
    #[test]
    fn record_splitter_counts_a_truncated_trailing_record() {
        let mut split = RecordSplitter::new(0);
        let mut got: Vec<Vec<u8>> = Vec::new();
        split.push(&vec![b'z'; MAX_RECORD + 1], |r| got.push(r.to_vec()));
        assert_eq!(split.finish(|r| got.push(r.to_vec())), 1);
        assert!(got.is_empty());
    }

    #[test]
    fn line_splitter_caps_one_absurd_line() {
        let mut split = LineSplitter::default();
        let mut got = Vec::new();
        let huge = vec![b'x'; MAX_LINE + 5_000];
        split.push(b"before\n", |l| got.push(l.to_string()));
        for piece in huge.chunks(64 * 1024) {
            split.push(piece, |l| got.push(l.to_string()));
        }
        split.push(b"\nafter\n", |l| got.push(l.to_string()));
        split.finish(|l| got.push(l.to_string()));

        assert_eq!(
            got.len(),
            3,
            "{:?}",
            got.iter().map(|l| l.len()).collect::<Vec<_>>()
        );
        assert_eq!(got[0], "before");
        assert_eq!(got[2], "after", "the next line is unaffected");
        assert!(
            got[1].starts_with(&"x".repeat(1000)),
            "the kept prefix is the real content"
        );
        assert!(
            got[1].contains("5000 more bytes"),
            "the cut says how much went: {}",
            &got[1][got[1].len() - 80..]
        );
        assert!(
            got[1].len() < MAX_LINE + 200,
            "nothing past the cap was retained"
        );
    }

    #[test]
    fn line_splitter_caps_a_final_unterminated_line() {
        let mut split = LineSplitter::default();
        let mut got = Vec::new();
        split.push(&vec![b'y'; MAX_LINE + 7], |l| got.push(l.to_string()));
        split.finish(|l| got.push(l.to_string()));
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("7 more bytes"), "{}", got[0]);
    }

    #[test]
    fn streaming_and_buffered_reads_agree() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        let args = ["log", "--oneline", "-n", "40"];
        let Ok(code) = git_stream(here, &args, |_| true) else {
            return;
        };
        if code != Some(0) {
            return;
        }
        let mut streamed = Vec::new();
        let mut split = LineSplitter::default();
        git_stream(here, &args, |chunk| {
            split.push(chunk, |l| streamed.push(l.to_string()));
            true
        })
        .unwrap();
        split.finish(|l| streamed.push(l.to_string()));

        let buffered = git_output(here, &args).unwrap();
        let expected: Vec<String> = String::from_utf8_lossy(&buffered.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(streamed, expected);
        assert!(!streamed.is_empty(), "this repo has commits");
    }

    #[test]
    fn non_repo_is_none() {
        let dir = std::env::temp_dir().join("tty7-git-status-not-a-repo-xyz");
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(probe(&*h(), &dir), None);
    }

    #[test]
    fn missing_path_is_none() {
        assert_eq!(probe(&*h(), Path::new("/no/such/tty7/path/here")), None);
    }

    #[test]
    fn own_repo_has_a_branch_and_root() {
        let here = env!("CARGO_MANIFEST_DIR");
        if let Some(snap) = probe(&*h(), Path::new(here)) {
            assert!(!snap.branch.is_empty());
            assert!(Path::new(here).starts_with(&snap.root));
        }
    }
    #[test]
    fn repo_home_resolves_worktree_layouts() {
        let root = Path::new("/repo/.wt/feat");

        assert_eq!(
            repo_home(Path::new("/repo"), Some("/repo/.git"), Some("/repo/.git")),
            PathBuf::from("/repo")
        );
        assert_eq!(
            repo_home(root, Some("/repo/.git/worktrees/feat"), Some("/repo/.git")),
            PathBuf::from("/repo")
        );
        assert_eq!(
            repo_home(root, Some("/bare.git/worktrees/feat"), Some("/bare.git")),
            PathBuf::from("/bare.git")
        );
        assert_eq!(
            repo_home(root, Some("/repo/.git"), None),
            root.to_path_buf()
        );
        assert_eq!(repo_home(root, None, None), root.to_path_buf());
    }
}

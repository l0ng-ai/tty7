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

fn repo_home(root: &Path, git_dir: Option<&str>, common_dir: Option<&str>) -> PathBuf {
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
        .stderr(Stdio::piped());
    let out = crate::core::proc::hide_console(&mut cmd).output()?;
    Ok(Output {
        status: out.status.code(),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

pub fn git_stream(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn h() -> crate::host::SharedHost {
        crate::host::local::LocalHost::new()
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

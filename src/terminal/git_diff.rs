use std::path::{Path, PathBuf};

use crate::terminal::git_status;
use crate::ui::host_ops::Host;

pub const MAX_LINES_PER_FILE: usize = 2000;

pub const MAX_TOTAL_LINES: usize = 20_000;

pub const MAX_FILES_WITH_HUNKS: usize = 500;

pub const AUTO_COLLAPSE_LINES: u32 = 400;

pub const AUTO_COLLAPSE_TOTAL_LINES: usize = 8_000;

pub const AUTO_COLLAPSE_TOTAL_FILES: usize = 100;

pub const MAX_RENDERED_FILES: usize = 300;

pub const MAX_UNTRACKED: usize = 500;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Truncation {
    PerFile,
    Budget,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DiffSnapshot {
    pub root: PathBuf,
    pub branch: String,
    pub files: Vec<FileDiff>,
    pub untracked: Vec<String>,
    pub untracked_total: usize,
    pub read_failed: bool,
}

impl DiffSnapshot {
    pub fn totals(&self) -> (u32, u32) {
        self.files
            .iter()
            .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed))
    }

    pub fn untracked_count(&self) -> usize {
        self.untracked_total.max(self.untracked.len())
    }

    pub fn stats(&self) -> DiffStats {
        let mut added = 0u32;
        let mut removed = 0u32;
        let mut retained_lines = 0usize;
        let mut budget_exhausted = false;
        let mut per_file_truncated = false;
        for file in &self.files {
            added += file.added;
            removed += file.removed;
            retained_lines += file.hunks.iter().map(|h| h.lines.len()).sum::<usize>();
            match file.truncated {
                Some(Truncation::Budget) => budget_exhausted = true,
                Some(Truncation::PerFile) => per_file_truncated = true,
                None => {}
            }
        }
        let untracked_count = self.untracked_count();
        DiffStats {
            totals: (added, removed),
            retained_lines,
            untracked_count,
            oversized: self.files.len() > AUTO_COLLAPSE_TOTAL_FILES
                || retained_lines > AUTO_COLLAPSE_TOTAL_LINES,
            budget_exhausted,
            per_file_truncated,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DiffStats {
    pub totals: (u32, u32),
    pub retained_lines: usize,
    pub untracked_count: usize,
    pub oversized: bool,
    pub budget_exhausted: bool,
    pub per_file_truncated: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub status: FileStatus,
    pub added: u32,
    pub removed: u32,
    pub binary: bool,
    pub truncated: Option<Truncation>,
    pub hunks: Vec<Hunk>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

pub fn probe(host: &dyn Host, cwd: &Path) -> Option<DiffSnapshot> {
    let root = git_status::git(host, cwd, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root.trim_end_matches(['\n', '\r']));
    let branch = git_status::branch_name(host, cwd)?;
    let mut parser = DiffParser::default();
    let diffed = host.git_lines(
        cwd,
        &["diff", "--no-color", "--no-ext-diff", "-M", "HEAD"],
        &mut |line| parser.push_line(line),
    );
    let files = match diffed {
        Ok(Some(0)) => parser.finish(),
        _ => Vec::new(),
    };
    let mut untracked: Vec<String> = Vec::new();
    let mut untracked_total = 0usize;
    let listed = host.git_lines(
        cwd,
        &["ls-files", "--others", "--exclude-standard", "--full-name"],
        &mut |line| {
            untracked_total += 1;
            if untracked.len() < MAX_UNTRACKED {
                untracked.push(line.to_string());
            }
        },
    );
    if !matches!(listed, Ok(Some(0))) {
        untracked.clear();
        untracked_total = 0;
    }
    Some(DiffSnapshot {
        root,
        branch,
        files,
        untracked,
        untracked_total,
        read_failed: !matches!(diffed, Ok(Some(0))) || !matches!(listed, Ok(Some(0))),
    })
}

#[cfg(test)]
pub fn parse_unified(out: &str) -> Vec<FileDiff> {
    let mut parser = DiffParser::default();
    for line in out.lines() {
        parser.push_line(line);
    }
    parser.finish()
}

#[derive(Default)]
pub struct DiffParser {
    files: Vec<FileDiff>,
    old_no: u32,
    new_no: u32,
    file_lines: usize,
    total_lines: usize,
    files_with_hunks: usize,
    in_hunk: bool,
}

impl DiffParser {
    pub fn push_line(&mut self, line: &str) {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let (old_p, new_p) = parse_git_header_paths(rest);
            self.files.push(FileDiff {
                path: new_p.clone(),
                old_path: (old_p != new_p).then_some(old_p),
                status: FileStatus::Modified,
                added: 0,
                removed: 0,
                binary: false,
                truncated: None,
                hunks: Vec::new(),
            });
            self.file_lines = 0;
            self.in_hunk = false;
            return;
        }
        let Some(file) = self.files.last_mut() else {
            return;
        };
        if line.starts_with("new file mode") {
            file.status = FileStatus::Added;
            return;
        }
        if line.starts_with("deleted file mode") {
            file.status = FileStatus::Deleted;
            return;
        }
        if line.starts_with("rename from ") {
            file.status = FileStatus::Renamed;
            return;
        }
        if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            file.binary = true;
            return;
        }
        if !self.in_hunk
            && (line.starts_with("--- ") || line.starts_with("+++ ") || !is_hunk_line(line))
            && !line.starts_with("@@")
        {
            return;
        }
        if line.starts_with("@@") {
            self.in_hunk = true;
            if file.truncated.is_some() {
                return;
            }
            let first_hunk = file.hunks.is_empty();
            if (first_hunk && self.files_with_hunks >= MAX_FILES_WITH_HUNKS)
                || self.total_lines >= MAX_TOTAL_LINES
            {
                file.truncated = Some(Truncation::Budget);
                return;
            }
            if first_hunk {
                self.files_with_hunks += 1;
            }
            let (o, n) = parse_hunk_starts(line).unwrap_or((0, 0));
            self.old_no = o;
            self.new_no = n;
            file.hunks.push(Hunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            return;
        }
        if !self.in_hunk {
            return;
        }
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Added, &line[1..]),
            Some(b'-') => (LineKind::Removed, &line[1..]),
            Some(b' ') => (LineKind::Context, &line[1..]),
            _ => return,
        };
        match kind {
            LineKind::Added => file.added += 1,
            LineKind::Removed => file.removed += 1,
            LineKind::Context => {}
        }
        if file.truncated.is_some() {
            return;
        }
        self.file_lines += 1;
        if self.file_lines > MAX_LINES_PER_FILE {
            file.truncated = Some(Truncation::PerFile);
            return;
        }
        if self.total_lines >= MAX_TOTAL_LINES {
            file.truncated = Some(Truncation::Budget);
            return;
        }
        let Some(hunk) = file.hunks.last_mut() else {
            return;
        };
        let (o, n) = match kind {
            LineKind::Added => {
                let n = self.new_no;
                self.new_no += 1;
                (None, Some(n))
            }
            LineKind::Removed => {
                let o = self.old_no;
                self.old_no += 1;
                (Some(o), None)
            }
            LineKind::Context => {
                let (o, n) = (self.old_no, self.new_no);
                self.old_no += 1;
                self.new_no += 1;
                (Some(o), Some(n))
            }
        };
        hunk.lines.push(DiffLine {
            kind,
            old_no: o,
            new_no: n,
            text: text.to_string(),
        });
        self.total_lines += 1;
    }

    pub fn finish(self) -> Vec<FileDiff> {
        self.files
    }
}

fn is_hunk_line(line: &str) -> bool {
    matches!(line.as_bytes().first(), Some(b'+' | b'-' | b' ' | b'\\')) || line.is_empty()
}

fn parse_git_header_paths(rest: &str) -> (String, String) {
    if rest.starts_with('"') {
        let parts: Vec<String> = parse_quoted_pair(rest);
        if parts.len() == 2 {
            return (strip_prefix_ab(&parts[0]), strip_prefix_ab(&parts[1]));
        }
    }
    if let Some(idx) = rest.rfind(" b/") {
        let old = &rest[..idx];
        let new = &rest[idx + 1..];
        return (strip_prefix_ab(old), strip_prefix_ab(new));
    }
    (rest.to_string(), rest.to_string())
}

fn parse_quoted_pair(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut escaped = false;
    for ch in s.chars() {
        if escaped {
            cur.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quote => escaped = true,
            '"' => {
                if in_quote {
                    parts.push(std::mem::take(&mut cur));
                }
                in_quote = !in_quote;
            }
            _ if in_quote => cur.push(ch),
            _ => {}
        }
    }
    parts
}

fn strip_prefix_ab(p: &str) -> String {
    p.strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p)
        .to_string()
}

fn parse_hunk_starts(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old_part, rest) = rest.split_once(" +")?;
    let (new_part, _) = rest.split_once(" @@")?;
    let old = old_part.split(',').next()?.parse().ok()?;
    let new = new_part.split(',').next()?.parse().ok()?;
    Some((old, new))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,4 +10,5 @@ fn main() {
 let a = 1;
-let b = old();
+let b = new();
+let c = 3;
 done();
diff --git a/docs/new.md b/docs/new.md
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/docs/new.md
@@ -0,0 +1,2 @@
+hello
+world
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 4444444..0000000
--- a/gone.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-bye
diff --git a/img.png b/img.png
index 5555555..6666666 100644
Binary files a/img.png and b/img.png differ
";

    #[test]
    fn parses_the_four_file_shapes() {
        let files = parse_unified(SAMPLE);
        assert_eq!(files.len(), 4);

        let m = &files[0];
        assert_eq!(m.path, "src/main.rs");
        assert_eq!(m.status, FileStatus::Modified);
        assert_eq!((m.added, m.removed), (2, 1));
        assert_eq!(m.hunks.len(), 1);
        assert_eq!(m.hunks[0].header, "@@ -10,4 +10,5 @@ fn main() {");
        let lines = &m.hunks[0].lines;
        assert_eq!(lines.len(), 5);
        assert_eq!((lines[0].old_no, lines[0].new_no), (Some(10), Some(10)));
        assert_eq!(lines[1].kind, LineKind::Removed);
        assert_eq!(lines[1].old_no, Some(11));
        assert_eq!(lines[1].new_no, None);
        assert_eq!(lines[2].kind, LineKind::Added);
        assert_eq!(lines[2].new_no, Some(11));
        assert_eq!(lines[3].new_no, Some(12));
        assert_eq!(lines[3].text, "let c = 3;");
        assert_eq!((lines[4].old_no, lines[4].new_no), (Some(12), Some(13)));

        let a = &files[1];
        assert_eq!(a.status, FileStatus::Added);
        assert_eq!((a.added, a.removed), (2, 0));

        let d = &files[2];
        assert_eq!(d.status, FileStatus::Deleted);
        assert_eq!((d.added, d.removed), (0, 1));

        let b = &files[3];
        assert!(b.binary);
        assert!(b.hunks.is_empty());
    }

    #[test]
    fn parses_renames() {
        let out = "\
diff --git a/old/name.rs b/new/name.rs
similarity index 100%
rename from old/name.rs
rename to new/name.rs
";
        let files = parse_unified(out);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, FileStatus::Renamed);
        assert_eq!(files[0].path, "new/name.rs");
        assert_eq!(files[0].old_path.as_deref(), Some("old/name.rs"));
        assert_eq!((files[0].added, files[0].removed), (0, 0));
    }

    #[test]
    fn parses_quoted_paths() {
        let out = "diff --git \"a/has space.txt\" \"b/has space.txt\"\n";
        let files = parse_unified(out);
        assert_eq!(files[0].path, "has space.txt");
        assert_eq!(files[0].old_path, None);
    }

    #[test]
    fn triple_dash_content_line_is_kept() {
        let out = "\
diff --git a/x.md b/x.md
index 1111111..2222222 100644
--- a/x.md
+++ b/x.md
@@ -1,2 +1,1 @@
 keep
---- a heading rule
";
        let files = parse_unified(out);
        let lines = &files[0].hunks[0].lines;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].kind, LineKind::Removed);
        assert_eq!(lines[1].text, "--- a heading rule");
    }

    #[test]
    fn caps_lines_per_file_but_keeps_counting() {
        let mut out = String::from(
            "diff --git a/big.txt b/big.txt\nindex 1..2 100644\n--- a/big.txt\n+++ b/big.txt\n@@ -0,0 +1,3000 @@\n",
        );
        for i in 0..3000 {
            out.push_str(&format!("+line {i}\n"));
        }
        let files = parse_unified(&out);
        assert_eq!(files[0].truncated, Some(Truncation::PerFile));
        assert_eq!(files[0].added, 3000);
        let kept: usize = files[0].hunks.iter().map(|h| h.lines.len()).sum();
        assert_eq!(kept, MAX_LINES_PER_FILE);
    }

    #[test]
    fn skips_no_newline_marker() {
        let out = "\
diff --git a/x b/x
index 1..2 100644
--- a/x
+++ b/x
@@ -1,1 +1,1 @@
-old
\\ No newline at end of file
+new
\\ No newline at end of file
";
        let files = parse_unified(out);
        assert_eq!(files[0].hunks[0].lines.len(), 2);
        assert_eq!((files[0].added, files[0].removed), (1, 1));
    }

    #[test]
    fn snapshot_totals() {
        let snap = DiffSnapshot {
            files: parse_unified(SAMPLE),
            ..Default::default()
        };
        assert_eq!(snap.totals(), (4, 2));
    }

    fn many_files(files: usize, lines_each: usize) -> String {
        let mut out = String::new();
        for f in 0..files {
            out.push_str(&format!(
                "diff --git a/f{f}.rs b/f{f}.rs\nindex 1..2 100644\n--- a/f{f}.rs\n+++ b/f{f}.rs\n@@ -0,0 +1,{lines_each} @@\n"
            ));
            for i in 0..lines_each {
                out.push_str(&format!("+file {f} line {i}\n"));
            }
        }
        out
    }

    #[test]
    fn repo_wide_budget_caps_retained_lines() {
        let files = parse_unified(&many_files(300, 300));
        assert_eq!(files.len(), 300, "every file keeps its header row");
        let retained: usize = files
            .iter()
            .flat_map(|f| &f.hunks)
            .map(|h| h.lines.len())
            .sum();
        assert!(
            retained <= MAX_TOTAL_LINES,
            "retained {retained} lines, budget is {MAX_TOTAL_LINES}"
        );
        assert!(
            files
                .iter()
                .any(|f| f.truncated == Some(Truncation::Budget))
        );
    }

    #[test]
    fn repo_wide_budget_keeps_totals_exact() {
        let snap = DiffSnapshot {
            files: parse_unified(&many_files(300, 300)),
            ..Default::default()
        };
        assert_eq!(snap.totals(), (90_000, 0));
        assert!(snap.stats().budget_exhausted);
    }

    #[test]
    fn repo_wide_budget_caps_files_with_hunks() {
        let files = parse_unified(&many_files(MAX_FILES_WITH_HUNKS + 50, 1));
        assert_eq!(files.len(), MAX_FILES_WITH_HUNKS + 50);
        let with_hunks = files.iter().filter(|f| !f.hunks.is_empty()).count();
        assert_eq!(with_hunks, MAX_FILES_WITH_HUNKS);
        assert_eq!(
            files.iter().map(|f| f.added).sum::<u32>(),
            (MAX_FILES_WITH_HUNKS + 50) as u32
        );
        assert_eq!(files.last().unwrap().truncated, Some(Truncation::Budget));
    }

    #[test]
    fn small_diff_is_not_truncated() {
        let snap = DiffSnapshot {
            files: parse_unified(SAMPLE),
            ..Default::default()
        };
        assert!(snap.files.iter().all(|f| f.truncated.is_none()));
        assert!(!snap.stats().oversized);
        assert!(!snap.stats().budget_exhausted);
    }

    #[test]
    fn oversized_trips_on_files_or_lines() {
        let by_files = DiffSnapshot {
            files: parse_unified(&many_files(AUTO_COLLAPSE_TOTAL_FILES + 1, 1)),
            ..Default::default()
        };
        assert!(by_files.stats().oversized);

        let per_file = MAX_LINES_PER_FILE / 2;
        let by_lines = DiffSnapshot {
            files: parse_unified(&many_files(
                AUTO_COLLAPSE_TOTAL_LINES / per_file + 1,
                per_file,
            )),
            ..Default::default()
        };
        assert!(by_lines.files.len() <= AUTO_COLLAPSE_TOTAL_FILES);
        assert!(by_lines.stats().retained_lines > AUTO_COLLAPSE_TOTAL_LINES);
        assert!(by_lines.stats().oversized);
    }

    #[test]
    fn truncated_file_counts_dash_prefixed_content() {
        let mut out = many_files(MAX_FILES_WITH_HUNKS, 1);
        out.push_str(
            "diff --git a/late.md b/late.md\nindex 1..2 100644\n--- a/late.md\n+++ b/late.md\n@@ -1,2 +1,1 @@\n keep\n--- a heading rule\n",
        );
        let files = parse_unified(&out);
        let late = files.last().unwrap();
        assert_eq!(late.path, "late.md");
        assert_eq!(late.truncated, Some(Truncation::Budget));
        assert!(late.hunks.is_empty(), "no body kept past the file cap");
        assert_eq!((late.added, late.removed), (0, 1), "but the line counts");
    }

    #[test]
    fn untracked_is_capped_but_counted() {
        let mut untracked: Vec<String> = Vec::new();
        let mut untracked_total = 0usize;
        for i in 0..(MAX_UNTRACKED * 3) {
            untracked_total += 1;
            if untracked.len() < MAX_UNTRACKED {
                untracked.push(format!("node_modules/p{i}/index.js"));
            }
        }
        let snap = DiffSnapshot {
            untracked,
            untracked_total,
            ..Default::default()
        };
        assert_eq!(snap.untracked.len(), MAX_UNTRACKED, "retention is bounded");
        assert_eq!(
            snap.untracked_count(),
            MAX_UNTRACKED * 3,
            "the count is not"
        );
    }

    #[test]
    #[ignore = "measurement, not an assertion"]
    fn bench_stream_vs_buffer() {
        use std::time::Instant;
        use tty7_core::core::git::{LineSplitter, git_output, git_stream};

        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let args = ["log", "-p", "-n", "400", "--no-color"];

        let t = Instant::now();
        let Ok(out) = git_output(here, &args) else {
            println!("no git here; skipping");
            return;
        };
        let read = t.elapsed();
        let resident = out.stdout.len();
        let t = Instant::now();
        let buffered_files = parse_unified(&String::from_utf8_lossy(&out.stdout));
        let buffered_parse = t.elapsed();
        println!(
            "buffered: {resident} bytes resident, read {read:?}, parse {buffered_parse:?}, \
             {} files",
            buffered_files.len()
        );
        drop(out);

        let t = Instant::now();
        let mut parser = DiffParser::default();
        let mut split = LineSplitter::default();
        let mut peak = 0usize;
        git_stream(here, &args, |chunk| {
            peak = peak.max(chunk.len());
            split.push(chunk, |line| parser.push_line(line));
            true
        })
        .unwrap();
        split.finish(|line| parser.push_line(line));
        let streamed = t.elapsed();
        let streamed_files = parser.finish();
        println!(
            "streamed: peak transient chunk {peak} bytes (vs {resident} resident), \
             read+parse {streamed:?}, {} files",
            streamed_files.len()
        );
        assert_eq!(buffered_files.len(), streamed_files.len());
    }

    #[test]
    #[ignore = "measurement, not an assertion"]
    fn bench_parse_budget() {
        use std::time::Instant;
        let out = many_files(300, 300);
        println!("input: {} bytes, 300 files × 300 lines", out.len());
        let t = Instant::now();
        let files = parse_unified(&out);
        let elapsed = t.elapsed();
        let retained: usize = files
            .iter()
            .flat_map(|f| &f.hunks)
            .map(|h| h.lines.len())
            .sum();
        let bytes: usize = files
            .iter()
            .flat_map(|f| &f.hunks)
            .flat_map(|h| &h.lines)
            .map(|l| l.text.capacity() + std::mem::size_of::<DiffLine>())
            .sum();
        println!(
            "parse {elapsed:?} → {retained} retained lines, ~{} KiB of DiffLine text \
             (unbudgeted would be 90000 lines / ~{} KiB)",
            bytes / 1024,
            90_000 * (24 + std::mem::size_of::<DiffLine>()) / 1024,
        );
    }
}

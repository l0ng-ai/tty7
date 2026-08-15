use std::path::{Path, PathBuf};

use crate::core::git;
use crate::host::Host;

pub const MAX_LINES_PER_FILE: usize = 2000;

pub const MAX_TOTAL_LINES: usize = 20_000;

pub const MAX_FILES_WITH_HUNKS: usize = 500;

pub const AUTO_COLLAPSE_LINES: u32 = 400;

pub const AUTO_COLLAPSE_TOTAL_LINES: usize = 8_000;

pub const AUTO_COLLAPSE_TOTAL_FILES: usize = 100;

pub const MAX_RENDERED_FILES: usize = 300;

pub const MAX_UNTRACKED: usize = 500;

/// `-U<n>`. Git's own default, spelled out because the request carries it.
pub const DEFAULT_CONTEXT: u32 = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Truncation {
    PerFile,
    Budget,
}

/// What a commit is called, for a header that would otherwise have eight hex
/// digits and nothing else to say.
///
/// Rides along on [`DiffSource::Commit`] and takes no part in its identity —
/// see [`DiffSource::tag`]. Defined here rather than in [`log`](super::log) so
/// the dependency between the two modules stays one-way: `log` reaches into
/// `diff` for [`FileStatus`], and nothing goes back. That is also why the
/// timestamp is a bare unix second rather than an
/// [`OffsetTs`](super::log::OffsetTs) — relative time is all the header shows.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CommitLabel {
    pub subject: String,
    pub author: String,
    /// Author time, unix seconds. Zero where it is not known.
    pub at: i64,
}

/// Which patch to ask git for.
///
/// The three working-tree variants are the same three questions the SCM panel
/// asks — `Worktree` is what is not staged, `Staged` is what is, and `Head` is
/// both at once, which is what the overlay has always shown.
#[derive(Clone, Debug, Default)]
pub enum DiffSource {
    /// `git diff` — unstaged changes.
    Worktree,
    /// `git diff --cached` — what a commit right now would contain.
    Staged,
    /// `git diff HEAD` — staged and unstaged together.
    #[default]
    Head,
    /// One commit against its first parent, and optionally what to call it.
    Commit {
        rev: String,
        label: Option<CommitLabel>,
    },
    /// `base...head`: what `head` added since the two diverged.
    Range { base: String, head: String },
}

impl PartialEq for DiffSource {
    fn eq(&self, other: &DiffSource) -> bool {
        self.tag() == other.tag()
    }
}

impl Eq for DiffSource {}

impl std::hash::Hash for DiffSource {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.tag().hash(state);
    }
}

impl DiffSource {
    /// A commit with nothing known about it yet beyond which one it is.
    pub fn commit(rev: impl Into<String>) -> DiffSource {
        DiffSource::Commit {
            rev: rev.into(),
            label: None,
        }
    }

    /// The identity of the patch, with nothing on it that only affects how the
    /// patch is *labelled*.
    ///
    /// `PartialEq`, `Hash` and the overlay's string cache key are all defined
    /// from this one function, so the three cannot drift apart. The rule it
    /// exists to enforce: the same commit opened from the graph (with a
    /// subject in hand) and from a keybinding (without one) is one patch, one
    /// in-flight probe and one overlay. A derived `PartialEq` would make them
    /// two, and the derived `Debug` the overlay's key used to be built from
    /// would have split the probe cache the same way.
    pub fn tag(&self) -> String {
        match self {
            DiffSource::Worktree => "worktree".to_string(),
            DiffSource::Staged => "staged".to_string(),
            DiffSource::Head => "head".to_string(),
            // US, which can occur in neither a refname nor an object id.
            DiffSource::Commit { rev, .. } => format!("commit\u{1f}{rev}"),
            DiffSource::Range { base, head } => format!("range\u{1f}{base}\u{1f}{head}"),
        }
    }

    /// The whole argv, minus pathspecs. Every diff tty7 runs is built here so
    /// there is one place to read, and one place to test, what git is asked.
    pub fn args(&self, context: u32, ignore_whitespace: bool) -> Vec<String> {
        // `core.quotePath=false` is not tidiness: with it on, git renders a
        // non-ASCII path in the `diff --git` header as C octal escapes, and
        // nothing downstream decodes those — the path would simply be wrong.
        let mut argv = strings(&["-c", "core.quotePath=false"]);
        match self {
            DiffSource::Worktree => argv.push("diff".to_string()),
            DiffSource::Staged => argv.extend(strings(&["diff", "--cached"])),
            DiffSource::Head => argv.extend(strings(&["diff", "HEAD"])),
            // `log -p -1`, not `diff-tree`: `diff-tree` does not honour
            // `--first-parent` as a *narrowing* of a merge. Measured on git
            // 2.50.1 against a merge of two branches that each added a file:
            // `diff-tree -p -m --first-parent` emits both files (one patch per
            // parent, concatenated), and dropping `-m` emits nothing at all.
            // `log -p -1 --first-parent` gives the one first-parent patch for a
            // merge, an ordinary commit and the initial commit alike, so there
            // is no special case and no need for `--root`. `--format=` empties
            // the commit header, at the cost of one blank line the parser
            // ignores.
            DiffSource::Commit { rev, .. } => {
                argv.extend(strings(&["log", "-p", "-1", "--format=", "--first-parent"]));
                argv.push(rev.clone());
            }
            // Three dots: measured from the merge base, so unrelated work on
            // `base` does not show up as if `head` had reverted it.
            DiffSource::Range { base, head } => {
                argv.push("diff".to_string());
                argv.push(format!("{base}...{head}"));
            }
        }
        argv.extend(strings(&[
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "-M",
        ]));
        argv.push(format!("-U{context}"));
        if ignore_whitespace {
            argv.push("-w".to_string());
        }
        argv
    }

    /// Whether untracked files belong in the snapshot. They are a property of
    /// the working tree, so a commit or a range has none, and a staged diff
    /// does not either — an untracked file is by definition not in the index.
    pub fn lists_untracked(&self) -> bool {
        matches!(self, DiffSource::Worktree | DiffSource::Head)
    }

    /// Whether every rev this source carries can be handed to git as an
    /// argument. The same rule log's `is_rev` applies: a rev the caller made
    /// up is still a rev git will be handed, and anything that could be read
    /// as an option — `--output=…` most damningly — is refused before it
    /// reaches an argv. Checked by [`probe_diff`], so no in-tree caller can
    /// forget it.
    fn revs_are_arguments(&self) -> bool {
        let ok = |rev: &str| {
            !rev.is_empty() && !rev.starts_with('-') && !rev.contains(|c: char| c.is_control())
        };
        match self {
            DiffSource::Worktree | DiffSource::Staged | DiffSource::Head => true,
            DiffSource::Commit { rev, .. } => ok(rev),
            DiffSource::Range { base, head } => ok(base) && ok(head),
        }
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// How much of a patch is worth keeping in memory.
///
/// The line counts on [`FileDiff`] stay exact past every one of these; only the
/// retained text is bounded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DiffBudget {
    pub max_lines_per_file: usize,
    pub max_total_lines: usize,
    pub max_files_with_hunks: usize,
}

impl DiffBudget {
    /// A whole tree at once: no single file may crowd out the rest.
    pub const PANEL: DiffBudget = DiffBudget {
        max_lines_per_file: MAX_LINES_PER_FILE,
        max_total_lines: MAX_TOTAL_LINES,
        max_files_with_hunks: MAX_FILES_WITH_HUNKS,
    };

    /// One file the user asked for by name. There is nothing to crowd out, so
    /// the per-file cap rises to where it is a defence against a generated
    /// file rather than a rationing rule.
    pub const SINGLE_FILE: DiffBudget = DiffBudget {
        max_lines_per_file: 50_000,
        max_total_lines: MAX_TOTAL_LINES,
        max_files_with_hunks: MAX_FILES_WITH_HUNKS,
    };
}

/// The whole point of the second budget. Pinned here so that trimming
/// [`MAX_LINES_PER_FILE`] one day cannot silently make the two identical.
const _: () =
    assert!(DiffBudget::SINGLE_FILE.max_lines_per_file > DiffBudget::PANEL.max_lines_per_file);

impl Default for DiffBudget {
    fn default() -> DiffBudget {
        DiffBudget::PANEL
    }
}

/// One patch to fetch and parse.
pub struct DiffRequest<'a> {
    pub source: DiffSource,
    /// Empty means the whole tree. Every entry must already be a pathspec —
    /// `:(literal)…` — because git globs a bare path and a file named `a[b].c`
    /// would then match nothing.
    pub paths: &'a [String],
    pub context: u32,
    pub budget: DiffBudget,
    pub ignore_whitespace: bool,
}

impl Default for DiffRequest<'_> {
    fn default() -> DiffRequest<'static> {
        DiffRequest {
            source: DiffSource::default(),
            paths: &[],
            context: DEFAULT_CONTEXT,
            budget: DiffBudget::PANEL,
            ignore_whitespace: false,
        }
    }
}

impl DiffRequest<'_> {
    pub fn args(&self) -> Vec<String> {
        let mut argv = self.source.args(self.context, self.ignore_whitespace);
        if !self.paths.is_empty() {
            argv.push("--".to_string());
            argv.extend(self.paths.iter().cloned());
        }
        argv
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DiffSnapshot {
    pub root: PathBuf,
    pub source: DiffSource,
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
    Copied,
    /// Regular file ↔ symlink ↔ submodule. Not a mode change: `100644` to
    /// `100755` is still [`FileStatus::Modified`].
    TypeChanged,
    /// A path with conflict markers, or one git refused to diff at all.
    Unmerged,
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

/// The whole working tree against `HEAD`, on the panel's budget.
pub fn probe(host: &dyn Host, cwd: &Path) -> Option<DiffSnapshot> {
    probe_diff(host, cwd, &DiffRequest::default())
}

/// A whole file rendered as one addition — what an *untracked* file looks
/// like as a patch. git cannot produce this one: `diff` does not know the
/// file, and `--no-index` needs a null device whose spelling is platform
/// business. So the overlay reads the bytes and this builds the same model a
/// parsed patch would, on the same budget a real single-file patch gets —
/// `added` stays the true count past every truncation, like the parser's.
pub fn synthesize_added(path: &str, bytes: &[u8], budget: &DiffBudget) -> FileDiff {
    // The same test git itself applies: a NUL anywhere in the first 8000
    // bytes means binary.
    let binary = bytes[..bytes.len().min(8000)].contains(&0);
    let mut file = FileDiff {
        path: path.to_string(),
        old_path: None,
        status: FileStatus::Added,
        added: 0,
        removed: 0,
        binary,
        truncated: None,
        hunks: Vec::new(),
    };
    if binary {
        return file;
    }
    let text = String::from_utf8_lossy(bytes);
    let total = text.lines().count();
    file.added = total as u32;
    if total == 0 {
        return file;
    }
    let mut lines = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if i >= budget.max_lines_per_file {
            file.truncated = Some(Truncation::PerFile);
            break;
        }
        lines.push(DiffLine {
            kind: LineKind::Added,
            old_no: None,
            new_no: Some(i as u32 + 1),
            text: line.to_string(),
        });
    }
    file.hunks.push(Hunk {
        header: format!("@@ -0,0 +1,{total} @@"),
        lines,
    });
    file
}

pub fn probe_diff(host: &dyn Host, root: &Path, req: &DiffRequest<'_>) -> Option<DiffSnapshot> {
    if !req.source.revs_are_arguments() {
        return None;
    }
    let toplevel = git::git(host, root, &["rev-parse", "--show-toplevel"])?;
    let toplevel = PathBuf::from(toplevel.trim_end_matches(['\n', '\r']));
    let branch = git::branch_name(host, root)?;

    let argv = req.args();
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let mut parser = DiffParser::with_budget(req.budget);
    let diffed = host.git_lines(root, &argv, &mut |line| parser.push_line(line));
    let files = match diffed {
        Ok(Some(0)) => parser.finish(),
        _ => Vec::new(),
    };

    let mut untracked: Vec<String> = Vec::new();
    let mut untracked_total = 0usize;
    // A pathspec-limited request is answering "what changed in these files",
    // so a list of everything else in the tree would be noise.
    let list_untracked = req.source.lists_untracked() && req.paths.is_empty();
    let listed = list_untracked.then(|| {
        host.git_lines(
            root,
            &[
                "-c",
                "core.quotePath=false",
                "ls-files",
                "--others",
                "--exclude-standard",
                "--full-name",
            ],
            &mut |line| {
                untracked_total += 1;
                if untracked.len() < MAX_UNTRACKED {
                    untracked.push(line.to_string());
                }
            },
        )
    });
    let untracked_ok = match &listed {
        Some(listed) => matches!(listed, Ok(Some(0))),
        None => true,
    };
    if !untracked_ok {
        untracked.clear();
        untracked_total = 0;
    }

    Some(DiffSnapshot {
        root: toplevel,
        source: req.source.clone(),
        branch,
        files,
        untracked,
        untracked_total,
        read_failed: !matches!(diffed, Ok(Some(0))) || !untracked_ok,
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
    budget: DiffBudget,
    files: Vec<FileDiff>,
    old_no: u32,
    new_no: u32,
    file_lines: usize,
    total_lines: usize,
    files_with_hunks: usize,
    in_hunk: bool,
    /// How many marker columns the current hunk's body lines carry: one for an
    /// ordinary patch, one per parent for a combined (`diff --cc`) one.
    markers: usize,
    old_mode: String,
}

impl DiffParser {
    pub fn with_budget(budget: DiffBudget) -> DiffParser {
        DiffParser {
            budget,
            ..DiffParser::default()
        }
    }

    pub fn push_line(&mut self, line: &str) {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let (old_p, new_p) = parse_git_header_paths(rest);
            self.start_file(new_p.clone(), (old_p != new_p).then_some(old_p), None);
            return;
        }
        // A conflicted path comes out as a combined diff against every merge
        // parent at once, which is git's only way of saying "unmerged" in a
        // patch — the `diff --git` header has no status field.
        if let Some(rest) = line
            .strip_prefix("diff --cc ")
            .or_else(|| line.strip_prefix("diff --combined "))
        {
            let path = parse_combined_header_path(rest);
            self.start_file(path, None, Some(FileStatus::Unmerged));
            return;
        }
        // `git diff --cached` cannot show a conflicted path at all and says so
        // on its own line, before any patch.
        if let Some(rest) = line.strip_prefix("* Unmerged path ") {
            self.start_file(rest.to_string(), None, Some(FileStatus::Unmerged));
            return;
        }
        let old_mode = std::mem::take(&mut self.old_mode);
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
        // These four carry one unambiguous path each — unlike the `diff --git`
        // header, where two unquoted paths containing ` b/` cannot be split
        // reliably (git does not quote a path for a mere space). They land
        // after the header, so what they say overrides what it guessed.
        if let Some(old) = line.strip_prefix("rename from ") {
            file.status = FileStatus::Renamed;
            file.old_path = Some(unquote_path(old));
            return;
        }
        if let Some(old) = line.strip_prefix("copy from ") {
            file.status = FileStatus::Copied;
            file.old_path = Some(unquote_path(old));
            return;
        }
        if let Some(new) = line
            .strip_prefix("rename to ")
            .or_else(|| line.strip_prefix("copy to "))
        {
            file.path = unquote_path(new);
            return;
        }
        if let Some(mode) = line.strip_prefix("old mode ") {
            self.old_mode = mode.trim().to_string();
            return;
        }
        if let Some(mode) = line.strip_prefix("new mode ") {
            if !old_mode.is_empty() && object_type(&old_mode) != object_type(mode.trim()) {
                file.status = FileStatus::TypeChanged;
            }
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
            // `@@` is one marker column, `@@@` two — a combined diff carries
            // one per merge parent. Set before any early return: the line
            // counts below stay exact even for a file whose body was dropped.
            self.markers = line.bytes().take_while(|b| *b == b'@').count().max(2) - 1;
            if file.truncated.is_some() {
                return;
            }
            let first_hunk = file.hunks.is_empty();
            if (first_hunk && self.files_with_hunks >= self.budget.max_files_with_hunks)
                || self.total_lines >= self.budget.max_total_lines
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
        let Some((kind, sides, text)) = split_body_line(line, self.markers) else {
            return;
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
        if self.file_lines > self.budget.max_lines_per_file {
            file.truncated = Some(Truncation::PerFile);
            return;
        }
        if self.total_lines >= self.budget.max_total_lines {
            file.truncated = Some(Truncation::Budget);
            return;
        }
        let Some(hunk) = file.hunks.last_mut() else {
            return;
        };
        // Numbered by side, not by colour: in a combined diff a ` +` line is
        // painted as an addition but *exists* in the first parent, and the
        // old-side counter has to walk past it or every number below drifts.
        let o = sides.in_old.then(|| {
            let o = self.old_no;
            self.old_no += 1;
            o
        });
        let n = sides.in_new.then(|| {
            let n = self.new_no;
            self.new_no += 1;
            n
        });
        hunk.lines.push(DiffLine {
            kind,
            old_no: o,
            new_no: n,
            text: text.to_string(),
        });
        self.total_lines += 1;
    }

    pub fn finish(self) -> Vec<FileDiff> {
        fold_type_changes(self.files)
    }

    fn start_file(&mut self, path: String, old_path: Option<String>, status: Option<FileStatus>) {
        self.files.push(FileDiff {
            path,
            old_path,
            status: status.unwrap_or(FileStatus::Modified),
            added: 0,
            removed: 0,
            binary: false,
            truncated: None,
            hunks: Vec::new(),
        });
        self.file_lines = 0;
        self.in_hunk = false;
        self.markers = 1;
        self.old_mode.clear();
    }
}

/// A path whose *type* changed is not one patch: git emits the deletion and the
/// creation back to back, and the header of neither says why. The adjacent pair
/// over one path is the whole signal — git has no other reason to produce it —
/// so it is folded back into the single entry the user thinks of.
fn fold_type_changes(files: Vec<FileDiff>) -> Vec<FileDiff> {
    let mut folded: Vec<FileDiff> = Vec::with_capacity(files.len());
    for file in files {
        let pair = folded.last().is_some_and(|prev| {
            prev.status == FileStatus::Deleted
                && file.status == FileStatus::Added
                && prev.path == file.path
        });
        match folded.last_mut().filter(|_| pair) {
            Some(prev) => {
                prev.status = FileStatus::TypeChanged;
                prev.added += file.added;
                prev.removed += file.removed;
                prev.binary |= file.binary;
                prev.truncated = prev.truncated.or(file.truncated);
                prev.hunks.extend(file.hunks);
            }
            None => folded.push(file),
        }
    }
    folded
}

/// The `100`/`120`/`160` of a git file mode: regular file, symlink, submodule.
/// Permission bits are deliberately dropped — `100644` to `100755` is a
/// modification, not a type change.
fn object_type(mode: &str) -> &str {
    &mode[..mode.len().min(3)]
}

/// Splits a hunk body line into its kind, which sides it exists on, and its
/// text, given how many marker columns the hunk carries. A combined diff marks
/// a line per parent; one `+` or `-` anywhere in those columns settles the
/// *colour*, but the line numbers come from the sides:
///
/// - the line is in the result iff no column says `-`;
/// - the line is in the first parent — the side tty7 numbers — iff its own
///   column says `-`, or says ` ` on a line that is in the result. (` ` on a
///   line outside the result is the other parent's removal; the first parent
///   never had it.)
///
/// For an ordinary one-column diff this reduces to exactly `+`/`-`/context.
fn split_body_line(line: &str, markers: usize) -> Option<(LineKind, LineSides, &str)> {
    let head = line.get(..markers)?;
    let kind = if head.contains('+') {
        LineKind::Added
    } else if head.contains('-') {
        LineKind::Removed
    } else if head.bytes().all(|b| b == b' ') {
        LineKind::Context
    } else {
        return None;
    };
    let in_new = !head.contains('-');
    let first = head.as_bytes().first().copied();
    let in_old = first == Some(b'-') || (first == Some(b' ') && in_new);
    Some((kind, LineSides { in_old, in_new }, &line[markers..]))
}

/// Which sides of the diff a body line exists on. See [`split_body_line`].
#[derive(Clone, Copy)]
struct LineSides {
    in_old: bool,
    in_new: bool,
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
            // The backslash stays in `cur`: the scanner only needs to know the
            // next `"` does not close the quote; `c_unescape` does the decode.
            '\\' if in_quote => {
                cur.push('\\');
                escaped = true;
            }
            '"' => {
                if in_quote {
                    parts.push(c_unescape(&std::mem::take(&mut cur)));
                }
                in_quote = !in_quote;
            }
            _ if in_quote => cur.push(ch),
            _ => {}
        }
    }
    parts
}

/// A single path as written after `rename from ` and friends: C-quoted when it
/// carries a character git always quotes (a control character or a `"` —
/// `core.quotePath=false` stops the quoting of non-ASCII only), bare
/// otherwise.
fn unquote_path(s: &str) -> String {
    let s = s.trim_end_matches(['\n', '\r']);
    match s
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
    {
        Some(inner) => c_unescape(inner),
        None => s.to_string(),
    }
}

/// Decodes the C escapes git writes inside a quoted path — the full set, not
/// just `\"` and `\\`: a path with a real tab arrives as `\t`, and decoding it
/// to a literal `t` breaks the `:(literal)` re-probe for that row. Octal
/// escapes are *bytes* — a multi-byte character arrives as several — so the
/// value is assembled as bytes and read back as UTF-8 at the end.
fn c_unescape(s: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len());
    let push_char = |bytes: &mut Vec<u8>, ch: char| {
        let mut buf = [0u8; 4];
        bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    };
    let mut it = s.chars().peekable();
    while let Some(ch) = it.next() {
        if ch != '\\' {
            push_char(&mut bytes, ch);
            continue;
        }
        match it.next() {
            Some(digit @ '0'..='7') => {
                let mut value = digit as u32 - '0' as u32;
                for _ in 0..2 {
                    match it.peek() {
                        Some(&next @ '0'..='7') => {
                            value = value * 8 + (next as u32 - '0' as u32);
                            it.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(value as u8);
            }
            Some('n') => bytes.push(b'\n'),
            Some('t') => bytes.push(b'\t'),
            Some('r') => bytes.push(b'\r'),
            Some('a') => bytes.push(0x07),
            Some('b') => bytes.push(0x08),
            Some('f') => bytes.push(0x0c),
            Some('v') => bytes.push(0x0b),
            // `\"`, `\\`, and anything git never writes: the character itself.
            Some(other) => push_char(&mut bytes, other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn strip_prefix_ab(p: &str) -> String {
    p.strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p)
        .to_string()
}

/// A combined hunk header carries one `-` range per merge parent before the
/// single `+` range, so the ranges are read positionally rather than by a fixed
/// shape. The first `-` is the first parent's, which is the side tty7 numbers.
fn parse_hunk_starts(line: &str) -> Option<(u32, u32)> {
    let ranges = line.trim_start_matches('@');
    let ranges = &ranges[..ranges.find(" @@")?];
    let mut old = None;
    let mut new = None;
    for range in ranges.split_whitespace() {
        let (sign, rest) = range.split_at_checked(1)?;
        let start: u32 = rest.split(',').next()?.parse().ok()?;
        match sign {
            "-" if old.is_none() => old = Some(start),
            "+" => new = Some(start),
            _ => {}
        }
    }
    Some((old?, new?))
}

/// `diff --cc <path>`: one path, repo-relative, with no `a/` or `b/` prefix.
fn parse_combined_header_path(rest: &str) -> String {
    match rest.starts_with('"') {
        true => parse_quoted_pair(rest)
            .pop()
            .unwrap_or_else(|| rest.to_string()),
        false => rest.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::git::test_support::{PINS, pin_repo_config};

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

    fn argv(source: DiffSource) -> Vec<String> {
        source.args(DEFAULT_CONTEXT, false)
    }

    #[test]
    fn every_source_spells_out_its_own_argv() {
        let common = ["--no-color", "--no-ext-diff", "--no-textconv", "-M", "-U3"];
        let cases: Vec<(DiffSource, Vec<&str>)> = vec![
            (DiffSource::Worktree, vec!["diff"]),
            (DiffSource::Staged, vec!["diff", "--cached"]),
            (DiffSource::Head, vec!["diff", "HEAD"]),
            (
                DiffSource::commit("deadbeef"),
                vec!["log", "-p", "-1", "--format=", "--first-parent", "deadbeef"],
            ),
            (
                DiffSource::Range {
                    base: "main".into(),
                    head: "topic".into(),
                },
                vec!["diff", "main...topic"],
            ),
        ];
        for (source, middle) in cases {
            let want: Vec<String> = ["-c", "core.quotePath=false"]
                .into_iter()
                .chain(middle)
                .chain(common)
                .map(str::to_string)
                .collect();
            assert_eq!(argv(source.clone()), want, "{source:?}");
        }
    }

    /// The one property the whole label mechanism rests on. Break it and the
    /// same commit becomes two probes, two overlays and two cache entries the
    /// moment one of them learns its own subject.
    #[test]
    fn a_commits_label_is_not_part_of_which_commit_it_is() {
        let bare = DiffSource::commit("deadbeef");
        let labelled = DiffSource::Commit {
            rev: "deadbeef".into(),
            label: Some(CommitLabel {
                subject: "fix(scm): the thing".into(),
                author: "Ada".into(),
                at: 1_786_255_391,
            }),
        };
        let other = DiffSource::Commit {
            rev: "deadbeef".into(),
            label: Some(CommitLabel {
                subject: "something else entirely".into(),
                ..Default::default()
            }),
        };

        assert_eq!(bare, labelled);
        assert_eq!(labelled, other);
        assert_eq!(bare.tag(), labelled.tag());
        assert_eq!(hash_of(&bare), hash_of(&labelled));
        assert_eq!(hash_of(&labelled), hash_of(&other));
        // …and the argv, which is the other thing a split cache would show up
        // in: two "different" sources running the identical command.
        assert_eq!(argv(bare.clone()), argv(labelled));

        // Which commit it is still separates them, of course.
        let elsewhere = DiffSource::commit("cafebabe");
        assert_ne!(bare, elsewhere);
        assert_ne!(hash_of(&bare), hash_of(&elsewhere));
        assert_ne!(bare, DiffSource::Head);
        assert_ne!(
            DiffSource::Range {
                base: "a".into(),
                head: "b".into()
            },
            DiffSource::Range {
                base: "b".into(),
                head: "a".into()
            }
        );
        // Every variant is still equal to itself, which `Eq` promises and a
        // hand-written `PartialEq` is exactly where it could stop being true.
        for source in [
            DiffSource::Worktree,
            DiffSource::Staged,
            DiffSource::Head,
            bare,
            elsewhere,
        ] {
            assert_eq!(source, source.clone(), "{source:?}");
            assert_eq!(hash_of(&source), hash_of(&source.clone()));
        }
    }

    fn hash_of(source: &DiffSource) -> u64 {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        source.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn quote_path_is_off_on_every_source() {
        // Left on, every non-ASCII path arrives as C octal escapes — decodable
        // (see below), but the raw spelling needs no decode at all. Verified
        // against git 2.50.1: `diff --git
        // "a/\344\270\255\346\226\207\345\220\215.txt" …` becomes
        // `diff --git a/中文名.txt b/中文名.txt` once this is off.
        for source in [
            DiffSource::Worktree,
            DiffSource::Staged,
            DiffSource::Head,
            DiffSource::commit("HEAD"),
            DiffSource::Range {
                base: "a".into(),
                head: "b".into(),
            },
        ] {
            assert_eq!(&argv(source.clone())[..2], ["-c", "core.quotePath=false"]);
        }
    }

    /// `core.quotePath=false` stops the quoting of non-ASCII only. A path
    /// with a control character or a `"` is *always* C-quoted, so the decoder
    /// has to speak the whole escape set — a tab decoded to a literal `t`
    /// names a path that does not exist, and the `:(literal)` re-probe for
    /// that row comes back empty.
    #[test]
    fn c_quoted_paths_decode_the_full_escape_set() {
        let escaped = parse_unified(
            "diff --git \"a/\\344\\270\\255\\346\\226\\207\\345\\220\\215.txt\" \
             \"b/\\344\\270\\255\\346\\226\\207\\345\\220\\215.txt\"\n",
        );
        assert_eq!(escaped[0].path, "中文名.txt");

        let control = parse_unified("diff --git \"a/x\\ty.rs\" \"b/x\\ty.rs\"\n");
        assert_eq!(control[0].path, "x\ty.rs");

        let quote =
            parse_unified("diff --git \"a/he said \\\"hi\\\".md\" \"b/he said \\\"hi\\\".md\"\n");
        assert_eq!(quote[0].path, "he said \"hi\".md");

        let raw = parse_unified("diff --git a/中文名.txt b/中文名.txt\n");
        assert_eq!(raw[0].path, "中文名.txt");
    }

    /// The synthesized card for an untracked file mirrors what a parsed
    /// added-file patch looks like: true counts past the budget, a hunk
    /// header the renderer can show, git's own binary rule.
    #[test]
    fn an_untracked_file_synthesizes_as_one_addition() {
        let file = synthesize_added("notes.md", b"one\ntwo\nthree\n", &DiffBudget::SINGLE_FILE);
        assert_eq!(file.status, FileStatus::Added);
        assert_eq!((file.added, file.removed), (3, 0));
        assert!(!file.binary);
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].header, "@@ -0,0 +1,3 @@");
        let lines = &file.hunks[0].lines;
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.kind == LineKind::Added));
        assert_eq!((lines[2].old_no, lines[2].new_no), (None, Some(3)));
        assert_eq!(lines[2].text, "three");

        let empty = synthesize_added("empty", b"", &DiffBudget::SINGLE_FILE);
        assert_eq!(empty.added, 0);
        assert!(empty.hunks.is_empty(), "no hunk for a file with no lines");

        let binary = synthesize_added("blob.png", b"\x89PNG\x00\x01", &DiffBudget::SINGLE_FILE);
        assert!(binary.binary);
        assert!(binary.hunks.is_empty());

        let over = "x\n".repeat(DiffBudget::SINGLE_FILE.max_lines_per_file + 5);
        let over = synthesize_added("big.txt", over.as_bytes(), &DiffBudget::SINGLE_FILE);
        assert_eq!(over.truncated, Some(Truncation::PerFile));
        assert_eq!(
            over.added as usize,
            DiffBudget::SINGLE_FILE.max_lines_per_file + 5,
            "the count stays true past the budget"
        );
        assert_eq!(
            over.hunks[0].lines.len(),
            DiffBudget::SINGLE_FILE.max_lines_per_file
        );
    }

    /// git never quotes a path for a mere space, so a `diff --git` header
    /// whose paths contain ` b/` cannot be split reliably — but the `rename
    /// from`/`rename to` lines that follow name one path each, and they win.
    #[test]
    fn rename_lines_override_an_ambiguous_header() {
        let files = parse_unified(
            "diff --git a/my b/old.rs b/my b/new.rs\n\
             similarity index 90%\n\
             rename from my b/old.rs\n\
             rename to my b/new.rs\n",
        );
        assert_eq!(files[0].path, "my b/new.rs");
        assert_eq!(files[0].old_path.as_deref(), Some("my b/old.rs"));
        assert_eq!(files[0].status, FileStatus::Renamed);
    }

    /// A rev that could be read as an option never reaches git — same guard
    /// log's `is_rev` applies, on the module that calls itself the one place
    /// to read what git is asked.
    #[test]
    fn a_rev_shaped_like_an_option_never_reaches_git() {
        let host = crate::host::local::LocalHost::new();
        for source in [
            DiffSource::commit("--output=/tmp/pwned"),
            DiffSource::Range {
                base: "--output=/tmp/pwned".into(),
                head: "main".into(),
            },
            DiffSource::Range {
                base: "main".into(),
                head: "".into(),
            },
        ] {
            let req = DiffRequest {
                source,
                ..DiffRequest::default()
            };
            assert!(
                probe_diff(&*host, Path::new("/"), &req).is_none(),
                "refused before any git runs"
            );
        }
    }

    #[test]
    fn a_request_appends_its_pathspecs_after_a_separator() {
        let paths = [":(literal)src/a[b].rs".to_string()];
        let req = DiffRequest {
            source: DiffSource::Staged,
            paths: &paths,
            context: 0,
            budget: DiffBudget::SINGLE_FILE,
            ignore_whitespace: true,
        };
        assert_eq!(
            req.args(),
            [
                "-c",
                "core.quotePath=false",
                "diff",
                "--cached",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "-M",
                "-U0",
                "-w",
                "--",
                ":(literal)src/a[b].rs",
            ]
        );
        assert_eq!(
            DiffRequest::default()
                .args()
                .iter()
                .filter(|a| *a == "--")
                .count(),
            0,
            "a whole-tree request has nothing to separate"
        );
    }

    #[test]
    fn the_default_request_is_the_panel_probe() {
        let req = DiffRequest::default();
        assert_eq!(req.source, DiffSource::Head);
        assert_eq!(req.budget, DiffBudget::PANEL);
        assert_eq!(req.args(), argv(DiffSource::Head));
    }

    #[test]
    fn a_default_snapshot_still_reads_as_head() {
        let snap = DiffSnapshot::default();
        assert_eq!(
            snap.source,
            DiffSource::Head,
            "`..Default::default()` callers keep the source they always had"
        );
    }

    #[test]
    fn the_single_file_budget_only_lifts_the_per_file_cap() {
        assert_eq!(DiffBudget::PANEL, DiffBudget::default());
        assert_eq!(
            DiffBudget::SINGLE_FILE.max_total_lines,
            DiffBudget::PANEL.max_total_lines
        );
        assert_eq!(
            DiffBudget::SINGLE_FILE.max_files_with_hunks,
            DiffBudget::PANEL.max_files_with_hunks
        );

        let mut out = String::from(
            "diff --git a/big.txt b/big.txt\nindex 1..2 100644\n--- a/big.txt\n+++ b/big.txt\n@@ -0,0 +1,3000 @@\n",
        );
        for i in 0..3000 {
            out.push_str(&format!("+line {i}\n"));
        }
        let mut parser = DiffParser::with_budget(DiffBudget::SINGLE_FILE);
        for line in out.lines() {
            parser.push_line(line);
        }
        let files = parser.finish();
        assert_eq!(files[0].truncated, None, "3000 lines fit under 50_000");
        assert_eq!(files[0].hunks[0].lines.len(), 3000);
    }

    #[test]
    fn untracked_files_belong_to_the_working_tree_only() {
        assert!(DiffSource::Worktree.lists_untracked());
        assert!(DiffSource::Head.lists_untracked());
        assert!(!DiffSource::Staged.lists_untracked());
        assert!(!DiffSource::commit("x").lists_untracked());
        assert!(
            !DiffSource::Range {
                base: "a".into(),
                head: "b".into()
            }
            .lists_untracked()
        );
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
    fn parses_copies() {
        let out = "\
diff --git a/tpl.rs b/copy.rs
similarity index 100%
copy from tpl.rs
copy to copy.rs
";
        let files = parse_unified(out);
        assert_eq!(files[0].status, FileStatus::Copied);
        assert_eq!(files[0].path, "copy.rs");
        assert_eq!(files[0].old_path.as_deref(), Some("tpl.rs"));
    }

    #[test]
    fn a_type_change_is_one_file_not_a_delete_and_an_add() {
        // git 2.50.1, `git diff` over a regular file replaced by a symlink.
        let out = "\
diff --git a/t.txt b/t.txt
deleted file mode 100644
index 587be6b..0000000
--- a/t.txt
+++ /dev/null
@@ -1 +0,0 @@
-x
diff --git a/t.txt b/t.txt
new file mode 120000
index 0000000..1de5659
--- /dev/null
+++ b/t.txt
@@ -0,0 +1 @@
+target
\\ No newline at end of file
";
        let files = parse_unified(out);
        assert_eq!(files.len(), 1, "one path, one row");
        assert_eq!(files[0].status, FileStatus::TypeChanged);
        assert_eq!((files[0].added, files[0].removed), (1, 1));
        assert_eq!(files[0].hunks.len(), 2, "both halves stay readable");
    }

    #[test]
    fn a_permission_change_is_still_a_modification() {
        let files = parse_unified("diff --git a/t.sh b/t.sh\nold mode 100644\nnew mode 100755\n");
        assert_eq!(files[0].status, FileStatus::Modified);
    }

    #[test]
    fn a_mode_change_across_object_types_is_a_type_change() {
        let files = parse_unified("diff --git a/t b/t\nold mode 100644\nnew mode 120000\n");
        assert_eq!(files[0].status, FileStatus::TypeChanged);
    }

    #[test]
    fn a_delete_and_an_add_of_different_paths_stay_two_files() {
        let files = parse_unified(
            "diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n\
             diff --git a/new.txt b/new.txt\nnew file mode 100644\n",
        );
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].status, FileStatus::Deleted);
        assert_eq!(files[1].status, FileStatus::Added);
    }

    #[test]
    fn a_conflicted_file_parses_as_a_combined_diff() {
        // git 2.50.1, `git diff` during a conflicted merge. Two marker columns,
        // and a hunk header with one range per parent.
        let out = "\
diff --cc f.txt
index af70335,f794161..0000000
--- a/f.txt
+++ b/f.txt
@@@ -1,3 -1,3 +1,7 @@@
  a
++<<<<<<< HEAD
 +MAIN
++=======
+ SIDE
++>>>>>>> side
  c
";
        let files = parse_unified(out);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "f.txt");
        assert_eq!(files[0].status, FileStatus::Unmerged);

        let lines = &files[0].hunks[0].lines;
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[0].kind, LineKind::Context);
        assert_eq!(lines[0].text, "a", "both marker columns come off the text");
        assert_eq!((lines[0].old_no, lines[0].new_no), (Some(1), Some(1)));
        assert_eq!(lines[1].kind, LineKind::Added);
        assert_eq!(lines[1].text, "<<<<<<< HEAD");
        assert_eq!(lines[4].kind, LineKind::Added);
        assert_eq!(lines[4].text, "SIDE", "added on one side is still added");
        assert_eq!(lines[6].text, "c");
        assert_eq!((files[0].added, files[0].removed), (5, 0));

        // Numbers follow the *sides*, not the colour: ` +MAIN` is painted as
        // an addition but exists in the first parent (it is HEAD's own line),
        // so the old counter walks past it — and `c` lands on old line 3,
        // exactly the `-1,3` the hunk header promises. `+ SIDE` is the other
        // parent's line: no old number.
        assert_eq!(
            (lines[2].old_no, lines[2].new_no),
            (Some(2), Some(3)),
            "MAIN: {:?}",
            lines[2]
        );
        assert_eq!((lines[4].old_no, lines[4].new_no), (None, Some(5)));
        assert_eq!((lines[6].old_no, lines[6].new_no), (Some(3), Some(7)));
    }

    #[test]
    fn a_staged_diff_reports_the_paths_it_refused_to_show() {
        let files = parse_unified("* Unmerged path f.txt\ndiff --git a/ok.rs b/ok.rs\n");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "f.txt");
        assert_eq!(files[0].status, FileStatus::Unmerged);
        assert!(files[0].hunks.is_empty());
        assert_eq!(files[1].path, "ok.rs");
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

    /// A repo with a merge whose two parents each added a file, plus an
    /// ordinary commit and an initial one. Returns `None` if git is missing.
    fn merge_repo(name: &str) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("tty7-diff-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        let run = |args: &[&str]| {
            let mut full = PINS.to_vec();
            full.extend_from_slice(args);
            git::git_output(&dir, &full)
                .ok()
                .filter(|out| out.success())
                .is_some()
        };
        if !run(&["init", "-q", "-b", "mainwork", "."]) {
            return None;
        }
        // Before the first blob is written, and in the repository rather than
        // only on this closure's command lines: `probe_diff` runs its own git,
        // and on Windows that git would otherwise inherit the system-wide
        // `core.autocrlf=true` and rewrite every checkout of this fixture.
        assert!(pin_repo_config(&dir));
        std::fs::write(dir.join("base.txt"), "base\n").ok()?;
        run(&["add", "-A"]);
        run(&["commit", "-qm", "initial"]);
        run(&["checkout", "-q", "-b", "side"]);
        std::fs::write(dir.join("s.txt"), "side\n").ok()?;
        run(&["add", "-A"]);
        run(&["commit", "-qm", "side"]);
        run(&["checkout", "-q", "mainwork"]);
        std::fs::write(dir.join("m.txt"), "main\n").ok()?;
        run(&["add", "-A"]);
        run(&["commit", "-qm", "main"]);
        run(&["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
        Some(dir)
    }

    fn rev(dir: &Path, spec: &str) -> String {
        git::git_output(dir, &["rev-parse", spec])
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_default()
    }

    fn commit_files(host: &dyn Host, dir: &Path, spec: &str) -> Vec<String> {
        let req = DiffRequest {
            source: DiffSource::commit(rev(dir, spec)),
            ..Default::default()
        };
        probe_diff(host, dir, &req)
            .expect("the scratch repo answers")
            .files
            .into_iter()
            .map(|f| f.path)
            .collect()
    }

    /// The reason `Commit` runs `log -p -1 --first-parent` and not `diff-tree`:
    /// on git 2.50.1 `diff-tree -p -m --first-parent` emits *both* parents'
    /// patches concatenated, and without `-m` it emits nothing for a merge at
    /// all. Either would show a file the merge did not touch on this side.
    #[test]
    fn a_merge_commit_yields_only_its_first_parent_patch() {
        let Some(dir) = merge_repo("merge") else {
            return;
        };
        let host = crate::host::local::LocalHost::new();

        assert_eq!(commit_files(&*host, &dir, "HEAD"), ["s.txt"]);
        assert_eq!(commit_files(&*host, &dir, "HEAD^"), ["m.txt"]);
        assert_eq!(
            commit_files(&*host, &dir, "HEAD^^"),
            ["base.txt"],
            "the initial commit diffs against the empty tree, not an error"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_three_working_tree_sources_split_staged_from_unstaged() {
        let Some(dir) = merge_repo("sources") else {
            return;
        };
        let host = crate::host::local::LocalHost::new();
        std::fs::write(dir.join("base.txt"), "base\nstaged\n").unwrap();
        let _ = git::git_output(&dir, &["add", "base.txt"]);
        std::fs::write(dir.join("m.txt"), "main\nunstaged\n").unwrap();
        std::fs::write(dir.join("fresh.txt"), "new\n").unwrap();

        let files = |source: DiffSource| -> Vec<String> {
            let req = DiffRequest {
                source,
                ..Default::default()
            };
            probe_diff(&*host, &dir, &req)
                .expect("the scratch repo answers")
                .files
                .into_iter()
                .map(|f| f.path)
                .collect()
        };
        assert_eq!(files(DiffSource::Staged), ["base.txt"]);
        assert_eq!(files(DiffSource::Worktree), ["m.txt"]);
        assert_eq!(files(DiffSource::Head), ["base.txt", "m.txt"]);

        let staged = probe_diff(
            &*host,
            &dir,
            &DiffRequest {
                source: DiffSource::Staged,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            staged.untracked.is_empty(),
            "an untracked file is by definition not staged"
        );
        assert_eq!(probe(&*host, &dir).unwrap().untracked, ["fresh.txt"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_non_ascii_path_survives_the_round_trip() {
        let Some(dir) = merge_repo("utf8") else {
            return;
        };
        let host = crate::host::local::LocalHost::new();
        std::fs::write(dir.join("中文名.txt"), "one\n").unwrap();
        let _ = git::git_output(&dir, &["add", "-A"]);

        let snap = probe_diff(
            &*host,
            &dir,
            &DiffRequest {
                source: DiffSource::Staged,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            snap.files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["中文名.txt"],
            "octal escapes would have made this `344\\270\\255…`"
        );

        std::fs::write(dir.join("未跟踪.txt"), "two\n").unwrap();
        assert!(
            probe(&*host, &dir)
                .unwrap()
                .untracked
                .contains(&"未跟踪.txt".to_string()),
            "ls-files quotes the same way, and is unquoted the same way"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pathspec_narrows_the_patch_and_drops_the_untracked_list() {
        let Some(dir) = merge_repo("pathspec") else {
            return;
        };
        let host = crate::host::local::LocalHost::new();
        std::fs::write(dir.join("base.txt"), "base\nedit\n").unwrap();
        std::fs::write(dir.join("m.txt"), "main\nedit\n").unwrap();
        std::fs::write(dir.join("fresh.txt"), "new\n").unwrap();

        let paths = [":(literal)m.txt".to_string()];
        let snap = probe_diff(
            &*host,
            &dir,
            &DiffRequest {
                source: DiffSource::Worktree,
                paths: &paths,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            snap.files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["m.txt"]
        );
        assert!(snap.untracked.is_empty());
        assert!(!snap.read_failed);
        assert_eq!(snap.source, DiffSource::Worktree);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "measurement, not an assertion"]
    fn bench_stream_vs_buffer() {
        use crate::core::git::{LineSplitter, git_output, git_stream};
        use std::time::Instant;

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

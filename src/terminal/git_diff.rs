//! The full working-tree diff behind the sidebar's `+N −N` counts: `git diff
//! HEAD` parsed into files → hunks → lines, for the read-only diff overlay
//! (see [`crate::ui::diff_overlay`], which owns how it is opened) that covers
//! the terminal.
//!
//! Same discipline as [`git_status`](crate::terminal::git_status): every
//! invocation goes through the shared [`git_status::git`] helper — so it runs
//! on the pane's own [`Host`], read-only via `GIT_OPTIONAL_LOCKS=0` — on a
//! background executor, and is never trusted to be fast; the UI shows the
//! previous snapshot (or a loading state) until a probe lands. Asking the pane's
//! host rather than this machine is also what makes the overlay work at all for
//! a pane whose repository lives somewhere else.
//!
//! And never trusted to be *small*, either. `git diff HEAD` is the one git read
//! in this app whose output scales with the working tree rather than with what
//! the UI can show, so two things bound it: the read is incremental
//! ([`Host::git_lines`](crate::ui::host_ops::Host::git_lines)) rather than
//! buffered whole, and the parser retains at most [`MAX_LINES_PER_FILE`] per
//! file and [`MAX_TOTAL_LINES`] / [`MAX_FILES_WITH_HUNKS`] across the
//! repository. The `+`/`−` counts deliberately escape all of it: they are
//! compared against `git diff --numstat` to decide whether the overlay is
//! stale, so a capped total would disagree forever and re-probe in a loop.

use std::path::{Path, PathBuf};

use crate::terminal::git_status;
use crate::ui::host_ops::Host;

/// Cap on parsed diff lines per file. A generated lockfile or vendored blob
/// can be tens of thousands of lines; past this the file's hunks stop and the
/// overlay shows a "truncated" notice instead of building a giant element
/// tree. Generous enough that real hand-written changes never hit it.
pub const MAX_LINES_PER_FILE: usize = 2000;

/// Repo-wide cap on retained diff lines. [`MAX_LINES_PER_FILE`] bounds one
/// pathological file; this bounds the *sum*, which is the shape a working tree
/// full of agent edits actually takes — two hundred files of three hundred
/// lines each never trip the per-file cap yet retain 60k `DiffLine`s, each an
/// owned `String`. Past this the parser keeps counting `+`/`−` (the header
/// numbers must stay honest — see the note in [`parse_unified`]) but stops
/// retaining line text.
pub const MAX_TOTAL_LINES: usize = 20_000;

/// Repo-wide cap on how many files keep their hunks. A branch that renames a
/// vendored tree can list thousands of files whose diffs are each tiny; every
/// one of them still costs a `Vec<Hunk>`. Files past this keep their header row
/// (path, status, counts) and lose only the body.
pub const MAX_FILES_WITH_HUNKS: usize = 500;

/// A file's added+removed size at which the overlay collapses it by default
/// (GitHub's "Load diff" treatment) — the user can still expand it by click.
pub const AUTO_COLLAPSE_LINES: u32 = 400;

/// Repo-wide counterpart to [`AUTO_COLLAPSE_LINES`]: once the snapshot's
/// *retained* lines exceed this, every file starts collapsed and the overlay
/// leads with the oversized-diff summary. Two differences from the per-file
/// threshold matter here — this counts context lines too (they are rendered,
/// so they are what costs), and it is a sum, so many medium files add up the
/// way one big file does.
///
/// Counting context is also why this sits well above the row count that first
/// looks alarming. A hunk carries three lines of context each side by default,
/// so an ordinary afternoon — forty files, a handful of small hunks each —
/// retains four to six lines for every line it actually changed: at 2000 the
/// threshold fired on a tree whose `+N −N` read about 400, which is nobody's
/// idea of a diff too big to open. The number to compare against is
/// [`MAX_TOTAL_LINES`], the point past which the parser stops retaining at all;
/// this is deliberately a large fraction of it, because collapsing everything is
/// the heavier intervention of the two and should not arrive first by much.
pub const AUTO_COLLAPSE_TOTAL_LINES: usize = 8_000;

/// The same idea by file count. Set well clear of a busy-but-ordinary tree —
/// forty changed files of a few lines each is a normal afternoon and must still
/// open expanded — because rows, not cards, are what actually cost: this axis
/// only catches the tree so wide that a card per file is itself the problem.
pub const AUTO_COLLAPSE_TOTAL_FILES: usize = 100;

/// Hard ceiling on file cards the overlay builds at all. Past this the list is
/// cut and a "… and N more" line stands in for the tail, so the element tree
/// stays bounded no matter what the working tree looks like. Also bounds the
/// untracked section, which is the same one-row-per-path shape.
pub const MAX_RENDERED_FILES: usize = 300;

/// Repo-wide cap on retained untracked paths. `git ls-files --others` answers
/// with the whole tree of anything not yet ignored — a fresh clone before
/// `node_modules` / `target` / `.venv` reach `.gitignore` reports tens of
/// thousands of paths — and every one of them is an owned `String` here and a
/// row in the overlay. The count stays exact past the cap
/// ([`DiffSnapshot::untracked_total`]); only the retained list is bounded.
pub const MAX_UNTRACKED: usize = 500;

/// Why a file's hunks stop short of its real diff.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Truncation {
    /// This one file exceeded [`MAX_LINES_PER_FILE`].
    PerFile,
    /// The repo-wide budget ([`MAX_TOTAL_LINES`] / [`MAX_FILES_WITH_HUNKS`])
    /// ran out — the file itself may be small.
    Budget,
}

/// One parsed `git diff HEAD` for a repo, plus the untracked files `diff`
/// itself can't see. This is the overlay's whole model.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DiffSnapshot {
    /// The work-tree root the diff was taken in.
    pub root: PathBuf,
    /// Branch name (or short sha when detached) — the overlay's title.
    pub branch: String,
    /// Changed tracked files, in `git diff` order.
    pub files: Vec<FileDiff>,
    /// Untracked (new, un-added) paths, repo-relative. Listed by name only:
    /// `git diff HEAD` has no blob to diff them against, and agents create
    /// files constantly — hiding them would make the overlay look like it
    /// lost work.
    ///
    /// Capped at [`MAX_UNTRACKED`]; [`untracked_count`](Self::untracked_count)
    /// is the honest total.
    pub untracked: Vec<String>,
    /// How many untracked paths `git ls-files --others` actually reported,
    /// which is not `untracked.len()` once the cap bites. Same discipline as
    /// [`totals`](Self::totals): what we *retain* is budgeted, what we *report*
    /// stays exact, because a count that shrank with the budget would read as
    /// files having disappeared. Read it through
    /// [`untracked_count`](Self::untracked_count).
    pub untracked_total: usize,
    /// One of the two reads behind this snapshot did not complete, so the
    /// emptiness below it means "we could not look", not "there is nothing".
    ///
    /// A failed probe still produces a snapshot — the overlay keeps its branch
    /// and its shape, and the next refresh fills it in, which is better than
    /// blanking. But an empty file list renders as *Working tree clean*, and
    /// that sentence is a claim about the repository: saying it because a read
    /// timed out tells the reader their changes are gone. This is the bit that
    /// keeps the two apart.
    ///
    /// Newly reachable, too. A buffered read either arrived or errored; a
    /// stream can also be refused ([`MAX_CONCURRENT_GIT_STREAMS`] on one
    /// connection) or go silent mid-diff, so the empty-because-broken case is
    /// no longer rare enough to leave conflated with the empty-because-clean
    /// one.
    ///
    /// [`MAX_CONCURRENT_GIT_STREAMS`]: tty7_core::daemon::control::MAX_CONCURRENT_GIT_STREAMS
    pub read_failed: bool,
}

impl DiffSnapshot {
    /// Total added/removed line counts across all files — the overlay's
    /// header numbers, matching the sidebar's `+N −N` by construction (both
    /// sum per-file counts of the same `HEAD` diff).
    ///
    /// Deliberately *not* affected by any truncation: the parser keeps counting
    /// past every cap, because this number is compared against the status
    /// cache's `git diff --numstat` totals to decide whether the overlay is
    /// stale. A capped total would never match, and the overlay would re-probe
    /// in a loop.
    pub fn totals(&self) -> (u32, u32) {
        self.files
            .iter()
            .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed))
    }

    /// The true number of untracked paths, whether or not the retained list was
    /// capped. Falls back to the retained length so a snapshot built by hand
    /// (tests, `..Default::default()`) can't under-report — the fallback is
    /// never wrong, since `untracked_total` is only ever ≥ `untracked.len()`.
    pub fn untracked_count(&self) -> usize {
        self.untracked_total.max(self.untracked.len())
    }

    /// Every whole-snapshot number the render path needs, in one pass.
    ///
    /// The overlay asks six questions of a landed snapshot — is it oversized,
    /// what are the totals, how many lines were retained, did the budget fire,
    /// did the per-file cap fire, how many untracked — and it asks them while
    /// building the element tree, so they run on the UI thread on every render.
    /// `files` is deliberately *not* capped (only hunks are, by
    /// [`MAX_FILES_WITH_HUNKS`]), so answering them one accessor at a time is
    /// six walks over a list whose length is the size of the working tree, on
    /// exactly the tree this whole module exists to keep responsive. Hence one
    /// walk answering all of them, and no per-question accessors to drift from
    /// it.
    ///
    /// Computed rather than cached in the struct on purpose: the snapshot is
    /// `PartialEq` and built by hand all over the tests with
    /// `..Default::default()`, and a stored count would silently read as zero
    /// for every one of them.
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
            // Changed files only. Untracked paths are bounded where they are
            // *rendered* instead — see the field's own note for why collapsing
            // bodies is the wrong lever for them.
            oversized: self.files.len() > AUTO_COLLAPSE_TOTAL_FILES
                || retained_lines > AUTO_COLLAPSE_TOTAL_LINES,
            budget_exhausted,
            per_file_truncated,
        }
    }
}

/// Everything [`DiffSnapshot::stats`] answers in one walk. See that method for
/// why the render path wants them together.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DiffStats {
    /// `(added, removed)` — exact, never affected by truncation. See
    /// [`DiffSnapshot::totals`].
    pub totals: (u32, u32),
    /// Diff lines actually kept, summed over every file. This — not
    /// [`totals`](Self::totals) — is what the overlay would have to build rows
    /// for if every file were expanded, so it's what the render-side thresholds
    /// compare against. Counted one `len()` per hunk, not per line.
    pub retained_lines: usize,
    /// True untracked count, cap or no cap. See
    /// [`DiffSnapshot::untracked_count`].
    pub untracked_count: usize,
    /// Too big to open expanded: every file starts collapsed and the overlay
    /// leads with the "too large to render efficiently" summary. Not a refusal —
    /// individual files still expand by click, which is the escape hatch the
    /// summary points at.
    ///
    /// Changed files and retained lines only. Untracked paths are the same
    /// one-row-per-entry cost, but this is not the lever that answers them:
    /// collapsing every file body leaves the untracked section rendering exactly
    /// as many rows as before, because that section has no bodies to fold. A
    /// tree with an un-ignored `node_modules` and three edited files would have
    /// folded away the three cheap things and kept the expensive one — while
    /// telling the reader their working tree was too large to render. The
    /// untracked list is bounded where it is actually built, by
    /// [`MAX_UNTRACKED`] on retention and [`MAX_RENDERED_FILES`] on rows.
    pub oversized: bool,
    /// The repo-wide budget dropped hunks that a smaller diff would have kept —
    /// the overlay says so, so a missing body reads as a cap rather than as tty7
    /// losing the change.
    pub budget_exhausted: bool,
    /// Any file was cut at [`MAX_LINES_PER_FILE`] — the sibling axis, which the
    /// oversized banner has to name separately because the two compose.
    pub per_file_truncated: bool,
}

/// How a file changed vs `HEAD` — drives the status glyph in its header row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// One changed file: its header-row facts plus the parsed hunks.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FileDiff {
    /// New path (repo-relative); for a deletion, the old path.
    pub path: String,
    /// The pre-rename path, only when `status == Renamed`.
    pub old_path: Option<String>,
    pub status: FileStatus,
    /// Lines added / removed in this file (counted from the parsed hunks).
    pub added: u32,
    pub removed: u32,
    /// Binary file — no hunks, the header row says "binary" instead.
    pub binary: bool,
    /// Hunk parsing stopped short of the file's real diff, and why; the overlay
    /// appends a "truncated" footer under the last hunk. `None` means the body
    /// is complete.
    pub truncated: Option<Truncation>,
    pub hunks: Vec<Hunk>,
}

/// One `@@` hunk: its header line (kept verbatim, function context and all)
/// and the diff lines under it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hunk {
    /// The full `@@ -a,b +c,d @@ …` line as git printed it.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

/// One diff line with the gutter numbers it carries: an added line has only a
/// new number, a removed line only an old one, context both.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    /// The line's text, without the leading `+`/`-`/space marker.
    pub text: String,
}

/// Probe the full diff snapshot for `cwd` on `host`, or `None` when it isn't
/// inside a git work tree. Blocking (three `git` invocations, three round trips
/// on a remote host) — call it through `HostOps`, never on the UI thread.
pub fn probe(host: &dyn Host, cwd: &Path) -> Option<DiffSnapshot> {
    // No `exists` pre-check: a vanished cwd fails the first invocation with
    // `NotFound`, which is the same `None` one round trip cheaper.
    // Doubles as the "is this a repo" gate, same as the status probe.
    let root = git_status::git(host, cwd, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root.trim_end_matches(['\n', '\r']));
    let branch = git_status::branch_name(host, cwd)?;
    // `-M` folds a delete+add pair back into one rename entry; `--no-ext-diff`
    // keeps a configured external diff tool from replacing the parseable
    // unified format. A failed diff (e.g. racing a concurrent git write) still
    // yields a snapshot — an empty file list with the branch — rather than
    // hiding the overlay; the next refresh fills it in.
    // Incremental, not buffered: `git diff HEAD` on a big work tree prints tens
    // of megabytes, and holding all of it before parsing could drop what it
    // doesn't keep is the cost issue #239 measured. `git_lines` funnels through
    // the pane's own host either way — it is streaming where the transport can
    // carry it and buffered where it can't, so the lines seen here are the same
    // either way.
    let mut parser = DiffParser::default();
    let diffed = host.git_lines(
        cwd,
        &["diff", "--no-color", "--no-ext-diff", "-M", "HEAD"],
        &mut |line| parser.push_line(line),
    );
    let files = match diffed {
        // A failed diff (e.g. racing a concurrent git write) still yields a
        // snapshot — an empty file list with the branch — rather than hiding
        // the overlay; the next refresh fills it in. A *partial* read is
        // discarded rather than shown: the stream is reassembled into lines, so
        // what a cut one is missing is the tail of the diff, and half a diff
        // presented as a whole one is worse than none.
        Ok(Some(0)) => parser.finish(),
        _ => Vec::new(),
    };
    // `--full-name` pins paths to the repo root regardless of which
    // subdirectory the pane sits in, matching the diff's path space. Capped for
    // the same reason the diff is: `--others` walks everything not yet ignored,
    // so one un-ignored dependency directory answers with tens of thousands of
    // paths.
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
        // A failed listing is "we don't know", not "there are none" — same
        // shape as the diff above.
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

/// Parse `git diff` unified output into per-file structures. Tolerant by
/// construction: unrecognized metadata lines between the `diff --git` header
/// and the first hunk (modes, index, similarity) are simply skipped, so a git
/// version printing extra headers degrades to "fewer facts", never a panic.
///
/// The whole-string form the tests drive the parser through. [`probe`] feeds
/// [`DiffParser`] a line at a time off the host's streaming read instead, so no
/// caller in the app ever holds the full diff as one `String`.
#[cfg(test)]
pub fn parse_unified(out: &str) -> Vec<FileDiff> {
    let mut parser = DiffParser::default();
    for line in out.lines() {
        parser.push_line(line);
    }
    parser.finish()
}

/// The unified-diff parser as an incremental state machine, so `git diff`'s
/// output can be consumed a line at a time off a pipe rather than buffered
/// whole. Enforces both the per-file and the repo-wide retention budgets; see
/// [`MAX_LINES_PER_FILE`] and [`MAX_TOTAL_LINES`].
#[derive(Default)]
pub struct DiffParser {
    files: Vec<FileDiff>,
    /// Line-number counters for the hunk currently being filled.
    old_no: u32,
    new_no: u32,
    /// Lines consumed by the current file's hunks, for the per-file cap.
    file_lines: usize,
    /// Lines retained across every file so far, for the repo-wide cap.
    total_lines: usize,
    /// Files that actually kept at least one hunk, for the repo-wide file cap.
    /// Counted at the first `@@`, not at the file header: a pure rename or a
    /// binary blob has no body and must not spend the budget on nothing.
    files_with_hunks: usize,
    /// Whether the last `@@` header opened a body we're still inside. Tracked
    /// explicitly rather than inferred from "the file has hunks": a file the
    /// budget truncated never gets a `Hunk` pushed, but its lines still have to
    /// be *counted*, and a removed line whose own text starts with `--- ` must
    /// not be mistaken for the file header it looks like.
    in_hunk: bool,
}

impl DiffParser {
    /// Feed one line of `git diff` output (no trailing newline).
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
            return; // preamble before any header (shouldn't happen)
        };
        // ── File-level metadata between the header and the first hunk ──────
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
        // `--- a/x` / `+++ b/x` repeat what the header said; `rename to`,
        // `index`, modes and similarity scores add nothing we render. But only
        // skip them *outside* hunk bodies — a removed line legitimately starts
        // with `--- ` inside one.
        if !self.in_hunk
            && (line.starts_with("--- ") || line.starts_with("+++ ") || !is_hunk_line(line))
            && !line.starts_with("@@")
        {
            return;
        }
        // ── Hunks ───────────────────────────────────────────────────────────
        if line.starts_with("@@") {
            // A truncated file still enters the body — its lines have to be
            // counted — it just doesn't get a `Hunk` to keep them in.
            self.in_hunk = true;
            if file.truncated.is_some() {
                return;
            }
            // The repo-wide budget is charged here rather than at the file
            // header, so a file with no body at all (a pure rename, a binary
            // blob) neither consumes the file budget nor gets flagged as
            // truncated for a body it never had.
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
            return; // stray content outside any hunk
        }
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Added, &line[1..]),
            Some(b'-') => (LineKind::Removed, &line[1..]),
            Some(b' ') => (LineKind::Context, &line[1..]),
            // `\ No newline at end of file` and anything else: not a diff line.
            _ => return,
        };
        // Count added/removed *before* the truncation gate: the caps are about
        // element volume, but the header numbers must stay honest (they are
        // compared against `--numstat` to detect staleness), so lines past a cap
        // still count even though they're never kept.
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

    /// The parsed files. A truncated file still counts +/− for its whole diff
    /// (the parser keeps counting past every cap), so totals stay consistent
    /// with `--numstat`.
    pub fn finish(self) -> Vec<FileDiff> {
        self.files
    }
}

/// Whether a line can only belong to a hunk body (`+`/`-`/space/`\` lead).
fn is_hunk_line(line: &str) -> bool {
    matches!(line.as_bytes().first(), Some(b'+' | b'-' | b' ' | b'\\')) || line.is_empty()
}

/// Split the `a/old b/new` tail of a `diff --git` header into the two paths.
///
/// Plain names split on the ` b/` separator; paths with spaces work because
/// git quotes *those* (`"a/x y" "b/x y"`), handled by the quoted branch. A
/// path containing a literal ` b/` unquoted is ambiguous in git's own format —
/// we take the last occurrence, matching git's convention of the `b/` side
/// naming the current file.
fn parse_git_header_paths(rest: &str) -> (String, String) {
    // Quoted form: "a/path with spaces" "b/path with spaces".
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
    // Unsplittable — show the whole tail rather than nothing.
    (rest.to_string(), rest.to_string())
}

/// Parse up to two double-quoted strings (git's C-style quoting, minus octal
/// escapes — good enough for spaces, the common case).
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

/// Drop the `a/` / `b/` prefix git puts on header paths.
fn strip_prefix_ab(p: &str) -> String {
    p.strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p)
        .to_string()
}

/// The old/new start line numbers from a `@@ -a,b +c,d @@` header.
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

    /// The sample covers modify / add / delete / binary; statuses, counts, and
    /// hunk line numbers all land where the unified format says they should.
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
        // Context line carries both numbers, tracking the hunk starts.
        assert_eq!((lines[0].old_no, lines[0].new_no), (Some(10), Some(10)));
        assert_eq!(lines[1].kind, LineKind::Removed);
        assert_eq!(lines[1].old_no, Some(11));
        assert_eq!(lines[1].new_no, None);
        assert_eq!(lines[2].kind, LineKind::Added);
        assert_eq!(lines[2].new_no, Some(11));
        assert_eq!(lines[3].new_no, Some(12));
        assert_eq!(lines[3].text, "let c = 3;");
        // Trailing context resumes both counters.
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

    /// Renames keep both paths and don't show phantom +/− lines.
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

    /// Quoted headers (paths with spaces) resolve to the unquoted paths.
    #[test]
    fn parses_quoted_paths() {
        let out = "diff --git \"a/has space.txt\" \"b/has space.txt\"\n";
        let files = parse_unified(out);
        assert_eq!(files[0].path, "has space.txt");
        assert_eq!(files[0].old_path, None);
    }

    /// A `--- ` *content* line inside a hunk is a removed line, not metadata.
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
        // Raw `---- a heading rule` = marker `-` + content `--- a heading rule`:
        // content that *itself* starts with `--- ` must not be eaten as metadata.
        assert_eq!(lines[1].text, "--- a heading rule");
    }

    /// Past the per-file cap the hunks stop growing and the file is flagged,
    /// but the +/− counts keep counting so the header stays honest.
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

    /// `\ No newline at end of file` markers are skipped, not rendered.
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

    /// Totals sum per-file counts.
    #[test]
    fn snapshot_totals() {
        let snap = DiffSnapshot {
            files: parse_unified(SAMPLE),
            ..Default::default()
        };
        assert_eq!(snap.totals(), (4, 2));
    }

    /// Build `files` synthetic modified files of `lines_each` added lines —
    /// the "many medium files" shape the per-file cap alone can't bound.
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

    /// The repo-wide budget bounds retained lines even when no single file is
    /// anywhere near [`MAX_LINES_PER_FILE`] — the case the reporter of #239
    /// called out as the one the per-file cap misses.
    #[test]
    fn repo_wide_budget_caps_retained_lines() {
        // 300 files × 300 lines = 90k lines, none of which trips the 2000-line
        // per-file cap.
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

    /// Budget or no budget, the +/− totals must stay exact: they are compared
    /// against `git diff --numstat` to decide whether the overlay is stale, and
    /// a short count would make every comparison disagree and re-probe forever.
    #[test]
    fn repo_wide_budget_keeps_totals_exact() {
        let snap = DiffSnapshot {
            files: parse_unified(&many_files(300, 300)),
            ..Default::default()
        };
        assert_eq!(snap.totals(), (90_000, 0));
        assert!(snap.stats().budget_exhausted);
    }

    /// The file cap keeps a rename-the-world diff from allocating a `Vec<Hunk>`
    /// per file, while every file still lists its path and counts.
    #[test]
    fn repo_wide_budget_caps_files_with_hunks() {
        // One line each: far under the line budget, so only the file cap can
        // stop this.
        let files = parse_unified(&many_files(MAX_FILES_WITH_HUNKS + 50, 1));
        assert_eq!(files.len(), MAX_FILES_WITH_HUNKS + 50);
        let with_hunks = files.iter().filter(|f| !f.hunks.is_empty()).count();
        assert_eq!(with_hunks, MAX_FILES_WITH_HUNKS);
        // The tail still counts, so totals stay honest.
        assert_eq!(
            files.iter().map(|f| f.added).sum::<u32>(),
            (MAX_FILES_WITH_HUNKS + 50) as u32
        );
        assert_eq!(files.last().unwrap().truncated, Some(Truncation::Budget));
    }

    /// A small diff is untouched by the budget — the setting-enabled,
    /// small-working-tree case must behave exactly as before.
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

    /// `oversized` trips on either axis: many files, or many retained lines.
    #[test]
    fn oversized_trips_on_files_or_lines() {
        let by_files = DiffSnapshot {
            files: parse_unified(&many_files(AUTO_COLLAPSE_TOTAL_FILES + 1, 1)),
            ..Default::default()
        };
        assert!(by_files.stats().oversized);

        // Few files, but past the line threshold. Spread over enough files to
        // clear it without any one of them hitting `MAX_LINES_PER_FILE` first —
        // the per-file cap would otherwise decide this test's outcome instead of
        // the repo-wide threshold it is about.
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

    /// Even a budget-truncated file keeps a removed line whose *content* starts
    /// with `--- ` out of the metadata skip — that line still has to count.
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

    /// The untracked cap keeps the retained list bounded while the reported
    /// count stays exact — the same split the diff side already makes between
    /// what is retained and what is counted.
    #[test]
    fn untracked_is_capped_but_counted() {
        // What `probe` builds while streaming `ls-files --others`.
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

    /// Measurement harness for issue #239 finding 1 — run with
    /// `cargo test --release -- --ignored --nocapture bench_stream_vs_buffer`.
    ///
    /// Measures the shipped code paths against real git output in this
    /// repository: `git_output` (what `Host::git` uses) versus `git_stream` +
    /// `LineSplitter` (what `Host::git_lines` uses on a local host), both fed
    /// to the same [`DiffParser`].
    #[test]
    #[ignore = "measurement, not an assertion"]
    fn bench_stream_vs_buffer() {
        use std::time::Instant;
        use tty7_core::core::git::{LineSplitter, git_output, git_stream};

        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // Real, sizeable git output: patches for the last few hundred commits.
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

    /// Measurement harness for issue #239, not a correctness gate — run with
    /// `cargo test -- --ignored --nocapture bench_parse_budget`.
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

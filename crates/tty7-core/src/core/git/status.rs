//! What the working tree looks like right now — the model behind the source
//! control panel, the file tree's decorations, and every button that is only
//! enabled for some file states.
//!
//! One `git status --porcelain=v2 --branch --show-stash -uall -z` answers all
//! of it. That format
//! is the only one that carries the staged and unstaged halves *separately*
//! (the `XY` pair), a rename's old path, unmerged stages, submodule sub-state,
//! and the branch header — getting the same picture out of `git diff` takes
//! four commands and a consistency window between them.
//!
//! The types live here rather than next to the parser because `ops` builds
//! commands out of them (`HeadState` decides how to unstage) and the GUI reads
//! them; the parser is just one producer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::RecordSplitter;
use crate::host::{Entry, Host};

/// A path relative to the repository root, always `/`-separated.
///
/// `-z` hands out raw bytes, and on Linux a path need not be UTF-8. When it is
/// not, `text` is the lossy rendering and `lossy` is set: such an entry can be
/// *shown* but never *acted on*, because `Host::git` takes `&[&str]` and the
/// control protocol carries args as `String` — the original bytes cannot reach
/// the far side. Callers must treat `pathspec() == None` as "read only".
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct RepoPath {
    pub text: String,
    pub lossy: bool,
}

impl RepoPath {
    pub fn from_bytes(bytes: &[u8]) -> RepoPath {
        match std::str::from_utf8(bytes) {
            Ok(text) => RepoPath {
                text: text.to_string(),
                lossy: false,
            },
            Err(_) => RepoPath {
                text: String::from_utf8_lossy(bytes).into_owned(),
                lossy: true,
            },
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The pathspec to hand git, or `None` if this path cannot be represented.
    ///
    /// `:(literal)` is not decoration: without it git globs the pathspec, so a
    /// file actually named `a[b].txt` or `foo*` would not match itself. Callers
    /// must additionally put a `--` ahead of the list so a file named `HEAD` or
    /// `-f` is not read as a rev or an option.
    pub fn pathspec(&self) -> Option<String> {
        (!self.lossy).then(|| format!(":(literal){}", self.text))
    }

    pub fn file_name(&self) -> &str {
        match self.text.rsplit_once('/') {
            Some((_, name)) => name,
            None => &self.text,
        }
    }

    pub fn parent(&self) -> &str {
        match self.text.rsplit_once('/') {
            Some((dir, _)) => dir,
            None => "",
        }
    }
}

/// One half of porcelain v2's `XY` pair.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ChangeCode {
    None,
    Modified,
    TypeChanged,
    Added,
    Deleted,
    Renamed,
    Copied,
    Unmerged,
}

impl ChangeCode {
    pub(crate) fn from_byte(b: u8) -> Option<ChangeCode> {
        Some(match b {
            b'.' | b' ' => ChangeCode::None,
            b'M' => ChangeCode::Modified,
            b'T' => ChangeCode::TypeChanged,
            b'A' => ChangeCode::Added,
            b'D' => ChangeCode::Deleted,
            b'R' => ChangeCode::Renamed,
            b'C' => ChangeCode::Copied,
            b'U' => ChangeCode::Unmerged,
            _ => return None,
        })
    }

    /// The single character the UI shows in its 14px status column.
    pub fn letter(self) -> char {
        match self {
            ChangeCode::None => ' ',
            ChangeCode::Modified => 'M',
            ChangeCode::TypeChanged => 'T',
            ChangeCode::Added => 'A',
            ChangeCode::Deleted => 'D',
            ChangeCode::Renamed => 'R',
            ChangeCode::Copied => 'C',
            ChangeCode::Unmerged => 'U',
        }
    }

    pub(crate) fn is_change(self) -> bool {
        self != ChangeCode::None
    }
}

/// The seven `XY` pairs porcelain v2 reports as unmerged (`u`) records.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConflictKind {
    BothDeleted,
    AddedByUs,
    DeletedByThem,
    AddedByThem,
    DeletedByUs,
    BothAdded,
    BothModified,
}

impl ConflictKind {
    pub(crate) fn from_xy(x: u8, y: u8) -> Option<ConflictKind> {
        Some(match (x, y) {
            (b'D', b'D') => ConflictKind::BothDeleted,
            (b'A', b'U') => ConflictKind::AddedByUs,
            (b'U', b'D') => ConflictKind::DeletedByThem,
            (b'U', b'A') => ConflictKind::AddedByThem,
            (b'D', b'U') => ConflictKind::DeletedByUs,
            (b'A', b'A') => ConflictKind::BothAdded,
            (b'U', b'U') => ConflictKind::BothModified,
            _ => return None,
        })
    }

    /// Whether our side still has a file — decides if "open changes" can show
    /// an ours/theirs diff or only one stage.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn ours_exists(self) -> bool {
        !matches!(self, ConflictKind::BothDeleted | ConflictKind::DeletedByUs)
    }

    /// The mirror of `ours_exists`, which the tests do use. Half a pair is worse
    /// /// than a spare, so it stays.
    #[allow(dead_code)]
    pub(crate) fn theirs_exists(self) -> bool {
        !matches!(
            self,
            ConflictKind::BothDeleted | ConflictKind::DeletedByThem
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SubmoduleState {
    pub commit_changed: bool,
    pub modified_content: bool,
    pub has_untracked: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    Tracked,
    Unmerged,
    Untracked,
    Ignored,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StatusEntry {
    pub path: RepoPath,
    /// Only set on rename/copy records, and it is the *old* path.
    pub orig_path: Option<RepoPath>,
    /// `X` — HEAD against the index, i.e. what is staged.
    pub index: ChangeCode,
    /// `Y` — the index against the working tree, i.e. what is not staged.
    pub worktree: ChangeCode,
    pub kind: EntryKind,
    /// `None` when the entry is not a submodule.
    pub submodule: Option<SubmoduleState>,
    /// Similarity score from `R<score>` / `C<score>`, 0..=100.
    pub rename_score: Option<u8>,
    /// Always `Some` when `kind == Unmerged`.
    pub conflict: Option<ConflictKind>,
}

impl StatusEntry {
    pub fn is_staged(&self) -> bool {
        self.index.is_change() && self.conflict.is_none()
    }

    /// A file can be both staged and unstaged at once (`XY == "MM"`) and then
    /// appears in both groups — which is exactly what VS Code shows.
    pub fn is_unstaged(&self) -> bool {
        self.worktree.is_change() && self.conflict.is_none()
    }

    pub fn is_untracked(&self) -> bool {
        matches!(self.kind, EntryKind::Untracked)
    }

    pub fn is_conflicted(&self) -> bool {
        self.conflict.is_some()
    }

    /// How this entry should be decorated wherever a single status is shown
    /// (file tree, commit detail): the worse of its two halves.
    pub fn deco(&self) -> DecoStatus {
        if self.is_conflicted() {
            return DecoStatus::Conflict;
        }
        if self.is_untracked() {
            return DecoStatus::Untracked;
        }
        // Explicit, not via the code match below: an ignored record carries no
        // change codes, so it would otherwise fall through to `Modified` the
        // day `--ignored` is passed — and light the whole ignored tree up.
        if matches!(self.kind, EntryKind::Ignored) {
            return DecoStatus::Ignored;
        }
        let worse = if code_rank(self.worktree) >= code_rank(self.index) {
            self.worktree
        } else {
            self.index
        };
        match worse {
            ChangeCode::Deleted => DecoStatus::Deleted,
            ChangeCode::Added => DecoStatus::Added,
            ChangeCode::Renamed | ChangeCode::Copied => DecoStatus::Renamed,
            ChangeCode::Unmerged => DecoStatus::Conflict,
            _ => DecoStatus::Modified,
        }
    }
}

fn code_rank(code: ChangeCode) -> u8 {
    match code {
        ChangeCode::None => 0,
        ChangeCode::Modified | ChangeCode::TypeChanged => 1,
        ChangeCode::Copied => 2,
        ChangeCode::Renamed => 3,
        ChangeCode::Added => 4,
        ChangeCode::Deleted => 5,
        ChangeCode::Unmerged => 6,
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HeadState {
    /// `# branch.oid (initial)` — the repository has no commits yet.
    Unborn {
        branch: String,
    },
    Detached {
        oid: String,
    },
    Branch {
        name: String,
        oid: String,
    },
}

impl HeadState {
    /// What the chrome shows: a branch name, or a short sha when detached.
    pub fn label(&self) -> String {
        match self {
            HeadState::Unborn { branch } => branch.clone(),
            HeadState::Detached { oid } => oid.chars().take(7).collect(),
            HeadState::Branch { name, .. } => name.clone(),
        }
    }

    /// `false` before the first commit, which is the one case where
    /// `git reset HEAD -- <path>` fails outright.
    pub(crate) fn has_commits(&self) -> bool {
        !matches!(self, HeadState::Unborn { .. })
    }
}

/// A sequencer operation left half-finished in the repository.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RepoOperation {
    Merge,
    Rebase,
    RebaseInteractive,
    CherryPick,
    Revert,
    Bisect,
    Am,
}

/// Entries past this are dropped; `total_entries` still reports the real count
/// so the panel can say so instead of quietly showing a short list.
pub(crate) const MAX_STATUS_ENTRIES: usize = 10_000;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WorkingTreeStatus {
    /// This worktree's toplevel.
    pub root: PathBuf,
    /// The shared repository home — differs from `root` inside a linked
    /// worktree. Same rule `core::git::probe` already uses to group tabs.
    pub home: PathBuf,
    pub head: HeadState,
    pub upstream: Option<String>,
    pub ahead_behind: Option<(u32, u32)>,
    pub entries: Vec<StatusEntry>,
    pub total_entries: usize,
    pub truncated: bool,
    pub stash_count: u32,
    pub operation: Option<RepoOperation>,
    /// `.git/MERGE_MSG` or `SQUASH_MSG`, to pre-fill the commit box mid-merge.
    pub prefilled_message: Option<String>,
}

impl WorkingTreeStatus {
    pub fn staged(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.is_staged())
    }

    pub fn unstaged(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries
            .iter()
            .filter(|e| e.is_unstaged() && !e.is_untracked())
    }

    pub fn untracked(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.is_untracked())
    }

    pub fn conflicts(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.is_conflicted())
    }

    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }
}

/// How a path is decorated wherever one status has to stand for a file.
///
/// `Ord` is display precedence, lowest to highest: a directory takes the max of
/// everything beneath it, so one conflict anywhere colours the whole path up to
/// the root.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum DecoStatus {
    Ignored,
    Untracked,
    Added,
    Modified,
    Renamed,
    Deleted,
    Conflict,
}

/// Beyond this many entries the per-file map is dropped and only directories
/// stay decorated — a `node_modules` that slipped past `.gitignore` should slow
/// nothing down.
pub const MAX_DECORATED_FILES: usize = 5_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DirRollup {
    pub changed: bool,
    pub conflict: bool,
}

impl DirRollup {
    pub fn merge(&mut self, status: DecoStatus) {
        match status {
            DecoStatus::Ignored => {}
            DecoStatus::Conflict => {
                self.conflict = true;
                self.changed = true;
            }
            _ => self.changed = true,
        }
    }
}

/// Repo-root-relative lookup for the file tree, built once per status refresh.
///
/// Cost is O(changed paths × depth) — independent of how big the tree is —
/// and both lookups are a single hash probe, so a row can ask during render.
#[derive(Clone, Debug, Default)]
pub struct StatusIndex {
    pub root: PathBuf,
    files: HashMap<String, DecoStatus>,
    dirs: HashMap<String, DirRollup>,
    /// Set when `files` was dropped for exceeding [`MAX_DECORATED_FILES`].
    pub files_dropped: bool,
}

impl StatusIndex {
    /// Fold a whole status into the lookup the file tree renders against.
    ///
    /// Built once per refresh on a background thread; the render path only ever
    /// probes it. Doing it the other way round — asking the status for a path
    /// while drawing a row — is a linear scan per visible row.
    pub fn build(status: &WorkingTreeStatus) -> StatusIndex {
        let mut index = StatusIndex {
            root: status.root.clone(),
            ..StatusIndex::default()
        };
        for entry in &status.entries {
            let deco = entry.deco();
            index.insert(entry.path.as_str(), deco);
            // A rename's old path goes into the directory rollup only — the
            // directory it left did lose a file. Not into `files`: the path
            // can be occupied again (`git mv a b && echo x > a` emits an
            // untracked record for `a`), and that row belongs to whatever
            // occupies it now, not to the rename it outranks.
            if let Some(orig) = &entry.orig_path {
                index.rollup(orig.as_str(), deco);
            }
        }
        if index.files.len() > MAX_DECORATED_FILES {
            index.drop_files();
        }
        index
    }

    pub fn file(&self, repo_rel: &str) -> Option<DecoStatus> {
        self.files.get(repo_rel).copied()
    }

    pub fn dir(&self, repo_rel: &str) -> Option<DirRollup> {
        self.dirs.get(repo_rel).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.dirs.is_empty()
    }

    /// Insert one path and roll it up through every ancestor. Exposed so the
    /// builder and its tests share exactly one definition of the walk.
    pub fn insert(&mut self, repo_rel: &str, status: DecoStatus) {
        self.files
            .entry(repo_rel.to_string())
            .and_modify(|slot| *slot = (*slot).max(status))
            .or_insert(status);
        self.rollup(repo_rel, status);
    }

    /// Only the ancestor walk — for a path that must not claim a file row of
    /// its own, like the old half of a rename.
    fn rollup(&mut self, repo_rel: &str, status: DecoStatus) {
        let mut cut = repo_rel;
        while let Some((parent, _)) = cut.rsplit_once('/') {
            self.dirs
                .entry(parent.to_string())
                .or_default()
                .merge(status);
            cut = parent;
        }
    }

    pub fn drop_files(&mut self) {
        self.files.clear();
        self.files_dropped = true;
    }
}

/// The one command behind everything above.
///
/// `-z` is not a preference: without it any path with a space, a quote or a
/// newline comes back C-quoted. It also makes `core.quotePath=false` a no-op
/// here — measured, identical output either way — so that flag is carried only
/// to keep every git invocation in this module shaped the same; the place it
/// actually fixes something is the `diff --git` header. `--untracked-files=all`
/// pays for an extra walk, but a collapsed `dir/` row cannot be staged file by
/// file, which is most of what the panel is for.
const STATUS_ARGS: &[&str] = &[
    "-c",
    "core.quotePath=false",
    "status",
    "--porcelain=v2",
    "--branch",
    "--show-stash",
    "--untracked-files=all",
    "-z",
];

/// `MERGE_MSG` is a commit message, not a file; anything past this is a
/// runaway and the commit box is better off empty than full of it.
const MAX_PREFILLED_MESSAGE: u64 = 64 * 1024;

/// Everything one `--porcelain=v2` run can tell us on its own.
///
/// Split out from [`WorkingTreeStatus`] because the remaining fields — where
/// the repository lives, and what sequencer operation is parked in it — come
/// from the filesystem, not from the parse. Keeping them apart is what lets the
/// header and record parsing be tested without a repository.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct ParsedStatus {
    pub head: HeadState,
    pub upstream: Option<String>,
    /// `(ahead, behind)`. `None` means *unknown*, never *in sync*.
    pub ahead_behind: Option<(u32, u32)>,
    pub entries: Vec<StatusEntry>,
    pub total_entries: usize,
    pub truncated: bool,
    pub stash_count: u32,
}

impl ParsedStatus {
    /// Finish the picture with what only a filesystem probe knows.
    pub(crate) fn into_status(
        self,
        root: PathBuf,
        home: PathBuf,
        operation: Option<RepoOperation>,
        prefilled_message: Option<String>,
    ) -> WorkingTreeStatus {
        WorkingTreeStatus {
            root,
            home,
            head: self.head,
            upstream: self.upstream,
            ahead_behind: self.ahead_behind,
            entries: self.entries,
            total_entries: self.total_entries,
            truncated: self.truncated,
            stash_count: self.stash_count,
            operation,
            prefilled_message,
        }
    }
}

/// Parse the stdout of [`STATUS_ARGS`].
///
/// Never fails: git's output is only ever malformed if we asked for the wrong
/// format, and a status that is missing one unreadable row is far better for
/// the panel than no status at all. Records that do not parse are dropped.
pub(crate) fn parse_porcelain_v2(stdout: &[u8]) -> ParsedStatus {
    let mut parser = Parser::default();
    let mut split = RecordSplitter::new(0);
    split.push(stdout, |record| parser.record(record));
    let dropped = split.finish(|record| parser.record(record));
    let mut parsed = parser.finish();
    // A record past `MAX_RECORD` (a pathological pathname) is dropped whole;
    // the status must say it is not the full picture, same as the entry cap.
    parsed.truncated |= dropped > 0;
    parsed
}

#[derive(Default)]
struct Parser {
    oid: Option<String>,
    head_name: Option<String>,
    upstream: Option<String>,
    ahead_behind: Option<(u32, u32)>,
    stash_count: u32,
    entries: Vec<StatusEntry>,
    total: usize,
    /// A `2` record's *old* path is a separate NUL token, so the record after
    /// one is data rather than a new record. Parking the half-built entry here
    /// is the whole reason a rename does not swallow the row behind it.
    pending_rename: Option<StatusEntry>,
}

impl Parser {
    fn record(&mut self, record: &[u8]) {
        if let Some(mut entry) = self.pending_rename.take() {
            entry.orig_path = Some(RepoPath::from_bytes(record));
            self.push(entry);
            return;
        }
        match record.first() {
            Some(b'#') => self.header(record),
            Some(b'1') => self.ordinary(record),
            Some(b'2') => self.renamed(record),
            Some(b'u') => self.unmerged(record),
            Some(b'?') => self.bare(record, EntryKind::Untracked),
            // We never pass `--ignored`, so this never arrives today. Handling
            // it anyway costs one arm and means adding the flag later is a
            // one-line change rather than a silent hole in the parse.
            Some(b'!') => self.bare(record, EntryKind::Ignored),
            _ => {}
        }
    }

    fn header(&mut self, record: &[u8]) {
        let Ok(text) = std::str::from_utf8(record) else {
            return;
        };
        let Some(rest) = text.strip_prefix("# ") else {
            return;
        };
        let (key, value) = rest.split_once(' ').unwrap_or((rest, ""));
        match key {
            "branch.oid" => self.oid = Some(value.to_string()),
            "branch.head" => self.head_name = Some(value.to_string()),
            "branch.upstream" => self.upstream = Some(value.to_string()),
            "branch.ab" => self.ahead_behind = parse_ab(value),
            "stash" => self.stash_count = value.parse().unwrap_or(0),
            _ => {}
        }
    }

    /// `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`
    fn ordinary(&mut self, record: &[u8]) {
        let mut fields = Fields::new(record);
        let Some((x, y, submodule)) = fields.prefix() else {
            return;
        };
        let (Some(index), Some(worktree)) = (ChangeCode::from_byte(x), ChangeCode::from_byte(y))
        else {
            return;
        };
        if fields.skip(5).is_none() {
            return;
        }
        self.push(StatusEntry {
            path: RepoPath::from_bytes(fields.tail()),
            orig_path: None,
            index,
            worktree,
            kind: EntryKind::Tracked,
            submodule,
            rename_score: None,
            conflict: None,
        });
    }

    /// `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\0<origPath>\0`
    fn renamed(&mut self, record: &[u8]) {
        let mut fields = Fields::new(record);
        let Some((x, y, submodule)) = fields.prefix() else {
            return;
        };
        let (Some(index), Some(worktree)) = (ChangeCode::from_byte(x), ChangeCode::from_byte(y))
        else {
            return;
        };
        if fields.skip(5).is_none() {
            return;
        }
        let Some(score) = fields.next() else {
            return;
        };
        self.pending_rename = Some(StatusEntry {
            path: RepoPath::from_bytes(fields.tail()),
            orig_path: None,
            index,
            worktree,
            kind: EntryKind::Tracked,
            submodule,
            rename_score: parse_score(score),
            conflict: None,
        });
    }

    /// `u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>`
    fn unmerged(&mut self, record: &[u8]) {
        let mut fields = Fields::new(record);
        let Some((x, y, submodule)) = fields.prefix() else {
            return;
        };
        if fields.skip(7).is_none() {
            return;
        }
        self.push(StatusEntry {
            path: RepoPath::from_bytes(fields.tail()),
            orig_path: None,
            index: ChangeCode::from_byte(x).unwrap_or(ChangeCode::Unmerged),
            worktree: ChangeCode::from_byte(y).unwrap_or(ChangeCode::Unmerged),
            kind: EntryKind::Unmerged,
            submodule,
            rename_score: None,
            // `XY` on a `u` record is the stage pair, not a change pair, so a
            // conflict git does not name is still a conflict.
            conflict: Some(ConflictKind::from_xy(x, y).unwrap_or(ConflictKind::BothModified)),
        });
    }

    /// `? <path>` and `! <path>` — no fields, just the path.
    fn bare(&mut self, record: &[u8], kind: EntryKind) {
        let mut fields = Fields::new(record);
        if fields.next().is_none() {
            return;
        }
        self.push(StatusEntry {
            path: RepoPath::from_bytes(fields.tail()),
            orig_path: None,
            index: ChangeCode::None,
            worktree: ChangeCode::None,
            kind,
            submodule: None,
            rename_score: None,
            conflict: None,
        });
    }

    /// Past the cap we keep counting but stop keeping, so the panel can say
    /// "10000 of 42311" rather than quietly showing a short list.
    fn push(&mut self, entry: StatusEntry) {
        self.total += 1;
        if self.entries.len() < MAX_STATUS_ENTRIES {
            self.entries.push(entry);
        }
    }

    fn finish(mut self) -> ParsedStatus {
        // A `2` with nothing behind it means the output was cut short. The file
        // is still real, so keep the row and lose only the old path.
        if let Some(entry) = self.pending_rename.take() {
            self.push(entry);
        }
        ParsedStatus {
            head: head_state(self.oid, self.head_name),
            upstream: self.upstream,
            ahead_behind: self.ahead_behind,
            truncated: self.total > self.entries.len(),
            total_entries: self.total,
            entries: self.entries,
            stash_count: self.stash_count,
        }
    }
}

/// Walks the fixed head of a record one space-delimited field at a time, then
/// hands back the rest verbatim — the path is always last and may contain
/// spaces, so it must never be split.
struct Fields<'a> {
    rest: &'a [u8],
}

impl<'a> Fields<'a> {
    fn new(record: &'a [u8]) -> Fields<'a> {
        Fields { rest: record }
    }

    fn next(&mut self) -> Option<&'a [u8]> {
        let at = self.rest.iter().position(|b| *b == b' ')?;
        let (field, after) = self.rest.split_at(at);
        self.rest = &after[1..];
        Some(field)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        for _ in 0..n {
            self.next()?;
        }
        Some(())
    }

    /// The `<marker> <XY> <sub>` prefix that `1`, `2` and `u` records share.
    fn prefix(&mut self) -> Option<(u8, u8, Option<SubmoduleState>)> {
        self.next()?;
        let &[x, y] = self.next()? else {
            return None;
        };
        Some((x, y, parse_submodule(self.next()?)))
    }

    fn tail(&self) -> &'a [u8] {
        self.rest
    }
}

/// `N...` when the entry is not a submodule, otherwise `S<c><m><u>`.
fn parse_submodule(field: &[u8]) -> Option<SubmoduleState> {
    let &[b'S', c, m, u] = field else {
        return None;
    };
    Some(SubmoduleState {
        commit_changed: c == b'C',
        modified_content: m == b'M',
        has_untracked: u == b'U',
    })
}

/// `R100` / `C75`. The letter repeats what `XY` already said, so only the
/// number is kept.
fn parse_score(field: &[u8]) -> Option<u8> {
    let digits = std::str::from_utf8(field.get(1..)?).ok()?;
    digits.parse::<u16>().ok().map(|n| n.min(100) as u8)
}

/// `+2 -1`, from `# branch.ab`.
///
/// `git status --no-ahead-behind` prints `+? -?` rather than dropping the line
/// (measured on git 2.50), so an unparsable pair has to mean *unknown*. Reading
/// it as zero would render as "in sync", which is the one wrong answer worse
/// than no answer at all.
fn parse_ab(value: &str) -> Option<(u32, u32)> {
    let (ahead, behind) = value.split_once(' ')?;
    Some((
        ahead.strip_prefix('+')?.parse().ok()?,
        behind.strip_prefix('-')?.parse().ok()?,
    ))
}

fn head_state(oid: Option<String>, head_name: Option<String>) -> HeadState {
    let branch = head_name.unwrap_or_default();
    match oid {
        Some(oid) if oid == "(initial)" => HeadState::Unborn { branch },
        Some(oid) if branch == "(detached)" => HeadState::Detached { oid },
        Some(oid) => HeadState::Branch { name: branch, oid },
        None => HeadState::Unborn { branch },
    }
}

/// What a status probe learned. The middle answer is the load-bearing one:
/// `Unreachable` is not an answer *about the repository* — the question could
/// not be asked — and treating it as "no repository here" made a dropped link
/// erase a panel that was showing perfectly good (if stale) data.
#[derive(Clone, PartialEq, Debug)]
pub enum StatusProbe {
    Status(Box<WorkingTreeStatus>),
    /// git ran and said so — an ordinary directory.
    NotARepo,
    /// git could not run, or ran and failed for a reason that is not "no
    /// repository": a dead link, a timeout, an `index.lock` held by someone
    /// else. Keep what is cached and ask again later.
    Unreachable,
}

/// The whole working tree state for the repository containing `cwd`.
///
/// Three round trips in the common case — `rev-parse`, `status`, `read_dir` —
/// and each one is an RPC on a remote workspace, which is why none of them is
/// split into the several calls that would read more naturally.
pub fn probe_status(host: &dyn Host, cwd: &Path) -> StatusProbe {
    let Ok(out) = host.git(
        cwd,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ],
    ) else {
        return StatusProbe::Unreachable;
    };
    if !out.success() {
        // Exit 128 with this phrase is the ordinary answer for an ordinary
        // directory; the phrase has been stable (modulo case) since git 1.x.
        // Anything else is a repository that could not be read.
        let stderr = String::from_utf8_lossy(&out.stderr).to_ascii_lowercase();
        return if stderr.contains("not a git repository") {
            StatusProbe::NotARepo
        } else {
            StatusProbe::Unreachable
        };
    }
    let paths = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines = paths.lines().map(|l| l.trim_end_matches(['\n', '\r']));
    let Some(root) = lines.next().map(PathBuf::from) else {
        return StatusProbe::Unreachable;
    };
    let git_dir = lines.next();
    let home = super::repo_home(&root, git_dir, lines.next());
    let Some(git_dir) = git_dir.map(PathBuf::from) else {
        return StatusProbe::Unreachable;
    };

    let Ok(out) = host.git(cwd, STATUS_ARGS) else {
        return StatusProbe::Unreachable;
    };
    if !out.success() {
        return StatusProbe::Unreachable;
    }
    let mut parsed = parse_porcelain_v2(&out.stdout);
    if parsed.ahead_behind.is_none()
        && let Some(upstream) = parsed.upstream.clone()
    {
        parsed.ahead_behind = rev_list_ahead_behind(host, cwd, &upstream);
    }

    let listing = host.read_dir(&git_dir, None).unwrap_or_default();
    let operation = detect_operation(host, &git_dir, &listing);
    let prefilled_message =
        operation.and_then(|_| read_prefilled_message(host, &git_dir, &listing));
    StatusProbe::Status(Box::new(parsed.into_status(
        root,
        home,
        operation,
        prefilled_message,
    )))
}

/// Ask for ahead/behind again when the header could not say.
///
/// Only reached when `# branch.ab` is missing or unreadable, which measurement
/// says is rare: on git 2.50, `status.aheadBehind=false` does *not* suppress the
/// line for porcelain v2 (that config is documented as applying to non-porcelain
/// formats only), and `--no-ahead-behind` prints `+? -?` rather than dropping
/// it. This is a fallback for old git and for the configurations we did not
/// measure — not dead code, but not the normal path either. Do not promote it to
/// unconditional: on a remote workspace every call here is another RPC on a
/// refresh that already made three.
fn rev_list_ahead_behind(host: &dyn Host, cwd: &Path, upstream: &str) -> Option<(u32, u32)> {
    let range = format!("{upstream}...HEAD");
    let out = super::git(host, cwd, &["rev-list", "--left-right", "--count", &range])?;
    // Left of the `...` is the upstream, so the left count is what we are
    // behind by — the opposite order from `# branch.ab`.
    let (behind, ahead) = out.trim().split_once('\t')?;
    Some((ahead.trim().parse().ok()?, behind.trim().parse().ok()?))
}

/// Which sequencer operation, if any, is parked in this repository.
///
/// The order mirrors git's own in `wt_status_print_state`, and it matters more
/// than any individual test does: a conflicted `am` leaves `rebase-apply`
/// behind, a rebase stopped on a pick leaves `MERGE_MSG` behind, and a merge
/// leaves `AUTO_MERGE` behind. Only the precedence tells them apart.
fn detect_operation(host: &dyn Host, git_dir: &Path, listing: &[Entry]) -> Option<RepoOperation> {
    let has = |name: &str| listing.iter().any(|e| e.name == name);
    if has("MERGE_HEAD") {
        return Some(RepoOperation::Merge);
    }
    if has("rebase-apply") {
        // `applying` is the only thing separating `git am` from a rebase on the
        // apply backend, and it sits one level down. The extra round trip is
        // paid only while a rebase is actually parked.
        return Some(
            match dir_has(host, &git_dir.join("rebase-apply"), "applying") {
                true => RepoOperation::Am,
                false => RepoOperation::Rebase,
            },
        );
    }
    if has("rebase-merge") {
        // Modern git writes `interactive` for *every* rebase on the merge
        // backend, not only `-i` — measured on 2.50, where plain `git rebase`
        // also makes `git status` say "interactive rebase in progress". Reading
        // the same marker git reads keeps the two labels from disagreeing.
        return Some(
            match dir_has(host, &git_dir.join("rebase-merge"), "interactive") {
                true => RepoOperation::RebaseInteractive,
                false => RepoOperation::Rebase,
            },
        );
    }
    if has("CHERRY_PICK_HEAD") {
        return Some(RepoOperation::CherryPick);
    }
    if has("REVERT_HEAD") {
        return Some(RepoOperation::Revert);
    }
    if has("BISECT_LOG") {
        return Some(RepoOperation::Bisect);
    }
    None
}

fn dir_has(host: &dyn Host, dir: &Path, name: &str) -> bool {
    host.read_dir(dir, None)
        .is_ok_and(|listing| listing.iter().any(|e| e.name == name))
}

/// The message git already wrote for the operation in progress.
///
/// Only read when something *is* in progress: outside a merge these files are
/// leftovers from the last commit, and pre-filling the box with a stale message
/// is how you accidentally commit it again.
fn read_prefilled_message(host: &dyn Host, git_dir: &Path, listing: &[Entry]) -> Option<String> {
    // `SQUASH_MSG` first: when both exist it is the more specific one.
    for name in ["SQUASH_MSG", "MERGE_MSG"] {
        if !listing.iter().any(|e| e.name == name) {
            continue;
        }
        let Ok(bytes) = host.read_file(&git_dir.join(name), MAX_PREFILLED_MESSAGE) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes).trim_end().to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::git::test_support::{PINS, one_spelling, pin_repo_config};

    /// One NUL-terminated record. Samples are written this way because a string
    /// literal with embedded NULs is unreadable, and because the separator is
    /// exactly what half of these tests are about.
    fn rec(parts: &[&str]) -> Vec<u8> {
        let mut out = parts.concat().into_bytes();
        out.push(0);
        out
    }

    fn rec_bytes(prefix: &str, path: &[u8]) -> Vec<u8> {
        let mut out = prefix.as_bytes().to_vec();
        out.extend_from_slice(path);
        out.push(0);
        out
    }

    /// The five `# branch.*` lines with a real sha, so record tests do not have
    /// to restate the header every time.
    fn head_records() -> Vec<u8> {
        [
            rec(&["# branch.oid 8d1b4a0e3c2f5b6a7d8e9f0a1b2c3d4e5f607182"]),
            rec(&["# branch.head main"]),
        ]
        .concat()
    }

    const SHA: &str = "8d1b4a0e3c2f5b6a7d8e9f0a1b2c3d4e5f607182";

    /// A `1` record's fixed fields; only `XY` and the path ever vary here.
    fn ordinary(xy: &str, path: &str) -> Vec<u8> {
        rec(&[
            "1 ",
            xy,
            " N... 100644 100644 100644 ",
            SHA,
            " ",
            SHA,
            " ",
            path,
        ])
    }

    fn by_path<'a>(parsed: &'a ParsedStatus, path: &str) -> &'a StatusEntry {
        parsed
            .entries
            .iter()
            .find(|e| e.path.text == path)
            .unwrap_or_else(|| panic!("no entry for {path}: {:?}", parsed.entries))
    }

    fn status_of(stdout: &[u8]) -> WorkingTreeStatus {
        parse_porcelain_v2(stdout).into_status(
            PathBuf::from("/repo"),
            PathBuf::from("/repo"),
            None,
            None,
        )
    }

    #[test]
    fn a_full_branch_header_lands_in_every_field() {
        let parsed = parse_porcelain_v2(
            &[
                rec(&["# branch.oid ", SHA]),
                rec(&["# branch.head feature/scm"]),
                rec(&["# branch.upstream origin/feature/scm"]),
                rec(&["# branch.ab +12 -3"]),
                rec(&["# stash 4"]),
            ]
            .concat(),
        );

        assert_eq!(
            parsed.head,
            HeadState::Branch {
                name: "feature/scm".into(),
                oid: SHA.into(),
            }
        );
        assert_eq!(parsed.upstream.as_deref(), Some("origin/feature/scm"));
        assert_eq!(parsed.ahead_behind, Some((12, 3)));
        assert_eq!(parsed.stash_count, 4);
        assert!(parsed.entries.is_empty());
        assert!(!parsed.truncated);
    }

    #[test]
    fn an_unborn_head_reports_its_branch_and_no_commits() {
        let parsed = parse_porcelain_v2(
            &[
                rec(&["# branch.oid (initial)"]),
                rec(&["# branch.head main"]),
                rec(&["? first.txt"]),
            ]
            .concat(),
        );

        assert_eq!(
            parsed.head,
            HeadState::Unborn {
                branch: "main".into()
            }
        );
        assert!(!parsed.head.has_commits(), "reset HEAD would fatal here");
        assert_eq!(parsed.head.label(), "main");
        assert_eq!(parsed.entries.len(), 1, "the untracked file still parses");
    }

    #[test]
    fn a_detached_head_keeps_the_sha_and_shows_it_short() {
        let parsed = parse_porcelain_v2(
            &[
                rec(&["# branch.oid ", SHA]),
                rec(&["# branch.head (detached)"]),
            ]
            .concat(),
        );

        assert_eq!(parsed.head, HeadState::Detached { oid: SHA.into() });
        assert!(parsed.head.has_commits());
        assert_eq!(parsed.head.label(), "8d1b4a0");
    }

    #[test]
    fn an_unknown_ahead_behind_pair_is_unknown_and_not_in_sync() {
        // What `git status --no-ahead-behind` actually prints — measured on
        // git 2.50, which does *not* drop the line.
        let parsed = parse_porcelain_v2(
            &[
                rec(&["# branch.upstream origin/main"]),
                rec(&["# branch.ab +? -?"]),
            ]
            .concat(),
        );
        assert_eq!(parsed.ahead_behind, None);
    }

    #[test]
    fn the_xy_pair_splits_staged_from_unstaged() {
        let parsed = parse_porcelain_v2(
            &[
                head_records(),
                ordinary(".M", "unstaged.rs"),
                ordinary("M.", "staged.rs"),
                ordinary("MM", "both.rs"),
                ordinary("A.", "added.rs"),
                ordinary(".D", "gone-from-disk.rs"),
                ordinary("D.", "staged-delete.rs"),
            ]
            .concat(),
        );
        let staged: Vec<&str> = parsed
            .entries
            .iter()
            .filter(|e| e.is_staged())
            .map(|e| e.path.as_str())
            .collect();
        let unstaged: Vec<&str> = parsed
            .entries
            .iter()
            .filter(|e| e.is_unstaged())
            .map(|e| e.path.as_str())
            .collect();

        assert_eq!(
            staged,
            ["staged.rs", "both.rs", "added.rs", "staged-delete.rs"]
        );
        assert_eq!(
            unstaged,
            ["unstaged.rs", "both.rs", "gone-from-disk.rs"],
            "`both.rs` belongs to both groups at once"
        );

        assert_eq!(by_path(&parsed, "both.rs").index, ChangeCode::Modified);
        assert_eq!(by_path(&parsed, "both.rs").worktree, ChangeCode::Modified);
        assert_eq!(by_path(&parsed, "added.rs").worktree, ChangeCode::None);
        assert_eq!(
            by_path(&parsed, "gone-from-disk.rs").worktree,
            ChangeCode::Deleted
        );
        assert_eq!(
            by_path(&parsed, "gone-from-disk.rs").deco(),
            DecoStatus::Deleted
        );
        assert!(parsed.entries.iter().all(|e| e.submodule.is_none()));
    }

    #[test]
    fn a_rename_eats_its_old_path_and_nothing_else() {
        // The regression this whole parser exists to avoid: `2` spends two NUL
        // records, so a naive one-record-per-entry loop reads the row *behind*
        // the rename as the old path and loses it.
        let parsed = parse_porcelain_v2(
            &[
                head_records(),
                rec(&[
                    "2 R. N... 100644 100644 100644 ",
                    SHA,
                    " ",
                    SHA,
                    " R100 src/new.rs",
                ]),
                rec(&["src/old.rs"]),
                ordinary(".M", "after-the-rename.rs"),
                rec(&["? untracked-behind-it.txt"]),
            ]
            .concat(),
        );

        assert_eq!(
            parsed
                .entries
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            [
                "src/new.rs",
                "after-the-rename.rs",
                "untracked-behind-it.txt"
            ],
            "the record after the rename is a record, not the old path"
        );

        let renamed = by_path(&parsed, "src/new.rs");
        assert_eq!(
            renamed.orig_path.as_ref().map(|p| p.as_str()),
            Some("src/old.rs")
        );
        assert_eq!(renamed.index, ChangeCode::Renamed);
        assert_eq!(renamed.rename_score, Some(100));
        assert_eq!(renamed.deco(), DecoStatus::Renamed);
        assert!(by_path(&parsed, "after-the-rename.rs").orig_path.is_none());
    }

    #[test]
    fn a_copy_record_carries_its_similarity_score() {
        let parsed = parse_porcelain_v2(&rec(&[
            "2 C. N... 100644 100644 100644 ",
            SHA,
            " ",
            SHA,
            " C75 copy.rs",
        ]));
        // Nothing followed it, so the old path is lost — but the file is not.
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].rename_score, Some(75));
        assert_eq!(parsed.entries[0].index, ChangeCode::Copied);
    }

    #[test]
    fn unmerged_records_become_conflicts_and_stay_out_of_both_groups() {
        let unmerged = |xy: &str, path: &str| {
            rec(&[
                "u ",
                xy,
                " N... 100644 100644 100644 100644 ",
                SHA,
                " ",
                SHA,
                " ",
                SHA,
                " ",
                path,
            ])
        };
        let parsed = parse_porcelain_v2(
            &[
                head_records(),
                unmerged("UU", "both-modified.rs"),
                unmerged("AA", "both-added.rs"),
                unmerged("DU", "deleted-by-us.rs"),
            ]
            .concat(),
        );

        assert_eq!(
            by_path(&parsed, "both-modified.rs").conflict,
            Some(ConflictKind::BothModified)
        );
        assert_eq!(
            by_path(&parsed, "both-added.rs").conflict,
            Some(ConflictKind::BothAdded)
        );
        assert_eq!(
            by_path(&parsed, "deleted-by-us.rs").conflict,
            Some(ConflictKind::DeletedByUs)
        );
        assert!(
            !by_path(&parsed, "deleted-by-us.rs")
                .conflict
                .unwrap()
                .ours_exists()
        );

        for entry in &parsed.entries {
            assert_eq!(entry.kind, EntryKind::Unmerged);
            assert!(!entry.is_staged(), "{} leaked into Staged", entry.path.text);
            assert!(
                !entry.is_unstaged(),
                "{} leaked into Changes",
                entry.path.text
            );
            assert_eq!(entry.deco(), DecoStatus::Conflict);
        }
    }

    #[test]
    fn untracked_and_ignored_records_parse_without_fields() {
        let parsed = parse_porcelain_v2(
            &[
                head_records(),
                rec(&["? build/out.o"]),
                // We never pass `--ignored`, but the parser must not choke the
                // day someone does.
                rec(&["! target/debug/tty7"]),
            ]
            .concat(),
        );

        let untracked = by_path(&parsed, "build/out.o");
        assert_eq!(untracked.kind, EntryKind::Untracked);
        assert!(untracked.is_untracked());
        assert!(!untracked.is_staged() && !untracked.is_unstaged());
        assert_eq!(untracked.deco(), DecoStatus::Untracked);

        assert_eq!(
            by_path(&parsed, "target/debug/tty7").kind,
            EntryKind::Ignored
        );
    }

    #[test]
    fn a_submodule_reports_its_three_sub_states() {
        let parsed = parse_porcelain_v2(&rec(&[
            "1 .M S.MU 160000 160000 160000 ",
            SHA,
            " ",
            SHA,
            " vendor/lib",
        ]));

        assert_eq!(
            parsed.entries[0].submodule,
            Some(SubmoduleState {
                commit_changed: false,
                modified_content: true,
                has_untracked: true,
            })
        );
    }

    #[test]
    fn paths_with_spaces_quotes_and_newlines_survive_intact() {
        // Every one of these is C-quoted without `-z`, which is the entire
        // reason the parser works on bytes instead of lines.
        let parsed = parse_porcelain_v2(
            &[
                head_records(),
                ordinary(".M", "d i r/sp ace.rs"),
                ordinary("M.", "quote\"file.rs"),
                rec(&["? new\nline.txt"]),
                ordinary(".M", "back\\slash.rs"),
            ]
            .concat(),
        );

        assert_eq!(
            by_path(&parsed, "d i r/sp ace.rs").path.file_name(),
            "sp ace.rs"
        );
        assert_eq!(
            by_path(&parsed, "quote\"file.rs").index,
            ChangeCode::Modified
        );
        assert_eq!(by_path(&parsed, "new\nline.txt").kind, EntryKind::Untracked);
        assert_eq!(by_path(&parsed, "back\\slash.rs").path.parent(), "");
        assert_eq!(parsed.entries.len(), 4);
    }

    #[test]
    fn a_non_utf8_path_is_shown_but_never_acted_on() {
        // `caf\xe9.rs` — latin-1, which a Linux filesystem is perfectly happy
        // to hold and which cannot be carried by the control protocol.
        let parsed = parse_porcelain_v2(&rec_bytes(
            &format!("1 .M N... 100644 100644 100644 {SHA} {SHA} "),
            b"caf\xe9.rs",
        ));

        let entry = &parsed.entries[0];
        assert!(entry.path.lossy);
        assert!(entry.path.text.starts_with("caf"));
        assert_eq!(
            entry.path.pathspec(),
            None,
            "the UI has to grey out staging this row"
        );
    }

    #[test]
    fn past_the_entry_cap_the_count_is_still_the_real_one() {
        let mut stdout = head_records();
        let overflow = 25;
        for i in 0..MAX_STATUS_ENTRIES + overflow {
            stdout.extend(ordinary(".M", &format!("f{i}.rs")));
        }
        let parsed = parse_porcelain_v2(&stdout);

        assert_eq!(parsed.entries.len(), MAX_STATUS_ENTRIES);
        assert_eq!(parsed.total_entries, MAX_STATUS_ENTRIES + overflow);
        assert!(parsed.truncated);
        assert_eq!(
            parsed.entries[0].path.as_str(),
            "f0.rs",
            "the kept entries are the first ones, not a random slice"
        );
    }

    #[test]
    fn the_index_decorates_every_ancestor_up_to_the_root() {
        let status = status_of(
            &[
                head_records(),
                ordinary(".M", "crates/core/src/git/status.rs"),
                rec(&["? docs/notes.md"]),
            ]
            .concat(),
        );
        let index = StatusIndex::build(&status);

        assert_eq!(index.root, PathBuf::from("/repo"));
        assert_eq!(
            index.file("crates/core/src/git/status.rs"),
            Some(DecoStatus::Modified)
        );
        for dir in [
            "crates",
            "crates/core",
            "crates/core/src",
            "crates/core/src/git",
        ] {
            assert_eq!(
                index.dir(dir),
                Some(DirRollup {
                    changed: true,
                    conflict: false
                }),
                "{dir} lost its rollup"
            );
        }
        assert_eq!(index.file("docs/notes.md"), Some(DecoStatus::Untracked));
        assert_eq!(index.dir("nowhere"), None);
        assert!(!index.is_empty());
    }

    #[test]
    fn one_conflict_colours_the_whole_path_to_the_root() {
        let status = status_of(
            &[
                head_records(),
                ordinary(".M", "a/b/quiet.rs"),
                rec(&[
                    "u UU N... 100644 100644 100644 100644 ",
                    SHA,
                    " ",
                    SHA,
                    " ",
                    SHA,
                    " a/b/c/clash.rs",
                ]),
            ]
            .concat(),
        );
        let index = StatusIndex::build(&status);

        for dir in ["a", "a/b", "a/b/c"] {
            assert!(index.dir(dir).unwrap().conflict, "{dir} should be red");
        }
        assert_eq!(index.file("a/b/quiet.rs"), Some(DecoStatus::Modified));
    }

    #[test]
    fn a_renames_old_directory_is_rolled_up_too() {
        let status = status_of(
            &[
                head_records(),
                rec(&[
                    "2 R. N... 100644 100644 100644 ",
                    SHA,
                    " ",
                    SHA,
                    " R090 new/home.rs",
                ]),
                rec(&["old/home.rs"]),
            ]
            .concat(),
        );
        let index = StatusIndex::build(&status);

        assert!(index.dir("new").unwrap().changed);
        assert!(
            index.dir("old").unwrap().changed,
            "the directory it left lost a file"
        );
        assert_eq!(
            index.file("old/home.rs"),
            None,
            "the old path holds no file row of its own"
        );
    }

    /// `git mv a b && echo x > a`: the rename record's old path and a fresh
    /// untracked file share a spelling. The row on disk is the untracked file;
    /// the rename must not outrank it just because `Renamed > Untracked`.
    #[test]
    fn a_file_recreated_at_a_renames_old_path_decorates_as_itself() {
        let status = status_of(
            &[
                head_records(),
                rec(&[
                    "2 R. N... 100644 100644 100644 ",
                    SHA,
                    " ",
                    SHA,
                    " R090 b.rs",
                ]),
                rec(&["a.rs"]),
                rec(&["? a.rs"]),
            ]
            .concat(),
        );
        let index = StatusIndex::build(&status);

        assert_eq!(index.file("a.rs"), Some(DecoStatus::Untracked));
        assert_eq!(index.file("b.rs"), Some(DecoStatus::Renamed));
    }

    /// `--ignored` is not passed today; the parser is future-proofed for it,
    /// and the decoration must be too — an ignored record carries no change
    /// codes and used to fall through to `Modified`.
    #[test]
    fn an_ignored_record_decorates_as_ignored_not_modified() {
        let parsed = parse_porcelain_v2(&[head_records(), rec(&["! target"])].concat());
        let entry = &parsed.entries[0];
        assert_eq!(entry.kind, EntryKind::Ignored);
        assert_eq!(entry.deco(), DecoStatus::Ignored);
    }

    #[test]
    fn past_the_decoration_cap_only_directories_stay_lit() {
        let mut stdout = head_records();
        for i in 0..MAX_DECORATED_FILES + 1 {
            stdout.extend(ordinary(".M", &format!("deep/dir/f{i}.rs")));
        }
        let index = StatusIndex::build(&status_of(&stdout));

        assert!(index.files_dropped);
        assert_eq!(index.file("deep/dir/f0.rs"), None);
        assert!(
            index.dir("deep").unwrap().changed,
            "folders still say where"
        );
        assert!(index.dir("deep/dir").unwrap().changed);
    }

    // ----- against a real repository -------------------------------------

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A probe that must have found a repository, unwrapped with a reason.
    fn probed(probe: StatusProbe, why: &str) -> WorkingTreeStatus {
        match probe {
            StatusProbe::Status(status) => *status,
            other => panic!("{why}: {other:?}"),
        }
    }

    fn scratch(name: &str) -> Option<Scratch> {
        // The pid keeps two concurrent `cargo test` runs off each other's
        // fixture, since the directory is wiped on the way in — the same
        // reason `log.rs` carries one.
        let dir =
            std::env::temp_dir().join(format!("tty7-scm-status-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        Some(Scratch(dir))
    }

    /// Runs git with the identity, signing and line-ending settings pinned, so
    /// the test does not depend on whatever is in the developer's
    /// `~/.gitconfig` or in Git for Windows' system config.
    fn run(host: &dyn Host, cwd: &Path, args: &[&str]) -> bool {
        let mut full = PINS.to_vec();
        full.extend_from_slice(args);
        host.git(cwd, &full).map(|o| o.success()).unwrap_or(false)
    }

    /// `git init` on a branch named here, with the pins written into the
    /// repository so that `probe_status`'s own git reads them too. `false`
    /// means there is no git on this machine, which is not a failure.
    fn init_repo(host: &dyn Host, repo: &Path) -> bool {
        if !run(host, repo, &["init", "--quiet"]) {
            return false;
        }
        assert!(pin_repo_config(repo));
        // Not `init -b`: that is git 2.28+, and the branch name is asserted on.
        assert!(run(
            host,
            repo,
            &["symbolic-ref", "HEAD", "refs/heads/main"]
        ));
        true
    }

    #[test]
    fn a_real_repository_reports_all_four_shapes_at_once() {
        let host = crate::host::local::LocalHost::new();
        let Some(scratch) = scratch("real-repo") else {
            return;
        };
        let repo = &scratch.0;

        if !init_repo(&*host, repo) {
            return; // no git on this machine
        }
        std::fs::write(repo.join("kept.txt"), "one\n").unwrap();
        std::fs::write(repo.join("moved.txt"), "a\nb\nc\nd\ne\nf\ng\nh\n").unwrap();
        assert!(run(&*host, repo, &["add", "-A"]));
        assert!(run(&*host, repo, &["commit", "--quiet", "-m", "base"]));

        std::fs::write(repo.join("staged.txt"), "new\n").unwrap();
        assert!(run(&*host, repo, &["add", "staged.txt"]));
        std::fs::write(repo.join("kept.txt"), "one\ntwo\n").unwrap();
        assert!(run(&*host, repo, &["mv", "moved.txt", "renamed.txt"]));
        std::fs::write(repo.join("untracked.txt"), "loose\n").unwrap();

        let status = probed(
            probe_status(&*host, repo),
            "a repository was just created here",
        );

        match &status.head {
            HeadState::Branch { name, oid } => {
                assert_eq!(name, "main");
                assert_eq!(oid.len(), 40, "the header carries the full sha: {oid}");
            }
            other => panic!("expected a branch, got {other:?}"),
        }
        assert_eq!(status.upstream, None);
        assert_eq!(status.ahead_behind, None, "no upstream, no number");
        assert_eq!(status.operation, None);
        assert_eq!(status.prefilled_message, None);
        assert_eq!(status.stash_count, 0);
        assert!(!status.truncated);
        assert!(!status.is_clean());
        // `status.root` is what `git rev-parse` answered, so the two sides have
        // to be brought into one spelling before they can be compared — see
        // `one_spelling`. Still an exact equality, just not a literal one.
        assert_eq!(
            one_spelling(&status.root),
            one_spelling(&std::fs::canonicalize(repo).unwrap())
        );
        assert_eq!(status.home, status.root);

        fn names(mut v: Vec<&str>) -> Vec<&str> {
            v.sort_unstable();
            v
        }
        assert_eq!(
            names(status.staged().map(|e| e.path.as_str()).collect()),
            ["renamed.txt", "staged.txt"]
        );
        assert_eq!(
            names(status.unstaged().map(|e| e.path.as_str()).collect()),
            ["kept.txt"]
        );
        assert_eq!(
            names(status.untracked().map(|e| e.path.as_str()).collect()),
            ["untracked.txt"]
        );
        assert_eq!(status.conflicts().count(), 0);

        let renamed = status
            .entries
            .iter()
            .find(|e| e.path.as_str() == "renamed.txt")
            .unwrap();
        assert_eq!(renamed.index, ChangeCode::Renamed);
        assert_eq!(
            renamed.orig_path.as_ref().map(|p| p.as_str()),
            Some("moved.txt")
        );

        let index = StatusIndex::build(&status);
        assert_eq!(index.file("kept.txt"), Some(DecoStatus::Modified));
        assert_eq!(index.file("untracked.txt"), Some(DecoStatus::Untracked));
    }

    #[test]
    fn a_merge_in_progress_is_named_and_pre_fills_its_message() {
        let host = crate::host::local::LocalHost::new();
        let Some(scratch) = scratch("merge-op") else {
            return;
        };
        let repo = &scratch.0;

        if !init_repo(&*host, repo) {
            return;
        }
        std::fs::write(repo.join("c.txt"), "base\n").unwrap();
        assert!(run(&*host, repo, &["add", "-A"]));
        assert!(run(&*host, repo, &["commit", "--quiet", "-m", "base"]));
        assert!(run(&*host, repo, &["checkout", "--quiet", "-b", "other"]));
        std::fs::write(repo.join("c.txt"), "theirs\n").unwrap();
        assert!(run(&*host, repo, &["commit", "--quiet", "-am", "theirs"]));
        assert!(run(&*host, repo, &["checkout", "--quiet", "main"]));
        std::fs::write(repo.join("c.txt"), "ours\n").unwrap();
        assert!(run(&*host, repo, &["commit", "--quiet", "-am", "ours"]));
        // Expected to fail — that is the point.
        run(&*host, repo, &["merge", "other"]);

        let status = probed(probe_status(&*host, repo), "still a repository mid-merge");
        assert_eq!(status.operation, Some(RepoOperation::Merge));
        assert!(
            status
                .prefilled_message
                .as_deref()
                .is_some_and(|m| m.contains("other")),
            "MERGE_MSG should seed the commit box: {:?}",
            status.prefilled_message
        );
        assert_eq!(status.conflicts().count(), 1);
        assert_eq!(
            status.conflicts().next().unwrap().conflict,
            Some(ConflictKind::BothModified)
        );
    }

    #[test]
    fn an_upstream_gives_ahead_and_behind_without_a_second_command() {
        let host = crate::host::local::LocalHost::new();
        let Some(scratch) = scratch("upstream") else {
            return;
        };
        let remote = scratch.0.join("remote.git");
        let repo = scratch.0.join("clone");
        std::fs::create_dir_all(&repo).unwrap();

        if !run(
            &*host,
            &scratch.0,
            &["init", "--quiet", "--bare", "remote.git"],
        ) {
            return;
        }
        assert!(init_repo(&*host, &repo));
        std::fs::write(repo.join("f.txt"), "one\n").unwrap();
        assert!(run(&*host, &repo, &["add", "-A"]));
        assert!(run(&*host, &repo, &["commit", "--quiet", "-m", "one"]));
        assert!(run(
            &*host,
            &repo,
            &["remote", "add", "origin", &remote.to_string_lossy()]
        ));
        assert!(run(
            &*host,
            &repo,
            &["push", "--quiet", "-u", "origin", "main"]
        ));
        std::fs::write(repo.join("f.txt"), "two\n").unwrap();
        assert!(run(&*host, &repo, &["commit", "--quiet", "-am", "two"]));

        let status = probed(probe_status(&*host, &repo), "a repository with a remote");
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        // Straight from `# branch.ab`; the `rev-list` fallback never runs here.
        assert_eq!(
            status.ahead_behind,
            Some((1, 0)),
            "ahead first, behind second"
        );
        assert!(status.is_clean());
    }

    /// The two negative answers stay distinct: an ordinary directory is
    /// `NotARepo` (record it, stop asking), a directory git could not even be
    /// run in is `Unreachable` (keep what is cached, ask again later).
    #[test]
    fn outside_a_repository_there_is_no_status() {
        let host = crate::host::local::LocalHost::new();
        let Some(scratch) = scratch("not-a-repo") else {
            return;
        };
        assert_eq!(probe_status(&*host, &scratch.0), StatusProbe::NotARepo);
        assert_eq!(
            probe_status(&*host, Path::new("/no/such/tty7/path")),
            StatusProbe::Unreachable,
            "a cwd that cannot be entered is not an answer about a repository"
        );
    }
}

//! History: the commits themselves, the refs pointing at them, and the lane
//! layout the graph is drawn from.
//!
//! The lane assignment lives here, not in the renderer, for two reasons. It is
//! a pure function over `(sha, parents)` and therefore the part of the graph
//! most worth testing exhaustively; and gpui re-runs `render` on every notify,
//! so an O(commits × lanes) pass in a paint closure would be burned every
//! frame for a result that only changes when the history does.

use std::collections::HashMap;
use std::path::Path;

use smallvec::SmallVec;

use super::RecordSplitter;
use super::diff::FileStatus;
use crate::host::Host;

/// Full hex object id. Kept as `String` rather than `[u8; 20]` because sha256
/// repositories exist and the extra allocation is noise next to the subject.
pub(crate) type Oid = String;

/// Lane index as assigned by the layout pass — the *true* column, before the
/// renderer folds anything past its width cap into an overflow column.
pub type Lane = u16;

/// Which palette entry a lane draws with. Equal to the lane it was created for
/// and never reassigned, which is what keeps a branch one colour for its whole
/// life: a branch holds the same lane from its tip until it is merged, because
/// the first parent inherits the lane in place and never migrates.
pub(crate) type ColorIdx = u16;

pub const GRAPH_PAGE: usize = 200;
pub const MAX_GRAPH_COMMITS: usize = 5_000;
pub const MAX_LANES: Lane = 32;
pub(crate) const MAX_REFS: usize = 2_000;
/// How many changed files one commit's detail view will hold. A vendored
/// dependency landing in a single commit is tens of thousands of paths, and
/// every one of them would become a row.
pub(crate) const MAX_COMMIT_FILES: usize = 1_000;
pub(crate) const MAX_SUBJECT_BYTES: usize = 512;
pub(crate) const MAX_BODY_BYTES: usize = 8 * 1024;
pub(crate) const MAX_LOG_BYTES: usize = 16 * 1024 * 1024;

/// Record separator for `log --pretty`. Deliberately not NUL: `git log -z`
/// already uses NUL between records, so a NUL field separator could only be
/// told apart by counting fields — and one NUL inside a commit message (git
/// objects allow it) would desynchronise the whole stream. RS and US cannot
/// occur in a sha, a refname, an ISO date or an address.
///
/// A commit *message* can still carry RS or US — nothing git accepts is out of
/// bounds there. The failure is contained, not eliminated: the body truncates
/// at the stray separator, `is_hex_oid` throws the tail away unless it is
/// deliberately shaped like a full record, and a deliberately shaped one can
/// fabricate at worst a bogus row in the graph — whose `git show` then fails.
/// Sealing that needs length-prefixed reads (`cat-file --batch`), a different
/// data path entirely.
pub(crate) const REC_SEP: u8 = 0x1e;
pub(crate) const FIELD_SEP: u8 = 0x1f;

/// A timestamp plus the author's own UTC offset, parsed from `%aI` / `%cI`.
///
/// `offset_minutes` is already subtracted out of `unix`, which is the field
/// everything currently renders from. Keeping the offset costs four bytes and
/// preserves the one thing the conversion to UTC throws away — what o'clock it
/// was where the commit was written. Nothing shows that yet; the parser is
/// simply not the place to lose it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OffsetTs {
    pub unix: i64,
    pub offset_minutes: i32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub at: OffsetTs,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum RefKind {
    /// Sorts last so the highest-priority chip wins a `max()`.
    Other,
    RemoteBranch,
    Tag,
    LocalBranch,
    Head,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RefDeco {
    pub kind: RefKind,
    /// `refs/heads/feature/x`
    pub full: String,
    /// `feature/x`
    pub short: String,
    /// Carried the `HEAD -> ` prefix in `%D`.
    pub is_head: bool,
    /// The full refname this branch tracks, where it tracks one. Only
    /// [`for_each_ref`] can fill it in — `%D` says nothing about upstreams —
    /// so a decoration parsed out of a `log` record always leaves it `None`.
    pub upstream: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Commit {
    pub oid: Oid,
    pub parents: SmallVec<[Oid; 2]>,
    pub author: Signature,
    pub committer: Signature,
    pub summary: String,
    pub body: String,
    pub refs: Vec<RefDeco>,
}

impl Commit {
    pub fn short(&self) -> &str {
        let n = self.oid.len().min(7);
        &self.oid[..n]
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}

/// One line inside a single row's band: from the row's top edge to its bottom.
///
/// Row-local on purpose. A model that described whole polylines across rows
/// could not emit a line until its far end arrived, so a long-lived branch
/// would stay invisible until the page holding its parent loaded — the bug
/// Zed's own graph has. Here a row is final the moment it is produced, which
/// is also what makes paging free of visual reflow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    /// Straight through the band without touching this row's node.
    Pass { lane: Lane, color: ColorIdx },
    /// Comes down from `from` on the top edge and ends at this row's node.
    In { from: Lane, color: ColorIdx },
    /// Leaves this row's node for `to` on the bottom edge.
    Out { to: Lane, color: ColorIdx },
}

impl Edge {
    pub fn color(self) -> ColorIdx {
        match self {
            Edge::Pass { color, .. } | Edge::In { color, .. } | Edge::Out { color, .. } => color,
        }
    }

    /// Paint order: pass-through lines first, so the node's own line lands on
    /// top of anything crossing behind it.
    pub fn paint_rank(&self) -> u8 {
        match self {
            Edge::Pass { .. } => 0,
            Edge::In { .. } => 1,
            Edge::Out { .. } => 2,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GraphRow {
    pub node: Lane,
    pub color: ColorIdx,
    /// 0 = root commit, 1 = ordinary, >1 = merge (>2 = octopus).
    pub parents: u8,
    pub edges: SmallVec<[Edge; 4]>,
}

/// Which refs the log is walked from.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub enum GraphScope {
    Head,
    /// HEAD plus its upstream — the default, matching VS Code.
    #[default]
    HeadAndUpstream,
    All,
    Refs(Vec<String>),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CommitPage {
    pub commits: Vec<Commit>,
    /// Same length as `commits`.
    pub rows: Vec<GraphRow>,
    pub max_lanes: Lane,
    pub scope: GraphScope,
    pub requested: usize,
    /// git returned fewer than asked for, so this is the end of history.
    pub complete: bool,
    pub truncated_lanes: bool,
    /// Lanes still open past the last row — drawn as fading stubs so a page
    /// boundary does not read as a row of root commits.
    pub open_lanes: Vec<Lane>,
}

/// The append-only lane assigner the rows come out of.
///
/// Fed `(sha, parents)` newest-first, it produces exactly one [`GraphRow`] per
/// commit and keeps only the state that has to survive a page boundary: which
/// oid each lane is holding a place for, and which lanes are holding a place
/// for a given oid.
///
/// Append-only is the point. A later page extends the graph without touching a
/// row already on screen, which is only possible because a row says nothing
/// about the rows below it — see [`Edge`].
#[derive(Default)]
pub(crate) struct LaneAlloc {
    /// Per lane, the oid that lane is currently waiting for. `None` is free.
    slots: Vec<Option<Oid>>,
    /// The reverse index. A `SmallVec` because one child is the common case
    /// and the second entry is the other side of a merge; more than two
    /// children of one commit is rare enough to spill.
    pending: HashMap<Oid, SmallVec<[Lane; 2]>>,
    truncated: bool,
}

impl LaneAlloc {
    pub fn new() -> LaneAlloc {
        LaneAlloc::default()
    }

    /// Lays out one page of commits, newest first. `--topo-order` is what makes
    /// a single forward pass enough: it guarantees a parent is listed after
    /// every one of its children, so by the time a commit is reached, every
    /// lane that wants it already exists.
    pub fn push(&mut self, page: &[(Oid, SmallVec<[Oid; 2]>)], out: &mut Vec<GraphRow>) {
        out.reserve(page.len());
        for (sha, parents) in page {
            let row = self.row(sha, parents);
            out.push(row);
        }
    }

    /// Lanes still held open past the last row laid out — the page boundary.
    /// The renderer draws these as stubs so the bottom of a page does not read
    /// as a row of root commits.
    pub fn open_lanes(&self) -> Vec<Lane> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.is_some())
            .map(|(lane, _)| lane as Lane)
            .collect()
    }

    /// How many columns are live right now, i.e. one past the rightmost lane in
    /// use. Lanes are never compacted, so this only shrinks when the rightmost
    /// lane itself dies.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn width(&self) -> Lane {
        self.slots
            .iter()
            .rposition(|slot| slot.is_some())
            .map_or(0, |lane| lane as Lane + 1)
    }

    /// Whether history was wider than [`MAX_LANES`] at some point, so lines
    /// were forced to share the last lane. Sticky: once true it stays true.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    fn row(&mut self, sha: &Oid, parents: &[Oid]) -> GraphRow {
        // Sorted so the lines entering this row are emitted left to right and
        // the node lands on the leftmost of them — mainline hugs the left.
        let mut waiting = self.pending.remove(sha).unwrap_or_default();
        waiting.sort_unstable();
        waiting.dedup();
        let node = match waiting.first() {
            Some(lane) => *lane,
            // Nothing is waiting: no child of this commit is inside the window,
            // so it is a tip and starts a lane of its own.
            None => self.alloc_lane(&[]),
        };

        let mut edges: SmallVec<[Edge; 4]> = SmallVec::new();
        for (lane, slot) in self.slots.iter().enumerate() {
            let lane = lane as Lane;
            if slot.is_some() && !waiting.contains(&lane) {
                edges.push(Edge::Pass { lane, color: lane });
            }
        }
        for &lane in &waiting {
            edges.push(Edge::In {
                from: lane,
                color: lane,
            });
            // Release the lane so a parent can claim it below — but only if it
            // really is this commit's. Past MAX_LANES two commits can share the
            // overflow lane, and clearing it there would strand the other one.
            if self.slots[lane as usize].as_deref() == Some(sha.as_str()) {
                self.slots[lane as usize] = None;
            }
        }

        // Lanes this row just gave up, plus the node's own. A second parent
        // that landed on one of them would draw `lane 2 → node → lane 2`, a V
        // that reads as one line bending rather than two lines meeting.
        let mut avoid = waiting;
        if !avoid.contains(&node) {
            avoid.push(node);
        }

        let mut claimed: SmallVec<[Lane; 4]> = SmallVec::new();
        for (k, parent) in parents.iter().enumerate() {
            let lane = if k == 0 {
                // The first parent inherits the node's lane in place, never
                // migrating. That is what keeps a branch on one lane — and so
                // in one colour — from its tip down to wherever it was merged.
                node
            } else if let Some(lane) = self
                .pending
                .get(parent)
                .and_then(|lanes| lanes.iter().copied().min())
            {
                // Some other child already reserved a lane for this parent.
                // Join it instead of opening a second lane to the same commit:
                // two lanes converging on one dot is what the merge row itself
                // is for, and lanes are the scarce resource.
                lane
            } else {
                self.alloc_lane(&avoid)
            };
            if claimed.contains(&lane) {
                continue;
            }
            claimed.push(lane);
            if self.slots[lane as usize].as_deref() != Some(parent.as_str()) {
                self.slots[lane as usize] = Some(parent.clone());
                self.pending.entry(parent.clone()).or_default().push(lane);
            }
            edges.push(Edge::Out {
                to: lane,
                color: lane,
            });
        }
        // No parents: a root. `slots[node]` was released above and nothing
        // claimed it, so the lane simply ends here.

        // Stable: two edges of one rank (a merge's several `Out`s) keep their
        // insertion order — first parent first — which the golden tests pin.
        edges.sort_by_key(Edge::paint_rank);
        GraphRow {
            node,
            // Colour is the lane number, fixed when the lane is created and
            // never reassigned. Not a per-branch counter (what the Git Graph
            // extension does): with a palette of N, branch 0 and branch N come
            // out the same colour, and in a 3-column panel those two are very
            // likely adjacent. Keying on the lane makes neighbouring columns
            // maximally distinct by construction, and the "one branch, one
            // colour" property falls out of the first parent inheriting in
            // place.
            color: node,
            parents: parents.len().min(u8::MAX as usize) as u8,
            edges,
        }
    }

    /// The lowest free lane, avoiding the given ones if that is possible
    /// without widening the graph past [`MAX_LANES`].
    fn alloc_lane(&mut self, avoid: &[Lane]) -> Lane {
        let free = self
            .slots
            .iter()
            .enumerate()
            .find(|(lane, slot)| slot.is_none() && !avoid.contains(&(*lane as Lane)))
            .map(|(lane, _)| lane as Lane);
        if let Some(lane) = free {
            return lane;
        }
        if self.slots.len() < MAX_LANES as usize {
            self.slots.push(None);
            return (self.slots.len() - 1) as Lane;
        }
        // At the cap the avoidance is dropped rather than honoured: a lane is
        // expensive in a 216px panel and a V-shaped kink is only ugly.
        if let Some(lane) = self.slots.iter().position(Option::is_none) {
            return lane as Lane;
        }
        // Genuinely out of lanes. Everything past here shares the last one,
        // which the caller reports so the renderer can say the graph is
        // incomplete rather than quietly drawing a lie.
        self.truncated = true;
        MAX_LANES - 1
    }
}

fn row_span(row: &GraphRow) -> Lane {
    let mut span = row.node + 1;
    for edge in &row.edges {
        let lane = match *edge {
            Edge::Pass { lane, .. } => lane,
            Edge::In { from, .. } => from,
            Edge::Out { to, .. } => to,
        };
        span = span.max(lane + 1);
    }
    span
}

/// The eleven fields, in order: sha, parents, author name/email/date,
/// committer name/email/date, decorations, subject, body.
pub(crate) const LOG_PRETTY: &str =
    "--pretty=format:%x1e%H%x1f%P%x1f%an%x1f%ae%x1f%aI%x1f%cn%x1f%ce%x1f%cI%x1f%D%x1f%s%x1f%b";

const LOG_FIELDS: usize = 11;

pub(crate) const REF_FORMAT: &str = "--format=%(objectname)%x1f%(refname)%x1f%(refname:short)%x1f%(upstream)%x1f%(HEAD)%x1f%(objecttype)%x1f%(*objectname)";

/// What [`parse_log`] read, and whether it read all of it.
pub(crate) struct ParsedLog {
    pub commits: Vec<Commit>,
    /// The stream was cut short — by [`MAX_LOG_BYTES`], or by a record past
    /// `MAX_RECORD` being dropped whole. The caller must not present the
    /// commits as "all of history": git returned more than was parsed.
    pub truncated: bool,
}

/// Parses the output of the `log` invocation [`LOG_PRETTY`] belongs to.
///
/// Records are split on RS and fields on US. Fields are taken with `splitn`, so
/// the body — the only field that can contain anything at all — absorbs every
/// separator past the tenth instead of shifting the parse.
pub(crate) fn parse_log(stdout: &[u8]) -> ParsedLog {
    let mut commits = Vec::new();
    let mut used = 0usize;
    let mut clipped = false;
    let mut on_record = |record: &[u8]| {
        used = used.saturating_add(record.len());
        if used > MAX_LOG_BYTES {
            clipped = true;
            return;
        }
        if let Some(commit) = parse_record(record) {
            commits.push(commit);
        }
    };
    let mut split = RecordSplitter::new(REC_SEP);
    split.push(stdout, &mut on_record);
    let dropped = split.finish(&mut on_record);
    ParsedLog {
        commits,
        truncated: clipped || dropped > 0,
    }
}

fn parse_record(record: &[u8]) -> Option<Commit> {
    let text = String::from_utf8_lossy(record);
    let mut fields = text.splitn(LOG_FIELDS, FIELD_SEP as char);
    let oid = fields.next()?;
    // The stream opens with an empty record (nothing precedes the first RS),
    // and a format drift would otherwise turn one bad record into a page of
    // nonsense. Anything that is not a sha is not a commit.
    if !is_hex_oid(oid) {
        return None;
    }
    let parents = fields
        .next()?
        .split_ascii_whitespace()
        .map(str::to_string)
        .collect();
    let author_name = fields.next()?;
    let author_email = fields.next()?;
    let author_at = fields.next()?;
    let committer_name = fields.next()?;
    let committer_email = fields.next()?;
    let committer_at = fields.next()?;
    let deco = fields.next()?;
    let subject = fields.next()?;
    // git joins records with a newline, so the last field carries it.
    let body = fields.next()?.trim_end_matches(['\n', '\r']);

    Some(Commit {
        oid: oid.to_string(),
        parents,
        author: signature(author_name, author_email, author_at),
        committer: signature(committer_name, committer_email, committer_at),
        summary: clip(subject, MAX_SUBJECT_BYTES).to_string(),
        body: clip(body, MAX_BODY_BYTES).to_string(),
        refs: parse_deco(deco),
    })
}

fn signature(name: &str, email: &str, at: &str) -> Signature {
    Signature {
        name: name.to_string(),
        email: email.to_string(),
        // A commit whose `%aI` will not parse is not worth dropping the commit
        // over; it loses its timestamp and keeps everything else.
        at: parse_iso8601(at).unwrap_or(OffsetTs {
            unix: 0,
            offset_minutes: 0,
        }),
    }
}

fn is_hex_oid(text: &str) -> bool {
    (4..=64).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Truncates to at most `max` bytes without splitting a character.
fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `%D` with `--decorate=full`: `HEAD -> refs/heads/main, tag: refs/tags/v1,
/// refs/remotes/origin/main`.
fn parse_deco(text: &str) -> Vec<RefDeco> {
    let mut out = Vec::new();
    for piece in text.split(',') {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let (is_head, name) = match piece.strip_prefix("HEAD -> ") {
            Some(rest) => (true, rest.trim()),
            None => (false, piece),
        };
        // `--decorate=full` spells refnames out in full but still marks tags
        // with a `tag: ` prefix, so both have to come off.
        let name = name.strip_prefix("tag: ").unwrap_or(name).trim();
        if name == "HEAD" {
            out.push(RefDeco {
                kind: RefKind::Head,
                full: "HEAD".to_string(),
                short: "HEAD".to_string(),
                is_head: true,
                upstream: None,
            });
            continue;
        }
        if let Some(deco) = ref_deco(name, is_head) {
            out.push(deco);
        }
    }
    out
}

fn ref_deco(full: &str, is_head: bool) -> Option<RefDeco> {
    let (kind, short) = if let Some(rest) = full.strip_prefix("refs/heads/") {
        (RefKind::LocalBranch, rest)
    } else if let Some(rest) = full.strip_prefix("refs/remotes/") {
        (RefKind::RemoteBranch, rest)
    } else if let Some(rest) = full.strip_prefix("refs/tags/") {
        (RefKind::Tag, rest)
    } else {
        (RefKind::Other, full.strip_prefix("refs/").unwrap_or(full))
    };
    if short.is_empty() {
        return None;
    }
    Some(RefDeco {
        kind,
        full: full.to_string(),
        short: short.to_string(),
        is_head,
        upstream: None,
    })
}

/// Parses the output of the `for-each-ref` invocation [`REF_FORMAT`] belongs
/// to, keyed by the commit each ref ultimately points at.
pub(crate) fn parse_refs(stdout: &[u8]) -> HashMap<Oid, Vec<RefDeco>> {
    let mut out: HashMap<Oid, Vec<RefDeco>> = HashMap::new();
    let text = String::from_utf8_lossy(stdout);
    for line in text.lines().take(MAX_REFS) {
        let mut fields = line.split(FIELD_SEP as char);
        let object = fields.next().unwrap_or_default().trim();
        let full = fields.next().unwrap_or_default().trim();
        // `%(refname:short)` is skipped in favour of stripping the prefix here,
        // so a ref named the same way from `%D` and from here reads identically
        // in the UI. `%(objecttype)` is asked for to keep the format one line
        // rather than two; only `%(*objectname)` below acts on it.
        let _short = fields.next();
        let upstream = fields.next().unwrap_or_default().trim();
        let head = fields.next().unwrap_or_default().trim();
        let _kind = fields.next();
        let peeled = fields.next().unwrap_or_default().trim();
        // `%(*objectname)` is empty unless this is an annotated tag. When it is
        // not, the chip belongs on the commit rather than on the tag object,
        // which is the only reason the field is in the format.
        let target = if peeled.is_empty() { object } else { peeled };
        if full.is_empty() || !is_hex_oid(target) {
            continue;
        }
        if let Some(mut deco) = ref_deco(full, head == "*") {
            deco.upstream = (!upstream.is_empty()).then(|| upstream.to_string());
            out.entry(target.to_string()).or_default().push(deco);
        }
    }
    out
}

pub(crate) fn for_each_ref(host: &dyn Host, root: &Path) -> HashMap<Oid, Vec<RefDeco>> {
    let count = format!("--count={MAX_REFS}");
    let args = [
        "for-each-ref",
        "--sort=-committerdate",
        &count,
        REF_FORMAT,
        "refs/heads",
        "refs/remotes",
        "refs/tags",
    ];
    match host.git(root, &args) {
        Ok(out) if out.success() => parse_refs(&out.stdout),
        _ => HashMap::new(),
    }
}

/// The local branch names, one per line, in git's own refname order.
///
/// `for-each-ref` rather than `branch`: no porcelain warnings, no column
/// layout, and one name per line whatever the user's config says. It is a
/// separate call from [`for_each_ref`] because that one groups by the commit a
/// ref points at, which is the wrong shape for a list of branches — the
/// switcher wants every branch, including the ones sharing a tip.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn local_branches(host: &dyn Host, root: &Path) -> Vec<String> {
    let count = format!("--count={MAX_REFS}");
    let args = [
        "for-each-ref",
        &count,
        "--format=%(refname:short)",
        "refs/heads",
    ];
    match host.git(root, &args) {
        Ok(out) if out.success() => parse_branch_names(&out.stdout),
        _ => Vec::new(),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn parse_branch_names(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .take(MAX_REFS)
        .map(str::to_string)
        .collect()
}

/// One commit's metadata, for a detail view that does not already have it.
///
/// A commit that is on screen in the graph is already a [`Commit`] in
/// [`CommitPage::commits`], and the caller is expected to hand that over
/// instead of paying for this. What is left is the case the page cannot
/// answer: a commit reached from a parent link, or from anywhere outside the
/// window the graph happens to be holding.
pub fn load_commit(host: &dyn Host, root: &Path, rev: &str) -> Option<Commit> {
    if !is_rev(rev) {
        return None;
    }
    let args = [
        "-c",
        "log.showSignature=false",
        "show",
        "--no-patch",
        // Without it `%D` prints short names, and `parse_deco` reads full
        // ones — every chip would come back as `RefKind::Other`.
        "--decorate=full",
        "--no-color",
        LOG_PRETTY,
        rev,
    ];
    let out = host.git(root, &args).ok()?;
    if !out.success() {
        return None;
    }
    parse_log(&out.stdout).commits.into_iter().next()
}

/// One path a commit touched, with the line counts beside it.
///
/// The counts are `Option` rather than `0` because "git did not say" and
/// "nothing changed" are different answers: a binary file reports neither, and
/// a pure rename reports `0 0`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CommitFile {
    /// Repository-relative, and for a rename the *new* name.
    pub path: String,
    /// Where a rename or a copy came from.
    pub orig_path: Option<String>,
    pub status: FileStatus,
    pub added: Option<u32>,
    pub removed: Option<u32>,
    pub binary: bool,
}

/// The paths one commit touched, against its first parent.
///
/// Two commands rather than one, because git will take `--numstat` and
/// `--name-status` together and then quietly drop the numstat half — measured
/// on 2.50.1, where the combined `-z` stream comes back as pure name-status.
/// So they are run separately and joined on the path.
///
/// `log -1 --first-parent`, *not* `diff-tree -m --first-parent`: `diff-tree`
/// does not honour `--first-parent` as a narrowing of a merge. On the same git
/// it emits one diff per parent and concatenates them, so a two-parent merge
/// comes back with a file list twice as long as the merge really is. `log` is
/// also exactly how [`DiffSource::Commit`](super::diff::DiffSource) walks the
/// patch, which is what makes this list and the overlay's cards agree file for
/// file — and it needs no `--root`, because `log` shows a root commit's
/// contents as additions without being asked.
pub fn commit_files(host: &dyn Host, root: &Path, rev: &str) -> Option<Vec<CommitFile>> {
    let numstat = commit_diff(host, root, rev, "--numstat")?;
    let name_status = commit_diff(host, root, rev, "--name-status")?;
    Some(join_commit_files(&numstat, &name_status))
}

fn commit_diff(host: &dyn Host, root: &Path, rev: &str, what: &str) -> Option<Vec<u8>> {
    if !is_rev(rev) {
        return None;
    }
    let args = [
        "-c",
        "log.showSignature=false",
        // Without it a non-ASCII path comes back wrapped in quotes with its
        // bytes spelled as C octal escapes, and nothing here decodes those.
        "-c",
        "core.quotePath=false",
        "log",
        "-1",
        "--format=",
        "--first-parent",
        "--no-color",
        "-z",
        what,
        "--find-renames",
        rev,
    ];
    let out = host.git(root, &args).ok()?;
    out.success().then_some(out.stdout)
}

/// A rev the caller made up is still a rev git will be handed, so anything
/// that could be read as an option is refused before it gets there.
fn is_rev(rev: &str) -> bool {
    !rev.is_empty() && !rev.starts_with('-') && !rev.contains(|c: char| c.is_control())
}

fn records(stdout: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut on_record = |record: &[u8]| out.push(String::from_utf8_lossy(record).into_owned());
    let mut split = RecordSplitter::new(0);
    split.push(stdout, &mut on_record);
    // A dropped record here is a >1 MiB *pathname* — losing that one row from
    // a commit's file list is the same answer `MAX_COMMIT_FILES` already gives
    // for lists that are merely long.
    let _ = split.finish(&mut on_record);
    out
}

/// `-z --numstat`: `<added>\t<removed>\t<path>\0` per file — except for a
/// rename or a copy, where the third field is *empty* and the old and the new
/// path follow as two records of their own. A binary file reports `-\t-`.
fn parse_numstat(stdout: &[u8]) -> HashMap<String, (Option<u32>, Option<u32>, bool)> {
    let mut out = HashMap::new();
    let records = records(stdout);
    let mut at = 0usize;
    while at < records.len() && out.len() < MAX_COMMIT_FILES {
        let record = &records[at];
        at += 1;
        let mut fields = record.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let binary = added == "-" && removed == "-";
        let counts = (added.parse::<u32>().ok(), removed.parse::<u32>().ok());
        let path = if path.is_empty() {
            let new = records.get(at + 1).cloned();
            at += 2;
            match new {
                Some(new) => new,
                // Truncated mid-rename. Nothing else can be read from here.
                None => break,
            }
        } else {
            path.to_string()
        };
        out.insert(path, (counts.0, counts.1, binary));
    }
    out
}

/// `-z --name-status`: `<status>\0<path>\0`, and `R<score>\0<old>\0<new>\0`
/// for the two statuses that name two paths.
fn parse_name_status(stdout: &[u8]) -> Vec<(String, Option<String>, FileStatus)> {
    let mut out = Vec::new();
    let records = records(stdout);
    let mut at = 0usize;
    while at < records.len() && out.len() < MAX_COMMIT_FILES {
        let code = records[at].trim().to_string();
        at += 1;
        let two_paths = matches!(code.as_bytes().first(), Some(b'R' | b'C'));
        let taken = 1 + usize::from(two_paths);
        let Some(paths) = records.get(at..at + taken) else {
            break;
        };
        at += taken;
        let Some(status) = file_status(&code) else {
            continue;
        };
        match paths {
            [path] => out.push((path.clone(), None, status)),
            [old, new] => out.push((new.clone(), Some(old.clone()), status)),
            _ => {}
        }
    }
    out
}

fn file_status(code: &str) -> Option<FileStatus> {
    match code.as_bytes().first()? {
        b'A' => Some(FileStatus::Added),
        b'M' => Some(FileStatus::Modified),
        b'D' => Some(FileStatus::Deleted),
        b'R' => Some(FileStatus::Renamed),
        b'C' => Some(FileStatus::Copied),
        b'T' => Some(FileStatus::TypeChanged),
        // `X` is git's own "unknown"; `B` only appears under
        // `--break-rewrites`, which nothing here passes.
        b'U' => Some(FileStatus::Unmerged),
        _ => None,
    }
}

/// Joins the two streams on the path.
///
/// `--name-status` is the spine: it carries the letter every row is drawn
/// from, and it is in git's own order. `--numstat` only contributes counts, so
/// losing it costs the numbers and nothing else. Losing the other way round is
/// worse — a path with counts and no letter would vanish — so anything left
/// over is appended rather than dropped.
pub(crate) fn join_commit_files(numstat: &[u8], name_status: &[u8]) -> Vec<CommitFile> {
    let mut counts = parse_numstat(numstat);
    let named = parse_name_status(name_status);
    let mut out: Vec<CommitFile> = Vec::with_capacity(named.len());
    for (path, orig_path, status) in named {
        let (added, removed, binary) = counts.remove(&path).unwrap_or((None, None, false));
        out.push(CommitFile {
            path,
            orig_path,
            status,
            added,
            removed,
            binary,
        });
    }
    let mut leftover: Vec<_> = counts.into_iter().collect();
    // A `HashMap` has no order to preserve, and a file list that reshuffles
    // itself between two reads of the same commit would be worse than a
    // list that is merely not in git's order.
    leftover.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, (added, removed, binary)) in leftover {
        if out.len() >= MAX_COMMIT_FILES {
            break;
        }
        out.push(CommitFile {
            path,
            orig_path: None,
            status: FileStatus::Modified,
            added,
            removed,
            binary,
        });
    }
    out
}

/// Loads the newest `count` commits of `scope` and lays them out.
///
/// Paging is a bigger `-n`, never `--skip`. `--skip=M` walks and discards M
/// commits every time, and any ref that moves between two pages shifts the
/// window so page two no longer continues page one. Re-walking is O(n) either
/// way, the layout is deterministic, so a larger page reproduces the previous
/// one as its prefix and nothing on screen moves.
pub fn load_page(
    host: &dyn Host,
    root: &Path,
    scope: &GraphScope,
    count: usize,
) -> Option<CommitPage> {
    let count = count.clamp(1, MAX_GRAPH_COMMITS);
    let revs = scope_revs(host, root, scope);
    if revs.is_empty() {
        // An unborn HEAD. Not a failure: there is simply no history yet.
        return Some(CommitPage {
            commits: Vec::new(),
            rows: Vec::new(),
            max_lanes: 0,
            scope: scope.clone(),
            requested: count,
            complete: true,
            truncated_lanes: false,
            open_lanes: Vec::new(),
        });
    }

    let n = count.to_string();
    let mut args = vec![
        "-c",
        // Verifying signatures on every commit costs more than everything else
        // in this command put together, and the graph never shows the result.
        "log.showSignature=false",
        "log",
        // Not `--date-order`: the layout needs every parent to come after all
        // of its children, and dates do not guarantee that. A rebase or a
        // cherry-pick across timezones is enough to invert a pair.
        "--topo-order",
        "--decorate=full",
        "--no-color",
        LOG_PRETTY,
        "-n",
        &n,
    ];
    args.extend(revs.iter().map(String::as_str));

    // Buffered rather than streamed on purpose. `Host::git_lines` splits on
    // newlines, which would chop these RS-delimited records apart and leave the
    // caller to glue them back together, and `Host::git` is byte-exact over the
    // control protocol's base64. 5000 commits is a few MB, which that carries
    // fine. If it ever stops being fine the fix is a raw byte stream on `Host`,
    // and that one does need a control protocol bump.
    let out = host.git(root, &args).ok()?;
    if !out.success() {
        return None;
    }
    let parsed = parse_log(&out.stdout);
    let mut commits = parsed.commits;
    // "End of history" needs both halves: git answered with fewer than asked
    // for, *and* the parse read everything git answered with. A stream cut at
    // `MAX_LOG_BYTES` also has fewer commits than `count` — calling that
    // complete would freeze paging on a truncated graph.
    let complete = !parsed.truncated && commits.len() < count;

    let page: Vec<(Oid, SmallVec<[Oid; 2]>)> = commits
        .iter()
        .map(|c| (c.oid.clone(), c.parents.clone()))
        .collect();
    let mut alloc = LaneAlloc::new();
    let mut rows = Vec::with_capacity(page.len());
    alloc.push(&page, &mut rows);
    let max_lanes = rows.iter().map(row_span).max().unwrap_or(0);

    let mut by_oid = for_each_ref(host, root);
    for commit in &mut commits {
        if let Some(extra) = by_oid.remove(&commit.oid) {
            for deco in extra {
                if !commit.refs.iter().any(|r| r.full == deco.full) {
                    commit.refs.push(deco);
                }
            }
        }
        // Highest priority first, so a row that has space for one chip can take
        // the first and count the rest.
        commit.refs.sort_by(|a, b| {
            b.is_head
                .cmp(&a.is_head)
                .then_with(|| b.kind.cmp(&a.kind))
                .then_with(|| a.short.cmp(&b.short))
        });
    }

    Some(CommitPage {
        commits,
        rows,
        max_lanes,
        scope: scope.clone(),
        requested: count,
        complete,
        truncated_lanes: alloc.truncated(),
        open_lanes: alloc.open_lanes(),
    })
}

/// The revs to walk for a scope.
///
/// Every symbolic name resolves to a sha first. Paging re-runs the walk with a
/// larger `-n`, and a symbolic `HEAD` or branch name would let a commit pushed
/// between the two runs change where page two starts — the second page would
/// no longer be a superset of the first, which is the one thing paging here
/// relies on. (`--all` cannot be pinned; that scope accepts the reflow.)
/// A name that no longer resolves — a deleted branch, an unborn HEAD — simply
/// contributes nothing, which reads as "no history" rather than as a failure.
fn scope_revs(host: &dyn Host, root: &Path, scope: &GraphScope) -> Vec<String> {
    match scope {
        GraphScope::Head => rev(host, root, "HEAD^{commit}").into_iter().collect(),
        GraphScope::All => vec!["--all".to_string()],
        GraphScope::Refs(refs) => refs
            .iter()
            // A refname cannot begin with `-`, so anything that does is
            // either a mistake or an option smuggled in through a scope.
            .filter(|r| !r.is_empty() && !r.starts_with('-'))
            .filter_map(|r| rev(host, root, &format!("{r}^{{commit}}")))
            .collect(),
        GraphScope::HeadAndUpstream => {
            let mut revs = Vec::new();
            if let Some(head) = rev(host, root, "HEAD^{commit}") {
                revs.push(head);
            }
            if let Some(upstream) = rev(host, root, "@{upstream}^{commit}")
                && !revs.contains(&upstream)
            {
                revs.push(upstream);
            }
            revs
        }
    }
}

fn rev(host: &dyn Host, root: &Path, spec: &str) -> Option<String> {
    let out = super::git(host, root, &["rev-parse", "--verify", "--quiet", spec])?;
    let sha = out.trim();
    is_hex_oid(sha).then(|| sha.to_string())
}

/// `%aI` is strict ISO 8601: `2026-08-09T14:03:11+08:00`, or `Z` for UTC.
///
/// Hand-rolled because the workspace carries neither `chrono` nor `time`, and
/// thirty lines of arithmetic is a poor reason to add a dependency tree to a
/// crate the headless server also builds.
fn parse_iso8601(text: &str) -> Option<OffsetTs> {
    let b = text.as_bytes();
    if !text.is_ascii() || b.len() < 19 {
        return None;
    }
    if b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    if b[10] != b'T' && b[10] != b't' && b[10] != b' ' {
        return None;
    }
    let year: i64 = text[0..4].parse().ok()?;
    let month: u32 = text[5..7].parse().ok()?;
    let day: u32 = text[8..10].parse().ok()?;
    let hour: i64 = text[11..13].parse().ok()?;
    let minute: i64 = text[14..16].parse().ok()?;
    // 60 is a leap second, which git will never emit but which is legal.
    let second: i64 = text[17..19].parse().ok()?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let offset_minutes = parse_offset(&text[19..])?;
    let unix = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
        - i64::from(offset_minutes) * 60;
    Some(OffsetTs {
        unix,
        offset_minutes,
    })
}

fn parse_offset(text: &str) -> Option<i32> {
    if text.is_empty() || text == "Z" || text == "z" {
        return Some(0);
    }
    let (sign, rest) = match text.as_bytes()[0] {
        b'+' => (1, &text[1..]),
        b'-' => (-1, &text[1..]),
        _ => return None,
    };
    let (hours, minutes) = match rest.len() {
        5 if rest.as_bytes()[2] == b':' => (&rest[0..2], &rest[3..5]),
        4 => (&rest[0..2], &rest[2..4]),
        2 => (rest, "0"),
        _ => return None,
    };
    let hours: i32 = hours.parse().ok()?;
    let minutes: i32 = minutes.parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 60 + minutes))
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01, by Hinnant's era algorithm. Shifting the year to
/// start in March is what keeps the leap rules out of the code: a 400-year era
/// is exactly 146097 days, so every correction collapses into a division.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted = i64::from((month + 9) % 12);
    let day_of_year = (153 * shifted + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::git::test_support::{PINS, pin_repo_config};

    /// The calendar the timestamps rest on, walked a day at a time.
    ///
    /// `days_from_civil` and `days_in_month` are pure arithmetic that nothing
    /// called in a test, and every date this crate prints goes through them.
    /// Hinnant's algorithm is correct as written; what it is easy to do is
    /// transcribe one constant wrong, and a wrong `146097` or `719468` puts
    /// every commit date out by a fixed amount that reads as plausible.
    ///
    /// Walking is the check that needs no second implementation to trust: step
    /// one day at a time with `days_in_month`, count as you go, and require
    /// `days_from_civil` to agree at every step. The two can only agree across
    /// two centuries if both are right — a leap rule wrong in either drifts
    /// them apart within four years and never recovers.
    #[test]
    fn the_civil_calendar_and_the_day_count_agree_for_two_centuries() {
        // The anchors first, so a walk that is merely self-consistent cannot
        // pass: these are the epoch, the century leap year that trips the
        // naive rule, and a recent leap day.
        assert_eq!(days_from_civil(1970, 1, 1), 0, "the epoch is day zero");
        assert_eq!(days_from_civil(1969, 12, 31), -1, "and the day before it");
        assert_eq!(
            days_in_month(2000, 2),
            29,
            "2000 is a leap year: divisible by 400"
        );
        assert_eq!(
            days_in_month(1900, 2),
            28,
            "1900 is not: divisible by 100, not 400"
        );
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2023, 2), 28);

        let (mut y, mut m, mut d) = (1900i64, 1u32, 1u32);
        let mut counted = days_from_civil(1900, 1, 1);
        let mut leap_days = 0usize;
        while y < 2100 {
            assert_eq!(
                days_from_civil(y, m, d),
                counted,
                "the two disagree at {y:04}-{m:02}-{d:02}"
            );
            if m == 2 && d == 29 {
                leap_days += 1;
            }
            let last = days_in_month(y, m);
            assert!((28..=31).contains(&last), "{y:04}-{m:02} has {last} days");
            d += 1;
            counted += 1;
            if d > last {
                d = 1;
                m += 1;
            }
            if m > 12 {
                m = 1;
                y += 1;
            }
        }
        // 1900 and 2000 are the two century years in range; only 2000 is a
        // leap year, so 200 years hold 49 + 1 = 48 ordinary leap years plus
        // 2000 itself.
        assert_eq!(leap_days, 49, "two centuries hold 49 leap days here");
        assert_eq!(
            days_in_month(2024, 13),
            0,
            "a month out of range has no days"
        );
        assert_eq!(days_in_month(2024, 0), 0);
    }

    /// The two calendars in this crate are inverses, so make them prove it.
    ///
    /// `civil_from_days` lives in `core::crash` — it dates the panic log —
    /// and `days_from_civil` lives here. They were written separately, they
    /// are each other's inverse, and each was checked on its own: this one by
    /// the walk above, that one against four known dates. Four dates leave
    /// the century years and everything before 1970 unexercised, and that is
    /// where the era arithmetic earns its keep.
    ///
    /// Round-tripping them is worth more than either check alone. Two
    /// separately written implementations can only agree on seventy-three
    /// thousand consecutive days if both are right; a single transcription
    /// slip in either shows up within one era.
    #[test]
    fn the_two_calendars_round_trip_through_each_other() {
        use crate::core::crash::civil_from_days;

        let (mut y, mut m, mut d) = (1900i64, 1u32, 1u32);
        let mut days = days_from_civil(1900, 1, 1);
        assert!(
            days < 0,
            "the walk has to start before the epoch to exercise it"
        );
        let mut checked = 0usize;
        while y < 2100 {
            assert_eq!(
                civil_from_days(days),
                (y, m, d),
                "day {days} should read as {y:04}-{m:02}-{d:02}"
            );
            assert_eq!(
                days_from_civil(y, m, d),
                days,
                "{y:04}-{m:02}-{d:02} should count as day {days}"
            );
            checked += 1;
            let last = days_in_month(y, m);
            d += 1;
            days += 1;
            if d > last {
                d = 1;
                m += 1;
            }
            if m > 12 {
                m = 1;
                y += 1;
            }
        }
        // 200 x 365 + 49 leap days: every fourth year from 1904 to 2096, less
        // 1900, plus 2000. The same 49 the walk above counts.
        assert_eq!(
            checked, 73_049,
            "two centuries of days, 1900-01-01 to 2099-12-31"
        );
    }

    fn commit(sha: &str, parents: &[&str]) -> (Oid, SmallVec<[Oid; 2]>) {
        (
            sha.to_string(),
            parents.iter().map(|p| p.to_string()).collect(),
        )
    }

    fn lay_out(page: &[(Oid, SmallVec<[Oid; 2]>)]) -> Vec<GraphRow> {
        let mut rows = Vec::new();
        LaneAlloc::new().push(page, &mut rows);
        rows
    }

    fn pass_at(lane: Lane) -> Edge {
        Edge::Pass { lane, color: lane }
    }

    fn in_at(lane: Lane) -> Edge {
        Edge::In {
            from: lane,
            color: lane,
        }
    }

    fn out_at(lane: Lane) -> Edge {
        Edge::Out {
            to: lane,
            color: lane,
        }
    }

    /// Lanes crossing the row's top edge, sorted.
    fn top(row: &GraphRow) -> Vec<Lane> {
        let mut lanes: Vec<Lane> = row
            .edges
            .iter()
            .filter_map(|e| match *e {
                Edge::Pass { lane, .. } => Some(lane),
                Edge::In { from, .. } => Some(from),
                Edge::Out { .. } => None,
            })
            .collect();
        lanes.sort_unstable();
        lanes
    }

    /// Lanes crossing the row's bottom edge, sorted and folded.
    ///
    /// Folded, because one lane can legally carry a `Pass` *and* an `Out`: a
    /// merge whose second parent already has a lane reserved by another child
    /// sends its `Out` onto that lane, joining the line rather than opening a
    /// second one. Below the row that is a single line in a single colour (an
    /// `Out`'s colour is its lane), so the cut sees one line — which the
    /// assertions below verify before folding.
    fn bottom(row: &GraphRow) -> Vec<Lane> {
        let mut lanes: Vec<Lane> = row
            .edges
            .iter()
            .filter_map(|e| match *e {
                Edge::Pass { lane, .. } => Some(lane),
                Edge::Out { to, .. } => Some(to),
                Edge::In { .. } => None,
            })
            .collect();
        lanes.sort_unstable();
        lanes.dedup();
        lanes
    }

    /// The property the whole layout rests on: at any horizontal cut through
    /// the graph a lane carries at most one visible line, and what leaves a
    /// row's bottom is exactly what enters the next row's top. Together those
    /// two mean colour-by-lane can never put two visible lines in one colour.
    ///
    /// "Visible" carries the one nuance: an `Out` may land on a lane a `Pass`
    /// already crosses — a join, see [`bottom`] — and that pair is one line.
    /// Two `Pass`es or two `Out`s on one lane are still bugs.
    struct Lcg(u64);
    impl Lcg {
        fn n(&mut self, m: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % m.max(1)
        }
    }

    /// A topo-ordered DAG, newest first: commit i may only name parents j > i,
    /// which is exactly the guarantee `--topo-order` gives `push`.
    fn dag(seed: u64, n: usize, max_parents: usize) -> Vec<(Oid, SmallVec<[Oid; 2]>)> {
        let mut r = Lcg(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
        let mut page = Vec::with_capacity(n);
        for i in 0..n {
            let remaining = n - i - 1;
            let want = if remaining == 0 {
                0
            } else {
                1 + r.n(max_parents)
            };
            let mut ps: SmallVec<[Oid; 2]> = SmallVec::new();
            for _ in 0..want.min(remaining) {
                let p = i + 1 + r.n(remaining);
                let id = format!("c{p}");
                if !ps.contains(&id) {
                    ps.push(id);
                }
            }
            // Some commits are genuine roots even mid-page.
            if r.n(12) == 0 {
                ps.clear();
            }
            page.push((format!("c{i}"), ps));
        }
        page
    }

    #[test]
    fn random_dags_lay_out_consistently() {
        for seed in 0..4000u64 {
            let n = 2 + (seed as usize % 40);
            let maxp = 1 + (seed as usize % 4); // up to octopus merges
            let page = dag(seed, n, maxp);
            let rows = lay_out(&page);
            assert_eq!(rows.len(), page.len(), "seed {seed}: one row per commit");
            for (i, (row, (_, parents))) in rows.iter().zip(page.iter()).enumerate() {
                assert_eq!(
                    row.parents as usize,
                    parents.len().min(u8::MAX as usize),
                    "seed {seed} row {i}: parent count"
                );
                let outs = row
                    .edges
                    .iter()
                    .filter(|e| matches!(e, Edge::Out { .. }))
                    .count();
                if parents.is_empty() {
                    assert_eq!(outs, 0, "seed {seed} row {i}: a root must not send an Out");
                }
                assert!(
                    !row.edges
                        .iter()
                        .any(|e| matches!(*e, Edge::Pass { lane, .. } if lane == row.node)),
                    "seed {seed} row {i}: a Pass crosses the row's own node lane: {row:?}"
                );
            }
            assert_lanes_line_up(&rows);
        }
    }

    /// The same, but split across page boundaries: the allocator has to hand
    /// its state from one page to the next without a seam.
    #[test]
    fn a_dag_split_into_pages_lays_out_the_same_as_one_page() {
        for seed in 0..1500u64 {
            let n = 6 + (seed as usize % 30);
            let page = dag(seed, n, 1 + (seed as usize % 3));
            let whole = lay_out(&page);
            let cut = 1 + (seed as usize % (n - 1));
            let mut split = Vec::new();
            let mut alloc = LaneAlloc::new();
            alloc.push(&page[..cut], &mut split);
            alloc.push(&page[cut..], &mut split);
            assert_eq!(
                whole, split,
                "seed {seed}: page boundary at {cut} changed the layout"
            );
            assert_lanes_line_up(&split);
        }
    }

    fn assert_lanes_line_up(rows: &[GraphRow]) {
        for (i, row) in rows.iter().enumerate() {
            let once = |mut lanes: Vec<Lane>| {
                lanes.sort_unstable();
                let len = lanes.len();
                lanes.dedup();
                assert_eq!(lanes.len(), len, "row {i} doubles up a lane: {row:?}");
            };
            once(top(row));
            once(
                row.edges
                    .iter()
                    .filter_map(|e| match *e {
                        Edge::Pass { lane, .. } => Some(lane),
                        _ => None,
                    })
                    .collect(),
            );
            once(
                row.edges
                    .iter()
                    .filter_map(|e| match *e {
                        Edge::Out { to, .. } => Some(to),
                        _ => None,
                    })
                    .collect(),
            );
        }
        for (i, pair) in rows.windows(2).enumerate() {
            assert_eq!(
                bottom(&pair[0]),
                top(&pair[1]),
                "row {i} does not hand its lanes to row {}",
                i + 1
            );
        }
    }

    #[test]
    fn a_linear_chain_stays_in_one_lane() {
        let page = [commit("a", &["b"]), commit("b", &["c"]), commit("c", &[])];
        let rows = lay_out(&page);

        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.node == 0 && r.color == 0));
        assert_eq!(rows[0].edges.as_slice(), [out_at(0)]);
        assert_eq!(rows[1].edges.as_slice(), [in_at(0), out_at(0)]);
        assert_eq!(rows[2].edges.as_slice(), [in_at(0)]);
        assert_eq!(rows[2].parents, 0, "the last one is a root");
        assert_lanes_line_up(&rows);
    }

    #[test]
    fn a_fork_and_a_merge_open_and_close_one_lane() {
        let page = [
            commit("m", &["a", "b"]),
            commit("a", &["base"]),
            commit("b", &["base"]),
            commit("base", &[]),
        ];
        let rows = lay_out(&page);

        assert_eq!(rows[0].node, 0);
        assert_eq!(rows[0].parents, 2);
        assert_eq!(
            rows[0].edges.as_slice(),
            [out_at(0), out_at(1)],
            "the merge leaves on its own lane and on a fresh one"
        );
        assert_eq!(rows[1].edges.as_slice(), [pass_at(1), in_at(0), out_at(0)]);
        assert_eq!(rows[2].node, 1, "the second parent kept the lane it opened");
        assert_eq!(rows[3].node, 0);
        assert_eq!(
            rows[3].edges.as_slice(),
            [in_at(0), in_at(1)],
            "both sides come back together at the base"
        );
        assert_eq!(rows[3].parents, 0);
        assert_lanes_line_up(&rows);
    }

    #[test]
    fn an_octopus_merge_leaves_on_one_lane_per_parent() {
        let page = [
            commit("m", &["p1", "p2", "p3"]),
            commit("p1", &[]),
            commit("p2", &[]),
            commit("p3", &[]),
        ];
        let rows = lay_out(&page);

        assert_eq!(rows[0].parents, 3);
        assert_eq!(rows[0].edges.as_slice(), [out_at(0), out_at(1), out_at(2)]);
        assert_eq!(
            rows.iter().map(|r| r.node).collect::<Vec<_>>(),
            [0, 0, 1, 2]
        );
        assert_lanes_line_up(&rows);
    }

    #[test]
    fn a_parent_outside_the_window_leaves_its_lane_open() {
        let page = [commit("a", &["b"])];
        let mut alloc = LaneAlloc::new();
        let mut rows = Vec::new();
        alloc.push(&page, &mut rows);

        assert_eq!(alloc.open_lanes(), [0], "b is below the page boundary");
        assert_eq!(alloc.width(), 1);
        assert!(!alloc.truncated());
    }

    #[test]
    fn two_independent_roots_end_their_own_lanes() {
        let page = [
            commit("a", &["a1"]),
            commit("b", &["b1"]),
            commit("a1", &[]),
            commit("b1", &[]),
        ];
        let mut alloc = LaneAlloc::new();
        let mut rows = Vec::new();
        alloc.push(&page, &mut rows);

        assert_eq!(
            rows.iter().map(|r| r.node).collect::<Vec<_>>(),
            [0, 1, 0, 1],
            "the two histories never share a lane"
        );
        assert_eq!(rows[2].parents, 0);
        assert_eq!(rows[3].parents, 0);
        assert!(
            alloc.open_lanes().is_empty(),
            "both lanes died at their root"
        );
        assert_lanes_line_up(&rows);
    }

    #[test]
    fn a_dead_lane_is_reused_without_two_live_lines_sharing_it() {
        let page = [
            commit("a", &["a1"]),
            commit("b", &["b1"]),
            commit("b1", &[]),
            commit("c", &["c1"]),
        ];
        let rows = lay_out(&page);

        assert_eq!(rows[1].node, 1);
        assert_eq!(rows[3].node, 1, "the freed lane was handed to the new tip");
        assert!(
            !bottom(&rows[2]).contains(&1),
            "the old line is gone from the band before the new one starts"
        );
        assert!(!top(&rows[3]).contains(&1), "the new tip has nothing above");
        assert_lanes_line_up(&rows);
    }

    #[test]
    fn splitting_a_page_in_two_lays_out_identically() {
        let page = [
            commit("m", &["a", "b"]),
            commit("a", &["base"]),
            commit("b", &["base"]),
            commit("t", &["q"]),
            commit("base", &["p"]),
            commit("p", &["q"]),
            commit("q", &[]),
        ];

        let whole = lay_out(&page);

        let mut alloc = LaneAlloc::new();
        let mut paged = Vec::new();
        alloc.push(&page[..3], &mut paged);
        let first_page = paged.clone();
        alloc.push(&page[3..], &mut paged);

        assert_eq!(first_page, whole[..3], "the first page is a prefix");
        assert_eq!(paged, whole, "and loading more never re-flows it");
        assert_lanes_line_up(&whole);
    }

    #[test]
    fn a_second_parent_avoids_the_lane_this_row_just_freed() {
        let page = [
            commit("t0", &["c"]),
            commit("t1", &["x"]),
            commit("t2", &["c"]),
            commit("c", &["p0", "p1"]),
        ];
        let rows = lay_out(&page);

        let merge = &rows[3];
        assert_eq!(merge.node, 0);
        assert_eq!(top(merge), [0, 1, 2], "two lines land here, one passes by");
        assert!(
            !merge.edges.contains(&out_at(2)),
            "lane 2 just ended here; leaving on it would draw a V: {merge:?}"
        );
        assert!(merge.edges.contains(&out_at(3)));
        assert_lanes_line_up(&rows);
    }

    /// The commonest merge topology of all: "merge main into topic after main
    /// advanced". The merge's second parent (`c`) already has a lane reserved
    /// by another child (`x`), so the merge's `Out` *joins* that lane instead
    /// of opening a second one to the same commit — the row legally carries a
    /// `Pass` and an `Out` on lane 0, one line below the cut, not two.
    #[test]
    fn a_second_parent_joins_a_line_another_child_opened() {
        let page = [
            commit("x", &["c"]),
            commit("m", &["a", "c"]),
            commit("a", &["c"]),
            commit("c", &[]),
        ];
        let rows = lay_out(&page);

        assert_eq!(rows[0].edges.as_slice(), [out_at(0)]);
        let merge = &rows[1];
        assert_eq!(merge.node, 1, "the merge tips a lane of its own");
        assert_eq!(
            merge.edges.as_slice(),
            [pass_at(0), out_at(1), out_at(0)],
            "first parent inherits the node's lane; the second joins lane 0"
        );
        assert_eq!(
            rows[3].edges.as_slice(),
            [in_at(0), in_at(1)],
            "both lines still converge on the shared parent"
        );
        assert_lanes_line_up(&rows);
    }

    #[test]
    fn more_parents_than_lanes_truncates_instead_of_panicking() {
        let parents: Vec<String> = (0..40).map(|i| format!("p{i}")).collect();
        let refs: Vec<&str> = parents.iter().map(String::as_str).collect();
        let mut page = vec![commit("m", &refs)];
        page.extend(parents.iter().map(|p| commit(p, &[])));

        let mut alloc = LaneAlloc::new();
        let mut rows = Vec::new();
        alloc.push(&page, &mut rows);

        assert!(alloc.truncated());
        assert_eq!(rows[0].parents, 40);
        assert!(
            rows.iter().all(|r| r.node < MAX_LANES),
            "every row stayed inside the lane budget"
        );
        assert!(rows.iter().flat_map(bottom).all(|lane| lane < MAX_LANES));
    }

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const SHA_C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    fn record(fields: &[&str]) -> String {
        format!("\x1e{}", fields.join("\x1f"))
    }

    fn one(oid: &str, parents: &str, deco: &str, subject: &str, body: &str) -> String {
        record(&[
            oid,
            parents,
            "Ada",
            "ada@example.com",
            "2026-08-09T14:03:11+08:00",
            "Grace",
            "grace@example.com",
            "2026-08-09T15:00:00+08:00",
            deco,
            subject,
            body,
        ])
    }

    /// A parse that could not read everything must say so — `load_page` turns
    /// `truncated` into `complete: false`, and a truncated graph that claimed
    /// to be the end of history would freeze paging on it forever.
    #[test]
    fn a_stream_the_parse_cannot_finish_is_never_called_complete() {
        // One record past MAX_RECORD: dropped whole by the splitter.
        let huge_body = "x".repeat(super::super::MAX_RECORD + 1);
        let stream = [
            one(SHA_A, SHA_B, "", "kept", ""),
            one(SHA_B, "", "", "monster", &huge_body),
        ]
        .concat();
        let parsed = parse_log(stream.as_bytes());
        assert_eq!(parsed.commits.len(), 1, "the readable record survives");
        assert!(parsed.truncated);

        // Cumulative bytes past MAX_LOG_BYTES: the tail is clipped.
        let body = "y".repeat(512 * 1024);
        let stream: String = (0..40)
            .map(|i| one(&format!("{i:040}"), "", "", "big", &body))
            .collect();
        let parsed = parse_log(stream.as_bytes());
        assert!(parsed.commits.len() < 40);
        assert!(parsed.truncated);

        let parsed = parse_log(one(SHA_A, "", "", "small", "fine").as_bytes());
        assert!(!parsed.truncated, "an ordinary stream is read in full");
    }

    #[test]
    fn a_multi_line_body_survives_the_record_split() {
        let stream = [
            one(
                SHA_A,
                SHA_B,
                "",
                "first",
                "line one\nline two\n\nline four\n",
            ),
            one(SHA_B, "", "", "second", ""),
        ]
        .join("\n");
        let commits = parse_log(stream.as_bytes()).commits;

        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].summary, "first");
        assert_eq!(commits[0].body, "line one\nline two\n\nline four");
        assert_eq!(commits[0].author.name, "Ada");
        assert_eq!(commits[0].committer.email, "grace@example.com");
        assert_eq!(commits[1].body, "");
        assert_eq!(commits[1].parents.len(), 0);
        assert!(!commits[0].is_merge());
    }

    #[test]
    fn a_merge_records_both_parents() {
        let stream = one(SHA_A, &format!("{SHA_B} {SHA_C}"), "", "merge", "");
        let commits = parse_log(stream.as_bytes()).commits;

        assert_eq!(commits[0].parents.as_slice(), [SHA_B, SHA_C]);
        assert!(commits[0].is_merge());
        assert_eq!(commits[0].short(), "aaaaaaa");
    }

    #[test]
    fn decorations_map_to_their_ref_kinds() {
        let deco = "HEAD -> refs/heads/main, refs/remotes/origin/main, tag: refs/tags/v1.0";
        let stream = one(SHA_A, "", deco, "subject", "");
        let refs = parse_log(stream.as_bytes()).commits.remove(0).refs;

        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].kind, RefKind::LocalBranch);
        assert_eq!(refs[0].short, "main");
        assert_eq!(refs[0].full, "refs/heads/main");
        assert!(refs[0].is_head);
        assert_eq!(refs[1].kind, RefKind::RemoteBranch);
        assert_eq!(refs[1].short, "origin/main");
        assert!(!refs[1].is_head);
        assert_eq!(refs[2].kind, RefKind::Tag);
        assert_eq!(refs[2].short, "v1.0");

        let detached = one(SHA_A, "", "HEAD, refs/tags/v2", "subject", "");
        let refs = parse_log(detached.as_bytes()).commits.remove(0).refs;
        assert_eq!(refs[0].kind, RefKind::Head);
        assert!(refs[0].is_head);
    }

    #[test]
    fn a_unit_separator_inside_a_body_does_not_shift_fields() {
        let body = "before\x1fafter\x1fand\x1fmore";
        let stream = one(SHA_A, "", "", "subject", body);
        let commits = parse_log(stream.as_bytes()).commits;

        assert_eq!(
            commits[0].summary, "subject",
            "the subject is still field 10"
        );
        assert_eq!(commits[0].body, body, "the body swallowed the extra ones");
    }

    #[test]
    fn a_record_that_does_not_start_with_a_sha_is_dropped() {
        let stream = [
            record(&["not a sha", "", "who", "", "", "", "", "", "", "junk", ""]),
            one(SHA_A, "", "", "real", ""),
            record(&[SHA_B, "only two fields"]),
        ]
        .join("\n");
        let commits = parse_log(stream.as_bytes()).commits;

        assert_eq!(commits.len(), 1, "{commits:?}");
        assert_eq!(commits[0].summary, "real");
    }

    #[test]
    fn iso_8601_offsets_and_leap_days_parse() {
        assert_eq!(
            parse_iso8601("1970-01-01T00:00:00Z"),
            Some(OffsetTs {
                unix: 0,
                offset_minutes: 0
            })
        );
        assert_eq!(
            parse_iso8601("1970-01-01T00:00:00+00:00"),
            Some(OffsetTs {
                unix: 0,
                offset_minutes: 0
            })
        );
        // Same instant, written from two sides of the planet.
        assert_eq!(
            parse_iso8601("2026-08-09T14:03:11+08:00"),
            Some(OffsetTs {
                unix: 1_786_255_391,
                offset_minutes: 480
            })
        );
        assert_eq!(
            parse_iso8601("2026-08-09T02:03:11-04:00"),
            Some(OffsetTs {
                unix: 1_786_255_391,
                offset_minutes: -240
            })
        );
        assert_eq!(
            parse_iso8601("2026-08-09T11:33:11+05:30").map(|t| t.unix),
            Some(1_786_255_391)
        );
        assert_eq!(
            parse_iso8601("2024-02-29T00:00:00Z").map(|t| t.unix),
            Some(1_709_164_800),
            "2024 is a leap year"
        );
        assert_eq!(
            parse_iso8601("2000-02-29T00:00:00Z").map(|t| t.unix),
            Some(951_782_400),
            "and so is 2000, the four-hundred-year exception"
        );
        assert_eq!(parse_iso8601("2023-02-29T00:00:00Z"), None);
        assert_eq!(parse_iso8601("1900-02-29T00:00:00Z"), None);
        assert_eq!(parse_iso8601("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso8601("2026-08-09T24:00:00Z"), None);
        assert_eq!(parse_iso8601("nope"), None);
        assert_eq!(parse_iso8601("2026-08-09T14:03:11 08:00"), None);
    }

    #[test]
    fn an_oversized_subject_and_body_are_cut_on_a_char_boundary() {
        let subject = "提".repeat(400);
        let body = "交".repeat(4000);
        let stream = one(SHA_A, "", "", &subject, &body);
        let commit = parse_log(stream.as_bytes()).commits.remove(0);

        assert_eq!(
            commit.summary.len(),
            MAX_SUBJECT_BYTES - MAX_SUBJECT_BYTES % 3,
            "cut back to the last whole character"
        );
        assert!(commit.summary.chars().all(|c| c == '提'));
        assert_eq!(commit.body.len(), MAX_BODY_BYTES - MAX_BODY_BYTES % 3);
        assert!(commit.body.chars().all(|c| c == '交'));
    }

    #[test]
    fn an_annotated_tag_lands_on_the_commit_not_the_tag_object() {
        let tag_object = "1111111111111111111111111111111111111111";
        let lines = [
            format!(
                "{SHA_A}\x1frefs/heads/main\x1fmain\x1frefs/remotes/origin/main\x1f*\x1fcommit\x1f"
            ),
            format!("{tag_object}\x1frefs/tags/v9\x1fv9\x1f\x1f \x1ftag\x1f{SHA_B}"),
            format!("{SHA_C}\x1frefs/remotes/origin/dev\x1forigin/dev\x1f\x1f \x1fcommit\x1f"),
        ];
        let by_oid = parse_refs(lines.join("\n").as_bytes());

        assert_eq!(by_oid[SHA_A][0].kind, RefKind::LocalBranch);
        assert!(by_oid[SHA_A][0].is_head, "the `*` column marks HEAD");
        assert!(
            !by_oid.contains_key(tag_object),
            "the tag object itself is never a graph row"
        );
        assert_eq!(by_oid[SHA_B][0].kind, RefKind::Tag);
        assert_eq!(by_oid[SHA_B][0].short, "v9");
        assert_eq!(by_oid[SHA_C][0].short, "origin/dev");
    }

    #[test]
    fn a_branch_keeps_the_upstream_it_tracks() {
        let lines = [
            format!(
                "{SHA_A}\x1frefs/heads/main\x1fmain\x1frefs/remotes/origin/main\x1f*\x1fcommit\x1f"
            ),
            // A branch nobody has published tracks nothing, and an empty
            // `%(upstream)` has to stay `None` rather than become `Some("")`.
            format!("{SHA_B}\x1frefs/heads/local-only\x1flocal-only\x1f\x1f \x1fcommit\x1f"),
            format!("{SHA_C}\x1frefs/tags/v9\x1fv9\x1f\x1f \x1fcommit\x1f"),
        ];
        let by_oid = parse_refs(lines.join("\n").as_bytes());

        assert_eq!(
            by_oid[SHA_A][0].upstream.as_deref(),
            Some("refs/remotes/origin/main")
        );
        assert_eq!(by_oid[SHA_B][0].upstream, None);
        assert_eq!(by_oid[SHA_C][0].upstream, None);
        // `%D` cannot carry an upstream at all, so a decoration parsed out of
        // a log record must not claim one.
        let logged = parse_log(one(SHA_A, "", "refs/heads/main", "s", "").as_bytes()).commits;
        assert_eq!(logged[0].refs[0].upstream, None);
    }

    #[test]
    fn branch_names_come_back_one_per_line() {
        let out = b"main\nfeature/a\n\n  spaced  \n";
        assert_eq!(
            parse_branch_names(out),
            ["main", "feature/a", "spaced"],
            "blank lines are not branches, and git pads nothing"
        );
        assert!(parse_branch_names(b"").is_empty());
        let many: String = (0..MAX_REFS + 50)
            .map(|i| format!("b{i}\n"))
            .collect::<Vec<_>>()
            .concat();
        assert_eq!(parse_branch_names(many.as_bytes()).len(), MAX_REFS);
    }

    /// Every shape `-z` can produce, from the streams git actually emits —
    /// each of these was captured from git 2.50.1 rather than guessed.
    #[test]
    fn a_commits_file_list_joins_the_two_z_streams() {
        // A rename, a path with a space, a path outside ASCII, and a binary.
        let numstat = b"1\t0\tbin.dat\x000\t0\t\x00a.txt\x00renamed.txt\x001\t0\twith space.txt\x002\t3\t\xe4\xb8\xad\xe6\x96\x87\xe5\x90\x8d.txt\x00";
        let name_status = b"A\x00bin.dat\x00R100\x00a.txt\x00renamed.txt\x00A\x00with space.txt\x00M\x00\xe4\xb8\xad\xe6\x96\x87\xe5\x90\x8d.txt\x00";
        let files = join_commit_files(numstat, name_status);

        assert_eq!(files.len(), 4, "{files:?}");
        assert_eq!(files[0].path, "bin.dat");
        assert_eq!(files[0].status, FileStatus::Added);
        assert_eq!((files[0].added, files[0].removed), (Some(1), Some(0)));

        // The rename: `--numstat` spells it as an empty third field followed
        // by two records of its own, and the counts belong to the new name.
        assert_eq!(files[1].path, "renamed.txt");
        assert_eq!(files[1].orig_path.as_deref(), Some("a.txt"));
        assert_eq!(files[1].status, FileStatus::Renamed);
        assert_eq!((files[1].added, files[1].removed), (Some(0), Some(0)));

        assert_eq!(
            files[2].path, "with space.txt",
            "a space is not a separator"
        );
        assert_eq!(files[3].path, "中文名.txt");
        assert_eq!((files[3].added, files[3].removed), (Some(2), Some(3)));
        assert!(files.iter().all(|f| !f.binary));
    }

    #[test]
    fn a_binary_file_reports_no_counts_rather_than_zero() {
        let files = join_commit_files(b"-\t-\tbin2.dat\x00", b"A\x00bin2.dat\x00");
        assert_eq!(files.len(), 1);
        assert!(files[0].binary);
        assert_eq!(
            (files[0].added, files[0].removed),
            (None, None),
            "`0 0` is a real answer and `-\t-` is not, so they must not read alike"
        );
        // A pure rename really does change nothing, and says so.
        let renamed = join_commit_files(b"0\t0\t\x00a\x00b\x00", b"R100\x00a\x00b\x00");
        assert!(!renamed[0].binary);
        assert_eq!((renamed[0].added, renamed[0].removed), (Some(0), Some(0)));
    }

    #[test]
    fn one_stream_going_missing_degrades_instead_of_emptying_the_list() {
        // No counts: every row still knows what happened to it.
        let no_numstat = join_commit_files(b"", b"M\x00a.txt\x00D\x00b.txt\x00");
        assert_eq!(no_numstat.len(), 2);
        assert_eq!(no_numstat[1].status, FileStatus::Deleted);
        assert!(no_numstat.iter().all(|f| f.added.is_none()));

        // No letters: the paths are the more important half, so they are kept
        // and given the one status that claims the least.
        let no_names = join_commit_files(b"1\t2\tb.txt\x003\t4\ta.txt\x00", b"");
        assert_eq!(
            no_names.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            ["a.txt", "b.txt"],
            "with no order to inherit the leftovers are sorted, not shuffled"
        );
        assert!(no_names.iter().all(|f| f.status == FileStatus::Modified));
        assert_eq!((no_names[0].added, no_names[0].removed), (Some(3), Some(4)));

        assert!(join_commit_files(b"", b"").is_empty());
    }

    #[test]
    fn a_truncated_or_unknown_record_is_dropped_rather_than_shifting_the_parse() {
        // A status letter with no path behind it ends the read; anything
        // already parsed still stands.
        let cut = join_commit_files(b"", b"M\x00a.txt\x00D\x00");
        assert_eq!(cut.len(), 1);
        assert_eq!(cut[0].path, "a.txt");

        // `X` is git's own "something went wrong". Its path is consumed so the
        // records after it stay aligned.
        let unknown = join_commit_files(b"", b"X\x00weird\x00A\x00good.txt\x00");
        assert_eq!(unknown.len(), 1);
        assert_eq!(unknown[0].path, "good.txt");

        // A rename cut off after its old name leaves nothing to attach to.
        assert!(join_commit_files(b"0\t0\t\x00a\x00", b"R100\x00a\x00").is_empty());
    }

    #[test]
    fn a_file_list_is_capped_without_losing_its_first_rows() {
        let mut numstat = Vec::new();
        let mut name_status = Vec::new();
        for i in 0..MAX_COMMIT_FILES + 20 {
            numstat.extend_from_slice(format!("1\t0\tf{i:05}.rs\0").as_bytes());
            name_status.extend_from_slice(format!("A\0f{i:05}.rs\0").as_bytes());
        }
        let files = join_commit_files(&numstat, &name_status);
        assert_eq!(files.len(), MAX_COMMIT_FILES);
        assert_eq!(files[0].path, "f00000.rs");
    }

    #[test]
    fn a_rev_that_could_be_read_as_an_option_never_reaches_git() {
        assert!(is_rev("HEAD"));
        assert!(is_rev(SHA_A));
        assert!(is_rev("v1.0^{commit}"));
        assert!(!is_rev(""));
        assert!(!is_rev("--upload-pack=touch /tmp/pwned"));
        assert!(!is_rev("-n"));
        assert!(!is_rev("HEAD\nrm -rf"));
    }

    // ----- against a real repository -------------------------------------
    //
    // Both of these build the history they assert on rather than reading the
    // tty7 checkout they were compiled in. CI clones shallow (one commit) and
    // checks a pull request out as a detached HEAD with no local branch, so
    // "how deep is the history" and "is there a branch" are facts about the
    // runner, not about this code — and a source tarball has no repository at
    // all. Owning the fixture is what lets the counts below be exact.

    struct Scratch(std::path::PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> Option<Scratch> {
        // The pid keeps two concurrent `cargo test` runs off each other's
        // fixture, since the directory is wiped on the way in.
        let dir = std::env::temp_dir().join(format!("tty7-scm-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        Some(Scratch(dir))
    }

    /// Runs git with the identity, signing and line-ending settings pinned: a
    /// CI runner has no `user.name` at all and would refuse to commit, a
    /// developer may have signing turned on globally, and Git for Windows has
    /// `core.autocrlf=true` in its system config.
    fn run(host: &dyn Host, cwd: &Path, args: &[&str]) -> bool {
        let mut full = PINS.to_vec();
        full.extend_from_slice(args);
        host.git(cwd, &full).map(|o| o.success()).unwrap_or(false)
    }

    /// `git init` on a branch named here rather than left to whatever
    /// `init.defaultBranch` says, with the same pins written into the
    /// repository so the code under test reads them too. `false` means there is
    /// no git on this machine, which is not a failure.
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

    fn commit_file(host: &dyn Host, repo: &Path, path: &str, body: &str, message: &str) {
        std::fs::write(repo.join(path), body).unwrap();
        assert!(run(host, repo, &["add", "--", path]));
        assert!(run(host, repo, &["commit", "--quiet", "-m", message]));
    }

    /// The other half of `a_stream_the_parse_cannot_finish_is_never_called_complete`,
    /// which asserted only that the *parse* raises the flag. What the flag is
    /// for is `load_page`, and both halves of its test are needed: git answered
    /// with fewer commits than asked for, which on its own reads as the end of
    /// history, *and* a record the parse had to drop. Taking the count alone
    /// would call a graph missing its middle complete, and paging would stop
    /// there for good — the history below it unreachable, with nothing on
    /// screen to say so.
    #[test]
    fn a_page_the_parse_had_to_cut_is_not_the_end_of_history() {
        let host = crate::host::local::LocalHost::new();
        let Some(scratch) = scratch("cut-page") else {
            return;
        };
        let repo = scratch.0.as_path();
        if !init_repo(&*host, repo) {
            return;
        }
        commit_file(&*host, repo, "a.txt", "1\n", "first");

        // A commit message past MAX_RECORD, which the splitter drops whole.
        // Written through a file: an argument this size is over the exec limit
        // on macOS, and would fail as a bad command rather than a big commit.
        let huge = repo.join("msg.txt");
        std::fs::write(&huge, "z".repeat(super::super::MAX_RECORD + 1)).unwrap();
        std::fs::write(repo.join("b.txt"), "2\n").unwrap();
        assert!(run(&*host, repo, &["add", "--", "b.txt"]));
        assert!(run(
            &*host,
            repo,
            &["commit", "--quiet", "-F", huge.to_str().unwrap()]
        ));

        // Thirty asked for and two exist, so the count alone says complete.
        let page =
            load_page(&*host, repo, &GraphScope::Head, 30).expect("a repository was just created");
        assert_eq!(
            page.commits.len(),
            1,
            "the record over the bound is dropped, the readable one survives"
        );
        assert!(
            !page.complete,
            "a page the parse had to cut is not the end of history, however few commits came back"
        );
    }

    #[test]
    fn a_real_repository_answers_for_one_commit_and_its_files() {
        let host = crate::host::local::LocalHost::new();
        let Some(scratch) = scratch("one-commit") else {
            return;
        };
        let repo = &scratch.0;
        if !init_repo(&*host, repo) {
            return; // no git on this machine
        }

        std::fs::write(repo.join("kept.txt"), "one\n").unwrap();
        std::fs::write(repo.join("moved.txt"), "a\nb\nc\nd\ne\nf\ng\nh\n").unwrap();
        std::fs::write(repo.join("gone.txt"), "bye\n").unwrap();
        assert!(run(&*host, repo, &["add", "--", "."]));
        assert!(run(&*host, repo, &["commit", "--quiet", "-m", "base"]));

        // One commit with four different things in it, because the join of
        // `--numstat` and `--name-status` is only exercised by a commit that
        // touches more than one path, and the rename is the record whose two
        // halves are shaped differently in the two streams.
        std::fs::write(repo.join("kept.txt"), "one\ntwo\n").unwrap();
        std::fs::write(repo.join("added.txt"), "new\n").unwrap();
        assert!(run(&*host, repo, &["mv", "moved.txt", "renamed.txt"]));
        assert!(run(&*host, repo, &["rm", "--quiet", "--", "gone.txt"]));
        assert!(run(&*host, repo, &["add", "--", "."]));
        assert!(run(
            &*host,
            repo,
            &["commit", "--quiet", "-m", "feat: four ways at once"]
        ));
        // A second branch, so `local_branches` has to return more than the one
        // HEAD happens to be on.
        assert!(run(&*host, repo, &["branch", "side"]));

        let page = load_page(&*host, repo, &GraphScope::Head, 2)
            .expect("a repository was just created here");
        let head = page.commits.first().expect("two commits were just made");

        let shown = load_commit(&*host, repo, &head.oid).expect("HEAD is a commit");
        assert_eq!(shown.oid, head.oid);
        assert_eq!(shown.summary, head.summary, "the two formats are the same");
        assert_eq!(shown.parents.as_slice(), head.parents.as_slice());
        assert_eq!(shown.author.at, head.author.at);
        assert_eq!(load_commit(&*host, repo, "-n"), None);

        let files = commit_files(&*host, repo, &head.oid).expect("HEAD touched something");
        assert!(
            files.iter().all(|f| !f.path.is_empty()),
            "an empty path means the join lost a record: {files:?}"
        );
        let mut got: Vec<(&str, FileStatus)> =
            files.iter().map(|f| (f.path.as_str(), f.status)).collect();
        got.sort_by_key(|(path, _)| *path);
        assert_eq!(
            got,
            [
                ("added.txt", FileStatus::Added),
                ("gone.txt", FileStatus::Deleted),
                ("kept.txt", FileStatus::Modified),
                ("renamed.txt", FileStatus::Renamed),
            ],
            "every path the commit touched, with the letter git gave it: {files:?}"
        );

        fn file<'a>(files: &'a [CommitFile], path: &str) -> &'a CommitFile {
            files.iter().find(|f| f.path == path).unwrap()
        }
        // The whole reason the two commands are run separately: the counts come
        // from `--numstat` and the letters from `--name-status`, so a row that
        // has both is a row the join put back together.
        assert_eq!(
            (
                file(&files, "kept.txt").added,
                file(&files, "kept.txt").removed
            ),
            (Some(1), Some(0))
        );
        assert_eq!(
            (
                file(&files, "gone.txt").added,
                file(&files, "gone.txt").removed
            ),
            (Some(0), Some(1))
        );
        let renamed = file(&files, "renamed.txt");
        assert_eq!(renamed.orig_path.as_deref(), Some("moved.txt"));
        assert_eq!((renamed.added, renamed.removed), (Some(0), Some(0)));
        assert!(files.iter().all(|f| !f.binary));

        let branches = local_branches(&*host, repo);
        assert_eq!(branches, ["main", "side"], "both of them, in refname order");
        assert!(branches.iter().all(|b| !b.starts_with("refs/heads/")));
    }

    /// The lane layout against a history with a shape, not a straight line:
    ///
    /// ```text
    ///   top          main
    ///   merge
    ///   |    \
    ///   main2 side2
    ///   main1 side1
    ///   |    /
    ///   root
    /// ```
    ///
    /// Seven commits, which is also what makes the paging assertion at the end
    /// honest — a page of five cannot be the whole history.
    #[test]
    fn a_real_repository_lays_out_one_row_per_commit() {
        let host = crate::host::local::LocalHost::new();
        let Some(scratch) = scratch("layout") else {
            return;
        };
        let repo = &scratch.0;
        if !init_repo(&*host, repo) {
            return; // no git on this machine
        }

        commit_file(&*host, repo, "root.txt", "0\n", "root");
        assert!(run(&*host, repo, &["checkout", "--quiet", "-b", "side"]));
        commit_file(&*host, repo, "side.txt", "1\n", "side one");
        commit_file(&*host, repo, "side.txt", "2\n", "side two");
        assert!(run(&*host, repo, &["checkout", "--quiet", "main"]));
        commit_file(&*host, repo, "main.txt", "1\n", "main one");
        commit_file(&*host, repo, "main.txt", "2\n", "main two");
        // The two branches touch different files, so this merges clean.
        assert!(run(
            &*host,
            repo,
            &[
                "merge",
                "--quiet",
                "--no-ff",
                "--no-edit",
                "-m",
                "merge side",
                "side"
            ]
        ));
        commit_file(&*host, repo, "main.txt", "3\n", "after the merge");

        let page =
            load_page(&*host, repo, &GraphScope::Head, 30).expect("a repository was just created");

        assert_eq!(page.rows.len(), page.commits.len());
        assert_eq!(
            page.commits.len(),
            7,
            "the root, two on the side branch, two on main, the merge, and the one on top"
        );
        assert!(page.complete, "thirty asked for, seven exist");
        assert!(
            page.max_lanes >= 2,
            "a branch that forks and merges back needs a second lane: {page:?}"
        );
        assert!(!page.truncated_lanes);
        assert!(page.rows.iter().all(|r| r.node < page.max_lanes));
        assert!(page.commits.iter().all(|c| is_hex_oid(&c.oid)));
        assert!(
            page.commits
                .iter()
                .all(|c| c.author.at.unix > 1_600_000_000),
            "every commit got a real timestamp"
        );
        assert_lanes_line_up(&page.rows);

        // Row *i* is commit *i*, which is only worth checking where the two
        // could come apart: the merge is the one commit with two parents, and
        // its row is the one that sends a line to each of them.
        let merges: Vec<usize> = page
            .commits
            .iter()
            .enumerate()
            .filter(|(_, c)| c.parents.len() == 2)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(merges.len(), 1, "one merge in the fixture");
        let row = &page.rows[merges[0]];
        assert_eq!(row.parents, 2, "the row agrees with the commit beside it");
        assert_eq!(
            row.edges
                .iter()
                .filter(|e| matches!(e, Edge::Out { .. }))
                .count(),
            2,
            "the merge row leaves for both parents: {row:?}"
        );

        let page = load_page(&*host, repo, &GraphScope::HeadAndUpstream, 5).unwrap();
        assert_eq!(page.commits.len(), 5);
        assert!(!page.complete, "five of the seven is not the whole history");
    }
}

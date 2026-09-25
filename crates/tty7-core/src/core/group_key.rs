//! Which sidebar group a tab belongs to, and who decided.
//!
//! "The sidebar groups tabs by repo automatically. Pin what you want to keep."
//! Everything here follows from that sentence. Groups above the divider are
//! *kept*: a [`PinnedGroup`] is stored with the workspace, has an id, an order
//! the user chose and a name they may change, and a tab in one only ever
//! leaves by hand. Everything below the divider is *derived*: an [`AutoKey`]
//! is worked out from the tab's cwd (or, for an SSH pane, from the host it is
//! on) every time the sidebar is drawn, so a tab that `cd`s into another
//! repository walks into another group on its own, and a group whose last tab
//! leaves simply stops existing. Nothing about an auto group is stored except
//! whether it is folded.
//!
//! A pinned group may name a folder. That is what lets a group be kept for a
//! project while its tabs come and go: a tab whose cwd *enters* the folder
//! joins it (see [`EntryWatch`]), and an empty folder group stays on screen
//! with a row to open a tab in it. A pinned group without a folder is a plain
//! label the user files tabs under by hand.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The identity of a pinned group. A name cannot be one — two groups may share
/// a name, and a rename must not orphan every tab filed under the old one —
/// so tabs point at this instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GroupId(uuid::Uuid);

impl GroupId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for GroupId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for GroupId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A group the user chose to keep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedGroup {
    pub id: GroupId,
    /// What the header reads. `None` falls back to the folder's last
    /// component, so pinning `~/src/tty7` reads `tty7` until someone renames
    /// it — and renaming it back to nothing brings that back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A directory on the workspace's own host, or `None` for a label group.
    ///
    /// A `String`, not a `PathBuf`, for the same reason `PaneRecord::cwd` is
    /// one: serde refuses to write a non-UTF-8 `PathBuf`, and the machine tree
    /// is written whole — one odd byte in one folder would stop every layout
    /// on the machine being saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub collapsed: bool,
}

impl PinnedGroup {
    /// A group for tabs the user files by hand, called `name`.
    pub fn label(name: impl Into<String>) -> Self {
        Self {
            id: GroupId::new(),
            name: Some(name.into()),
            folder: None,
            collapsed: false,
        }
    }

    /// A group that keeps `folder`, named after it until renamed.
    pub fn folder(folder: &Path) -> Self {
        Self {
            id: GroupId::new(),
            name: None,
            folder: Some(folder.to_string_lossy().into_owned()),
            collapsed: false,
        }
    }

    pub fn folder_path(&self) -> Option<&Path> {
        self.folder.as_deref().map(Path::new)
    }

    /// The name the user typed, when there is one worth printing. A blank
    /// name is no name: it would draw an unlabelled header.
    pub fn given_name(&self) -> Option<&str> {
        self.name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
    }
}

/// A group the sidebar works out for itself. Never stored as a membership —
/// only as the key a fold is remembered under.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(from = "AutoKeyWire", into = "AutoKeyWire")]
pub enum AutoKey {
    /// The repository home a tab's cwd resolved to. A linked worktree resolves
    /// to the repo it belongs to, and a submodule to itself — so a submodule
    /// gets its own group, and a worktree sits with its main checkout.
    Repo(PathBuf),
    /// The host an SSH pane is on, as its `user@host` target.
    ///
    /// By host and not by remote path: two machines' `/home/ubuntu` are not
    /// the same directory, and grouping by the path alone filed tabs on two
    /// different boxes under one header.
    SshHost(String),
}

/// The spelling [`AutoKey`] is stored in. A repo root goes down as a lossy
/// string for the reason [`PinnedGroup::folder`] is one.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AutoKeyWire {
    Repo(String),
    SshHost(String),
}

impl From<AutoKeyWire> for AutoKey {
    fn from(w: AutoKeyWire) -> Self {
        match w {
            AutoKeyWire::Repo(p) => AutoKey::Repo(PathBuf::from(p)),
            AutoKeyWire::SshHost(h) => AutoKey::SshHost(h),
        }
    }
}

impl From<AutoKey> for AutoKeyWire {
    fn from(k: AutoKey) -> Self {
        match k {
            AutoKey::Repo(p) => AutoKeyWire::Repo(p.to_string_lossy().into_owned()),
            AutoKey::SshHost(h) => AutoKeyWire::SshHost(h),
        }
    }
}

/// Where a tab is drawn: a pinned group, an auto group, or — as `None` beside
/// it — Ungrouped.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GroupKey {
    Pinned(GroupId),
    Auto(AutoKey),
}

impl GroupKey {
    pub fn is_pinned(&self) -> bool {
        matches!(self, Self::Pinned(_))
    }

    pub fn pinned(&self) -> Option<GroupId> {
        match self {
            Self::Pinned(id) => Some(*id),
            Self::Auto(_) => None,
        }
    }

    pub fn auto(&self) -> Option<&AutoKey> {
        match self {
            Self::Auto(key) => Some(key),
            Self::Pinned(_) => None,
        }
    }
}

/// Everything about a workspace's sidebar groups that outlives a frame, stored
/// on the workspace in the machine tree so every window onto it agrees.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceGroups {
    /// In display order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned: Vec<PinnedGroup>,
    /// The auto groups folded shut. A list of the folded ones rather than a
    /// flag per group because auto groups come and go with the tabs — one
    /// nobody has seen yet has to start open.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_collapsed: Vec<AutoKey>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ungrouped_collapsed: bool,
}

impl WorkspaceGroups {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn get(&self, id: GroupId) -> Option<&PinnedGroup> {
        self.pinned.iter().find(|g| g.id == id)
    }

    pub fn get_mut(&mut self, id: GroupId) -> Option<&mut PinnedGroup> {
        self.pinned.iter_mut().find(|g| g.id == id)
    }

    pub fn contains(&self, id: GroupId) -> bool {
        self.get(id).is_some()
    }

    /// Whether the group drawn under `key` is folded; `None` is Ungrouped.
    pub fn is_folded(&self, key: Option<&GroupKey>) -> bool {
        match key {
            Some(GroupKey::Pinned(id)) => self.get(*id).is_some_and(|g| g.collapsed),
            Some(GroupKey::Auto(auto)) => self.auto_collapsed.contains(auto),
            None => self.ungrouped_collapsed,
        }
    }

    pub fn toggle_folded(&mut self, key: Option<&GroupKey>) {
        match key {
            Some(GroupKey::Pinned(id)) => {
                if let Some(g) = self.get_mut(*id) {
                    g.collapsed = !g.collapsed;
                }
            }
            Some(GroupKey::Auto(auto)) => {
                match self.auto_collapsed.iter().position(|k| k == auto) {
                    Some(at) => {
                        self.auto_collapsed.remove(at);
                    }
                    None => self.auto_collapsed.push(auto.clone()),
                }
            }
            None => self.ungrouped_collapsed = !self.ungrouped_collapsed,
        }
    }

    /// The pinned folder group a cwd is inside, if any — see
    /// [`pinned_folder_for`].
    pub fn folder_for(&self, cwd: Option<&Path>, repo_home: Option<&Path>) -> Option<GroupId> {
        pinned_folder_for(&self.pinned, cwd, repo_home)
    }
}

/// The pinned folder group a tab sitting in `cwd` (whose repo home, when it is
/// in one, is `repo_home`) belongs in.
///
/// A folder counts when the cwd is inside it, or when the cwd's repo home *is*
/// it: a linked worktree usually lives outside the checkout it was made from
/// (`~/wt/feature` for `~/src/tty7`), and pinning the repo is a statement about
/// all of its checkouts. When folders nest the deepest one wins, so pinning a
/// monorepo root and one package inside it files the package's tabs under the
/// package. Depth is counted in path components; two folders at the same depth
/// cannot both contain one cwd unless they are the same folder, and then the
/// first in the list wins.
pub fn pinned_folder_for(
    pinned: &[PinnedGroup],
    cwd: Option<&Path>,
    repo_home: Option<&Path>,
) -> Option<GroupId> {
    let mut best: Option<(usize, GroupId)> = None;
    for group in pinned {
        let Some(folder) = group.folder_path() else {
            continue;
        };
        let inside = cwd.is_some_and(|c| c.starts_with(folder)) || repo_home == Some(folder);
        if !inside {
            continue;
        }
        let depth = folder.components().count();
        if best.is_none_or(|(d, _)| depth > d) {
            best = Some((depth, group.id));
        }
    }
    best.map(|(_, id)| id)
}

/// The auto group a tab resolves to, from what is known about it right now.
///
/// `ssh_host` is the target of an SSH pane — one whose shell runs on a machine
/// no `Host` of ours reaches, so there is no repo to probe and its cwd names a
/// directory on a box the other tabs are not on. It outranks everything: the
/// host is the only thing about such a tab that is certain.
///
/// Otherwise `known` is the repo cache's three-valued answer for the tab's cwd:
/// `Some(Some(home))` a repo, `Some(None)` a directory that is known not to be
/// in one (Ungrouped), and `None` a probe that has not landed. That last one is
/// no decision at all, answered as `None`, so the caller keeps whatever it had
/// rather than bouncing the tab through Ungrouped mid-probe.
pub fn auto_key(ssh_host: Option<&str>, known: Option<Option<PathBuf>>) -> Option<Option<AutoKey>> {
    if let Some(host) = ssh_host.map(str::trim).filter(|h| !h.is_empty()) {
        return Some(Some(AutoKey::SshHost(host.to_string())));
    }
    Some(known?.map(AutoKey::Repo))
}

/// Where a tab is drawn, given the pinned group it names (if any), the auto
/// group it resolved to, and whether auto grouping is on.
///
/// A tab naming a group that no longer exists is an auto tab: deleting a
/// group returns its tabs to auto grouping, and a window that hears of the
/// deletion before it hears of the tabs being cleared must already draw them
/// that way.
pub fn place(
    stated: Option<GroupId>,
    groups: &WorkspaceGroups,
    auto_grouping: bool,
    auto: Option<AutoKey>,
) -> Option<GroupKey> {
    if let Some(id) = stated.filter(|id| groups.contains(*id)) {
        return Some(GroupKey::Pinned(id));
    }
    if !auto_grouping {
        return None;
    }
    auto.map(GroupKey::Auto)
}

/// Edge detection for rule 2: a tab joins a pinned folder when its cwd
/// *enters* the folder, not whenever it happens to be inside.
///
/// The difference is the user's say. A tab dragged out of a folder group while
/// it is still sitting in the folder has been told where to go; re-applying
/// "inside means member" on the next frame would put it straight back. So the
/// watch remembers which pinned folder the tab was last seen in, and only a
/// change of answer — outside to inside, or one folder to another — counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EntryWatch {
    /// `None` until the tab is first looked at; then the folder it was in.
    last: Option<Option<GroupId>>,
}

impl EntryWatch {
    /// For a tab that already existed when this window met it — restored from
    /// the machine tree, or created by another window. The first look only
    /// records where it is: whatever it is sitting in, it was already there,
    /// and whoever last had it decided its group.
    pub fn baseline() -> Self {
        Self { last: None }
    }

    /// For a tab this window just opened. It starts outside every folder, so
    /// the first cwd it reports inside one is an entry — which is what makes a
    /// tab opened in a pinned folder land in that group.
    pub fn fresh() -> Self {
        Self { last: Some(None) }
    }

    /// Records that the tab is now in `now` (the deepest pinned folder
    /// containing it, if any), and answers the folder it just entered.
    pub fn observe(&mut self, now: Option<GroupId>) -> Option<GroupId> {
        let entered = match self.last {
            Some(before) => now.filter(|g| before != Some(*g)),
            None => None,
        };
        self.last = Some(now);
        entered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn folders(paths: &[&str]) -> Vec<PinnedGroup> {
        paths
            .iter()
            .map(|f| PinnedGroup::folder(Path::new(f)))
            .collect()
    }

    /// Pinning a monorepo root and a package in it: the package's tabs go to
    /// the package, the rest of the repo's to the root.
    #[test]
    fn nested_pinned_folders_file_a_tab_under_the_deepest() {
        let pinned = folders(&["/w/mono", "/w/mono/pkg/app", "/w/mono/pkg"]);
        let at = |cwd: &str| pinned_folder_for(&pinned, Some(Path::new(cwd)), None);
        assert_eq!(at("/w/mono/pkg/app/src"), Some(pinned[1].id));
        assert_eq!(at("/w/mono/pkg/lib"), Some(pinned[2].id));
        assert_eq!(at("/w/mono/docs"), Some(pinned[0].id));
        assert_eq!(at("/w/other"), None);
    }

    /// Order in the list says nothing about depth — the deepest wins even
    /// when it was pinned first.
    #[test]
    fn the_deepest_folder_wins_whichever_was_pinned_first() {
        let pinned = folders(&["/w/mono/pkg", "/w/mono"]);
        assert_eq!(
            pinned_folder_for(&pinned, Some(Path::new("/w/mono/pkg/x")), None),
            Some(pinned[0].id)
        );
    }

    /// Component-wise, not by string prefix: `/w/tty7-old` is not inside
    /// `/w/tty7`.
    #[test]
    fn a_sibling_sharing_a_prefix_is_not_inside() {
        let pinned = folders(&["/w/tty7"]);
        assert_eq!(
            pinned_folder_for(&pinned, Some(Path::new("/w/tty7-old")), None),
            None
        );
    }

    /// A worktree lives outside the checkout it came from, but its repo home
    /// is that checkout — pinning the repo keeps its worktrees too.
    #[test]
    fn a_worktree_joins_the_folder_its_repo_home_is_pinned_as() {
        let pinned = folders(&["/w/tty7"]);
        assert_eq!(
            pinned_folder_for(
                &pinned,
                Some(Path::new("/tmp/wt/feature/src")),
                Some(Path::new("/w/tty7"))
            ),
            Some(pinned[0].id)
        );
    }

    /// The repo home counts as the folder itself, so a deeper folder the cwd
    /// is inside still wins over it.
    #[test]
    fn a_deeper_folder_beats_a_repo_home_match() {
        let pinned = folders(&["/w/tty7", "/tmp/wt/feature"]);
        assert_eq!(
            pinned_folder_for(
                &pinned,
                Some(Path::new("/tmp/wt/feature/src")),
                Some(Path::new("/w/tty7"))
            ),
            Some(pinned[1].id)
        );
    }

    /// A label group has no folder, so no cwd is ever inside it.
    #[test]
    fn a_label_group_never_pulls_a_tab_in() {
        let pinned = vec![PinnedGroup::label("work")];
        assert_eq!(pinned_folder_for(&pinned, Some(Path::new("/")), None), None);
    }

    #[test]
    fn a_fresh_tab_entering_a_folder_joins_it() {
        let g = GroupId::new();
        let mut watch = EntryWatch::fresh();
        assert_eq!(watch.observe(None), None);
        assert_eq!(watch.observe(Some(g)), Some(g), "outside to inside");
        assert_eq!(watch.observe(Some(g)), None, "staying inside is no entry");
    }

    #[test]
    fn a_fresh_tab_opened_inside_a_folder_joins_on_its_first_look() {
        let g = GroupId::new();
        let mut watch = EntryWatch::fresh();
        assert_eq!(watch.observe(Some(g)), Some(g));
    }

    /// A tab this window met already in place keeps the group whoever had it
    /// gave it — sitting in a folder is not the same as walking into it.
    #[test]
    fn a_restored_tab_is_not_pulled_in_by_where_it_already_is() {
        let g = GroupId::new();
        let mut watch = EntryWatch::baseline();
        assert_eq!(watch.observe(Some(g)), None);
        assert_eq!(watch.observe(None), None);
        assert_eq!(watch.observe(Some(g)), Some(g), "but walking back in is");
    }

    /// Dragged out while inside: not pulled back until it leaves and comes
    /// back. The watch is fed whether or not the tab is in a group, so it
    /// already knows the tab is inside when the drag lands.
    #[test]
    fn a_tab_dragged_out_is_not_pulled_back_until_it_re_enters() {
        let g = GroupId::new();
        let mut watch = EntryWatch::fresh();
        assert_eq!(watch.observe(Some(g)), Some(g), "joined on the way in");
        // … dragged out here; the cwd has not moved …
        assert_eq!(watch.observe(Some(g)), None, "still inside: stays out");
        assert_eq!(watch.observe(Some(g)), None);
        assert_eq!(watch.observe(None), None, "left");
        assert_eq!(watch.observe(Some(g)), Some(g), "re-entered: joins again");
    }

    #[test]
    fn moving_from_one_folder_into_another_is_an_entry() {
        let (a, b) = (GroupId::new(), GroupId::new());
        let mut watch = EntryWatch::fresh();
        watch.observe(Some(a));
        assert_eq!(watch.observe(Some(b)), Some(b));
    }

    /// The same remote path on two machines is two directories.
    #[test]
    fn ssh_tabs_on_two_hosts_with_one_path_land_apart() {
        let one = auto_key(Some("ubuntu@alpha"), Some(Some(p("/home/ubuntu"))));
        let two = auto_key(Some("ubuntu@beta"), Some(Some(p("/home/ubuntu"))));
        assert_eq!(one, Some(Some(AutoKey::SshHost("ubuntu@alpha".into()))));
        assert_eq!(two, Some(Some(AutoKey::SshHost("ubuntu@beta".into()))));
        assert_ne!(one, two);
    }

    #[test]
    fn a_probe_that_has_not_landed_is_no_decision() {
        assert_eq!(auto_key(None, None), None);
        assert_eq!(auto_key(None, Some(None)), Some(None), "known: no repo");
        assert_eq!(
            auto_key(None, Some(Some(p("/w/r")))),
            Some(Some(AutoKey::Repo(p("/w/r"))))
        );
    }

    #[test]
    fn a_tab_in_a_pinned_group_is_drawn_there_whatever_its_cwd_says() {
        let mut groups = WorkspaceGroups::default();
        let work = PinnedGroup::label("work");
        let id = work.id;
        groups.pinned.push(work);
        let repo = Some(AutoKey::Repo(p("/w/r")));
        assert_eq!(
            place(Some(id), &groups, true, repo.clone()),
            Some(GroupKey::Pinned(id))
        );
        assert_eq!(
            place(None, &groups, true, repo),
            Some(GroupKey::Auto(AutoKey::Repo(p("/w/r"))))
        );
    }

    /// Deleting a group returns its tabs to auto grouping — even in a window
    /// that has heard of the deletion and not yet of the tabs being cleared.
    #[test]
    fn a_tab_naming_a_deleted_group_is_filed_automatically() {
        let groups = WorkspaceGroups::default();
        let repo = Some(AutoKey::Repo(p("/w/r")));
        assert_eq!(
            place(Some(GroupId::new()), &groups, true, repo),
            Some(GroupKey::Auto(AutoKey::Repo(p("/w/r"))))
        );
    }

    /// Auto grouping off: no auto group, but a pinned one still stands.
    #[test]
    fn auto_grouping_off_keeps_pinned_groups_and_drops_the_rest() {
        let mut groups = WorkspaceGroups::default();
        let work = PinnedGroup::label("work");
        let id = work.id;
        groups.pinned.push(work);
        let repo = Some(AutoKey::Repo(p("/w/r")));
        assert_eq!(place(None, &groups, false, repo.clone()), None);
        assert_eq!(
            place(Some(id), &groups, false, repo),
            Some(GroupKey::Pinned(id))
        );
    }

    #[test]
    fn folds_are_kept_per_kind_of_group() {
        let mut groups = WorkspaceGroups::default();
        let work = PinnedGroup::label("work");
        let id = work.id;
        groups.pinned.push(work);
        let repo = GroupKey::Auto(AutoKey::Repo(p("/w/r")));
        let pinned = GroupKey::Pinned(id);
        for key in [Some(&repo), Some(&pinned), None] {
            assert!(!groups.is_folded(key));
            groups.toggle_folded(key);
            assert!(groups.is_folded(key));
        }
        assert_eq!(groups.auto_collapsed, vec![AutoKey::Repo(p("/w/r"))]);
        assert!(groups.pinned[0].collapsed);
        assert!(groups.ungrouped_collapsed);
        groups.toggle_folded(Some(&repo));
        assert!(groups.auto_collapsed.is_empty(), "unfolding takes it out");
    }

    #[test]
    fn groups_round_trip_and_an_empty_set_writes_nothing() {
        assert_eq!(
            serde_json::to_string(&WorkspaceGroups::default()).unwrap(),
            "{}"
        );
        let mut groups = WorkspaceGroups::default();
        groups
            .pinned
            .push(PinnedGroup::folder(Path::new("/w/tty7")));
        groups.pinned.push(PinnedGroup::label("工作"));
        groups.auto_collapsed.push(AutoKey::SshHost("u@h".into()));
        groups.auto_collapsed.push(AutoKey::Repo(p("/w/r")));
        let text = serde_json::to_string(&groups).unwrap();
        assert!(text.contains(r#"{"repo":"/w/r"}"#), "{text}");
        let back: WorkspaceGroups = serde_json::from_str(&text).unwrap();
        assert_eq!(back, groups);
    }

    #[test]
    fn a_blank_name_falls_back_to_the_folder() {
        let mut g = PinnedGroup::folder(Path::new("/w/tty7"));
        assert_eq!(g.given_name(), None);
        g.name = Some("  ".into());
        assert_eq!(g.given_name(), None);
        g.name = Some(" tty ".into());
        assert_eq!(g.given_name(), Some("tty"));
    }
}

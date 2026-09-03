//! Projects: the declared half of the sidebar.
//!
//! A repo group in the sidebar is derived — its identity is a path, it appears
//! when a tab lands in it and vanishes with its last tab. A project is
//! declared: it has an id, someone made it on purpose, it carries a name that
//! owes nothing to its directory, and it stays when the last tab in it closes.
//!
//! Nothing here probes anything. A tab joins a project because someone said
//! so, and leaves it the same way; the filing-by-cwd that used to be the only
//! grouping there was still runs, one section further down, over the tabs no
//! project has claimed.

use std::path::PathBuf;

use gpui::{Context, Entity, Subscription, Window};
use gpui_component::WindowExt as _;
use gpui_component::input::{InputEvent, InputState};
use tty7_core::core::machine::{MAX_PROJECTS, Project, ProjectId};

use crate::core::session::WorkspaceStore;
use crate::ui::app::Tty7App;
use crate::ui::i18n::{L10nKey, t, t_fmt};

/// A project header that has turned into a text box.
pub(crate) struct ProjectRename {
    pub(crate) project: ProjectId,
    pub(crate) input: Entity<InputState>,
    _subs: Vec<Subscription>,
}

impl Tty7App {
    pub(crate) fn project(&self, id: ProjectId) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    /// The folder a tab would make a project out of: its repo home when the
    /// probe found one, otherwise whatever directory it is sitting in.
    pub(crate) fn tab_project_root(
        &self,
        index: usize,
        window: &Window,
        cx: &gpui::App,
    ) -> Option<PathBuf> {
        let tab = self.tabs.get(index)?;
        if let Some(group) = tab.sidebar_group.borrow().clone() {
            return Some(group);
        }
        tab.pane
            .focused_or_first(window, cx)
            .and_then(|leaf| leaf.read(cx).spawnable_cwd())
    }

    /// Declares `root` a project, or hands back the one already on it.
    ///
    /// `None` means the workspace is full. The machine refuses past
    /// [`MAX_PROJECTS`] too, and a refusal there resynchronizes — which would
    /// re-push the project this window kept and be refused again, so the limit
    /// has to be held on this side of the wire as well.
    pub(crate) fn declare_project(
        &mut self,
        root: PathBuf,
        cx: &mut Context<Self>,
    ) -> Option<ProjectId> {
        let root = root.to_string_lossy().into_owned();
        // One project per directory: declaring the same folder twice would put
        // two headers on screen that mean the same thing, and the second one
        // could never be told apart from the first.
        if let Some(existing) = self.projects.iter().find(|p| p.root == root) {
            return Some(existing.id);
        }
        if self.projects.len() >= MAX_PROJECTS {
            return None;
        }
        let project = Project::at(root);
        let id = project.id;
        self.projects.push(project);
        self.save_session(cx);
        cx.notify();
        Some(id)
    }

    /// The `+` on the projects heading: pick a folder and declare it.
    ///
    /// The native panel browses the machine this window runs on, which on a
    /// remote workspace is the wrong machine — so there the folder comes from
    /// the tab that is open instead. A button that opens a panel onto paths
    /// the workspace cannot reach would be worse than one that guesses.
    pub(crate) fn new_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace_is_remote(cx) {
            self.project_from_tab(self.active, window, cx);
            return;
        }
        // Checked before the panel opens rather than after it closes: being
        // told the workspace is full is one thing, being told it after picking
        // a folder is another.
        if !self.room_for_a_project(window, cx) {
            return;
        }
        self.pick_folder(cx, |this, path, cx| {
            this.declare_project(path, cx);
        });
    }

    /// Whether another project fits, saying so on the way out if not.
    fn room_for_a_project(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.projects.len() < MAX_PROJECTS {
            return true;
        }
        window.push_notification(
            t_fmt(
                L10nKey::ProjectLimitReached,
                &[("max", &MAX_PROJECTS.to_string())],
            ),
            cx,
        );
        false
    }

    pub(crate) fn pick_project_root(&mut self, project: ProjectId, cx: &mut Context<Self>) {
        self.pick_folder(cx, move |this, path, cx| {
            this.set_project_root(project, path, cx);
        });
    }

    fn pick_folder(
        &mut self,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, PathBuf, &mut Context<Self>) + 'static,
    ) {
        debug_assert!(
            !self.workspace_is_remote(cx),
            "the folder panel browses this machine; a remote workspace has to \
             reach its folders another way"
        );
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| then(this, path, cx));
        })
        .detach();
    }

    pub(crate) fn workspace_is_remote(&self, cx: &gpui::App) -> bool {
        WorkspaceStore::all(cx)
            .get(self.workspace)
            .is_some_and(|w| w.is_remote())
    }

    pub(crate) fn set_project_root(
        &mut self,
        project: ProjectId,
        root: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let root = root.to_string_lossy().into_owned();
        // The same one-project-per-directory rule `declare_project` holds on
        // the way in. Pointing one project at another's folder would reach the
        // state that rule exists to keep out — two headers that mean the same
        // thing — by the back door.
        if self
            .projects
            .iter()
            .any(|p| p.id != project && p.root == root)
        {
            return;
        }
        let Some(p) = self.projects.iter_mut().find(|p| p.id == project) else {
            return;
        };
        if p.root == root {
            return;
        }
        p.root = root;
        self.save_session(cx);
        cx.notify();
    }

    /// Deletes a project. Its tabs stay open and fall back to the derived
    /// grouping — deleting a project says the grouping is over, not the work.
    pub(crate) fn delete_project(&mut self, project: ProjectId, cx: &mut Context<Self>) {
        let before = self.projects.len();
        self.projects.retain(|p| p.id != project);
        if self.projects.len() == before {
            return;
        }
        for tab in &self.tabs {
            if tab.project.get() == Some(project) {
                tab.project.set(None);
            }
        }
        self.save_session(cx);
        cx.notify();
    }

    pub(crate) fn set_tab_project(
        &mut self,
        index: usize,
        project: Option<ProjectId>,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.project.get() == project {
            return;
        }
        tab.project.set(project);
        self.save_session(cx);
        cx.notify();
    }

    /// Declares the tab's folder a project and files the tab under it in one
    /// act — the shortest path from "I am working here" to a named project.
    pub(crate) fn project_from_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.tab_project_root(index, window, cx) else {
            window.push_notification(t(L10nKey::ProjectNoFolder), cx);
            return;
        };
        if !self
            .projects
            .iter()
            .any(|p| p.root == root.to_string_lossy())
            && !self.room_for_a_project(window, cx)
        {
            return;
        }
        let Some(id) = self.declare_project(root, cx) else {
            return;
        };
        self.set_tab_project(index, Some(id), cx);
        self.start_project_rename(id, window, cx);
    }

    pub(crate) fn new_tab_in_project(
        &mut self,
        project: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.project(project).map(|p| PathBuf::from(&p.root)) else {
            return;
        };
        self.new_tab_at_in(root, Some(project), window, cx);
    }

    pub(crate) fn start_project_rename(
        &mut self,
        project: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A box already open on another project is committed, not dropped.
        // Dropping it would take the subscription with it, so the typing that
        // was sitting in it would never reach `commit_project_rename` — the
        // rename would be silently thrown away by opening a second one.
        if self
            .project_rename
            .as_ref()
            .is_some_and(|r| r.project != project)
        {
            self.commit_project_rename(window, cx);
        }
        let Some(p) = self.project(project) else {
            return;
        };
        // The box opens on the name the header is showing, derived title and
        // all, so renaming an unnamed project starts from what it reads as
        // rather than from nothing.
        let current = p
            .name
            .clone()
            .unwrap_or_else(|| derived_project_name(&self.projects, project).unwrap_or_default());
        let input = Self::rename_box(current, window, cx);
        let subs = vec![cx.subscribe_in(
            &input,
            window,
            |this, _input, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    this.commit_project_rename(window, cx)
                }
                _ => {}
            },
        )];
        self.project_rename = Some(ProjectRename {
            project,
            input,
            _subs: subs,
        });
        cx.notify();
    }

    pub(crate) fn commit_project_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.project_rename.take() else {
            return;
        };
        let value = rename.input.read(cx).value().trim().to_string();
        // Typing the derived title back is not a rename: it would pin a name
        // that is already what the header says, and the project would then
        // stop following its folder for no visible reason.
        let derived = derived_project_name(&self.projects, rename.project);
        let name = match value {
            v if v.is_empty() => None,
            v if Some(&v) == derived.as_ref() => None,
            v => Some(v),
        };
        if let Some(p) = self.projects.iter_mut().find(|p| p.id == rename.project)
            && p.name != name
        {
            p.name = name;
            self.save_session(cx);
        }
        self.focus_active(window, cx);
        cx.notify();
    }
}

/// What each project's header reads when nobody has named it: the last
/// component of its root, widened a component at a time until no two projects
/// read the same. The same walk-up the derived group headers do, over the
/// project roots instead of the repo roots.
pub(crate) fn project_names(projects: &[Project]) -> Vec<String> {
    let roots: Vec<PathBuf> = projects.iter().map(|p| PathBuf::from(&p.root)).collect();
    let refs: Vec<&PathBuf> = roots.iter().collect();
    let derived = crate::ui::tab_sidebar::group_names(&refs);
    projects
        .iter()
        .zip(derived)
        .map(|(p, fallback)| match p.name.as_deref().map(str::trim) {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => fallback,
        })
        .collect()
}

fn derived_project_name(projects: &[Project], project: ProjectId) -> Option<String> {
    let at = projects.iter().position(|p| p.id == project)?;
    let roots: Vec<PathBuf> = projects.iter().map(|p| PathBuf::from(&p.root)).collect();
    let refs: Vec<&PathBuf> = roots.iter().collect();
    crate::ui::tab_sidebar::group_names(&refs)
        .into_iter()
        .nth(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(root: &str, name: Option<&str>) -> Project {
        Project {
            name: name.map(str::to_string),
            ..Project::at(root)
        }
    }

    #[test]
    fn an_unnamed_project_reads_as_its_folder_and_disambiguates() {
        let projects = vec![
            project("/home/u/work/app", None),
            project("/home/u/fork/app", None),
            project("/home/u/tty7", Some("套利研究")),
        ];
        assert_eq!(
            project_names(&projects),
            vec!["work/app", "fork/app", "套利研究"]
        );
    }

    #[test]
    fn a_blank_name_falls_back_rather_than_rendering_nothing() {
        let projects = vec![project("/w/repo", Some("   "))];
        assert_eq!(project_names(&projects), vec!["repo"]);
    }
}

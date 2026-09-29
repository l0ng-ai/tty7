//! The editor split in two: two groups of files side by side, each with a
//! file in front, the way VS Code's ⌘\ splits an editor.
//!
//! **One group has the focus, and it is `TabCode`'s own `files`/`active`.**
//! The other one waits in [`OtherGroup`]. Moving the focus swaps the two, so
//! everything that acts on "the file in front" — saving, go to line, the
//! language servers, the change markers, multiple cursors, back/forward, the
//! strip in the header — acts on the focused group without knowing a split
//! exists. The header's strip is the focused group's; the other group shows
//! its file in front and its breadcrumbs.
//!
//! **The same file in both groups.** A buffer is one `OpenFile` with one
//! `InputState`, which is what the language servers, the gutter markers, the
//! undo history and every hook in `code_editor.rs` key on. A second
//! `InputState` mirroring the first would need every one of those attached to
//! it twice, and edits copied back and forth without echoing — a lot of
//! surface for two views of one text. So the text is drawn once: in the group
//! with the focus. The other group shows a placeholder naming the file, and a
//! click on it moves the focus (and with it the live editor) over. The caret
//! and scroll of that file are therefore shared between the two groups; two
//! *different* files each keep their own, being different buffers.
//!
//! A group whose last file closes goes away, and the split with it.

use gpui::Focusable as _;
use gpui::prelude::*;
use gpui::{AnyElement, Context, MouseButton, SharedString, Window, div, px};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use super::{BufferId, TabCode};
use crate::ui::app::Tty7App;
use crate::ui::i18n::{L10nKey, t};

/// The group without the focus.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OtherGroup {
    pub(crate) files: Vec<BufferId>,
    pub(crate) active: usize,
    /// Whether the group *with* the focus is the left one.
    pub(crate) focus_left: bool,
}

impl OtherGroup {
    fn active_id(&self) -> Option<BufferId> {
        self.files.get(self.active).copied()
    }
}

/// Takes `id` out of one group's strip, bringing its neighbour forward the
/// way `TabCode::forget` does.
fn remove(files: &mut Vec<BufferId>, active: &mut usize, id: BufferId) -> bool {
    let Some(pos) = files.iter().position(|f| *f == id) else {
        return false;
    };
    files.remove(pos);
    if *active > pos || *active >= files.len() {
        *active = active.saturating_sub(1);
    }
    true
}

/// One side of the editor, as it is drawn.
pub(crate) struct Side {
    pub(crate) files: Vec<BufferId>,
    pub(crate) active: usize,
    pub(crate) focused: bool,
}

impl TabCode {
    pub(crate) fn is_split(&self) -> bool {
        self.split.is_some()
    }

    /// Whether either group shows this buffer.
    pub(crate) fn shows(&self, id: BufferId) -> bool {
        self.files.contains(&id) || self.split.as_ref().is_some_and(|o| o.files.contains(&id))
    }

    /// How many groups show this buffer: 0, 1 or 2.
    pub(crate) fn views_of(&self, id: BufferId) -> usize {
        usize::from(self.files.contains(&id))
            + usize::from(self.split.as_ref().is_some_and(|o| o.files.contains(&id)))
    }

    /// Every buffer either group shows, once each, the focused group's first.
    pub(crate) fn all_files(&self) -> Vec<BufferId> {
        let mut all = self.files.clone();
        if let Some(other) = &self.split {
            for id in &other.files {
                if !all.contains(id) {
                    all.push(*id);
                }
            }
        }
        all
    }

    /// The file in front of the group without the focus.
    pub(crate) fn other_active_id(&self) -> Option<BufferId> {
        self.split.as_ref().and_then(OtherGroup::active_id)
    }

    /// Gives the focus to the other group.
    pub(crate) fn swap_focus(&mut self) {
        let Some(other) = self.split.as_mut() else {
            return;
        };
        std::mem::swap(&mut self.files, &mut other.files);
        std::mem::swap(&mut self.active, &mut other.active);
        other.focus_left = !other.focus_left;
    }

    /// Which side the focused group is drawn on: 0 left, 1 right. What tells
    /// the two groups' elements apart.
    pub(crate) fn focused_slot(&self) -> usize {
        usize::from(!self.focus_is_left())
    }

    /// Whether the focused group is on the left. `true` when not split.
    pub(crate) fn focus_is_left(&self) -> bool {
        self.split.as_ref().is_none_or(|o| o.focus_left)
    }

    /// Shows `id` in the other group and gives that group the focus —
    /// splitting the editor first if it is not split yet, with the new group
    /// on the right.
    pub(crate) fn split_with(&mut self, id: BufferId) {
        match self.split.as_mut() {
            None => {
                self.split = Some(OtherGroup {
                    files: std::mem::replace(&mut self.files, vec![id]),
                    active: std::mem::replace(&mut self.active, 0),
                    focus_left: false,
                });
            }
            Some(_) => {
                self.swap_focus();
                self.show(id);
            }
        }
    }

    /// Closes a file in the focused group only. The group goes if it was its
    /// last file, and the focus passes to the one that is left.
    pub(crate) fn close_in_focused(&mut self, id: BufferId) -> bool {
        let closed = self.forget(id);
        self.collapse();
        closed
    }

    /// Takes a buffer out of both groups, for a buffer that is going away.
    pub(crate) fn forget_everywhere(&mut self, id: BufferId) -> bool {
        let mut gone = self.forget(id);
        if let Some(other) = self.split.as_mut() {
            gone |= remove(&mut other.files, &mut other.active, id);
        }
        self.collapse();
        gone
    }

    /// A group with nothing left in it closes.
    fn collapse(&mut self) {
        let Some(other) = self.split.as_ref() else {
            return;
        };
        if other.files.is_empty() {
            self.split = None;
        } else if self.files.is_empty() {
            let other = self.split.take().unwrap_or_default();
            self.files = other.files;
            self.active = other.active;
        }
    }

    /// Lays a tab's recorded groups back out once their files have loaded.
    ///
    /// `restored` are the buffers the restore opened, already listed in the
    /// focused group (they arrive in the background). Anything else the tab
    /// shows was opened while they loaded, and is kept:
    ///
    /// - A tab split in the meantime keeps its layout; the restored files
    ///   stay where they arrived, behind the file in front.
    /// - Otherwise the recorded layout is put back, the files opened in the
    ///   meantime join the focused group, and the last of them stays in front.
    ///
    /// `true` when nothing had been opened in the meantime.
    pub(crate) fn restore_groups(
        &mut self,
        restored: &[BufferId],
        left: (Vec<BufferId>, usize),
        right: Option<(Vec<BufferId>, usize, bool)>,
    ) -> bool {
        if self.is_split() {
            return false;
        }
        let extras: Vec<BufferId> = self
            .files
            .iter()
            .copied()
            .filter(|id| !restored.contains(id))
            .collect();
        let front = self.active_id().filter(|id| extras.contains(id));
        let (left, left_active) = left;
        let right = right.filter(|(files, ..)| !files.is_empty());
        match (left.is_empty(), right) {
            (false, Some((right, right_active, focused))) => {
                self.files = left;
                self.active = left_active;
                self.split = Some(OtherGroup {
                    files: right,
                    active: right_active,
                    focus_left: true,
                });
                if focused {
                    self.swap_focus();
                }
            }
            (false, None) => {
                self.files = left;
                self.active = left_active;
            }
            (true, Some((right, right_active, _))) => {
                self.files = right;
                self.active = right_active;
            }
            (true, None) => {}
        }
        for id in &extras {
            if !self.files.contains(id) {
                self.files.push(*id);
            }
        }
        if let Some(front) = front {
            self.show(front);
        }
        extras.is_empty()
    }

    /// The two sides, left first. One when the editor is not split.
    pub(crate) fn sides(&self) -> Vec<Side> {
        let focused = Side {
            files: self.files.clone(),
            active: self.active,
            focused: true,
        };
        let Some(other) = &self.split else {
            return vec![focused];
        };
        let other_side = Side {
            files: other.files.clone(),
            active: other.active,
            focused: false,
        };
        match other.focus_left {
            true => vec![focused, other_side],
            false => vec![other_side, focused],
        }
    }
}

impl Tty7App {
    /// Splits the editor: the file in front opens in a second group to the
    /// right, which takes the focus. Already split, it opens in the other
    /// group instead. `false` with no file to split.
    pub(crate) fn editor_split(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(code) = self.tab_code_mut() else {
            return false;
        };
        let Some(id) = code.active_id() else {
            return false;
        };
        code.split_with(id);
        self.editor.bar = None;
        self.focus_editor(window, cx);
        cx.notify();
        true
    }

    /// Gives the focus to the group on the left (or right). `false` when there
    /// is no such group to go to — not split, or already there — so the chord
    /// can carry on to the panes.
    pub(crate) fn editor_focus_group(
        &mut self,
        left: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(code) = self.tab_code_mut() else {
            return false;
        };
        if !code.is_split() || code.focus_is_left() == left {
            return false;
        }
        code.swap_focus();
        self.editor.bar = None;
        self.focus_editor(window, cx);
        cx.notify();
        true
    }

    /// Keeps the focused group the one whose text has the keyboard: a click,
    /// or anything else that focuses the other group's editor, moves the
    /// group focus there too. Run on every draw, and before every editor
    /// command ([`Self::editor_split_command_sync`]): focus moves between
    /// frames, and F12 or F2 in the first frame after one must act on the
    /// group the caret is now in.
    pub(crate) fn editor_split_follow_focus(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(other) = self.tab_code().and_then(TabCode::other_active_id) else {
            return;
        };
        if self.tab_code().and_then(TabCode::active_id) == Some(other) {
            return;
        }
        let focused = self.buffer(other).is_some_and(|f| {
            f.input
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
        });
        if focused && let Some(code) = self.tab_code_mut() {
            code.swap_focus();
            cx.notify();
        }
    }

    /// Settles the group focus before any editor command runs, on the element
    /// that holds the editor.
    ///
    /// gpui draws a window whose focus moved before it dispatches the next
    /// key, which settles the group on its own; a command from the menu bar
    /// or a context menu is dispatched without that draw. A capture-phase
    /// listener runs before the command's own handler wherever that handler
    /// sits — here, in `app.rs` or in `ui::lsp` — and lets the command carry
    /// on.
    pub(crate) fn editor_split_command_sync<E: InteractiveElement>(
        &self,
        element: E,
        cx: &mut Context<Self>,
    ) -> E {
        use crate::core::actions::*;
        macro_rules! before {
            ($el:expr, $($action:ty),+ $(,)?) => {
                $el$(.capture_action(cx.listener(|this, _: &$action, window, cx| {
                    this.editor_split_follow_focus(window, cx);
                })))+
            };
        }
        before!(
            element,
            EditorSave,
            EditorSaveAs,
            EditorGoToLine,
            EditorGoToSymbol,
            EditorNavigateBack,
            EditorNavigateForward,
            EditorSplitRight,
            EditorGoToDefinition,
            EditorQuickFix,
            EditorRenameSymbol,
            EditorFormatDocument,
            EditorTransformUppercase,
            EditorTransformLowercase,
            EditorTransformTitleCase,
            EditorTrimTrailingWhitespace,
            EditorJoinLines,
            EditorRemoveSurroundingBrackets,
            EditorFindReferences,
            EditorNextChange,
            EditorPrevChange,
            EditorRevertChange,
            EditorPeekChange,
            ToggleDocumentPreview,
            ToggleDocumentWrap,
        )
    }

    /// The editor's text area: one group, or two side by side.
    pub(crate) fn render_editor_groups(
        &mut self,
        focused_crumbs: Option<AnyElement>,
        focused_body: AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pane = |crumbs: Option<AnyElement>, body: AnyElement| {
            v_flex()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .h_full()
                .children(crumbs)
                .child(div().flex_1().min_h_0().child(body))
        };
        let Some(code) = self.tab_code() else {
            return pane(focused_crumbs, focused_body).into_any_element();
        };
        if !code.is_split() {
            return pane(focused_crumbs, focused_body).into_any_element();
        }
        let focus_left = code.focus_is_left();
        let other = code.other_active_id();
        let same = other.is_some() && other == code.active_id();
        let other_slot = usize::from(focus_left);
        let (other_crumbs, other_body) = match other {
            Some(id) if same => (None, self.render_editor_same_file(id, focus_left, cx)),
            Some(id) => (
                self.render_editor_breadcrumbs_for(id, other_slot, false, window, cx),
                self.render_editor_body(Some(id), other_slot, cx),
            ),
            None => (None, self.render_editor_empty(cx).into_any_element()),
        };
        let accent = cx.theme().primary;
        let focused = pane(focused_crumbs, focused_body)
            .border_t_2()
            .border_color(accent)
            .into_any_element();
        let unfocused = pane(other_crumbs, other_body)
            .border_t_2()
            .border_color(gpui::transparent_black())
            .id(("editor-group", other_slot))
            // Before the text sees the click, so what the click does lands in
            // the group it was made in.
            .capture_any_mouse_down(cx.listener(move |this, _, window, cx| {
                this.editor_focus_group(!focus_left, window, cx);
            }))
            .into_any_element();
        let divider = div()
            .flex_none()
            .w(px(1.))
            .h_full()
            .bg(cx.theme().sidebar_border);
        let (left, right) = match focus_left {
            true => (focused, unfocused),
            false => (unfocused, focused),
        };
        h_flex()
            .flex_1()
            .min_h_0()
            .size_full()
            .child(left)
            .child(divider)
            .child(right)
            .into_any_element()
    }

    /// The other group's view of the file the focused group is showing: it is
    /// drawn once, there, and a click brings it here.
    fn render_editor_same_file(
        &self,
        id: BufferId,
        focus_left: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name: SharedString = self.buffer(id).map(|f| f.label()).unwrap_or_default();
        let theme = cx.theme();
        v_flex()
            .id("editor-group-same-file")
            .size_full()
            .items_center()
            .justify_center()
            .gap_1()
            .cursor_pointer()
            .text_size(gpui::rems(crate::ui::right_panel::TEXT))
            .text_color(theme.muted_foreground)
            .child(div().text_color(theme.foreground).child(name))
            .child(t(L10nKey::EditorSplitSameFile))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.editor_focus_group(!focus_left, window, cx);
                }),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: u64) -> Vec<BufferId> {
        (1..=n).map(gpui::EntityId::from).collect()
    }

    fn code(files: &[BufferId], active: usize) -> TabCode {
        let mut code = TabCode::new();
        code.files = files.to_vec();
        code.active = active;
        code
    }

    #[test]
    fn splitting_opens_the_file_in_front_in_a_focused_right_group() {
        let ids = ids(3);
        let mut code = code(&ids, 1);
        code.split_with(ids[1]);
        assert_eq!(code.files, [ids[1]]);
        assert_eq!(code.active_id(), Some(ids[1]));
        assert!(!code.focus_is_left());
        let sides = code.sides();
        assert_eq!(sides.len(), 2);
        assert_eq!(
            (sides[0].files.clone(), sides[0].focused),
            (ids.clone(), false)
        );
        assert!(sides[1].focused);
        assert_eq!(code.views_of(ids[1]), 2);
        assert_eq!(code.views_of(ids[0]), 1);
        assert_eq!(code.all_files(), [ids[1], ids[0], ids[2]]);
    }

    #[test]
    fn the_focus_swaps_the_groups_but_not_their_places() {
        let ids = ids(3);
        let mut code = code(&ids[..2], 0);
        code.split_with(ids[0]);
        code.show(ids[2]);
        code.swap_focus();
        assert!(code.focus_is_left());
        assert_eq!(code.files, ids[..2]);
        assert_eq!(code.other_active_id(), Some(ids[2]));
        let sides = code.sides();
        assert!(sides[0].focused && !sides[1].focused);
        assert_eq!(sides[1].files, [ids[0], ids[2]]);
    }

    #[test]
    fn a_group_that_loses_its_last_file_closes() {
        let ids = ids(2);
        let mut code = code(&ids, 0);
        code.split_with(ids[1]);
        // Closing the right group's only file leaves the left one, focused.
        assert!(code.close_in_focused(ids[1]));
        assert!(!code.is_split());
        assert_eq!(code.files, ids);
        assert_eq!(code.active_id(), Some(ids[0]), "it keeps what was in front");

        // Closing the other group's last file from elsewhere (the buffer went
        // away) closes that group instead.
        code.split_with(ids[0]);
        code.swap_focus();
        assert!(code.forget_everywhere(ids[0]));
        assert!(!code.is_split());
        assert_eq!(code.files, [ids[1]]);
        assert!(!code.shows(ids[0]));
    }

    #[test]
    fn a_restore_lays_out_the_recorded_groups() {
        let ids = ids(3);
        // As the restore finds it: every file arrived at the end of the strip.
        let mut code = code(&ids, 0);
        let untouched = code.restore_groups(
            &ids,
            (vec![ids[0], ids[1]], 1),
            Some((vec![ids[2]], 0, true)),
        );
        assert!(untouched);
        assert!(!code.focus_is_left());
        assert_eq!(code.active_id(), Some(ids[2]));
        assert_eq!(code.other_active_id(), Some(ids[1]));
    }

    #[test]
    fn a_file_opened_while_the_restore_loaded_stays_in_front() {
        let ids = ids(4);
        // ids[3] was opened by hand; the restore's three arrived behind it.
        let mut code = code(&[ids[3], ids[0], ids[1], ids[2]], 0);
        let untouched = code.restore_groups(
            &ids[..3],
            (vec![ids[0], ids[1]], 0),
            Some((vec![ids[2]], 0, false)),
        );
        assert!(!untouched);
        assert!(code.is_split(), "the recorded split is still put back");
        assert!(code.focus_is_left());
        assert_eq!(code.files, [ids[0], ids[1], ids[3]]);
        assert_eq!(
            code.active_id(),
            Some(ids[3]),
            "what was opened stays in front"
        );
        assert_eq!(code.other_active_id(), Some(ids[2]));
    }

    #[test]
    fn a_split_made_while_the_restore_loaded_is_kept() {
        let ids = ids(4);
        let mut code = code(&[ids[3]], 0);
        code.split_with(ids[3]);
        // The restore's files arrive in the focused group.
        code.files.extend([ids[0], ids[1]]);
        let before = (code.files.clone(), code.active, code.split.clone());
        let untouched =
            code.restore_groups(&ids[..2], (vec![ids[0]], 0), Some((vec![ids[1]], 0, true)));
        assert!(!untouched);
        assert_eq!(
            (code.files.clone(), code.active, code.split.clone()),
            before
        );
    }

    #[test]
    fn closing_in_one_group_keeps_the_file_in_the_other() {
        let ids = ids(2);
        let mut code = code(&ids, 0);
        code.split_with(ids[0]);
        code.show(ids[1]);
        code.close_in_focused(ids[0]);
        assert!(code.is_split());
        assert!(code.shows(ids[0]));
        assert_eq!(code.views_of(ids[0]), 1);
    }
}

#[cfg(test)]
mod gpui_tests {
    use std::path::PathBuf;

    use gpui::{Entity, TestAppContext, VisualTestContext};

    use super::super::nav::gpui_tests::{caret, open_rs, put_caret};
    use super::*;
    use crate::ui::app::test_window::harness_with_tabs;
    use crate::ui::editor_session::{self, SplitEditor, TabEditor};

    fn front(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> PathBuf {
        app.read_with(vcx, |app, _| app.active_buffer().unwrap().path.clone())
    }

    fn split(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> bool {
        app.read_with(vcx, |app, _| app.tab_code().is_some_and(TabCode::is_split))
    }

    #[gpui::test]
    fn two_groups_keep_their_own_file_and_caret(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        open_rs(&app, &mut vcx, "/split/a.rs");
        put_caret(&app, &mut vcx, 5);
        assert!(app.update_in(&mut vcx, |app, window, cx| app.editor_split(window, cx)));
        assert!(split(&app, &mut vcx));
        // The new group is on the right, focused, showing the same file.
        app.read_with(&vcx, |app, _| {
            let code = app.tab_code().unwrap();
            assert!(!code.focus_is_left());
            assert_eq!(code.active_id(), code.other_active_id());
        });
        // A file opened now opens in the focused (right) group only.
        open_rs(&app, &mut vcx, "/split/b.rs");
        put_caret(&app, &mut vcx, 40);
        assert_eq!(front(&app, &mut vcx), PathBuf::from("/split/b.rs"));

        // To the left: a.rs, with its own caret.
        assert!(app.update_in(&mut vcx, |app, window, cx| {
            app.editor_focus_group(true, window, cx)
        }));
        assert_eq!(front(&app, &mut vcx), PathBuf::from("/split/a.rs"));
        assert_eq!(caret(&app, &mut vcx).line, 5);
        assert!(
            !app.update_in(&mut vcx, |app, window, cx| app
                .editor_focus_group(true, window, cx)),
            "no group further left: the chord carries on to the panes"
        );
        // And back.
        assert!(app.update_in(&mut vcx, |app, window, cx| {
            app.editor_focus_group(false, window, cx)
        }));
        assert_eq!(caret(&app, &mut vcx).line, 40);

        // Closing both files of the right group closes the group.
        app.update_in(&mut vcx, |app, window, cx| {
            let n = app.tab_code().unwrap().files.len();
            for _ in 0..n {
                app.editor_close_file(0, window, cx);
            }
        });
        assert!(!split(&app, &mut vcx));
        assert_eq!(front(&app, &mut vcx), PathBuf::from("/split/a.rs"));
        assert!(
            app.read_with(&vcx, |app, _| app
                .editor
                .buffers
                .iter()
                .all(|b| b.path != std::path::Path::new("/split/b.rs"))),
            "a file no group shows any more is closed"
        );
    }

    #[gpui::test]
    fn a_split_is_recorded_and_comes_back(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        // Canonical, as a loaded file's path is: /var is /private/var here.
        let root = crate::ui::code_editor::tests::test_real_dir(dir.path());
        let (a, b, c) = (root.join("a.rs"), root.join("b.rs"), root.join("c.rs"));
        for p in [&a, &b, &c] {
            std::fs::write(p, "fn main() {}\n").unwrap();
        }
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 2);
        crate::ui::i18n::set_locale("en");
        let state = TabEditor {
            files: vec![a.clone(), b.clone()],
            active: 1,
            visible: true,
            split: Some(SplitEditor {
                files: vec![c.clone()],
                active: 0,
                focused: true,
            }),
        };
        // The second tab was split when the window last closed.
        let tab = app.read_with(&vcx, |app, _| app.tabs[1].tree_id.get());
        vcx.update(|_, cx| editor_session::put(cx, tab, state.clone()));
        app.update_in(&mut vcx, |app, window, cx| {
            app.active = 1;
            // As on a relaunch: nothing has been put back for it yet.
            app.editor.restored.remove(&tab);
            app.editor_restore_active(window, cx);
        });
        // The files are read on a real thread; let them land.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !split(&app, &mut vcx) {
            assert!(
                std::time::Instant::now() < deadline,
                "the files never landed"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
            vcx.run_until_parked();
        }
        app.read_with(&vcx, |app, _| {
            let code = app.tab_code().unwrap();
            assert!(code.is_split());
            assert!(!code.focus_is_left(), "the right group had the focus");
            let path = |id| app.buffer(id).unwrap().path.clone();
            assert_eq!(path(code.active_id().unwrap()), c);
            assert_eq!(path(code.other_active_id().unwrap()), b);
        });
        // And recording it writes the same thing down.
        app.update(&mut vcx, |app, cx| app.editor_record_sessions(cx));
        let recorded = vcx.update(|_, cx| editor_session::get(cx, tab)).unwrap();
        assert_eq!(recorded, state);
    }

    #[gpui::test]
    fn a_split_with_nothing_on_the_left_still_comes_back(cx: &mut TestAppContext) {
        // The left group held only untitled or remote buffers, which are
        // not recorded: only the right group's files were written down.
        let dir = tempfile::tempdir().unwrap();
        let root = crate::ui::code_editor::tests::test_real_dir(dir.path());
        let c = root.join("c.rs");
        std::fs::write(&c, "fn main() {}\n").unwrap();
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        let state = TabEditor {
            files: vec![],
            active: 0,
            visible: true,
            split: Some(SplitEditor {
                files: vec![c.clone()],
                active: 0,
                focused: true,
            }),
        };
        let tab = app.read_with(&vcx, |app, _| app.tabs[0].tree_id.get());
        vcx.update(|_, cx| editor_session::put(cx, tab, state.clone()));
        app.update_in(&mut vcx, |app, window, cx| {
            app.editor.restored.remove(&tab);
            app.editor_restore_active(window, cx);
        });
        let has_files = |app: &Entity<Tty7App>, vcx: &mut VisualTestContext| {
            app.read_with(vcx, |app, _| {
                app.tab_code().is_some_and(|c| c.active_id().is_some())
            })
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !has_files(&app, &mut vcx) {
            assert!(
                std::time::Instant::now() < deadline,
                "the right group's file never came back"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
            vcx.run_until_parked();
        }
        app.read_with(&vcx, |app, _| {
            let code = app.tab_code().unwrap();
            assert_eq!(app.buffer(code.active_id().unwrap()).unwrap().path, c);
        });
    }

    #[gpui::test]
    fn a_key_pressed_right_after_a_focus_move_acts_on_the_new_group(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        open_rs(&app, &mut vcx, "/split/a.rs");
        app.update_in(&mut vcx, |app, window, cx| app.editor_split(window, cx));
        open_rs(&app, &mut vcx, "/split/b.rs");
        vcx.run_until_parked();
        assert_eq!(front(&app, &mut vcx), PathBuf::from("/split/b.rs"));
        // The keyboard moves to the left group's text, and a key follows
        // before anything is drawn.
        vcx.update(|window, cx| {
            let left = app.read(cx).tab_code().unwrap().other_active_id().unwrap();
            let input = app.read(cx).buffer(left).unwrap().input.clone();
            input.update(cx, |state, cx| state.focus(window, cx));
            let before = app.read(cx).active_buffer().unwrap().path.clone();
            assert_eq!(
                before,
                PathBuf::from("/split/b.rs"),
                "nothing has drawn yet"
            );
            // Go to Line asks whether the editor has the keyboard, which it
            // only does if the group focus has followed it.
            window.dispatch_keystroke(gpui::Keystroke::parse("ctrl-g").unwrap(), cx);
            assert!(
                app.read(cx).editor.bar.is_some(),
                "the command saw the group the keyboard is in"
            );
        });
    }

    /// A command from a menu is dispatched without a draw first, so nothing
    /// but the capture listener can have settled the group.
    #[gpui::test]
    fn a_menu_command_right_after_a_focus_move_acts_on_the_new_group(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        open_rs(&app, &mut vcx, "/split/a.rs");
        app.update_in(&mut vcx, |app, window, cx| app.editor_split(window, cx));
        open_rs(&app, &mut vcx, "/split/b.rs");
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            let left = app.read(cx).tab_code().unwrap().other_active_id().unwrap();
            let input = app.read(cx).buffer(left).unwrap().input.clone();
            input.update(cx, |state, cx| state.focus(window, cx));
            let node = window.focused(cx).unwrap();
            node.dispatch_action(&crate::core::actions::EditorGoToLine, window, cx);
            assert!(
                app.read(cx).editor.bar.is_some(),
                "the command saw the group the keyboard is in"
            );
            let front = app.read(cx).active_buffer().unwrap().path.clone();
            assert_eq!(front, PathBuf::from("/split/a.rs"));
        });
    }

    #[gpui::test]
    fn what_is_opened_while_a_restore_loads_survives_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::ui::code_editor::tests::test_real_dir(dir.path());
        let (a, b, c) = (root.join("a.rs"), root.join("b.rs"), root.join("c.rs"));
        for p in [&a, &b, &c] {
            std::fs::write(p, "fn main() {}\n").unwrap();
        }
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        crate::ui::i18n::set_locale("en");
        let state = TabEditor {
            files: vec![a.clone(), b.clone()],
            active: 0,
            visible: true,
            split: Some(SplitEditor {
                files: vec![c.clone()],
                active: 0,
                focused: false,
            }),
        };
        let tab = app.read_with(&vcx, |app, _| app.tabs[0].tree_id.get());
        vcx.update(|_, cx| editor_session::put(cx, tab, state));
        app.update_in(&mut vcx, |app, window, cx| {
            app.editor.restored.remove(&tab);
            app.editor_restore_active(window, cx);
        });
        // Opened by hand while the three are still being read.
        open_rs(&app, &mut vcx, "/split/opened.rs");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !split(&app, &mut vcx) {
            assert!(
                std::time::Instant::now() < deadline,
                "the files never landed"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
            vcx.run_until_parked();
        }
        app.read_with(&vcx, |app, _| {
            let code = app.tab_code().unwrap();
            let path = |id| app.buffer(id).unwrap().path.clone();
            assert_eq!(
                path(code.active_id().unwrap()),
                PathBuf::from("/split/opened.rs")
            );
            let shown: Vec<PathBuf> = code.all_files().into_iter().map(path).collect();
            for p in [&a, &b, &c] {
                assert!(shown.contains(p), "{} came back", p.display());
            }
            assert_eq!(path(code.other_active_id().unwrap()), c);
            assert!(code.visible);
        });
    }
}

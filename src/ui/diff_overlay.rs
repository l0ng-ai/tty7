use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, Background, FocusHandle, FontWeight, Hsla, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, SharedString, Window, div, prelude::*, px,
};
use gpui_component::button::Button;
use gpui_component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use crate::core::config::{Config, DiffViewMode};
use crate::core::git::status::DecoStatus;
use crate::terminal::git_diff::{
    self, CommitLabel, DiffSnapshot, DiffSource, DiffStats, FileDiff, FileStatus, LineKind,
    Truncation,
};

/// How much of an untracked file the preview will read. Past this the card
/// says the read failed rather than showing a silently cut-off file — and the
/// line budget below cuts rendering long before this does anyway.
const MAX_PREVIEW_BYTES: u64 = 4 * 1024 * 1024;
use crate::ui::app::Tty7App;
use crate::ui::diff_list::{DiffRow, FileHead, RowAt};
use crate::ui::diff_rows::{DiffSelection, Side, SplitCell, SplitRow, UnifiedRow};
use crate::ui::document_column::DocumentChrome;
use crate::ui::i18n::{L10nKey, t, t_fmt, t_plural};
use crate::ui::right_panel::info_chip;
use crate::ui::rounding;
use crate::ui::scm::path::relative_time;
use crate::ui::scm::status::{status_color, status_glyph};

pub(crate) enum DiffLoad {
    Loading,
    Ready(Arc<DiffSnapshot>),
    NotARepo,
}

pub(crate) struct DiffOverlayState {
    pub(crate) host_id: crate::ui::host_ops::HostId,
    pub(crate) cwd: PathBuf,
    /// Which patch this overlay is showing. Part of its identity, not a
    /// setting: two sources over one directory are two different overlays.
    pub(crate) source: DiffSource,
    pub(crate) focus_handle: FocusHandle,
    pub(crate) load: DiffLoad,
    pub(crate) loading: bool,
    pub(crate) expanded: HashMap<String, bool>,
    pub(crate) focus: Option<String>,
    /// A synthesized all-added card for a focused *untracked* file, keyed by
    /// path; `None` in the value means the read failed. git has no patch for
    /// an untracked file, so focusing one reads its bytes instead — lazily,
    /// only for the file on screen, never for the whole list. Cleared when a
    /// fresh snapshot lands, so an edit to the file shows up on the same
    /// cadence a tracked file's does.
    pub(crate) preview: Option<(String, Option<Arc<FileDiff>>)>,
    pub(crate) preview_loading: Option<String>,
    /// The rows the pointer has dragged over, and whether it is still down.
    ///
    /// A diff is read in order to be copied out of, and until this existed the
    /// text on screen was unreachable — no selection, no clipboard, nothing
    /// but retyping it (#721). Line-granular on purpose: the rows are a grid
    /// of independent elements, not one text run, so a range of them is the
    /// selection this layout can honestly offer.
    pub(crate) selection: Option<DiffSelection>,
    pub(crate) selecting: bool,
    /// The virtualised list the rows scroll in. Held across frames: it owns
    /// the scroll position, and the row heights gpui has measured.
    pub(crate) list: gpui::ListState,
    /// The patch, flattened into one row per line — see
    /// [`crate::ui::diff_list`]. Rebuilt only when [`RowsKey`] changes.
    pub(crate) rows: Rc<Vec<DiffRow>>,
    rows_key: Option<RowsKey>,
    /// The [`ScmData`](crate::terminal::git_data::ScmData) epoch this patch was
    /// read at, for the two sources that can go stale.
    ///
    /// Recorded when a probe *starts*, so a `git add` that lands while one is
    /// running is not mistaken for a change the result already reflects.
    /// `None` until the first snapshot arrives: the epoch is keyed by the
    /// repository root, and only a snapshot knows where that is.
    pub(crate) epoch: Option<u64>,
}

/// Paints the full-window diff surface without inheriting workspace opacity.
/// The preset's solid or gradient design remains intact, but neither it nor
/// the plain theme fallback may reveal the OS backdrop through diff text.
fn diff_overlay_background(
    active: Option<&crate::ui::presets::ActiveBackground>,
    fallback: Hsla,
) -> Background {
    match active {
        Some(bg) => crate::ui::theme::window_background_opaque(bg),
        None => fallback.alpha(1.0).into(),
    }
}

impl Tty7App {
    pub(crate) fn toggle_diff_overlay(
        &mut self,
        host: crate::ui::host_ops::HostId,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_diff_overlay_at(host, cwd, None, window, cx)
    }

    pub(crate) fn toggle_diff_overlay_at(
        &mut self,
        host: crate::ui::host_ops::HostId,
        cwd: PathBuf,
        focus: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The sidebar's `+N −M` is `diff --numstat HEAD`, so opening it has
        // to show the same span. The panel names its own source per group.
        self.open_diff_overlay(host, cwd, DiffSource::Head, focus, window, cx)
    }

    pub(crate) fn open_diff_overlay(
        &mut self,
        host: crate::ui::host_ops::HostId,
        cwd: PathBuf,
        source: DiffSource,
        focus: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = self.active;
        let was_front = self.tabs.get(active).is_some_and(|t| {
            t.overlay_top == crate::ui::app::OverlayTop::Diff || !self.code_panel_visible()
        });
        if let Some(tab) = self.tabs.get_mut(active) {
            tab.overlay_top = crate::ui::app::OverlayTop::Diff;
        }
        // The source belongs in this filter: an open worktree overlay reused
        // for a staged file would only move the focus, and go on showing the
        // unstaged patch under the staged file's name.
        match self
            .tabs
            .get_mut(active)
            .and_then(|t| t.diff_overlay.as_mut())
            .filter(|o| o.cwd == cwd && o.host_id == host && o.source == source)
        {
            Some(o) if o.focus == focus && was_front => {
                self.close_diff_overlay(window, cx);
                return;
            }
            Some(o) => {
                o.focus = focus;
                // Another file is on screen now; the range belonged to the
                // one that left.
                o.selection = None;
                o.selecting = false;
                let handle = o.focus_handle.clone();
                window.focus(&handle, cx);
                cx.notify();
                return;
            }
            None => {}
        }
        self.remember_active_pane(window, cx);
        let Some(tab) = self.tabs.get_mut(active) else {
            return;
        };
        let focus_handle = cx.focus_handle();
        tab.diff_overlay = Some(DiffOverlayState {
            host_id: host,
            cwd,
            source,
            focus_handle: focus_handle.clone(),
            // Every open starts at Loading until its own probe lands. The old
            // panel-snapshot seeding died with the panel that held a snapshot
            // per source; re-seeding would need the caller to carry one.
            load: DiffLoad::Loading,
            loading: false,
            expanded: HashMap::new(),
            focus,
            preview: None,
            preview_loading: None,
            selection: None,
            selecting: false,
            list: gpui::ListState::new(0, gpui::ListAlignment::Top, px(256.))
                .with_size_hint(DIFF_LINE_H),
            rows: Rc::new(Vec::new()),
            rows_key: None,
            epoch: None,
        });
        window.focus(&focus_handle, cx);
        self.spawn_diff_probe(cx);
        cx.notify();
    }

    /// Which file the open overlay is focused on — for the row that asked,
    /// which means the *source* has to match too: a file staged and edited
    /// again sits in two panel groups, and only the row whose patch is
    /// actually on screen may draw itself selected.
    pub(crate) fn diff_overlay_focus(
        &self,
        host: crate::ui::host_ops::HostId,
        cwd: &std::path::Path,
        source: &crate::terminal::git_diff::DiffSource,
    ) -> Option<&str> {
        let overlay = self.tabs.get(self.active)?.diff_overlay.as_ref()?;
        (overlay.cwd == cwd && overlay.host_id == host && overlay.source == *source)
            .then_some(overlay.focus.as_deref())?
    }

    pub(crate) fn close_diff_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.active;
        let taken = self
            .tabs
            .get_mut(active)
            .and_then(|t| t.diff_overlay.take());
        if taken.is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// Begin a drag at `at`, in the column it was pressed in.
    fn start_diff_selection(
        &mut self,
        at: &RowAt,
        mode: DiffViewMode,
        side: Option<Side>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = self.active;
        let Some(overlay) = self
            .tabs
            .get_mut(active)
            .and_then(|t| t.diff_overlay.as_mut())
        else {
            return;
        };
        overlay.selection = Some(DiffSelection {
            path: at.path.to_string(),
            mode,
            side,
            anchor: at.id,
            head: at.id,
        });
        overlay.selecting = true;
        let handle = overlay.focus_handle.clone();
        // Copying needs the overlay to hold the keyboard. Docked beside a
        // shell it often does not, and Ctrl+C would otherwise reach the pane
        // and interrupt whatever is running in it.
        window.focus(&handle, cx);
        cx.notify();
    }

    /// Extend the drag in flight to `at`.
    ///
    /// `held` is what the pointer is still pressing. A move with nothing held
    /// means the button came up somewhere the overlay never saw — over a pane,
    /// or outside the window — so the drag ends here rather than resuming the
    /// next time the pointer wanders back over a row.
    fn extend_diff_selection(
        &mut self,
        at: &RowAt,
        side: Option<Side>,
        held: Option<MouseButton>,
        cx: &mut Context<Self>,
    ) {
        let active = self.active;
        let Some(overlay) = self
            .tabs
            .get_mut(active)
            .and_then(|t| t.diff_overlay.as_mut())
        else {
            return;
        };
        if !overlay.selecting {
            return;
        }
        if held != Some(MouseButton::Left) {
            overlay.selecting = false;
            cx.notify();
            return;
        }
        let Some(sel) = overlay.selection.as_mut() else {
            return;
        };
        if sel.path != at.path.as_ref() || sel.side != side || sel.head == at.id {
            return;
        }
        sel.head = at.id;
        cx.notify();
    }

    fn end_diff_selection(&mut self, cx: &mut Context<Self>) {
        let active = self.active;
        if let Some(overlay) = self
            .tabs
            .get_mut(active)
            .and_then(|t| t.diff_overlay.as_mut())
            && overlay.selecting
        {
            overlay.selecting = false;
            cx.notify();
        }
    }

    /// Put the selected rows on the clipboard, as the file spells them.
    fn copy_diff_selection(&self, cx: &mut Context<Self>) {
        let Some(overlay) = self
            .tabs
            .get(self.active)
            .and_then(|t| t.diff_overlay.as_ref())
        else {
            return;
        };
        let Some(sel) = overlay.selection.as_ref() else {
            return;
        };
        let hunks = match &overlay.load {
            DiffLoad::Ready(snap) => snap
                .files
                .iter()
                .find(|f| f.path == sel.path)
                .map(|f| f.hunks.as_slice()),
            _ => None,
        }
        // An untracked file has no patch in the snapshot — its rows are
        // synthesized from the file's own bytes, and so is its text.
        .or_else(|| match &overlay.preview {
            Some((held, Some(file))) if *held == sel.path => Some(file.hunks.as_slice()),
            _ => None,
        });
        let Some(hunks) = hunks else {
            return;
        };
        let text = sel.text(hunks);
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
    }

    fn spawn_diff_probe(&mut self, cx: &mut Context<Self>) {
        let active = self.active;
        let Some(overlay) = self.tabs.get(active).and_then(|t| t.diff_overlay.as_ref()) else {
            return;
        };
        if overlay.loading {
            return;
        }
        let cwd = overlay.cwd.clone();
        let source = overlay.source.clone();
        let id = overlay.host_id;
        // Read before the probe is dispatched, not after it lands: anything
        // bumped in between belongs to the next read, not this one.
        let epoch = match &overlay.load {
            DiffLoad::Ready(snap) => Some(scm_epoch(cx, id, &snap.root)),
            _ => None,
        };
        let Some(host) = crate::ui::host_registry::HostRegistry::lookup(cx, id) else {
            return;
        };
        let Some(overlay) = self
            .tabs
            .get_mut(active)
            .and_then(|t| t.diff_overlay.as_mut())
        else {
            return;
        };
        overlay.loading = true;
        overlay.epoch = epoch;
        self.spawn_diff_probe_for(host, cwd, source, cx);
    }

    pub(crate) fn spawn_diff_probe_for(
        &mut self,
        host: crate::ui::host_ops::SharedHost,
        cwd: PathBuf,
        source: DiffSource,
        cx: &mut Context<Self>,
    ) {
        let key = probe_key(host.id(), &cwd, &source);
        if !self.diff_probes_inflight.insert(key.clone()) {
            self.diff_probes_restale.insert(key);
            return;
        }
        let host_for_retry = host.clone();
        let probe_cwd = cwd.clone();
        let probe_source = source.clone();
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            move |h| {
                let req = git_diff::DiffRequest {
                    source: probe_source,
                    ..Default::default()
                };
                git_diff::probe_diff(h, &probe_cwd, &req)
            },
            move |app, result, cx| {
                let id = key.0;
                app.diff_probes_inflight.remove(&key);
                app.install_diff_snapshot(id, &cwd, &source, result.map(Arc::new), cx);
                if app.diff_probes_restale.remove(&key) {
                    app.spawn_diff_probe_for(host_for_retry, cwd, source, cx);
                }
            },
        );
    }

    fn install_diff_snapshot(
        &mut self,
        host: crate::ui::host_ops::HostId,
        cwd: &Path,
        source: &DiffSource,
        snap: Option<Arc<DiffSnapshot>>,
        cx: &mut Context<Self>,
    ) {
        // A diff read is a fresher answer to the question the sidebar's branch
        // and +N −N ask, and it is the one the reader is looking at. Hand the
        // branch and the numbers back before anything renders, or the row can
        // disagree with the overlay it just opened — and `maybe_refresh` reads
        // that disagreement as a reason to probe again.
        //
        // A read that failed is not an answer at all: its totals are whatever
        // got parsed before git gave up, usually zero. Publishing those wipes
        // the counts the sidebar already had right — the overlay says so in
        // words a few lines below, and the row would silently disagree.
        let mut landed = if let Some(snap) = snap.as_ref().filter(|s| !s.read_failed) {
            // Only a HEAD snapshot counts what the sidebar counts. A worktree
            // or staged patch is a smaller answer to a different question, and
            // a commit or a range is not about the working tree at all.
            let counts = matches!(snap.source, DiffSource::Head).then(|| snap.totals());
            let root = snap.root.clone();
            let branch = snap.branch.clone();
            cx.default_global::<crate::terminal::git_status::GitStatusCache>();
            cx.update_global::<crate::terminal::git_status::GitStatusCache, _>(|cache, _| {
                cache.note_diff_read(host, &root, &branch, counts)
            })
        } else {
            false
        };
        // Only wanted by an overlay whose first probe could not know the root,
        // and so could not read its own epoch before dispatching.
        let landing_epoch = snap.as_ref().map(|s| scm_epoch(cx, host, &s.root));
        for tab in self.tabs.iter_mut() {
            let Some(overlay) = tab
                .diff_overlay
                .as_mut()
                .filter(|o| o.cwd == cwd && o.host_id == host && o.source == *source)
            else {
                continue;
            };
            overlay.loading = false;
            overlay.epoch = overlay.epoch.or(landing_epoch);
            overlay.load = match &snap {
                Some(snap) => DiffLoad::Ready(Arc::clone(snap)),
                None => DiffLoad::NotARepo,
            };
            // A new snapshot restarts any untracked preview: the file may
            // have changed with the tree, and the re-read costs one file.
            overlay.preview = None;
            // The rows it was drawn against are gone. A range that survived
            // would keep its coordinates and quietly cover other code.
            overlay.selection = None;
            overlay.selecting = false;
            landed = true;
        }
        if landed {
            cx.notify();
        }
    }

    pub(crate) fn maybe_refresh_diff_overlay(&mut self, cx: &mut Context<Self>) {
        let Some(overlay) = self
            .tabs
            .get(self.active)
            .and_then(|t| t.diff_overlay.as_ref())
        else {
            return;
        };
        if overlay.loading {
            return;
        }
        let DiffLoad::Ready(snap) = &overlay.load else {
            return;
        };
        let stale = match overlay.source {
            // A commit and a range are fixed patches. Nothing can make either
            // of them out of date, so nothing should reprobe them.
            DiffSource::Commit { .. } | DiffSource::Range { .. } => return,
            // The cached counts come from `git diff --numstat HEAD`, so only a
            // HEAD snapshot is comparable to them.
            DiffSource::Head => {
                let Some(status) = cx
                    .try_global::<crate::terminal::git_status::GitStatusCache>()
                    .and_then(|cache| cache.status_for(overlay.host_id, &overlay.cwd))
                else {
                    return;
                };
                status.branch != snap.branch || (status.added, status.removed) != snap.totals()
            }
            // Those same counts would differ from a staged or unstaged patch
            // the moment anything is staged, and the overlay would reprobe
            // forever. The epoch answers the question that was actually being
            // asked — "did anything happen to this repository" — without
            // knowing what either side is counting.
            DiffSource::Worktree | DiffSource::Staged => {
                let Some(seen) = overlay.epoch else {
                    return;
                };
                scm_epoch(cx, overlay.host_id, &snap.root) != seen
            }
        };
        if stale {
            self.spawn_diff_probe(cx);
        }
    }

    pub(crate) fn render_diff_overlay(
        &mut self,
        chrome: DocumentChrome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.spawn_untracked_preview_if_needed(cx);
        let body = self.sync_diff_rows(cx)?;
        let overlay = self.tabs.get(self.active)?.diff_overlay.as_ref()?;

        let content = match body {
            DiffBody::Message(text) => self.diff_message(text, cx),
            DiffBody::Rows(snap) => self.diff_rows_list(overlay, snap, cx),
        };

        let header = chrome
            .renders_own_header()
            .then(|| self.diff_header(overlay, chrome, window, cx));
        let focus_handle = overlay.focus_handle.clone();

        let shell = v_flex();
        let shell = match chrome {
            DocumentChrome::Fill => shell
                .absolute()
                .inset_0()
                .occlude()
                // Opaque on purpose: this overlay covers the entire workspace,
                // so window translucency and backdrop material must stop here.
                .bg(diff_overlay_background(
                    cx.try_global::<crate::ui::presets::ActiveBackground>(),
                    cx.theme().background,
                ))
                // The opaque fill above covers the theme background image the
                // workspace root paints, so the overlay carries its own copy,
                // dimmed back to the strength it had when this overlay was
                // itself translucent.
                .children(crate::ui::app::overlay_surface_layers(cx)),
            // Docked, the column wrapper has already painted the surface this
            // sits on — the same one the right panel uses — and nothing behind
            // it needs stopping.
            DocumentChrome::Dock | DocumentChrome::DockHoisted => shell.size_full().min_w_0(),
        };
        Some(
            shell
                .text_color(cx.theme().foreground)
                .track_focus(&focus_handle)
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                    if ev.keystroke.key.as_str() == "escape" {
                        this.close_diff_overlay(window, cx);
                    }
                    // The overlay takes focus when a row is dragged, so this is
                    // the copy key for the selection that drag made — and only
                    // then: with nothing selected it falls through to whatever
                    // else the window binds it to.
                    let mods = ev.keystroke.modifiers;
                    if ev.keystroke.key.as_str() == "c" && mods.secondary() && !mods.alt {
                        this.copy_diff_selection(cx);
                    }
                }))
                // A drag that ends anywhere in the overlay ends here; one that
                // ends outside it is caught by the next move over a row, which
                // sees no button held.
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _window, cx| this.end_diff_selection(cx)),
                )
                .children(header)
                .child(content)
                .into_any_element(),
        )
    }

    /// The diff header alone, for the strip above a docked column.
    pub(crate) fn render_diff_header_only(
        &mut self,
        chrome: DocumentChrome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let overlay = self.tabs.get(self.active)?.diff_overlay.as_ref()?;
        Some(
            self.diff_header(overlay, chrome, window, cx)
                .into_any_element(),
        )
    }

    fn diff_header(
        &self,
        overlay: &DiffOverlayState,
        chrome: DocumentChrome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let (branch, files, untracked, added, removed) = match &overlay.load {
            DiffLoad::Ready(s) => {
                let stats = s.stats();
                let (a, r) = stats.totals;
                (s.branch.clone(), s.files.len(), stats.untracked_count, a, r)
            }
            _ => (String::new(), 0, 0, 0, 0),
        };
        // See `render_editor_header`: the traffic-light inset belongs to a
        // header that starts at the window's left edge, which a column's does
        // not.
        let lead = if self.left_panel_open(cx) || chrome.is_dock() {
            crate::ui::app::CONTENT_INSET
        } else {
            crate::ui::app::TITLE_BAR_LEAD
        };
        let mono = SharedString::from(self.font_family.clone());
        let subject = source_subject(&overlay.source, branch);
        let subject_takes_the_slack =
            chrome.is_dock() && !subject.is_rev && subject.label.is_none();
        let menu_app = cx.entity().downgrade();
        let row = h_flex().id("diff-overlay-header");
        let row = if chrome.header_is_title_strip() {
            crate::ui::app::title_bar_drag(row, "diff-overlay-header", window, cx)
        } else {
            row
        };
        row.flex_shrink_0()
            .h(px(crate::ui::app::TITLE_BAR_HEIGHT))
            .pl(px(lead))
            .pr(px(crate::ui::app::tile_trailing_inset()))
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                gpui::svg()
                    .path(subject.icon)
                    .flex_shrink_0()
                    .size(px(13.))
                    .text_color(cx.theme().muted_foreground),
            )
            .child(if subject.is_rev {
                // A revision is an identifier, not a name: it belongs in the
                // same monospace the patch below it is set in.
                div()
                    .flex_shrink_0()
                    .text_size(px(13.))
                    .font_family(self.font_family.clone())
                    .child(subject.text)
                    .into_any_element()
            } else {
                // Docked, this is the name that gives: the header has a
                // column's width rather than a window's, and a branch name that
                // refused to yield any of it pushed the view toggle and the
                // close tile off the end. It takes the slack the spacer below
                // would otherwise have — the same trade the label branch makes,
                // and for the same reason two `flex_1` siblings would split the
                // line and truncate the name with empty space beside it.
                div()
                    .when(subject_takes_the_slack, |d| d.flex_1().min_w_0().truncate())
                    .when(!subject_takes_the_slack, |d| d.flex_shrink_0())
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(subject.text)
                    .into_any_element()
            })
            .when_some(subject.chip, |bar, text| {
                bar.child(info_chip(
                    text,
                    cx.theme().accent.opacity(0.16),
                    cx.theme().foreground,
                    &mono,
                ))
            })
            // The subject takes the slack the spacer below would otherwise
            // have, which is why that one is skipped when a label is present:
            // two `flex_1` siblings split the line in half and the subject
            // would truncate with empty space beside it.
            .when_some(subject.label.as_ref(), |bar, label| {
                bar.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .child(SharedString::from(label.subject.clone())),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(label_byline(label, now_unix())),
                )
            })
            .when_some(focused_name(overlay), |bar, name| {
                bar.child(
                    div().occlude().flex_shrink_0().child(
                        h_flex()
                            .id("diff-overlay-unfocus")
                            .items_center()
                            .gap_1()
                            .px_1p5()
                            .py_0p5()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().list_hover))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                let active = this.active;
                                if let Some(overlay) = this
                                    .tabs
                                    .get_mut(active)
                                    .and_then(|t| t.diff_overlay.as_mut())
                                {
                                    overlay.focus = None;
                                    cx.notify();
                                }
                            }))
                            .child(
                                Icon::new(IconName::ChevronLeft)
                                    .small()
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .font_family(self.font_family.clone())
                                    .child(name),
                            ),
                    ),
                )
            })
            .when(
                matches!(overlay.load, DiffLoad::Ready(_)) && overlay.focus.is_none(),
                |bar| {
                    let mut summary = t_plural(L10nKey::DiffChangedFiles, files, &[]);
                    if untracked > 0 {
                        summary.push_str(&t_plural(L10nKey::DiffUntrackedCount, untracked, &[]));
                    }
                    // The file count is the first thing a column drops: the
                    // same number is one line down, at the top of the list.
                    // The totals stay — they have no second home.
                    bar.when(!chrome.is_dock(), |bar| {
                        bar.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(summary),
                        )
                    })
                    .when(added > 0, |bar| {
                        bar.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().success)
                                .child(format!("+{added}")),
                        )
                    })
                    .when(removed > 0, |bar| {
                        bar.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .child(format!("−{removed}")),
                        )
                    })
                },
            )
            .when(
                overlay.loading && matches!(overlay.load, DiffLoad::Ready(_)),
                |bar| {
                    bar.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(t(L10nKey::Refreshing)),
                    )
                },
            )
            .when(subject.label.is_none() && !subject_takes_the_slack, |bar| {
                bar.child(div().flex_1())
            })
            .child(
                div()
                    .occlude()
                    .flex_shrink_0()
                    .child(self.diff_view_switch(cx)),
            )
            .child(
                div().occlude().flex_shrink_0().child(
                    crate::ui::tab_strip::chrome_tile_sized(
                        Button::new("diff-overlay-close").icon(Icon::new(IconName::Close)),
                        crate::ui::app::TILE_SIZE,
                        crate::ui::app::TILE_GLYPH_LINE,
                        false,
                        cx,
                    )
                    .rounded_lg()
                    .tooltip(t(L10nKey::DiffCloseTooltip))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_diff_overlay(window, cx);
                    })),
                ),
            )
            .context_menu(move |menu, _window, cx| {
                Tty7App::document_header_menu(menu, &menu_app, cx)
            })
    }

    /// The two views, as a switch rather than a control.
    ///
    /// Not [`Tty7App::segmented_on`]: that one is a bordered track, which is
    /// right in a settings row, where it ends a line of prose and has to
    /// announce itself as something you operate. On a title bar it was the
    /// only bordered thing on the strip — the close tile beside it is a bare
    /// glyph, and so is every tile at the other end of the window — so it read
    /// as pasted on. Same two choices, no frame: the live one carries a soft
    /// fill, the other is quiet text that lights up under the pointer.
    fn diff_view_switch(&self, cx: &mut Context<Self>) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().window;
        let current = view_mode(cx);
        let cells = [
            (DiffViewMode::Split, t(L10nKey::DiffViewSplit)),
            (DiffViewMode::Unified, t(L10nKey::DiffViewUnified)),
        ];
        h_flex()
            .id("diff-overlay-view")
            .flex_shrink_0()
            .gap(px(2.))
            .children(cells.into_iter().enumerate().map(|(i, (mode, label))| {
                let live = mode == current;
                h_flex()
                    .id(("diff-overlay-view-cell", i))
                    .items_center()
                    .h(px(22.))
                    .px(px(8.))
                    .rounded(ROW_RADIUS)
                    .text_sm()
                    .cursor_pointer()
                    .when(live, |cell| {
                        cell.bg(gpui::rgb(sf.selected))
                            .text_color(gpui::rgb(sf.text_selected))
                            .font_weight(FontWeight::MEDIUM)
                    })
                    .when(!live, |cell| {
                        cell.text_color(cx.theme().muted_foreground)
                            .hover(|h| h.bg(gpui::rgb(sf.hover)))
                    })
                    .active(|cell| cell.bg(gpui::rgb(sf.pressed)))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.update_config(cx, |cfg| cfg.diff_view = mode);
                    }))
                    .child(label)
            }))
            .into_any_element()
    }

    /// Dispatch the byte read behind an untracked file's preview, at most
    /// once per (path, snapshot). Runs from `render`, so the guards are the
    /// point: `preview` says the answer is in hand, `preview_loading` says it
    /// is on the way.
    fn spawn_untracked_preview_if_needed(&mut self, cx: &mut Context<Self>) {
        let want = {
            let overlay = self
                .tabs
                .get(self.active)
                .and_then(|t| t.diff_overlay.as_ref());
            match overlay {
                Some(o) => match &o.load {
                    DiffLoad::Ready(snap) => {
                        untracked_focus(snap, o.focus.as_deref()).and_then(|path| {
                            let seen = o.preview.as_ref().is_some_and(|(held, _)| held == path)
                                || o.preview_loading.as_deref() == Some(path);
                            (!seen).then(|| (o.host_id, snap.root.clone(), path.to_string()))
                        })
                    }
                    _ => None,
                },
                None => None,
            }
        };
        let Some((host_id, root, path)) = want else {
            return;
        };
        let Some(host) = crate::ui::host_registry::HostRegistry::lookup(cx, host_id) else {
            return;
        };
        let active = self.active;
        if let Some(o) = self
            .tabs
            .get_mut(active)
            .and_then(|t| t.diff_overlay.as_mut())
        {
            o.preview_loading = Some(path.clone());
        }
        let read_path = root.join(&path);
        let key_path = path.clone();
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            move |h| {
                h.read_file(&read_path, MAX_PREVIEW_BYTES)
                    .ok()
                    .map(|bytes| {
                        Arc::new(git_diff::synthesize_added(
                            &path,
                            &bytes,
                            &git_diff::DiffBudget::SINGLE_FILE,
                        ))
                    })
            },
            move |this, file, cx| {
                let active = this.active;
                let Some(o) = this
                    .tabs
                    .get_mut(active)
                    .and_then(|t| t.diff_overlay.as_mut())
                    .filter(|o| o.host_id == host_id)
                else {
                    return;
                };
                if o.preview_loading.as_deref() == Some(key_path.as_str()) {
                    o.preview_loading = None;
                }
                o.preview = Some((key_path.clone(), file));
                cx.notify();
            },
        );
    }

    fn diff_message(&self, text: &'static str, cx: &Context<Self>) -> AnyElement {
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(text)
            .into_any_element()
    }

    /// Brings the active overlay's flattened rows up to date with what it is
    /// meant to be showing, and says what to draw.
    ///
    /// Called from `render`, so the [`RowsKey`] comparison is what keeps it
    /// cheap: flattening a twenty-thousand-line patch allocates a row per
    /// line, and nothing about that changes between two frames of scrolling.
    fn sync_diff_rows(&mut self, cx: &mut Context<Self>) -> Option<DiffBody> {
        let mode = view_mode(cx);
        let active = self.active;
        let overlay = self.tabs.get_mut(active)?.diff_overlay.as_mut()?;

        // Forget a range the rows under it no longer answer to. The two views
        // pair the same lines differently, so a row range drawn in one of them
        // points at other code in the other. Here rather than beside the
        // switch that flips the mode: this is the one place that knows which
        // rows are about to be drawn.
        if overlay.selection.as_ref().is_some_and(|s| s.mode != mode) {
            overlay.selection = None;
            overlay.selecting = false;
        }

        let snap = match &overlay.load {
            DiffLoad::Loading => return Some(DiffBody::Message(t(L10nKey::DiffReading))),
            DiffLoad::NotARepo => return Some(DiffBody::Message(t(L10nKey::DiffNotARepo))),
            DiffLoad::Ready(snap) if empty_snapshot(snap) && snap.read_failed => {
                return Some(DiffBody::Message(t(L10nKey::DiffReadFailed)));
            }
            DiffLoad::Ready(snap) if empty_snapshot(snap) => {
                return Some(DiffBody::Message(t(L10nKey::DiffWorkingTreeClean)));
            }
            DiffLoad::Ready(snap) => Arc::clone(snap),
        };

        // A focused *untracked* file has no patch in the snapshot; its card is
        // synthesized from the file's own bytes — see `preview`.
        let preview = match untracked_focus(&snap, overlay.focus.as_deref()) {
            Some(path) => match &overlay.preview {
                Some((held, file)) if held == path => match file {
                    Some(file) => Some(Arc::clone(file)),
                    None => return Some(DiffBody::Message(t(L10nKey::DiffReadFailed))),
                },
                _ => return Some(DiffBody::Message(t(L10nKey::DiffReading))),
            },
            None => None,
        };

        let focused = focused_file(&snap, overlay);
        let from = RowsFrom {
            snap: &snap,
            preview: preview.as_ref(),
            mode,
            focused,
            oversized: focused.is_none() && snap.stats().oversized,
            expanded: &overlay.expanded,
        };
        let stale = overlay
            .rows_key
            .as_ref()
            .is_none_or(|held| !held.describes(&from));
        if stale {
            let rows = match from.preview {
                Some(file) => crate::ui::diff_list::preview_rows(file, from.mode),
                None => crate::ui::diff_list::build_rows(
                    from.snap,
                    from.expanded,
                    from.focused,
                    from.mode,
                    from.oversized,
                ),
            };
            let key = from.to_key();
            resync_list(&overlay.list, &overlay.rows, &rows);
            overlay.rows = Rc::new(rows);
            overlay.rows_key = Some(key);
        } else if let Some(held) = overlay.rows_key.as_mut() {
            held.retarget(&from);
        }
        Some(DiffBody::Rows(snap))
    }

    /// The rows, in the virtualised list that draws only the visible ones.
    fn diff_rows_list(
        &self,
        overlay: &DiffOverlayState,
        snap: Arc<DiffSnapshot>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = Rc::clone(&overlay.rows);
        let font = SharedString::from(self.font_family.clone());
        let app = cx.entity().downgrade();
        let list = overlay.list.clone();
        // The selection is read here, once a frame, rather than keyed into
        // `RowsKey`: it changes what a row *looks like*, not which rows there
        // are, and re-flattening the patch for every step of a drag is the
        // cost this list exists to avoid. `extend_diff_selection` notifies,
        // the view renders, and the list rebuilds the rows on screen from the
        // `Drag` this frame carries.
        let drag = Drag {
            sel: overlay.selection.clone().map(Rc::new),
            selecting: overlay.selecting,
            mode: view_mode(cx),
        };
        let body = gpui::list(list.clone(), move |ix, _window, cx| {
            #[cfg(test)]
            row_probe::record();
            match rows.get(ix) {
                Some(row) => diff_row_element(row, ix, &drag, &font, &snap, &app, cx),
                // The list is spliced in step with `rows`, so this is
                // unreachable — and an empty row is a better answer to a bug
                // than an index panic in a paint.
                None => div().into_any_element(),
            }
        })
        .size_full()
        // Only the vertical padding: `List` lays every item out at its own
        // full width and puts it at its own left edge, so a horizontal
        // padding here would be silently ignored. The rows carry their own —
        // see `diff_row_element`.
        .py_4();
        // The bar reads the list's own height, and a list only counts the
        // rows it has measured. Left at that, a patch of any length would
        // report itself as one screen long and the thumb would fill the
        // track: a drag from top to bottom would travel one screen and stop,
        // on the one document in the app long enough to need the bar. The
        // rows below the fold are counted at `DIFF_LINE_H` until they are
        // laid out — `ListState::measure_all` would settle it exactly, by
        // laying out every row on the first frame, which is the cost this
        // whole list exists to avoid.
        crate::ui::scrollbar::with_vertical_scrollbar("diff-overlay-scrollbar", body, &list)
    }
}

/// Counts the rows the list actually built, so a test can tell that a patch of
/// any size costs the handful of rows on screen rather than all of them.
#[cfg(test)]
pub(crate) mod row_probe {
    use std::cell::Cell;

    thread_local! {
        static BUILT: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn record() {
        BUILT.set(BUILT.get() + 1);
    }

    /// The count since the last call, and zero from here.
    pub(crate) fn take() -> u64 {
        BUILT.replace(0)
    }
}

/// What a row needs to take part in a drag, for the frame it is drawn in.
///
/// Read off the overlay once and moved into the list's item builder, so a step
/// of a drag costs a refcount bump rather than a walk of the patch.
struct Drag {
    sel: Option<Rc<DiffSelection>>,
    /// Whether a drag is in flight. Rows only listen for pointer movement
    /// while one is: a diff runs to thousands of rows, and a listener each is
    /// worth paying for during a drag and not otherwise.
    selecting: bool,
    /// The view the rows on screen are drawn in, which is the view a press
    /// starts its selection in.
    mode: DiffViewMode,
}

impl Drag {
    /// Whether the selection covers this cell. `side` names the column a split
    /// cell sits in, and is `None` for a unified row — a selection made in one
    /// column never lights up the other.
    fn covers(&self, at: &RowAt, side: Option<Side>) -> bool {
        self.sel
            .as_ref()
            .is_some_and(|sel| sel.covers(at.path.as_ref(), at.id, side))
    }

    /// Whether this row is inside the selection at all, whichever column the
    /// drag ran down. What decides whether the row offers to copy it.
    fn holds(&self, at: &RowAt) -> bool {
        self.sel
            .as_ref()
            .is_some_and(|sel| sel.covers(at.path.as_ref(), at.id, sel.side))
    }
}

/// What the overlay's scrolling area holds this frame.
enum DiffBody {
    Message(&'static str),
    /// The rows are in [`DiffOverlayState::rows`]; the snapshot rides along
    /// for the few rows whose text is derived from it.
    Rows(Arc<DiffSnapshot>),
}

/// What this frame would flatten its rows from, borrowed from the overlay.
struct RowsFrom<'a> {
    snap: &'a Arc<DiffSnapshot>,
    preview: Option<&'a Arc<FileDiff>>,
    mode: DiffViewMode,
    focused: Option<usize>,
    oversized: bool,
    expanded: &'a HashMap<String, bool>,
}

impl RowsFrom<'_> {
    fn to_key(&self) -> RowsKey {
        RowsKey {
            snap: Arc::clone(self.snap),
            preview: self.preview.cloned(),
            mode: self.mode,
            focused: self.focused,
            oversized: self.oversized,
            expanded: self.expanded.clone(),
        }
    }
}

/// What [`DiffOverlayState::rows`] was flattened from, kept so the next frame
/// can tell whether it would flatten the same rows again.
struct RowsKey {
    snap: Arc<DiffSnapshot>,
    preview: Option<Arc<FileDiff>>,
    mode: DiffViewMode,
    focused: Option<usize>,
    oversized: bool,
    expanded: HashMap<String, bool>,
}

impl RowsKey {
    /// Whether the rows built from `from` would be the rows already held.
    ///
    /// The snapshot is compared by pointer first and by contents second: a
    /// probe that found nothing new still lands a fresh `Arc` over an equal
    /// snapshot, and rebuilding every row of the patch for that would undo the
    /// point of keeping them.
    ///
    /// The scalars go first so that the walk of the patch behind that second
    /// comparison is only ever paid to answer a question the cheap fields
    /// have not already answered.
    fn describes(&self, from: &RowsFrom<'_>) -> bool {
        self.mode == from.mode
            && self.focused == from.focused
            && self.oversized == from.oversized
            && self.expanded == *from.expanded
            && self.same_preview(from)
            && (Arc::ptr_eq(&self.snap, from.snap) || self.snap == *from.snap)
    }

    /// The preview, by pointer and then by contents — a re-read of an
    /// untracked file lands a fresh `Arc` over bytes that did not change.
    fn same_preview(&self, from: &RowsFrom<'_>) -> bool {
        match (&self.preview, from.preview) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a == b,
            _ => false,
        }
    }

    /// Points the key at the `Arc`s this frame was asked about, having just
    /// found them equal to the ones held.
    ///
    /// Without this the key goes on holding the snapshot from the last
    /// *rebuild*, so every frame after a probe that found nothing new proves
    /// the two equal the long way — a walk of every line of the patch, once
    /// per wheel event, which is the cost this key exists to avoid.
    fn retarget(&mut self, from: &RowsFrom<'_>) {
        self.snap = Arc::clone(from.snap);
        self.preview = from.preview.cloned();
    }
}

/// Tells the list which rows changed, rather than that all of them did.
///
/// `ListState::reset` would drop the scroll position, so collapsing one file
/// would throw the reader back to the top of the tree. The rows either side of
/// an edit are untouched, so the shared prefix and suffix are kept and only
/// what is between them is spliced.
fn resync_list(list: &gpui::ListState, old: &[DiffRow], new: &[DiffRow]) {
    let (replaced, with) = spliced_range(old, new);
    list.splice(replaced, with);
}

/// Which of the old rows were replaced, and by how many new ones.
fn spliced_range(old: &[DiffRow], new: &[DiffRow]) -> (std::ops::Range<usize>, usize) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    // Whatever is left of the shorter list once the shared head is off it —
    // the most the shared tail can be, and what keeps the two slices below in
    // step with each other.
    let rest = old.len().min(new.len()) - prefix;
    let suffix = old[old.len() - rest..]
        .iter()
        .rev()
        .zip(new[new.len() - rest..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    (prefix..old.len() - suffix, new.len() - prefix - suffix)
}

/// The row inset every row of the list shares, matching the source control
/// panel's — the overlay is a second view of that panel's list, and the two
/// stopped looking like one app when this one drew cards.
const ROW_INSET: Pixels = px(10.);

/// The height of a row that is a *file* rather than a line of one: the same
/// 26px the panel gives its file rows.
const FILE_ROW_H: Pixels = px(26.);

/// The radius on a row that lights up under the pointer. Matches the panel's.
const ROW_RADIUS: Pixels = px(5.);

/// The height of one line of a patch, in either view.
///
/// Also what the list counts a row it has not laid out yet at. A list knows
/// only the rows it has measured, so without an estimate for the rest a patch
/// of any length reports itself as one screen long — and the scrollbar, which
/// reads that height, drags the reader one screen and stops. The file and hunk
/// rows are a few pixels taller, so the estimate runs a little short until
/// they have been measured; a diff is overwhelmingly its lines.
const DIFF_LINE_H: Pixels = px(19.);

/// The rule between one hunk and the last line of the one before it.
///
/// Barely there on purpose: with the cards gone it is the only line left in
/// the list, and it is separating two parts of one file rather than two
/// files.
fn hunk_rule(cx: &gpui::App) -> Hsla {
    cx.theme().border.opacity(0.6)
}

/// One row, inset the way every row in the list is.
fn diff_row_element(
    row: &DiffRow,
    ix: usize,
    drag: &Drag,
    font: &SharedString,
    snap: &Arc<DiffSnapshot>,
    app: &gpui::WeakEntity<Tty7App>,
    cx: &mut gpui::App,
) -> AnyElement {
    match row {
        // Stands in for the gap between the groups this list used to be a
        // flex column of.
        DiffRow::Gap => div().w_full().h(gpui::rems(0.75)).into_any_element(),
        DiffRow::Oversized => padded(diff_oversized_notice(snap, cx)),
        DiffRow::FileHeader(head) => padded(diff_file_header(head, font, app, cx)),
        DiffRow::HunkHeader { text, leads } => padded(
            div()
                .w_full()
                .px(ROW_INSET)
                .py_1()
                .when(!leads, |h| {
                    h.mt_1().border_t_1().border_color(hunk_rule(cx))
                })
                .text_xs()
                .font_family(font.clone())
                .text_color(cx.theme().muted_foreground)
                .truncate()
                .child(text.clone())
                .into_any_element(),
        ),
        // The lines run the full width of the list. A diff is read as a
        // column of code, and code that is inset from both sides reads as a
        // quotation of itself.
        DiffRow::Split { row, at } => copy_menu(
            diff_split_row(row, at, drag, font, app, cx),
            ix,
            at,
            drag,
            app,
        ),
        DiffRow::Unified { row, at } => copy_menu(
            diff_unified_row(row, at, drag, font, app, cx),
            ix,
            at,
            drag,
            app,
        ),
        DiffRow::Truncated(reason) => {
            let note = match reason {
                Truncation::PerFile => t_fmt(
                    L10nKey::DiffTruncatedPerFile,
                    &[("limit", &git_diff::MAX_LINES_PER_FILE.to_string())],
                ),
                Truncation::Budget => t(L10nKey::DiffTruncatedBudget).to_string(),
            };
            padded(note_row(note, cx))
        }
        DiffRow::MoreFiles { rest } => {
            padded(note_row(t_plural(L10nKey::DiffMoreFiles, *rest, &[]), cx))
        }
        // A section label, in the shape the sidebar gives its group headings:
        // small, quiet, and carried by the space around it rather than a bar
        // of its own.
        DiffRow::UntrackedHeader { total } => padded(
            div()
                .w_full()
                .px(ROW_INSET)
                .py_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(t_plural(L10nKey::DiffUntrackedHeader, *total, &[]))
                .into_any_element(),
        ),
        DiffRow::Untracked { index, path } => {
            padded(diff_untracked_row(*index, path, font, app, cx))
        }
        DiffRow::MoreUntracked { rest } => padded(note_row(
            t_plural(L10nKey::DiffMoreUntracked, *rest, &[]),
            cx,
        )),
    }
}

/// The one place a copy is offered by name, on the rows that would be copied.
///
/// A drag says what will be copied; the menu says that copying is a thing you
/// can do. It hangs on the selected rows themselves because with the cards
/// gone there is no longer an element that owns a file's lines, and the
/// overlay root already carries the header's own menu — two of them over one
/// right-click would open two popups.
fn copy_menu(
    row: gpui::Div,
    ix: usize,
    at: &RowAt,
    drag: &Drag,
    app: &gpui::WeakEntity<Tty7App>,
) -> AnyElement {
    if !drag.holds(at) {
        return row.into_any_element();
    }
    let app = app.clone();
    row.id(("diff-row-menu", ix))
        .context_menu(move |menu, _window, _cx| {
            menu.item(PopupMenuItem::new(t(L10nKey::DiffCopySelection)).on_click({
                let app = app.clone();
                move |_, _window, cx| {
                    app.update(cx, |this, cx| this.copy_diff_selection(cx)).ok();
                }
            }))
        })
        .into_any_element()
}

/// Wire one drawn row into the drag: a press starts a selection there, and
/// while one is in flight a move across the row extends it.
fn diff_row_drag<E: InteractiveElement + Styled>(
    el: E,
    at: &RowAt,
    side: Option<Side>,
    drag: &Drag,
    app: &gpui::WeakEntity<Tty7App>,
) -> E {
    let mode = drag.mode;
    // The I-beam is the only standing sign that this text can be taken;
    // nothing else about a row says so until one is dragged.
    let el = el.cursor_text().on_mouse_down(MouseButton::Left, {
        let (app, at) = (app.clone(), at.clone());
        move |_: &MouseDownEvent, window, cx| {
            app.update(cx, |this, cx| {
                this.start_diff_selection(&at, mode, side, window, cx);
            })
            .ok();
        }
    });
    if !drag.selecting {
        return el;
    }
    el.on_mouse_move({
        let (app, at) = (app.clone(), at.clone());
        move |ev: &MouseMoveEvent, _window, cx| {
            let held = ev.pressed_button;
            app.update(cx, |this, cx| {
                this.extend_diff_selection(&at, side, held, cx);
            })
            .ok();
        }
    })
}

/// The margin the file rows keep from the edge of the list.
fn padded(row: AnyElement) -> AnyElement {
    div().w_full().px_2().child(row).into_any_element()
}

/// An aside in the list's own voice — a cap that was hit, a tail that was not
/// drawn. Never a row you can act on, so never one that lights up.
fn note_row(text: String, cx: &gpui::App) -> AnyElement {
    div()
        .w_full()
        .px(ROW_INSET)
        .py_1()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element()
}

fn diff_oversized_notice(snap: &DiffSnapshot, cx: &gpui::App) -> AnyElement {
    let stats = snap.stats();
    let text = t_fmt(
        L10nKey::DiffOversizedNotice,
        &[("summary", &oversized_summary(snap, &stats))],
    );
    div()
        .w_full()
        .px(ROW_INSET)
        .py_2()
        .rounded(rounding::CARD_RADIUS)
        .bg(cx.theme().secondary)
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
        .into_any_element()
}

fn diff_file_header(
    head: &FileHead,
    font: &SharedString,
    app: &gpui::WeakEntity<Tty7App>,
    cx: &gpui::App,
) -> AnyElement {
    let hover = gpui::rgb(cx.global::<crate::ui::presets::Surfaces>().window.hover);
    let deco = deco_status(head.status);
    let (glyph, glyph_color) = (status_glyph(deco), status_color(deco, cx));
    let mut header = h_flex()
        .id(("diff-file-header", head.index))
        .w_full()
        .items_center()
        .gap_2()
        .h(FILE_ROW_H)
        .px(ROW_INSET)
        .rounded(ROW_RADIUS)
        .when(head.expandable, |h| {
            let path = head.path.clone();
            let want = !head.expanded;
            let app = app.clone();
            h.cursor_pointer()
                .hover(|s| s.bg(hover))
                .on_click(move |_, _window, cx| {
                    let path = path.clone();
                    app.update(cx, |this, cx| {
                        let active = this.active;
                        if let Some(overlay) = this
                            .tabs
                            .get_mut(active)
                            .and_then(|t| t.diff_overlay.as_mut())
                        {
                            overlay.expanded.insert(path, want);
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .child(
                    Icon::new(if head.expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .small()
                    .text_color(cx.theme().muted_foreground),
                )
        })
        .child(
            div()
                .flex_shrink_0()
                .font_family(font.clone())
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(glyph_color)
                .child(glyph),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_xs()
                .font_family(font.clone())
                .child(head.shown_path.clone()),
        );
    if head.binary {
        header = header.child(
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(t(L10nKey::Binary)),
        );
    }
    if head.added > 0 {
        header = header.child(
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(cx.theme().success)
                .child(format!("+{}", head.added)),
        );
    }
    if head.removed > 0 {
        header = header.child(
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(cx.theme().danger)
                .child(format!("−{}", head.removed)),
        );
    }
    header.into_any_element()
}

// An untracked file has no patch in the snapshot, so it cannot be expanded in
// place the way the files above it are — its contents are read one file at a
// time and shown on their own. The row asks for that read, which until now
// only the Source Control panel could: in the overlay these rows were the only
// files in a list of files that did nothing when clicked.
fn diff_untracked_row(
    index: usize,
    path: &str,
    font: &SharedString,
    app: &gpui::WeakEntity<Tty7App>,
    cx: &gpui::App,
) -> AnyElement {
    let hover = gpui::rgb(cx.global::<crate::ui::presets::Surfaces>().window.hover);
    let for_focus = path.to_string();
    let app = app.clone();
    h_flex()
        .id(("diff-untracked", index))
        .w_full()
        .items_center()
        .gap_2()
        .h(FILE_ROW_H)
        .px(ROW_INSET)
        .rounded(ROW_RADIUS)
        .text_xs()
        .font_family(font.clone())
        .cursor_pointer()
        .hover(|s| s.bg(hover))
        .on_click(move |_, window, cx| {
            let for_focus = for_focus.clone();
            app.update(cx, |this, cx| {
                let Some((host, cwd, source)) = this
                    .tabs
                    .get(this.active)
                    .and_then(|t| t.diff_overlay.as_ref())
                    .map(|o| (o.host_id, o.cwd.clone(), o.source.clone()))
                else {
                    return;
                };
                this.open_diff_overlay(host, cwd, source, Some(for_focus), window, cx);
            })
            .ok();
        })
        .child(
            div()
                .flex_shrink_0()
                .font_weight(FontWeight::BOLD)
                .text_color(status_color(DecoStatus::Untracked, cx))
                .child(status_glyph(DecoStatus::Untracked)),
        )
        .child(div().flex_1().min_w_0().truncate().child(path.to_string()))
        .into_any_element()
}

fn diff_split_row(
    row: &SplitRow,
    at: &RowAt,
    drag: &Drag,
    font: &SharedString,
    app: &gpui::WeakEntity<Tty7App>,
    cx: &gpui::App,
) -> gpui::Div {
    h_flex()
        .w_full()
        .h(DIFF_LINE_H)
        .items_stretch()
        .text_xs()
        .font_family(font.clone())
        .child(diff_split_cell(
            row.left.as_ref(),
            Side::Old,
            at,
            drag,
            app,
            cx,
        ))
        .child(div().flex_shrink_0().w(px(1.)).bg(hunk_rule(cx)))
        .child(diff_split_cell(
            row.right.as_ref(),
            Side::New,
            at,
            drag,
            app,
            cx,
        ))
}

fn diff_split_cell(
    cell: Option<&SplitCell>,
    side: Side,
    at: &RowAt,
    drag: &Drag,
    app: &gpui::WeakEntity<Tty7App>,
    cx: &gpui::App,
) -> AnyElement {
    let base = h_flex().flex_1().min_w_0().h_full().items_center();
    let Some(cell) = cell else {
        // Blank, but still this row's half of this column. Left inert it is a
        // dead band under the pointer — no I-beam, a press that starts
        // nothing, and a range that visibly stops at the padding and resumes
        // below it. A one-sided change is the ordinary shape of a diff, so
        // that band runs down most of one column.
        let fill = match drag.covers(at, Some(side)) {
            true => cx.theme().selection,
            false => cx.theme().muted.opacity(0.3),
        };
        return diff_row_drag(base, at, Some(side), drag, app)
            .bg(fill)
            .into_any_element();
    };
    let (marker, tint) = match (cell.changed, side) {
        (true, Side::Old) => ("−", Some(cx.theme().danger.opacity(0.12))),
        (true, Side::New) => ("+", Some(cx.theme().success.opacity(0.12))),
        (false, _) => (" ", None),
    };
    // A selected cell wears the theme's selection colour in place of its own
    // wash, the way selected text does anywhere else. The `+`/`−` in front of
    // the code still says which side of the change it is.
    let fill = match drag.covers(at, Some(side)) {
        true => Some(cx.theme().selection),
        false => tint,
    };
    diff_row_drag(base, at, Some(side), drag, app)
        .when_some(fill, |row, bg| row.bg(bg))
        .child(
            h_flex()
                .flex_shrink_0()
                .w(px(42.))
                .justify_end()
                .pr_1p5()
                .text_color(cx.theme().muted_foreground.opacity(0.7))
                .child(cell.no.map(|n| n.to_string()).unwrap_or_default()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(format!("{marker} {}", cell.text)),
        )
        .into_any_element()
}

/// One line of the unified view.
///
/// Every measurement it shares with [`diff_split_cell`] is shared on purpose —
/// the same 19px row, the same `text_xs` in the same family, and above all the
/// same `0.12` wash behind an addition and a removal. The two views are one
/// diff seen twice; a different green would read as a different thing.
///
/// What differs is forced by the shape. The line numbers get 34px a side
/// rather than 42 (there are two gutters here in front of one column of text,
/// not one in front of each), and the `+`/`−` gets a column of its own rather
/// than riding in the text: with three kinds of line stacked in one column, an
/// inlined marker would leave the context lines' code starting two characters
/// left of everything else.
fn diff_unified_row(
    row: &UnifiedRow,
    at: &RowAt,
    drag: &Drag,
    font: &SharedString,
    app: &gpui::WeakEntity<Tty7App>,
    cx: &gpui::App,
) -> gpui::Div {
    let (marker_color, tint) = match row.kind {
        LineKind::Added => (cx.theme().success, Some(cx.theme().success.opacity(0.12))),
        LineKind::Removed => (cx.theme().danger, Some(cx.theme().danger.opacity(0.12))),
        LineKind::Context => (cx.theme().muted_foreground, None),
    };
    let gutter = |no: Option<u32>| {
        h_flex()
            .flex_shrink_0()
            .w(px(34.))
            .justify_end()
            .pr_1p5()
            .text_color(cx.theme().muted_foreground.opacity(0.7))
            .child(no.map(|n| n.to_string()).unwrap_or_default())
    };
    let fill = match drag.covers(at, None) {
        true => Some(cx.theme().selection),
        false => tint,
    };
    diff_row_drag(h_flex(), at, None, drag, app)
        .w_full()
        .h(DIFF_LINE_H)
        .items_center()
        .text_xs()
        .font_family(font.clone())
        .when_some(fill, |line, bg| line.bg(bg))
        .child(gutter(row.old))
        .child(gutter(row.new))
        // The split view's centre rule, in the one place it still means the
        // same thing: everything left of it is a number, everything right of
        // it is the file.
        .child(div().flex_shrink_0().w(px(1.)).h_full().bg(hunk_rule(cx)))
        .child(
            div()
                .flex_shrink_0()
                .w(px(12.))
                .text_center()
                .text_color(marker_color)
                .child(unified_marker(row.kind)),
        )
        .child(div().flex_1().min_w_0().truncate().child(row.text.clone()))
}

/// Which layout the overlay draws. One setting for the window, not one per
/// overlay: VS Code's `diffEditor.renderSideBySide` is global for the same
/// reason — re-picking on every open is a chore, not a choice.
fn view_mode(cx: &gpui::App) -> DiffViewMode {
    cx.try_global::<Config>()
        .map(|cfg| cfg.diff_view)
        .unwrap_or_default()
}

/// The change column. `−` is U+2212, matching the split view: the ASCII hyphen
/// is narrower than `+` and the two columns would not line up.
fn unified_marker(kind: LineKind) -> &'static str {
    match kind {
        LineKind::Added => "+",
        LineKind::Removed => "−",
        LineKind::Context => "",
    }
}

/// The git status letter and colour every part of the app agrees on.
///
/// `Copied` and `TypeChanged` have no decoration of their own — porcelain v2's
/// index folds them the same way — so they take the nearest one rather than
/// inventing a `C` and a `T` that appear in the overlay and nowhere else.
pub(crate) fn deco_status(status: FileStatus) -> DecoStatus {
    match status {
        FileStatus::Added => DecoStatus::Added,
        FileStatus::Modified => DecoStatus::Modified,
        FileStatus::Deleted => DecoStatus::Deleted,
        FileStatus::Renamed | FileStatus::Copied => DecoStatus::Renamed,
        FileStatus::TypeChanged => DecoStatus::Modified,
        FileStatus::Unmerged => DecoStatus::Conflict,
    }
}

/// The current epoch for a repository, or 0 where nothing has ever bumped one.
/// Zero is the same value a never-touched repository reports, so an overlay
/// that reads it before the global exists simply never looks stale.
fn scm_epoch(cx: &gpui::App, host: crate::ui::host_ops::HostId, root: &Path) -> u64 {
    cx.try_global::<crate::terminal::git_data::ScmData>()
        .map(|data| data.epoch(host, root))
        .unwrap_or(0)
}

/// What the header calls the patch it is showing.
struct SourceSubject {
    icon: &'static str,
    text: String,
    /// Set only where the branch name alone would be ambiguous.
    chip: Option<&'static str>,
    is_rev: bool,
    /// What the commit is *about*, where whoever opened it knew. An object id
    /// is an address, not a name, and a header with nothing but eight hex
    /// digits leaves the reader to remember which commit that was.
    label: Option<CommitLabel>,
}

fn source_subject(source: &DiffSource, branch: String) -> SourceSubject {
    let branch_of = |chip| SourceSubject {
        icon: "icons/git-branch.svg",
        text: branch.clone(),
        chip,
        is_rev: false,
        label: None,
    };
    match source {
        // Worktree and Head are both "the branch, right now"; the header for
        // them is what it has always been.
        DiffSource::Worktree | DiffSource::Head => branch_of(None),
        // Staged is the branch too, but a patch that does not match the files
        // on disk — without the chip it is indistinguishable from the above.
        DiffSource::Staged => branch_of(Some(t(L10nKey::ScmChipStaged))),
        DiffSource::Commit { rev, label } => SourceSubject {
            icon: "icons/git-commit.svg",
            text: short_rev(rev),
            chip: None,
            is_rev: true,
            // An empty subject is no more use than no label at all, and a
            // `Default::default()` that leaked through would render as one.
            label: label.clone().filter(|l| !l.subject.is_empty()),
        },
        DiffSource::Range { base, head } => SourceSubject {
            icon: "icons/git-commit.svg",
            text: format!("{}…{}", short_rev(base), short_rev(head)),
            chip: None,
            is_rev: true,
            label: None,
        },
    }
}

/// `Ada · 2h`, the byline under a commit's subject.
///
/// One string rather than two elements: the separator has to disappear along
/// with whichever half is missing, and a `when_some` chain around a middle dot
/// says less than this does.
fn label_byline(label: &CommitLabel, now: i64) -> String {
    let when = (label.at > 0).then(|| relative_time(now, label.at));
    match (label.author.trim(), when) {
        ("", Some(when)) => when,
        (author, Some(when)) => format!("{author} · {when}"),
        (author, None) => author.to_string(),
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Object ids get cut to eight characters; anything else is already a name a
/// person chose, and cutting `origin/main` in half would only hide which it is.
fn short_rev(rev: &str) -> String {
    let is_oid = rev.len() >= 40 && rev.chars().all(|c| c.is_ascii_hexdigit());
    match is_oid {
        true => rev[..8].to_string(),
        false => rev.to_string(),
    }
}

fn focused_file(snap: &DiffSnapshot, overlay: &DiffOverlayState) -> Option<usize> {
    let path = overlay.focus.as_deref()?;
    snap.files.iter().position(|f| f.path == path)
}

/// The focused path, when it is an *untracked* file — one the snapshot lists
/// by name but holds no patch for. A path that is both (staged half tracked,
/// say) prefers the real patch.
fn untracked_focus<'a>(snap: &DiffSnapshot, focus: Option<&'a str>) -> Option<&'a str> {
    let path = focus?;
    if snap.files.iter().any(|f| f.path == path) {
        return None;
    }
    snap.untracked.iter().any(|u| u == path).then_some(path)
}

/// The file the overlay is focused on, for the header's way back to the list.
///
/// An untracked file is focused like any other but has no entry in `files` —
/// its card is synthesized from the file's own bytes — so reading only `files`
/// left the one view with no way out of it: the breadcrumb never drew, and the
/// list was reachable again only by closing the overlay and reopening it.
fn focused_name(overlay: &DiffOverlayState) -> Option<String> {
    let DiffLoad::Ready(snap) = &overlay.load else {
        return None;
    };
    if let Some(idx) = focused_file(snap, overlay) {
        return Some(snap.files[idx].path.clone());
    }
    untracked_focus(snap, overlay.focus.as_deref()).map(str::to_string)
}

fn empty_snapshot(snap: &DiffSnapshot) -> bool {
    snap.files.is_empty() && snap.untracked.is_empty()
}

fn oversized_summary(snap: &DiffSnapshot, stats: &DiffStats) -> String {
    let mut parts = vec![t_plural(L10nKey::DiffChangedFiles, snap.files.len(), &[])];
    let (added, removed) = stats.totals;
    let total_lines = (added + removed) as usize;
    let loaded = stats.retained_lines;
    let budget = stats.budget_exhausted;
    let per_file = stats.per_file_truncated;
    parts.push(match (budget, per_file) {
        (false, false) => t_plural(L10nKey::DiffLines, total_lines, &[]),
        _ => {
            let cap_key = match (budget, per_file) {
                (true, true) => L10nKey::DiffBudgetAndCap,
                (true, false) => L10nKey::DiffBudget,
                _ => L10nKey::DiffPerFileCap,
            };
            t_fmt(
                L10nKey::DiffChangedLines,
                &[
                    ("total", &total_lines.to_string()),
                    ("loaded", &loaded.to_string()),
                    ("cap", t(cap_key)),
                ],
            )
        }
    });
    if stats.untracked_count > 0 {
        parts.push(t_plural(
            L10nKey::DiffUntrackedSummary,
            stats.untracked_count,
            &[],
        ));
    }
    parts.join(", ")
}

/// The de-duplication sets on `Tty7App` are keyed by `(HostId, PathBuf)`, so
/// the source rides along inside the path: two sources over one directory are
/// two independent probes and must not cancel one another.
///
/// `DiffSource::tag` rather than `Debug`, which is what this used to be built
/// from. `Debug` prints a commit's label too, so the same commit opened with a
/// subject in hand and without one would have been two keys and two probes for
/// one patch — the same split `DiffSource`'s own `PartialEq` is written to
/// avoid. The separator is a byte no path contains.
fn probe_key(
    host: crate::ui::host_ops::HostId,
    cwd: &Path,
    source: &DiffSource,
) -> (crate::ui::host_ops::HostId, PathBuf) {
    let mut tagged = std::ffi::OsString::from(format!("{}\u{1}", source.tag()));
    tagged.push(cwd.as_os_str());
    (host, PathBuf::from(tagged))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::git_diff::{AUTO_COLLAPSE_LINES, DiffLine, LineKind, MAX_RENDERED_FILES};
    use crate::ui::diff_list::{build_rows, file_expanded};
    use crate::ui::diff_rows::split_hunk;
    use crate::ui::i18n::set_locale;

    #[test]
    fn full_window_diff_background_is_opaque_with_or_without_a_preset() {
        let active = crate::ui::presets::ActiveBackground {
            fill: crate::ui::presets::Fill::Solid(0x12_34_56),
            opacity: Some(0.2),
            image: None,
        };
        let mut fallback: Hsla = gpui::rgb(0x65_43_21).into();
        fallback.a = 0.3;
        let mut opaque_fallback = fallback;
        opaque_fallback.a = 1.0;

        assert_eq!(
            diff_overlay_background(Some(&active), fallback),
            crate::ui::theme::window_background_opaque(&active),
            "the active preset must keep its fill while discarding workspace translucency"
        );
        assert_eq!(
            diff_overlay_background(None, fallback),
            opaque_fallback.into(),
            "the theme fallback must also block the window material"
        );
    }

    fn line(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old_no: old,
            new_no: new,
            text: text.to_string(),
        }
    }

    #[test]
    fn the_probe_key_separates_the_sources_over_one_directory() {
        let host = crate::ui::host_ops::HostId::LOCAL;
        let cwd = Path::new("/repo");
        let worktree = probe_key(host, cwd, &DiffSource::Worktree);
        assert_ne!(worktree, probe_key(host, cwd, &DiffSource::Staged));
        assert_ne!(worktree, probe_key(host, cwd, &DiffSource::Head));
        assert_ne!(
            probe_key(host, cwd, &DiffSource::commit("a")),
            probe_key(host, cwd, &DiffSource::commit("b")),
            "two commits are two probes"
        );
        assert_eq!(worktree, probe_key(host, cwd, &DiffSource::Worktree));
        assert_ne!(
            worktree,
            probe_key(host, Path::new("/other"), &DiffSource::Worktree)
        );
        // …and one commit is one probe however much is known about it. Built
        // from `Debug`, as this key once was, the labelled one would have been
        // a second in-flight probe for a patch already being read.
        assert_eq!(
            probe_key(host, cwd, &DiffSource::commit("a")),
            probe_key(
                host,
                cwd,
                &DiffSource::Commit {
                    rev: "a".into(),
                    label: Some(CommitLabel {
                        subject: "s".into(),
                        author: "Ada".into(),
                        at: 1,
                    }),
                }
            )
        );
    }

    #[test]
    fn every_file_status_lands_on_a_shared_decoration() {
        use DecoStatus as D;
        for (status, want) in [
            (FileStatus::Added, D::Added),
            (FileStatus::Modified, D::Modified),
            (FileStatus::Deleted, D::Deleted),
            (FileStatus::Renamed, D::Renamed),
            // A copy is a rename that left the original behind: same letter.
            (FileStatus::Copied, D::Renamed),
            // A symlink that became a file is a modification, not a category
            // of its own — the overlay is the only place that ever saw a `T`.
            (FileStatus::TypeChanged, D::Modified),
            (FileStatus::Unmerged, D::Conflict),
        ] {
            assert_eq!(deco_status(status), want, "{status:?}");
        }
        assert_eq!(status_glyph(deco_status(FileStatus::Unmerged)), "U");
        assert_eq!(status_glyph(deco_status(FileStatus::Copied)), "R");
    }

    #[test]
    fn the_change_column_uses_the_typographic_minus() {
        assert_eq!(unified_marker(LineKind::Added), "+");
        assert_eq!(unified_marker(LineKind::Removed), "\u{2212}");
        assert_ne!(
            unified_marker(LineKind::Removed),
            "-",
            "the ASCII hyphen is narrower than `+`, and the column would wobble"
        );
        assert_eq!(
            unified_marker(LineKind::Context),
            "",
            "a context line is neither, and a placeholder glyph would be noise"
        );
    }

    #[test]
    fn the_header_shortens_an_object_id_and_nothing_else() {
        let oid = "3f2a1b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a";
        assert_eq!(short_rev(oid), "3f2a1b9c");
        assert_eq!(short_rev("v26.7.5"), "v26.7.5");
        assert_eq!(
            short_rev("origin/main"),
            "origin/main",
            "half a ref name says less than the whole of it"
        );
        assert_eq!(short_rev("3f2a1b9"), "3f2a1b9", "already short");
    }

    #[test]
    fn each_source_names_itself_in_the_header() {
        let branch = || "main".to_string();
        let plain = source_subject(&DiffSource::Worktree, branch());
        assert_eq!((plain.icon, plain.text.as_str()), (BRANCH_ICON, "main"));
        assert_eq!(plain.chip, None);
        assert!(!plain.is_rev);

        assert_eq!(source_subject(&DiffSource::Head, branch()).chip, None);

        let staged = source_subject(&DiffSource::Staged, branch());
        assert_eq!(staged.icon, BRANCH_ICON, "still a branch, still its name");
        assert_eq!(
            staged.chip,
            Some("STAGED"),
            "without it the staged patch is indistinguishable from the unstaged one"
        );

        let commit = source_subject(
            &DiffSource::commit("3f2a1b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a"),
            branch(),
        );
        assert_eq!(commit.icon, COMMIT_ICON);
        assert_eq!(commit.text, "3f2a1b9c", "the branch is not what is shown");
        assert!(commit.is_rev);

        let range = source_subject(
            &DiffSource::Range {
                base: "main".into(),
                head: "feature".into(),
            },
            branch(),
        );
        assert_eq!(range.icon, COMMIT_ICON);
        assert_eq!(range.text, "main…feature");
    }

    #[test]
    fn a_labelled_commit_says_what_it_was_about() {
        let label = CommitLabel {
            subject: "fix(scm): stop the panel asking twice".into(),
            author: "Ada".into(),
            at: 1_786_255_391,
        };
        let with = source_subject(
            &DiffSource::Commit {
                rev: "3f2a1b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a".into(),
                label: Some(label.clone()),
            },
            "main".to_string(),
        );
        assert_eq!(with.text, "3f2a1b9c", "the sha is still the identifier");
        assert_eq!(
            with.label.as_ref().map(|l| l.subject.as_str()),
            Some(label.subject.as_str())
        );

        // Nothing else grows a subject line, least of all a working-tree
        // patch, whose "subject" would be a branch name repeated.
        assert!(
            source_subject(&DiffSource::Worktree, "main".into())
                .label
                .is_none()
        );
        assert!(
            source_subject(&DiffSource::Head, "main".into())
                .label
                .is_none()
        );
        assert!(
            source_subject(&DiffSource::commit("deadbeef"), "main".into())
                .label
                .is_none(),
            "a commit nobody has read yet has nothing to say"
        );
        // A default-constructed label is indistinguishable from none, and must
        // not paint an empty row where the subject would go.
        let empty = source_subject(
            &DiffSource::Commit {
                rev: "deadbeef".into(),
                label: Some(CommitLabel::default()),
            },
            "main".into(),
        );
        assert!(empty.label.is_none());
    }

    #[test]
    fn the_byline_drops_the_separator_along_with_the_half_it_joined() {
        let now = 1_786_255_391 + 7200;
        let full = CommitLabel {
            subject: "s".into(),
            author: "Ada".into(),
            at: 1_786_255_391,
        };
        assert_eq!(label_byline(&full, now), "Ada · 2h");
        assert_eq!(
            label_byline(
                &CommitLabel {
                    author: String::new(),
                    ..full.clone()
                },
                now
            ),
            "2h",
            "a commit with no author is not `· 2h`"
        );
        assert_eq!(
            label_byline(&CommitLabel { at: 0, ..full }, now),
            "Ada",
            "and a timestamp that would not parse is not `Ada · 56y`"
        );
    }

    const BRANCH_ICON: &str = "icons/git-branch.svg";
    const COMMIT_ICON: &str = "icons/git-commit.svg";

    fn small_file(path: &str, added: u32) -> FileDiff {
        FileDiff {
            path: path.to_string(),
            old_path: None,
            status: FileStatus::Modified,
            added,
            removed: 0,
            binary: false,
            truncated: None,
            hunks: vec![git_diff::Hunk {
                header: "@@ -1,1 +1,1 @@".to_string(),
                lines: (0..added)
                    .map(|i| line(LineKind::Added, None, Some(i + 1), "x"))
                    .collect(),
            }],
        }
    }

    fn context_heavy_file(path: &str) -> FileDiff {
        FileDiff {
            path: path.to_string(),
            old_path: None,
            status: FileStatus::Modified,
            added: 1,
            removed: 0,
            binary: false,
            truncated: None,
            hunks: vec![git_diff::Hunk {
                header: "@@ -1,7 +1,7 @@".to_string(),
                lines: (0..6)
                    .map(|i| line(LineKind::Context, Some(i + 1), Some(i + 1), "ctx"))
                    .chain(std::iter::once(line(LineKind::Added, None, Some(7), "x")))
                    .collect(),
            }],
        }
    }

    fn choices<const N: usize>(pairs: [(&str, bool); N]) -> HashMap<String, bool> {
        pairs.into_iter().map(|(p, v)| (p.to_string(), v)).collect()
    }

    fn banner(snap: &DiffSnapshot) -> String {
        set_locale("en");
        oversized_summary(snap, &snap.stats())
    }

    #[test]
    fn per_file_collapse_is_unchanged_below_the_repo_threshold() {
        let small = small_file("small.rs", 10);
        let big = small_file("big.rs", AUTO_COLLAPSE_LINES + 1);
        let none = HashMap::new();
        assert!(file_expanded(&small, &none, false));
        assert!(!file_expanded(&big, &none, false));

        let picked = choices([("small.rs", false), ("big.rs", true)]);
        assert!(!file_expanded(&small, &picked, false));
        assert!(file_expanded(&big, &picked, false));
    }

    #[test]
    fn repo_wide_collapse_overrides_the_per_file_default() {
        let small = small_file("small.rs", 10);
        let none = HashMap::new();
        assert!(file_expanded(&small, &none, false));
        assert!(!file_expanded(&small, &none, true), "collapsed en masse");

        assert!(
            file_expanded(&small, &choices([("small.rs", true)]), true),
            "the user's own click still opens it"
        );
    }

    #[test]
    fn explicit_choices_survive_an_oversized_transition() {
        let opened = small_file("opened.rs", 10);
        let closed = small_file("closed.rs", 10);
        let untouched = small_file("untouched.rs", 10);
        let picked = choices([("opened.rs", true), ("closed.rs", false)]);

        for collapse_all in [true, false] {
            assert!(
                file_expanded(&opened, &picked, collapse_all),
                "an explicitly opened file stays open (collapse_all={collapse_all})"
            );
            assert!(
                !file_expanded(&closed, &picked, collapse_all),
                "an explicitly closed file stays closed (collapse_all={collapse_all})"
            );
        }
        assert!(!file_expanded(&untouched, &picked, true));
        assert!(file_expanded(&untouched, &picked, false));
    }

    #[test]
    fn many_medium_files_are_oversized_and_build_no_rows() {
        let snap = DiffSnapshot {
            files: (0..60)
                .map(|i| small_file(&format!("f{i}.rs"), 150))
                .collect(),
            ..Default::default()
        };
        assert!(
            snap.files.iter().all(|f| f.added <= AUTO_COLLAPSE_LINES),
            "no single file is over the per-file threshold"
        );
        assert!(snap.stats().oversized);

        let none = HashMap::new();
        let rows_expanded: usize = snap
            .files
            .iter()
            .filter(|f| file_expanded(f, &none, false))
            .flat_map(|f| &f.hunks)
            .map(|h| split_hunk(&h.lines).len())
            .sum();
        let rows_collapsed: usize = snap
            .files
            .iter()
            .filter(|f| file_expanded(f, &none, true))
            .flat_map(|f| &f.hunks)
            .map(|h| split_hunk(&h.lines).len())
            .sum();
        assert_eq!(rows_expanded, 9000, "what the old rule would have built");
        assert_eq!(rows_collapsed, 0);
    }

    #[test]
    fn an_ordinary_busy_tree_is_not_oversized() {
        let snap = DiffSnapshot {
            files: (0..40)
                .map(|i| {
                    let mut f = context_heavy_file(&format!("f{i}.rs"));
                    f.hunks = std::iter::repeat_n(f.hunks[0].clone(), 10).collect();
                    f.added = 10;
                    f
                })
                .collect(),
            ..Default::default()
        };
        let (added, removed) = snap.totals();
        assert!(
            snap.stats().retained_lines > (added + removed) as usize * 4,
            "the context lines dominate, as they do in a real diff"
        );
        assert!(
            !snap.stats().oversized,
            "an ordinary afternoon must not read as a tree too large to render \
             ({} retained lines)",
            snap.stats().retained_lines
        );
        let none = HashMap::new();
        assert!(snap.files.iter().all(|f| file_expanded(f, &none, false)));
    }

    #[test]
    fn an_empty_snapshot_reads_as_clean_only_when_the_probe_worked() {
        let clean = DiffSnapshot {
            branch: "main".into(),
            ..Default::default()
        };
        assert!(empty_snapshot(&clean));
        assert!(!clean.read_failed, "nothing went wrong: this tree is clean");

        let broken = DiffSnapshot {
            branch: "main".into(),
            read_failed: true,
            ..Default::default()
        };
        assert!(
            empty_snapshot(&broken),
            "indistinguishable by shape — which is the point"
        );
        assert!(broken.read_failed, "and distinguishable by this");

        let partial = DiffSnapshot {
            files: vec![small_file("one.rs", 3)],
            read_failed: true,
            ..Default::default()
        };
        assert!(!empty_snapshot(&partial));
    }

    #[test]
    fn a_huge_untracked_list_does_not_collapse_the_diff() {
        let snap = DiffSnapshot {
            files: vec![small_file("one.rs", 3)],
            untracked: (0..git_diff::MAX_UNTRACKED)
                .map(|i| format!("node_modules/p{i}/index.js"))
                .collect(),
            untracked_total: 40_000,
            ..Default::default()
        };
        assert!(snap.stats().retained_lines < git_diff::AUTO_COLLAPSE_TOTAL_LINES);
        assert!(snap.files.len() < git_diff::AUTO_COLLAPSE_TOTAL_FILES);
        assert!(
            !snap.stats().oversized,
            "collapsing the diff would not have removed a single untracked row"
        );

        assert_eq!(
            snap.untracked.len(),
            git_diff::MAX_UNTRACKED,
            "retention is capped at the parser"
        );
        assert_eq!(
            snap.untracked.len().min(MAX_RENDERED_FILES),
            MAX_RENDERED_FILES,
            "and rows at the renderer"
        );
        assert_eq!(
            snap.untracked_count(),
            40_000,
            "while the reported count stays the true total"
        );
    }

    /// `ListState::reset` would drop the scroll position, so opening or
    /// closing one file in a long tree would throw the reader back to the top.
    /// Only the rows that actually changed are spliced.
    #[test]
    fn only_the_rows_that_changed_are_spliced() {
        let snap = DiffSnapshot {
            files: vec![
                small_file("a.rs", 2),
                small_file("b.rs", 2),
                small_file("c.rs", 2),
            ],
            ..Default::default()
        };
        let shut = |path: &str| -> HashMap<String, bool> {
            [(path.to_string(), false)].into_iter().collect()
        };
        let open = build_rows(&snap, &HashMap::new(), None, DiffViewMode::Unified, false);
        let middle_shut = build_rows(&snap, &shut("b.rs"), None, DiffViewMode::Unified, false);

        let (replaced, with) = spliced_range(&open, &middle_shut);
        assert_eq!(
            (replaced.start, with),
            (5, 1),
            "the first card and the gap after it are untouched"
        );
        assert_eq!(
            open.len() - replaced.end,
            middle_shut.len() - (replaced.start + with),
            "and so is everything below the file that closed"
        );

        let (replaced, with) = spliced_range(&open, &open);
        assert_eq!(
            (replaced.start, replaced.end, with),
            (open.len(), open.len(), 0)
        );
    }

    #[test]
    fn a_list_that_was_empty_or_becomes_empty_splices_in_one_piece() {
        let snap = DiffSnapshot {
            files: vec![small_file("a.rs", 2)],
            ..Default::default()
        };
        let rows = build_rows(&snap, &HashMap::new(), None, DiffViewMode::Unified, false);

        let (replaced, with) = spliced_range(&[], &rows);
        assert_eq!((replaced.start, replaced.end, with), (0, 0, rows.len()));

        let (replaced, with) = spliced_range(&rows, &[]);
        assert_eq!((replaced.start, replaced.end, with), (0, rows.len(), 0));
    }

    /// A probe that found nothing new still lands a fresh `Arc` over an equal
    /// snapshot. The rows are rightly kept — and the key has to come away
    /// holding the `Arc` that landed, or every frame from then on proves the
    /// two equal the long way: a walk of every line of the patch, per wheel
    /// event.
    #[test]
    fn an_equal_snapshot_leaves_the_key_pointing_at_the_one_that_landed() {
        let held = Arc::new(DiffSnapshot {
            files: vec![small_file("a.rs", 2)],
            ..Default::default()
        });
        let landed = Arc::new(DiffSnapshot {
            files: vec![small_file("a.rs", 2)],
            ..Default::default()
        });
        assert!(
            !Arc::ptr_eq(&held, &landed),
            "two separate Arcs over equal contents"
        );

        let expanded = HashMap::new();
        let mut key = RowsFrom {
            snap: &held,
            preview: None,
            mode: DiffViewMode::Unified,
            focused: None,
            oversized: false,
            expanded: &expanded,
        }
        .to_key();
        let landed_from = RowsFrom {
            snap: &landed,
            preview: None,
            mode: DiffViewMode::Unified,
            focused: None,
            oversized: false,
            expanded: &expanded,
        };

        assert!(
            key.describes(&landed_from),
            "nothing about the rows changed"
        );
        key.retarget(&landed_from);
        assert!(
            Arc::ptr_eq(&key.snap, &landed),
            "so the next frame settles it by pointer rather than by contents"
        );
    }

    #[test]
    fn a_focused_untracked_file_asks_for_a_preview_not_the_list() {
        let snap = DiffSnapshot {
            files: vec![small_file("tracked.rs", 3)],
            untracked: vec!["new.md".to_string()],
            ..Default::default()
        };
        assert_eq!(untracked_focus(&snap, Some("new.md")), Some("new.md"));
        assert_eq!(
            untracked_focus(&snap, Some("tracked.rs")),
            None,
            "a real patch wins over the name list"
        );
        assert_eq!(untracked_focus(&snap, Some("absent.rs")), None);
        assert_eq!(untracked_focus(&snap, None), None);
    }

    #[test]
    fn untracked_rows_are_capped_but_the_count_stays_true() {
        let snap = DiffSnapshot {
            untracked: (0..git_diff::MAX_UNTRACKED)
                .map(|i| format!("p{i}"))
                .collect(),
            untracked_total: 12_345,
            ..Default::default()
        };
        let rendered = snap.untracked.len().min(MAX_RENDERED_FILES);
        assert_eq!(rendered, MAX_RENDERED_FILES);
        assert_eq!(snap.untracked_count(), 12_345, "header count is honest");
        assert_eq!(snap.untracked_count() - rendered, 12_045);
    }

    #[test]
    fn untracked_count_falls_back_to_the_retained_length() {
        let snap = DiffSnapshot {
            untracked: vec!["a".into(), "b".into(), "c".into()],
            ..Default::default()
        };
        assert_eq!(snap.untracked_total, 0);
        assert_eq!(snap.untracked_count(), 3);
    }

    #[test]
    fn file_cards_are_capped() {
        let snap = DiffSnapshot {
            files: (0..MAX_RENDERED_FILES + 25)
                .map(|i| small_file(&format!("f{i}.rs"), 1))
                .collect(),
            ..Default::default()
        };
        let shown = snap.files.len().min(MAX_RENDERED_FILES);
        assert_eq!(shown, MAX_RENDERED_FILES);
        assert_eq!(
            snap.files.len() - shown,
            25,
            "the tail gets one summary line"
        );
    }

    #[test]
    fn the_banner_names_the_budget_when_context_outweighs_the_changes() {
        let files: Vec<FileDiff> = (0..200)
            .map(|i| context_heavy_file(&format!("f{i}.rs")))
            .collect();

        let mut truncated = files.clone();
        truncated.push(FileDiff {
            hunks: vec![],
            truncated: Some(Truncation::Budget),
            ..context_heavy_file("dropped.rs")
        });
        let snap = DiffSnapshot {
            files: truncated,
            ..Default::default()
        };
        let (added, removed) = snap.totals();
        let total = (added + removed) as usize;
        assert!(
            snap.stats().retained_lines > total,
            "the context lines outweigh the changed ones — the shape that slipped through"
        );
        assert!(snap.stats().budget_exhausted);

        let summary = banner(&snap);
        assert!(summary.contains("budget"), "{summary}");
        assert!(
            summary.contains(&format!("{total} changed lines")),
            "the exact `+N −N` from the header, not the retained count: {summary}"
        );

        let whole = DiffSnapshot {
            files,
            ..Default::default()
        };
        assert!(!whole.stats().budget_exhausted);
        assert!(!banner(&whole).contains("budget"));
    }

    #[test]
    fn the_banner_names_the_per_file_cap_when_context_outweighs_the_changes() {
        let files: Vec<FileDiff> = (0..200)
            .map(|i| context_heavy_file(&format!("f{i}.rs")))
            .collect();

        let mut cut = files.clone();
        cut.push(FileDiff {
            truncated: Some(Truncation::PerFile),
            ..context_heavy_file("huge.rs")
        });
        let snap = DiffSnapshot {
            files: cut,
            ..Default::default()
        };
        let (added, removed) = snap.totals();
        let total = (added + removed) as usize;
        assert!(
            snap.stats().retained_lines > total,
            "the shape the comparison reads backwards"
        );
        assert!(
            !snap.stats().budget_exhausted,
            "the budget axis is not what fired"
        );

        let summary = banner(&snap);
        assert!(summary.contains("per-file cap"), "{summary}");
        assert!(
            summary.contains(&format!("{total} changed lines")),
            "the exact `+N −N` from the header, not the retained count: {summary}"
        );

        let whole = DiffSnapshot {
            files: files.clone(),
            ..Default::default()
        };
        assert!(!banner(&whole).contains("per-file"));

        let mut both = files;
        both.push(FileDiff {
            truncated: Some(Truncation::PerFile),
            ..context_heavy_file("huge.rs")
        });
        both.push(FileDiff {
            hunks: vec![],
            truncated: Some(Truncation::Budget),
            ..context_heavy_file("dropped.rs")
        });
        let summary = banner(&DiffSnapshot {
            files: both,
            ..Default::default()
        });
        assert!(summary.contains("budget"), "{summary}");
        assert!(summary.contains("per-file cap"), "{summary}");
    }

    #[test]
    #[ignore = "measurement, not an assertion"]
    fn bench_snapshot_share() {
        use std::time::Instant;

        for (label, files, per_file) in [
            ("unbudgeted (v26.7.5 shape)", 300, 300),
            ("budgeted (this build retains)", 300, 67),
        ] {
            let snap = Arc::new(DiffSnapshot {
                files: (0..files)
                    .map(|i| small_file(&format!("f{i}.rs"), per_file))
                    .collect(),
                ..Default::default()
            });
            let lines: usize = snap.stats().retained_lines;

            let t = Instant::now();
            for _ in 0..10 {
                let _deep = (*snap).clone();
            }
            let deep = t.elapsed() / 10;

            let t = Instant::now();
            for _ in 0..100 {
                let _shared = Arc::clone(&snap);
            }
            let shared = t.elapsed() / 100;
            println!(
                "{label}: {files} files / {lines} lines — deep clone {deep:?} vs \
                 Arc::clone {shared:?}, per holder on the UI thread"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod overlay_gpui_tests {
    use super::*;
    use crate::ui::app::test_window;
    use crate::ui::host_ops::HostId;
    use gpui::{Entity, TestAppContext, VisualTestContext};

    fn overlay_source_and_load(
        app: &Entity<Tty7App>,
        vcx: &mut VisualTestContext,
    ) -> (DiffSource, bool) {
        app.update_in(vcx, |app, _, _| {
            let overlay = app.tabs[app.active]
                .diff_overlay
                .as_ref()
                .expect("an overlay is open");
            (
                overlay.source.clone(),
                matches!(overlay.load, DiffLoad::Loading),
            )
        })
    }

    /// Opening a staged file while a worktree overlay is up must re-probe. The
    /// filter used to match on `(cwd, host)` alone, so it took the "just move
    /// the focus" branch and left the unstaged patch on screen under the
    /// staged file's name.
    #[gpui::test]
    fn a_second_source_over_one_directory_is_a_second_overlay(cx: &mut TestAppContext) {
        let (app, mut vcx, _pane) = test_window::harness_with_tabs(cx, 1);
        let cwd = std::path::PathBuf::from("/no/such/tty7/repo");

        app.update_in(&mut vcx, |app, window, cx| {
            app.open_diff_overlay(
                HostId::LOCAL,
                cwd.clone(),
                DiffSource::Worktree,
                Some("a.rs".to_string()),
                window,
                cx,
            );
        });
        assert_eq!(
            overlay_source_and_load(&app, &mut vcx),
            (DiffSource::Worktree, true)
        );

        // Pretend the worktree probe landed, so a reused overlay would show it.
        app.update_in(&mut vcx, |app, _, _| {
            let active = app.active;
            let overlay = app.tabs[active].diff_overlay.as_mut().unwrap();
            overlay.loading = false;
            overlay.load = DiffLoad::Ready(Arc::new(DiffSnapshot {
                source: DiffSource::Worktree,
                branch: "main".into(),
                ..Default::default()
            }));
        });

        app.update_in(&mut vcx, |app, window, cx| {
            app.open_diff_overlay(
                HostId::LOCAL,
                cwd.clone(),
                DiffSource::Staged,
                Some("a.rs".to_string()),
                window,
                cx,
            );
        });
        assert_eq!(
            overlay_source_and_load(&app, &mut vcx),
            (DiffSource::Staged, true),
            "the same file from a different source is a different question"
        );
    }

    fn one_file_snapshot(source: DiffSource) -> DiffSnapshot {
        use crate::terminal::git_diff::{DiffLine, Hunk, LineKind};
        DiffSnapshot {
            root: std::path::PathBuf::from("/no/such/tty7/repo"),
            source,
            branch: "main".into(),
            files: vec![FileDiff {
                path: "a.rs".into(),
                old_path: None,
                status: FileStatus::Modified,
                added: 1,
                removed: 1,
                binary: false,
                truncated: None,
                hunks: vec![Hunk {
                    header: "@@ -1,2 +1,2 @@".into(),
                    lines: vec![
                        DiffLine {
                            kind: LineKind::Context,
                            old_no: Some(1),
                            new_no: Some(1),
                            text: "keep".into(),
                        },
                        DiffLine {
                            kind: LineKind::Removed,
                            old_no: Some(2),
                            new_no: None,
                            text: "old".into(),
                        },
                        DiffLine {
                            kind: LineKind::Added,
                            old_no: None,
                            new_no: Some(2),
                            text: "new".into(),
                        },
                    ],
                }],
            }],
            untracked: vec!["scratch.txt".into()],
            untracked_total: 1,
            read_failed: false,
        }
    }

    fn show(app: &Entity<Tty7App>, vcx: &mut VisualTestContext, source: DiffSource) {
        let cwd = std::path::PathBuf::from("/no/such/tty7/repo");
        app.update_in(vcx, |app, window, cx| {
            app.open_diff_overlay(HostId::LOCAL, cwd.clone(), source.clone(), None, window, cx);
            let active = app.active;
            let overlay = app.tabs[active].diff_overlay.as_mut().unwrap();
            overlay.loading = false;
            overlay.load = DiffLoad::Ready(Arc::new(one_file_snapshot(source)));
            // The card is what carries the rows, so open it.
            overlay.expanded.insert("a.rs".to_string(), true);
        });
    }

    /// Every header branch, every row renderer, once each. A missing icon, an
    /// unset global or a panicking helper shows up here rather than the first
    /// time somebody opens a commit.
    #[gpui::test]
    fn every_source_renders_in_both_views(cx: &mut TestAppContext) {
        let (app, mut vcx, _pane) = test_window::harness_with_tabs(cx, 1);

        for mode in [DiffViewMode::Split, DiffViewMode::Unified] {
            app.update_in(&mut vcx, |app, _, cx| {
                app.update_config(cx, |cfg| cfg.diff_view = mode);
            });
            for source in [
                DiffSource::Worktree,
                DiffSource::Staged,
                DiffSource::Head,
                DiffSource::commit("3f2a1b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a"),
                DiffSource::Commit {
                    rev: "3f2a1b9c8d7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a".into(),
                    label: Some(CommitLabel {
                        subject: "fix(scm): read a commit's own header".into(),
                        author: "Ada".into(),
                        at: 1_786_255_391,
                    }),
                },
                DiffSource::Range {
                    base: "main".into(),
                    head: "feature".into(),
                },
            ] {
                show(&app, &mut vcx, source.clone());
                // A real frame, so layout and paint run too: `title_bar_drag`
                // and the segmented track both want a window that is drawing.
                crate::ui::app::render_probe::arm(10_000);
                app.update_in(&mut vcx, |_, _, cx| cx.notify());
                vcx.background_executor.run_until_parked();
                assert!(
                    crate::ui::app::render_probe::draws() > 0,
                    "nothing was drawn, so nothing was proved: {source:?} in {mode:?}"
                );
                app.update_in(&mut vcx, |app, window, cx| {
                    app.close_diff_overlay(window, cx)
                });
            }
        }
    }

    #[gpui::test]
    fn toggling_the_diff_view_mode_writes_config(cx: &mut TestAppContext) {
        let (app, mut vcx, _pane) = test_window::harness_with_tabs(cx, 1);
        let mode =
            |vcx: &mut VisualTestContext| vcx.update(|_, cx| cx.global::<Config>().diff_view);

        assert_eq!(
            mode(&mut vcx),
            DiffViewMode::Split,
            "side by side is what everyone already sees"
        );

        app.update_in(&mut vcx, |app, _, cx| app.toggle_diff_view_mode(cx));
        assert_eq!(mode(&mut vcx), DiffViewMode::Unified);

        app.update_in(&mut vcx, |app, _, cx| app.toggle_diff_view_mode(cx));
        assert_eq!(mode(&mut vcx), DiffViewMode::Split, "and back again");
    }

    /// Focusing an untracked file has to leave a way back to the list.
    ///
    /// Its card is synthesized from the file's own bytes, so it has no entry in
    /// `files` — and the header's breadcrumb read only `files`. The one view
    /// that could be entered was the one view that could not be left except by
    /// closing the overlay and opening it again.
    #[gpui::test]
    fn a_focused_untracked_file_still_has_a_way_back(cx: &mut TestAppContext) {
        let (app, mut vcx, _pane) = test_window::harness_with_tabs(cx, 1);
        let cwd = std::path::PathBuf::from("/no/such/tty7/repo");

        app.update_in(&mut vcx, |app, window, cx| {
            app.open_diff_overlay(
                HostId::LOCAL,
                cwd.clone(),
                DiffSource::Worktree,
                Some("scratch.txt".to_string()),
                window,
                cx,
            );
            let active = app.active;
            let overlay = app.tabs[active].diff_overlay.as_mut().unwrap();
            overlay.loading = false;
            overlay.load = DiffLoad::Ready(Arc::new(DiffSnapshot {
                source: DiffSource::Worktree,
                branch: "main".into(),
                untracked: vec!["scratch.txt".to_string()],
                untracked_total: 1,
                ..Default::default()
            }));
        });

        app.update_in(&mut vcx, |app, _, _| {
            let overlay = app.tabs[app.active].diff_overlay.as_ref().unwrap();
            assert_eq!(
                focused_name(overlay).as_deref(),
                Some("scratch.txt"),
                "the breadcrumb is the way back"
            );
        });
    }

    /// The same source and the same focus still toggles the overlay shut.
    #[gpui::test]
    fn the_same_source_twice_still_closes(cx: &mut TestAppContext) {
        let (app, mut vcx, _pane) = test_window::harness_with_tabs(cx, 1);
        let cwd = std::path::PathBuf::from("/no/such/tty7/repo");

        for _ in 0..2 {
            app.update_in(&mut vcx, |app, window, cx| {
                app.open_diff_overlay(
                    HostId::LOCAL,
                    cwd.clone(),
                    DiffSource::Worktree,
                    None,
                    window,
                    cx,
                );
            });
        }
        app.update_in(&mut vcx, |app, _, _| {
            assert!(app.tabs[app.active].diff_overlay.is_none());
        });
    }
}

/// An overlay that has read its patch has to stop reading it.
///
/// `maybe_refresh_diff_overlay` calls a `Head` overlay stale when the cached
/// git status disagrees with the snapshot on screen, and every landed probe
/// wakes that check by touching the status cache. So a disagreement the probe
/// cannot settle is not a stale badge — it is a loop: read the diff, publish
/// it, wake the watchers, find the same disagreement, read it again. Two `git`
/// processes a lap, `refreshing…` pinned to the header, and a window that
/// costs 7% of a core sitting still.
///
/// The way in is ordinary: switch branches anywhere outside tty7 — another
/// terminal, an editor, a worktree command — and the cached branch is a branch
/// the repository has left. That is what the stale entry below stands for.
///
/// Unix-gated like every other window harness in this tree: `harness_with_tabs`
/// hands back a `std::os::unix::net::UnixStream` for the pane.
#[cfg(all(test, unix))]
mod render_idle_gpui_tests {
    use super::*;
    use crate::terminal::git_status::{GitStatusCache, RepoSnapshot};
    use crate::ui::app::{render_probe, test_window};
    use crate::ui::host_ops::HostId;
    use gpui::TestAppContext;

    const BUDGET: u64 = 200;

    fn git(root: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?} failed");
    }

    /// The whole point of the flattened row list: a patch of any size costs
    /// the rows on screen, not the rows in the patch.
    ///
    /// Before this, the overlay built a card per file and an element per line
    /// on every frame, and gpui notifies the view on every scroll wheel event
    /// — so a few hundred lines of diff rebuilt tens of thousands of elements
    /// tens of times a second, and the window visibly stalled.
    #[gpui::test]
    fn a_long_patch_builds_only_the_rows_on_screen(cx: &mut TestAppContext) {
        const LINES: usize = 800;

        let root = std::env::temp_dir().join(format!("tty7-diff-rows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        git(&root, &["init", "--quiet"]);
        let before: String = (0..LINES).map(|i| format!("line {i}\n")).collect();
        std::fs::write(root.join("long.txt"), &before).unwrap();
        git(&root, &["add", "long.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.email=t@x",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "one",
            ],
        );
        // Every line rewritten, so the patch is a removal and an addition per
        // line rather than a handful of hunks in a sea of context.
        let after: String = (0..LINES).map(|i| format!("edited {i}\n")).collect();
        std::fs::write(root.join("long.txt"), &after).unwrap();

        let (app, mut vcx, _pane) = test_window::harness_with_tabs(cx, 1);
        let open = root.clone();
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_diff_overlay(HostId::LOCAL, open, DiffSource::Head, None, window, cx);
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            vcx.background_executor.run_until_parked();
            let ready = app.update_in(&mut vcx, |app, _, _| {
                app.tabs[app.active]
                    .diff_overlay
                    .as_ref()
                    .is_some_and(|o| matches!(o.load, DiffLoad::Ready(_)) && !o.loading)
            });
            if ready {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the overlay never landed a snapshot"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // 1600 changed lines is well past the auto-collapse threshold, so the
        // card starts shut. Open it: a reader opening a large file is exactly
        // the frame this is about.
        app.update_in(&mut vcx, |app, _, cx| {
            let active = app.active;
            app.tabs[active]
                .diff_overlay
                .as_mut()
                .expect("the overlay is open")
                .expanded
                .insert("long.txt".to_string(), true);
            cx.notify();
        });
        vcx.background_executor.run_until_parked();

        let rows = app.update_in(&mut vcx, |app, _, _| {
            app.tabs[app.active]
                .diff_overlay
                .as_ref()
                .map(|o| o.rows.len())
                .unwrap_or(0)
        });
        assert!(
            rows > LINES,
            "the patch flattens to a row per changed line ({rows} rows)"
        );

        row_probe::take();
        app.update_in(&mut vcx, |_, _, cx| cx.notify());
        vcx.background_executor.run_until_parked();
        let built = row_probe::take();
        assert!(
            built > 0,
            "the list drew something — a probe that counts nothing proves nothing"
        );
        assert!(
            built < rows as u64 / 4,
            "a frame built {built} of {rows} rows: the list is not virtualised"
        );

        // The other half of virtualising: a list counts the rows it has not
        // laid out at zero unless it is given an estimate, and the scrollbar
        // reads that count as the length of the document. Without the size
        // hint the bar reaches 248px into this patch — its thumb fills the
        // track, and dragging it to the bottom lands a screen down.
        let reach = app.update_in(&mut vcx, |app, _, _| {
            app.tabs[app.active]
                .diff_overlay
                .as_ref()
                .expect("the overlay is open")
                .list
                .max_offset_for_scrollbar()
                .y
        });
        assert!(
            reach > DIFF_LINE_H * (rows as f32 * 0.75),
            "the scrollbar reaches {reach} into a patch of {rows} rows"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[gpui::test]
    fn an_overlay_over_a_stale_branch_reaches_render_idle(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!("tty7-diff-idle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        git(&root, &["init", "--quiet"]);
        std::fs::write(root.join("a.rs"), "fn main() {}\n").unwrap();
        git(&root, &["add", "a.rs"]);
        git(
            &root,
            &[
                "-c",
                "user.email=t@x",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "one",
            ],
        );
        // Something for the overlay to actually show, so it settles on a
        // snapshot rather than on the empty state.
        std::fs::write(root.join("a.rs"), "fn main() { /* edited */ }\n").unwrap();

        let (app, mut vcx, _pane) = test_window::harness_with_tabs(cx, 1);

        // The cache as a branch switch outside tty7 leaves it: a branch this
        // repository is no longer on, and counts from before the switch.
        let stale = root.clone();
        app.update_in(&mut vcx, |_, _, cx| {
            cx.default_global::<GitStatusCache>();
            cx.update_global::<GitStatusCache, _>(|cache, _| {
                cache.finish_probe(
                    HostId::LOCAL,
                    &stale,
                    Some(RepoSnapshot {
                        root: stale.clone(),
                        home: stale.clone(),
                        branch: "a-branch-this-repo-has-left".into(),
                        counts: Some((99, 99)),
                    }),
                );
            });
        });

        let open = root.clone();
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_diff_overlay(HostId::LOCAL, open, DiffSource::Head, None, window, cx);
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            vcx.background_executor.run_until_parked();
            let ready = app.update_in(&mut vcx, |app, _, _| {
                app.tabs[app.active]
                    .diff_overlay
                    .as_ref()
                    .is_some_and(|o| matches!(o.load, DiffLoad::Ready(_)) && !o.loading)
            });
            if ready {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the overlay never landed a snapshot"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        test_window::quiesce(&mut vcx, Some(&root));
        render_probe::arm(BUDGET);
        vcx.background_executor.run_until_parked();
        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(3));
        vcx.background_executor.run_until_parked();
        render_probe::arm(BUDGET);
        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(9));
        vcx.background_executor.run_until_parked();

        assert_eq!(
            render_probe::draws(),
            0,
            "a settled overlay must stop re-reading its own diff"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// The drag itself, in a window: a press, a move, and what lands on the
/// clipboard. The row geometry is `diff_rows`' business and tested there —
/// what these check is the wiring the list hangs on it.
#[cfg(test)]
mod selection_gpui_tests {
    use super::*;
    use crate::terminal::git_diff::{DiffLine, LineKind};
    use crate::ui::app::test_window;
    use crate::ui::diff_rows::RowId;
    use crate::ui::pane::{Pane, PaneSlot};
    use crate::ui::pending_pane::{PendingPane, PendingSpawn};
    use gpui::{Entity, MouseButton, TestAppContext, VisualTestContext};

    const PATH: &str = "src/a.rs";

    fn line(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old_no: old,
            new_no: new,
            text: text.to_string(),
        }
    }

    /// `a` kept, `b`/`c` replaced by `B`, `d` kept — four split rows, five
    /// unified ones.
    fn patched_file() -> FileDiff {
        FileDiff {
            path: PATH.to_string(),
            old_path: None,
            status: FileStatus::Modified,
            added: 1,
            removed: 2,
            binary: false,
            truncated: None,
            hunks: vec![git_diff::Hunk {
                header: "@@ -1,4 +1,3 @@".to_string(),
                lines: vec![
                    line(LineKind::Context, Some(1), Some(1), "a"),
                    line(LineKind::Removed, Some(2), None, "b"),
                    line(LineKind::Removed, Some(3), None, "c"),
                    line(LineKind::Added, None, Some(2), "B"),
                    line(LineKind::Context, Some(4), Some(3), "d"),
                ],
            }],
        }
    }

    /// The coordinate the list carries on the row `row` of the only hunk.
    fn at(row: usize) -> RowAt {
        RowAt {
            path: PATH.into(),
            id: RowId { hunk: 0, row },
        }
    }

    /// A window with one tab, showing that patch. Built by hand rather than
    /// through `open_diff_overlay`: that dispatches a probe, and a probe
    /// landing mid-test would drop the selection under it.
    fn window(cx: &mut TestAppContext) -> (Entity<Tty7App>, VisualTestContext) {
        let (app, mut vcx) = test_window::harness(cx);
        app.update_in(&mut vcx, |app, _, cx| {
            let pending = cx.new(|cx| {
                PendingPane::new(
                    "test-box",
                    PendingSpawn {
                        workspace: None,
                        working_directory: None,
                        restore_pane: None,
                        shell: None,
                        agent: None,
                        agent_session_id: None,
                        agent_launch_argv: None,
                        owner: None,
                        font_size: 14.0,
                    },
                    cx,
                )
            });
            app.tabs
                .push(crate::ui::app::Tab::new(Pane::leaf(PaneSlot::Connecting(
                    pending,
                ))));
            app.active = 0;
            app.tabs[0].diff_overlay = Some(DiffOverlayState {
                host_id: crate::ui::host_ops::HostId::LOCAL,
                cwd: PathBuf::from("/repo"),
                source: DiffSource::Head,
                focus_handle: cx.focus_handle(),
                load: DiffLoad::Ready(Arc::new(DiffSnapshot {
                    files: vec![patched_file()],
                    ..Default::default()
                })),
                loading: false,
                expanded: HashMap::new(),
                focus: None,
                preview: None,
                preview_loading: None,
                selection: None,
                selecting: false,
                list: gpui::ListState::new(0, gpui::ListAlignment::Top, px(256.))
                    .with_size_hint(DIFF_LINE_H),
                rows: Rc::new(Vec::new()),
                rows_key: None,
                epoch: None,
            });
        });
        (app, vcx)
    }

    fn drag(
        app: &Entity<Tty7App>,
        vcx: &mut VisualTestContext,
        mode: DiffViewMode,
        side: Option<Side>,
        from: usize,
        to: usize,
    ) {
        // The view mode is a window-wide setting, and the overlay drops a
        // selection whose rows the current view never drew — so a drag in the
        // unified view has to happen with the unified view on.
        vcx.update(|_, cx| {
            let mut cfg = cx.global::<Config>().clone();
            cfg.diff_view = mode;
            cx.set_global(cfg);
        });
        app.update_in(vcx, |this, window, cx| {
            this.start_diff_selection(&at(from), mode, side, window, cx);
            this.extend_diff_selection(&at(to), side, Some(MouseButton::Left), cx);
        });
    }

    fn copied(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> Option<String> {
        app.update_in(vcx, |this, _, cx| this.copy_diff_selection(cx));
        vcx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text()))
    }

    fn selection(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> Option<DiffSelection> {
        app.update_in(vcx, |this, _, _| {
            this.tabs[0]
                .diff_overlay
                .as_ref()
                .and_then(|o| o.selection.clone())
        })
    }

    #[gpui::test]
    fn a_drag_down_a_column_copies_that_column(cx: &mut TestAppContext) {
        let (app, mut vcx) = window(cx);

        drag(&app, &mut vcx, DiffViewMode::Split, Some(Side::New), 0, 3);
        assert_eq!(copied(&app, &mut vcx).as_deref(), Some("a\nB\nd"));

        drag(&app, &mut vcx, DiffViewMode::Split, Some(Side::Old), 0, 3);
        assert_eq!(copied(&app, &mut vcx).as_deref(), Some("a\nb\nc\nd"));

        drag(&app, &mut vcx, DiffViewMode::Unified, None, 1, 3);
        assert_eq!(copied(&app, &mut vcx).as_deref(), Some("b\nc\nB"));
    }

    /// A drag that runs up the list leaves the head above the anchor. The
    /// range is read in drawn order either way, so it copies what the same two
    /// rows copy dragged the other way round.
    #[gpui::test]
    fn a_drag_up_a_column_copies_the_same_rows(cx: &mut TestAppContext) {
        let (app, mut vcx) = window(cx);

        drag(&app, &mut vcx, DiffViewMode::Split, Some(Side::New), 3, 0);
        let sel = selection(&app, &mut vcx).expect("the drag this test just made");
        assert!(
            sel.head < sel.anchor,
            "the drag ended above where it started"
        );
        assert_eq!(copied(&app, &mut vcx).as_deref(), Some("a\nB\nd"));

        drag(&app, &mut vcx, DiffViewMode::Split, Some(Side::Old), 3, 0);
        assert_eq!(copied(&app, &mut vcx).as_deref(), Some("a\nb\nc\nd"));

        drag(&app, &mut vcx, DiffViewMode::Unified, None, 3, 1);
        assert_eq!(copied(&app, &mut vcx).as_deref(), Some("b\nc\nB"));
    }

    /// The overlay is often drawn beside a live shell that holds the keyboard.
    /// Ctrl+C is the copy key only once the overlay has taken focus — until
    /// then the same keystroke would reach the pane and interrupt whatever is
    /// running in it.
    #[gpui::test]
    fn starting_a_drag_takes_the_keyboard(cx: &mut TestAppContext) {
        let (app, mut vcx) = window(cx);
        drag(&app, &mut vcx, DiffViewMode::Split, Some(Side::New), 0, 1);
        let focused = app.update_in(&mut vcx, |this, window, _| {
            this.tabs[0]
                .diff_overlay
                .as_ref()
                .expect("the overlay this window was built with")
                .focus_handle
                .is_focused(window)
        });
        assert!(focused);
    }

    /// A move with no button held is a release the overlay never saw — over a
    /// pane, or outside the window. The drag ends there rather than resuming
    /// the next time the pointer crosses a row.
    #[gpui::test]
    fn a_release_the_overlay_missed_ends_the_drag(cx: &mut TestAppContext) {
        let (app, mut vcx) = window(cx);
        drag(&app, &mut vcx, DiffViewMode::Split, Some(Side::New), 0, 1);

        app.update_in(&mut vcx, |this, _, cx| {
            this.extend_diff_selection(&at(3), Some(Side::New), None, cx);
        });
        assert_eq!(
            copied(&app, &mut vcx).as_deref(),
            Some("a\nB"),
            "the range stops where the pointer was last seen holding the button"
        );
    }

    /// The two views pair the same lines into different rows, so a range drawn
    /// in one of them points at other code in the other. `sync_diff_rows` is
    /// where the rows for a frame are settled, so it is where the range that
    /// no longer names any of them is dropped.
    #[gpui::test]
    fn switching_the_view_drops_the_selection(cx: &mut TestAppContext) {
        let (app, mut vcx) = window(cx);
        drag(&app, &mut vcx, DiffViewMode::Split, Some(Side::New), 0, 3);

        vcx.update(|_, cx| {
            let mut cfg = cx.global::<Config>().clone();
            cfg.diff_view = DiffViewMode::Unified;
            cx.set_global(cfg);
        });
        app.update_in(&mut vcx, |this, _, cx| {
            let _ = this.sync_diff_rows(cx);
        });
        assert!(selection(&app, &mut vcx).is_none());
    }

    /// A fresh read re-cuts the hunks. A range that survived one would keep
    /// its coordinates and quietly cover other code.
    #[gpui::test]
    fn a_fresh_snapshot_drops_the_selection(cx: &mut TestAppContext) {
        let (app, mut vcx) = window(cx);
        drag(&app, &mut vcx, DiffViewMode::Split, Some(Side::New), 0, 3);

        app.update_in(&mut vcx, |this, _, cx| {
            this.install_diff_snapshot(
                crate::ui::host_ops::HostId::LOCAL,
                &PathBuf::from("/repo"),
                &DiffSource::Head,
                Some(Arc::new(DiffSnapshot {
                    files: vec![patched_file()],
                    ..Default::default()
                })),
                cx,
            );
            let overlay = this.tabs[0].diff_overlay.as_ref().unwrap();
            assert!(overlay.selection.is_none());
            assert!(!overlay.selecting);
        });
    }

    #[gpui::test]
    fn a_copy_with_nothing_selected_leaves_the_clipboard_alone(cx: &mut TestAppContext) {
        let (app, mut vcx) = window(cx);
        vcx.update(|_, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("untouched".into()))
        });
        assert_eq!(copied(&app, &mut vcx).as_deref(), Some("untouched"));
    }

    /// Collapsing a file above the selection re-cuts the list, but not the
    /// rows the range names. A selection keyed on list positions would come
    /// away pointing at whatever slid into those slots.
    #[gpui::test]
    fn collapsing_a_file_above_the_range_leaves_it_on_the_same_lines(cx: &mut TestAppContext) {
        let (app, mut vcx) = window(cx);
        app.update_in(&mut vcx, |this, _, _| {
            let overlay = this.tabs[0].diff_overlay.as_mut().unwrap();
            overlay.load = DiffLoad::Ready(Arc::new(DiffSnapshot {
                files: vec![
                    FileDiff {
                        path: "src/above.rs".to_string(),
                        ..patched_file()
                    },
                    patched_file(),
                ],
                ..Default::default()
            }));
        });
        drag(&app, &mut vcx, DiffViewMode::Unified, None, 1, 3);

        app.update_in(&mut vcx, |this, _, cx| {
            let active = this.active;
            {
                let overlay = this.tabs[active].diff_overlay.as_mut().unwrap();
                overlay.expanded.insert("src/above.rs".to_string(), false);
            }
            let _ = this.sync_diff_rows(cx);
        });
        assert_eq!(
            copied(&app, &mut vcx).as_deref(),
            Some("b\nc\nB"),
            "the range still names the same three lines of the same file"
        );
    }
}

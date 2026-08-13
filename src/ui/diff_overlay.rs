use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    AnyElement, Background, FocusHandle, FontWeight, Hsla, KeyDownEvent, Pixels, SharedString,
    Window, div, prelude::*, px,
};
use gpui_component::button::Button;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use crate::core::config::{Config, DiffViewMode};
use crate::core::git::status::DecoStatus;
use crate::terminal::git_diff::{
    self, AUTO_COLLAPSE_LINES, CommitLabel, DiffSnapshot, DiffSource, DiffStats, FileDiff,
    FileStatus, LineKind, MAX_RENDERED_FILES, Truncation,
};

/// How much of an untracked file the preview will read. Past this the card
/// says the read failed rather than showing a silently cut-off file — and the
/// line budget below cuts rendering long before this does anyway.
const MAX_PREVIEW_BYTES: u64 = 4 * 1024 * 1024;
use crate::ui::app::Tty7App;
use crate::ui::diff_rows::{Side, SplitCell, SplitRow, UnifiedRow, split_hunk, unified_rows};
use crate::ui::i18n::{L10nKey, t, t_fmt, t_plural};
use crate::ui::right_panel::info_chip;
use crate::ui::rounding;
use crate::ui::rounding::RoundedCorners as _;
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
    pub(crate) scroll: gpui::ScrollHandle,
    /// The [`ScmData`](crate::terminal::git_data::ScmData) epoch this patch was
    /// read at, for the two sources that can go stale.
    ///
    /// Recorded when a probe *starts*, so a `git add` that lands while one is
    /// running is not mistaken for a change the result already reflects.
    /// `None` until the first snapshot arrives: the epoch is keyed by the
    /// repository root, and only a snapshot knows where that is.
    pub(crate) epoch: Option<u64>,
}

/// One hunk, already turned into whichever kind of row the current view draws.
enum HunkRows {
    Split(Vec<SplitRow>),
    Unified(Vec<UnifiedRow>),
}

impl HunkRows {
    fn len(&self) -> usize {
        match self {
            HunkRows::Split(rows) => rows.len(),
            HunkRows::Unified(rows) => rows.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
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
            scroll: gpui::ScrollHandle::new(),
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
            let (added, removed) = snap.totals();
            let root = snap.root.clone();
            let branch = snap.branch.clone();
            cx.default_global::<crate::terminal::git_status::GitStatusCache>();
            cx.update_global::<crate::terminal::git_status::GitStatusCache, _>(|cache, _| {
                cache.note_diff_read(host, &root, &branch, added, removed)
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.spawn_untracked_preview_if_needed(cx);
        let overlay = self.tabs.get(self.active)?.diff_overlay.as_ref()?;

        let content = match &overlay.load {
            DiffLoad::Loading => self.diff_message(t(L10nKey::DiffReading), cx),
            DiffLoad::NotARepo => self.diff_message(t(L10nKey::DiffNotARepo), cx),
            DiffLoad::Ready(snap) if empty_snapshot(snap) && snap.read_failed => {
                self.diff_message(t(L10nKey::DiffReadFailed), cx)
            }
            DiffLoad::Ready(snap) if empty_snapshot(snap) => {
                self.diff_message(t(L10nKey::DiffWorkingTreeClean), cx)
            }
            // A focused *untracked* file has no patch in the snapshot; its
            // card is synthesized from the file's own bytes — see `preview`.
            DiffLoad::Ready(snap) if untracked_focus(snap, overlay.focus.as_deref()).is_some() => {
                let path = untracked_focus(snap, overlay.focus.as_deref()).unwrap();
                match &overlay.preview {
                    Some((held, Some(file))) if held == path => {
                        self.diff_preview_card(file.as_ref(), &overlay.scroll, cx)
                    }
                    Some((held, None)) if held == path => {
                        self.diff_message(t(L10nKey::DiffReadFailed), cx)
                    }
                    _ => self.diff_message(t(L10nKey::DiffReading), cx),
                }
            }
            DiffLoad::Ready(snap) => self.diff_file_list(
                snap,
                &overlay.expanded,
                focused_file(snap, overlay),
                &overlay.scroll,
                cx,
            ),
        };

        let header = self.diff_header(overlay, window, cx);

        Some(
            v_flex()
                .absolute()
                .inset_0()
                .occlude()
                // Opaque on purpose: this overlay covers the entire workspace,
                // so window translucency and backdrop material must stop here.
                .bg(diff_overlay_background(
                    cx.try_global::<crate::ui::presets::ActiveBackground>(),
                    cx.theme().background,
                ))
                .text_color(cx.theme().foreground)
                .track_focus(&overlay.focus_handle)
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                    if ev.keystroke.key.as_str() == "escape" {
                        this.close_diff_overlay(window, cx);
                    }
                }))
                // The opaque fill above covers the theme background image the
                // workspace root paints, so the overlay carries its own copy,
                // dimmed back to the strength it had when this overlay was
                // itself translucent.
                .children(crate::ui::app::overlay_surface_layers(cx))
                .child(header)
                .child(content)
                .into_any_element(),
        )
    }

    fn diff_header(
        &self,
        overlay: &DiffOverlayState,
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
        let lead = if self.left_panel_open(cx) {
            crate::ui::app::CONTENT_INSET
        } else {
            crate::ui::app::TITLE_BAR_LEAD
        };
        let mono = SharedString::from(self.font_family.clone());
        let subject = source_subject(&overlay.source, branch);
        let row = crate::ui::app::title_bar_drag(
            h_flex().id("diff-overlay-header"),
            "diff-overlay-header",
            window,
            cx,
        );
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
                div()
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
                    bar.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(summary),
                    )
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
            .when(subject.label.is_none(), |bar| bar.child(div().flex_1()))
            .child(div().occlude().flex_shrink_0().child({
                let sf = cx.global::<crate::ui::presets::Surfaces>().window;
                let selected = usize::from(view_mode(cx) == DiffViewMode::Unified);
                self.segmented_on(
                    sf,
                    "diff-overlay-view",
                    &[t(L10nKey::DiffViewSplit), t(L10nKey::DiffViewUnified)],
                    selected,
                    cx,
                    |this, index, _window, cx| {
                        let mode = if index == 0 {
                            DiffViewMode::Split
                        } else {
                            DiffViewMode::Unified
                        };
                        this.update_config(cx, |cfg| cfg.diff_view = mode);
                    },
                )
            }))
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

    /// The one synthesized card, in the same scroll shell the file list uses.
    fn diff_preview_card(
        &self,
        file: &FileDiff,
        scroll: &gpui::ScrollHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mode = view_mode(cx);
        let list = v_flex()
            .gap_3()
            .p_4()
            .w_full()
            // `usize::MAX` keeps the element ids clear of the real list's.
            .child(self.diff_file_card(usize::MAX, file, true, mode, cx));
        crate::ui::scrollbar::with_vertical_scrollbar(
            "diff-overlay-scrollbar",
            div()
                .id("diff-overlay-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(scroll)
                .child(list),
            scroll,
        )
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

    fn diff_file_list(
        &self,
        snap: &DiffSnapshot,
        expanded: &HashMap<String, bool>,
        focused: Option<usize>,
        scroll: &gpui::ScrollHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let stats = snap.stats();
        let mode = view_mode(cx);
        let oversized = focused.is_none() && stats.oversized;
        let mut list = v_flex().gap_3().p_4().w_full();
        if oversized {
            list = list.child(self.diff_oversized_notice(snap, &stats, cx));
        }
        let shown = snap.files.len().min(MAX_RENDERED_FILES);
        for (idx, file) in snap.files.iter().enumerate() {
            if focused.is_some_and(|f| f != idx) {
                continue;
            }
            if focused.is_none() && idx >= shown {
                break;
            }
            let is_expanded = if focused == Some(idx) {
                expanded.get(&file.path).copied().unwrap_or(true)
            } else {
                file_expanded(file, expanded, oversized)
            };
            list = list.child(self.diff_file_card(idx, file, is_expanded, mode, cx));
        }
        if focused.is_none() && snap.files.len() > shown {
            let rest = snap.files.len() - shown;
            list = list.child(
                div()
                    .w_full()
                    .px_2p5()
                    .py_1p5()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(t_plural(L10nKey::DiffMoreFiles, rest, &[])),
            );
        }
        if focused.is_none() && !snap.untracked.is_empty() {
            list = list.child(self.diff_untracked_section(snap, cx));
        }
        // A whole working tree can scroll past here with nothing to say how
        // far it runs or where in it you are — the one long document in the
        // app without the bar every other scroll area has.
        crate::ui::scrollbar::with_vertical_scrollbar(
            "diff-overlay-scrollbar",
            div()
                .id("diff-overlay-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(scroll)
                .child(list),
            scroll,
        )
    }

    fn diff_oversized_notice(
        &self,
        snap: &DiffSnapshot,
        stats: &DiffStats,
        cx: &Context<Self>,
    ) -> AnyElement {
        let text = t_fmt(
            L10nKey::DiffOversizedNotice,
            &[("summary", &oversized_summary(snap, stats))],
        );
        div()
            .w_full()
            .px_2p5()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().secondary)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(text)
            .into_any_element()
    }

    fn diff_file_card(
        &self,
        idx: usize,
        file: &FileDiff,
        expanded: bool,
        mode: DiffViewMode,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expandable =
            !file.binary && (!file.hunks.is_empty() || file.truncated == Some(Truncation::Budget));
        let deco = deco_status(file.status);
        let (glyph, glyph_color) = (status_glyph(deco), status_color(deco, cx));
        let shown_path = match &file.old_path {
            Some(old) => format!("{old} → {}", file.path),
            None => file.path.clone(),
        };

        let has_body = expanded && (!file.hunks.is_empty() || file.truncated.is_some());

        let header_corners = rounding::stack_corners(
            0,
            if has_body { 2 } else { 1 },
            rounding::CARD_RADIUS,
            rounding::HAIRLINE,
        );
        let mut header = h_flex()
            .id(("diff-file-header", idx))
            .w_full()
            .items_center()
            .gap_2()
            .px_2p5()
            .py_1p5()
            .rounded_corners(header_corners)
            .bg(cx.theme().secondary)
            .when(expandable, |h| {
                let path = file.path.clone();
                h.cursor_pointer()
                    .hover(|s| s.bg(cx.theme().list_hover))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        let active = this.active;
                        if let Some(overlay) = this
                            .tabs
                            .get_mut(active)
                            .and_then(|t| t.diff_overlay.as_mut())
                        {
                            overlay.expanded.insert(path.clone(), !expanded);
                            cx.notify();
                        }
                    }))
                    .child(
                        Icon::new(if expanded {
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
                    .font_family(self.font_family.clone())
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
                    .font_family(self.font_family.clone())
                    .child(shown_path),
            );
        if file.binary {
            header = header.child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(t(L10nKey::Binary)),
            );
        }
        if file.added > 0 {
            header = header.child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(cx.theme().success)
                    .child(format!("+{}", file.added)),
            );
        }
        if file.removed > 0 {
            header = header.child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(format!("−{}", file.removed)),
            );
        }

        let mut card = v_flex()
            .w_full()
            .border_1()
            .border_color(cx.theme().border)
            .rounded(rounding::CARD_RADIUS)
            .overflow_hidden()
            .child(header);

        if has_body {
            let mut body = v_flex().w_full();
            let hunks: Vec<_> = file
                .hunks
                .iter()
                .map(|hunk| {
                    let rows = match mode {
                        DiffViewMode::Split => HunkRows::Split(split_hunk(&hunk.lines)),
                        DiffViewMode::Unified => HunkRows::Unified(unified_rows(&hunk.lines)),
                    };
                    (hunk, rows)
                })
                .collect();
            let closing_row = if file.truncated.is_some() {
                None
            } else {
                hunks
                    .iter()
                    .rposition(|(_, rows)| !rows.is_empty())
                    .map(|h| (h, hunks[h].1.len() - 1))
            };
            for (h, (hunk, rows)) in hunks.iter().enumerate() {
                body = body.child(
                    div()
                        .w_full()
                        .px_2()
                        .py_0p5()
                        .bg(cx.theme().muted)
                        .text_xs()
                        .font_family(self.font_family.clone())
                        .text_color(cx.theme().muted_foreground)
                        .truncate()
                        .child(hunk.header.clone()),
                );
                match rows {
                    HunkRows::Split(rows) => {
                        for (r, row) in rows.iter().enumerate() {
                            body = body.child(self.diff_split_row(
                                row,
                                closing_row == Some((h, r)),
                                cx,
                            ));
                        }
                    }
                    HunkRows::Unified(rows) => {
                        for (r, row) in rows.iter().enumerate() {
                            body = body.child(self.diff_unified_row(
                                row,
                                closing_row == Some((h, r)),
                                cx,
                            ));
                        }
                    }
                }
            }
            if let Some(reason) = file.truncated {
                let note = match reason {
                    Truncation::PerFile => t_fmt(
                        L10nKey::DiffTruncatedPerFile,
                        &[("limit", &git_diff::MAX_LINES_PER_FILE.to_string())],
                    ),
                    Truncation::Budget => t(L10nKey::DiffTruncatedBudget).to_string(),
                };
                body = body.child(
                    div()
                        .w_full()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(note),
                );
            }
            card = card.child(body);
        }
        card.into_any_element()
    }

    fn diff_split_row(&self, row: &SplitRow, closes_card: bool, cx: &Context<Self>) -> AnyElement {
        let radius = if closes_card {
            rounding::inner_radius(rounding::CARD_RADIUS, rounding::HAIRLINE)
        } else {
            px(0.)
        };
        h_flex()
            .w_full()
            .h(px(19.))
            .items_stretch()
            .text_xs()
            .font_family(self.font_family.clone())
            .child(self.diff_split_cell(row.left.as_ref(), Side::Old, radius, cx))
            .child(div().flex_shrink_0().w(px(1.)).bg(cx.theme().border))
            .child(self.diff_split_cell(row.right.as_ref(), Side::New, radius, cx))
            .into_any_element()
    }

    fn diff_split_cell(
        &self,
        cell: Option<&SplitCell>,
        side: Side,
        outer_radius: Pixels,
        cx: &Context<Self>,
    ) -> AnyElement {
        let base = h_flex().flex_1().min_w_0().h_full().items_center();
        let base = match side {
            Side::Old => base.rounded_bl(outer_radius),
            Side::New => base.rounded_br(outer_radius),
        };
        let Some(cell) = cell else {
            return base.bg(cx.theme().muted.opacity(0.3)).into_any_element();
        };
        let (marker, tint) = match (cell.changed, side) {
            (true, Side::Old) => ("−", Some(cx.theme().danger.opacity(0.12))),
            (true, Side::New) => ("+", Some(cx.theme().success.opacity(0.12))),
            (false, _) => (" ", None),
        };
        base.when_some(tint, |row, bg| row.bg(bg))
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
    /// Every measurement it shares with [`Self::diff_split_cell`] is shared on
    /// purpose — the same 19px row, the same `text_xs` in the same family, and
    /// above all the same `0.12` wash behind an addition and a removal. The two
    /// views are one diff seen twice; a different green would read as a
    /// different thing.
    ///
    /// What differs is forced by the shape. The line numbers get 34px a side
    /// rather than 42 (there are two gutters here in front of one column of
    /// text, not one in front of each), and the `+`/`−` gets a column of its
    /// own rather than riding in the text: with three kinds of line stacked in
    /// one column, an inlined marker would leave the context lines' code
    /// starting two characters left of everything else.
    fn diff_unified_row(
        &self,
        row: &UnifiedRow,
        closes_card: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let radius = if closes_card {
            rounding::inner_radius(rounding::CARD_RADIUS, rounding::HAIRLINE)
        } else {
            px(0.)
        };
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
        h_flex()
            .w_full()
            .h(px(19.))
            .items_center()
            .text_xs()
            .font_family(self.font_family.clone())
            .rounded_bl(radius)
            .rounded_br(radius)
            .when_some(tint, |line, bg| line.bg(bg))
            .child(gutter(row.old))
            .child(gutter(row.new))
            // The split view's centre rule, in the one place it still means the
            // same thing: everything left of it is a number, everything right
            // of it is the file.
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(1.))
                    .h_full()
                    .bg(cx.theme().border),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(12.))
                    .text_center()
                    .text_color(marker_color)
                    .child(unified_marker(row.kind)),
            )
            .child(div().flex_1().min_w_0().truncate().child(row.text.clone()))
            .into_any_element()
    }

    fn diff_untracked_section(&self, snap: &DiffSnapshot, cx: &Context<Self>) -> AnyElement {
        let total = snap.untracked_count();
        let untracked = &snap.untracked[..snap.untracked.len().min(MAX_RENDERED_FILES)];
        let header_corners = rounding::stack_corners(
            0,
            if total == 0 { 1 } else { 2 },
            rounding::CARD_RADIUS,
            rounding::HAIRLINE,
        );
        let mut section = v_flex()
            .w_full()
            .border_1()
            .border_color(cx.theme().border)
            .rounded(rounding::CARD_RADIUS)
            .overflow_hidden()
            .child(
                div()
                    .w_full()
                    .px_2p5()
                    .py_1p5()
                    .rounded_corners(header_corners)
                    .bg(cx.theme().secondary)
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(t_plural(L10nKey::DiffUntrackedHeader, total, &[])),
            );
        for path in untracked {
            section = section.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_2p5()
                    .py_1()
                    .text_xs()
                    .font_family(self.font_family.clone())
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_weight(FontWeight::BOLD)
                            .text_color(status_color(DecoStatus::Untracked, cx))
                            .child(status_glyph(DecoStatus::Untracked)),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(path.clone())),
            );
        }
        if total > untracked.len() {
            let rest = total - untracked.len();
            section = section.child(
                div()
                    .w_full()
                    .px_2p5()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(t_plural(L10nKey::DiffMoreUntracked, rest, &[])),
            );
        }
        section.into_any_element()
    }
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

fn focused_name(overlay: &DiffOverlayState) -> Option<String> {
    let DiffLoad::Ready(snap) = &overlay.load else {
        return None;
    };
    let idx = focused_file(snap, overlay)?;
    Some(snap.files[idx].path.clone())
}

fn empty_snapshot(snap: &DiffSnapshot) -> bool {
    snap.files.is_empty() && snap.untracked.is_empty()
}

fn file_expanded(file: &FileDiff, expanded: &HashMap<String, bool>, collapse_all: bool) -> bool {
    if let Some(&want) = expanded.get(&file.path) {
        return want;
    }
    !collapse_all && file.added + file.removed <= AUTO_COLLAPSE_LINES
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
    use crate::terminal::git_diff::{DiffLine, LineKind};
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
#[cfg(test)]
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

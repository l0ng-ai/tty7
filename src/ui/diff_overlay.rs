use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    AnyElement, FocusHandle, FontWeight, KeyDownEvent, Pixels, Window, div, prelude::*, px,
};
use gpui_component::button::Button;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use crate::terminal::git_diff::{
    self, AUTO_COLLAPSE_LINES, DiffSnapshot, DiffStats, FileDiff, FileStatus, LineKind,
    MAX_RENDERED_FILES, Truncation,
};
use crate::ui::app::Tty7App;
use crate::ui::rounding;
use crate::ui::rounding::RoundedCorners as _;

pub(crate) enum DiffLoad {
    Loading,
    Ready(Arc<DiffSnapshot>),
    NotARepo,
}

pub(crate) struct DiffOverlayState {
    pub(crate) host_id: crate::ui::host_ops::HostId,
    pub(crate) cwd: PathBuf,
    pub(crate) focus_handle: FocusHandle,
    pub(crate) load: DiffLoad,
    pub(crate) loading: bool,
    pub(crate) expanded: HashMap<String, bool>,
    pub(crate) focus: Option<String>,
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
        let active = self.active;
        let was_front = self.tabs.get(active).is_some_and(|t| {
            t.overlay_top == crate::ui::app::OverlayTop::Diff || !self.code_panel_visible()
        });
        if let Some(tab) = self.tabs.get_mut(active) {
            tab.overlay_top = crate::ui::app::OverlayTop::Diff;
        }
        match self
            .tabs
            .get_mut(active)
            .and_then(|t| t.diff_overlay.as_mut())
            .filter(|o| o.cwd == cwd && o.host_id == host)
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
        let seed = match (&self.right_panel.diff_cwd, &self.right_panel.diff) {
            (Some(panel_key), Some(Some(snap))) if *panel_key == (host, cwd.clone()) => {
                DiffLoad::Ready(Arc::clone(snap))
            }
            _ => DiffLoad::Loading,
        };
        self.remember_active_pane(window, cx);
        let Some(tab) = self.tabs.get_mut(active) else {
            return;
        };
        let focus_handle = cx.focus_handle();
        tab.diff_overlay = Some(DiffOverlayState {
            host_id: host,
            cwd,
            focus_handle: focus_handle.clone(),
            load: seed,
            loading: false,
            expanded: HashMap::new(),
            focus,
        });
        window.focus(&focus_handle, cx);
        self.spawn_diff_probe(cx);
        cx.notify();
    }

    pub(crate) fn diff_overlay_focus(
        &self,
        host: crate::ui::host_ops::HostId,
        cwd: &std::path::Path,
    ) -> Option<&str> {
        let overlay = self.tabs.get(self.active)?.diff_overlay.as_ref()?;
        (overlay.cwd == cwd && overlay.host_id == host).then_some(overlay.focus.as_deref())?
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
        let Some(overlay) = self
            .tabs
            .get_mut(active)
            .and_then(|t| t.diff_overlay.as_mut())
        else {
            return;
        };
        if overlay.loading {
            return;
        }
        let cwd = overlay.cwd.clone();
        let id = overlay.host_id;
        let Some(host) = crate::ui::host_registry::HostRegistry::lookup(cx, id) else {
            return;
        };
        overlay.loading = true;
        self.spawn_shared_diff_probe(host, cwd, cx);
    }

    pub(crate) fn spawn_shared_diff_probe(
        &mut self,
        host: crate::ui::host_ops::SharedHost,
        cwd: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let key = (host.id(), cwd.clone());
        if !self.diff_probes_inflight.insert(key.clone()) {
            self.diff_probes_restale.insert(key);
            return;
        }
        let host_for_retry = host.clone();
        let probe_cwd = cwd.clone();
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            move |h| git_diff::probe(h, &probe_cwd),
            move |app, result, cx| {
                app.diff_probes_inflight.remove(&key);
                app.install_diff_snapshot(key.0, &cwd, result.map(Arc::new), cx);
                if app.diff_probes_restale.remove(&(key.0, cwd.clone())) {
                    app.spawn_shared_diff_probe(host_for_retry, cwd, cx);
                }
            },
        );
    }

    fn install_diff_snapshot(
        &mut self,
        host: crate::ui::host_ops::HostId,
        cwd: &Path,
        snap: Option<Arc<DiffSnapshot>>,
        cx: &mut Context<Self>,
    ) {
        let mut landed = false;
        for tab in self.tabs.iter_mut() {
            let Some(overlay) = tab
                .diff_overlay
                .as_mut()
                .filter(|o| o.cwd == cwd && o.host_id == host)
            else {
                continue;
            };
            overlay.loading = false;
            overlay.load = match &snap {
                Some(snap) => DiffLoad::Ready(Arc::clone(snap)),
                None => DiffLoad::NotARepo,
            };
            landed = true;
        }
        let key = (host, cwd.to_path_buf());
        if self.right_panel.diff_pending.as_ref() == Some(&key) {
            self.right_panel.diff_pending = None;
            landed = true;
        }
        if self.right_panel.diff_cwd.as_ref() == Some(&key) {
            self.right_panel.diff = Some(snap);
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
        let Some(status) = cx
            .try_global::<crate::terminal::git_status::GitStatusCache>()
            .and_then(|cache| cache.status_for(overlay.host_id, &overlay.cwd))
        else {
            return;
        };
        if status.branch != snap.branch || (status.added, status.removed) != snap.totals() {
            self.spawn_diff_probe(cx);
        }
    }

    pub(crate) fn render_diff_overlay(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let overlay = self.tabs.get(self.active)?.diff_overlay.as_ref()?;

        let content = match &overlay.load {
            DiffLoad::Loading => self.diff_message("Reading diff…", cx),
            DiffLoad::NotARepo => self.diff_message("Not a git repository", cx),
            DiffLoad::Ready(snap) if empty_snapshot(snap) && snap.read_failed => self.diff_message(
                "Couldn't read the working-tree diff — retrying on the next refresh.",
                cx,
            ),
            DiffLoad::Ready(snap) if empty_snapshot(snap) => {
                self.diff_message("Working tree clean", cx)
            }
            DiffLoad::Ready(snap) => {
                self.diff_file_list(snap, &overlay.expanded, focused_file(snap, overlay), cx)
            }
        };

        let header = self.diff_header(overlay, window, cx);

        Some(
            v_flex()
                .absolute()
                .inset_0()
                .occlude()
                .bg(
                    match cx.try_global::<crate::ui::presets::ActiveBackground>() {
                        Some(bg) => crate::ui::theme::window_background(bg),
                        None => cx.theme().background.into(),
                    },
                )
                .text_color(cx.theme().foreground)
                .track_focus(&overlay.focus_handle)
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                    if ev.keystroke.key.as_str() == "escape" {
                        this.close_diff_overlay(window, cx);
                    }
                }))
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
                    .path("icons/git-branch.svg")
                    .flex_shrink_0()
                    .size(px(13.))
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(branch),
            )
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
                    let mut summary = format!(
                        "{} changed file{}",
                        files,
                        if files == 1 { "" } else { "s" }
                    );
                    if untracked > 0 {
                        summary.push_str(&format!(" · {untracked} untracked"));
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
                            .child("refreshing…"),
                    )
                },
            )
            .child(div().flex_1())
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
                    .tooltip("Close Diff (Esc)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_diff_overlay(window, cx);
                    })),
                ),
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
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let stats = snap.stats();
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
            list = list.child(self.diff_file_card(idx, file, is_expanded, cx));
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
                    .child(format!(
                        "… and {rest} more changed file{} — run `git diff` in the terminal to see them.",
                        if rest == 1 { "" } else { "s" }
                    )),
            );
        }
        if focused.is_none() && !snap.untracked.is_empty() {
            list = list.child(self.diff_untracked_section(snap, cx));
        }
        div()
            .id("diff-overlay-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(list)
            .into_any_element()
    }

    fn diff_oversized_notice(
        &self,
        snap: &DiffSnapshot,
        stats: &DiffStats,
        cx: &Context<Self>,
    ) -> AnyElement {
        let text = format!(
            "This working tree is too large to render efficiently ({}). Every file is \
             collapsed — expand individual files, or run `git diff` in the terminal.",
            oversized_summary(snap, stats),
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
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expandable =
            !file.binary && (!file.hunks.is_empty() || file.truncated == Some(Truncation::Budget));
        let (glyph, glyph_color) = match file.status {
            FileStatus::Added => ("A", cx.theme().success),
            FileStatus::Modified => ("M", cx.theme().warning),
            FileStatus::Deleted => ("D", cx.theme().danger),
            FileStatus::Renamed => ("R", cx.theme().muted_foreground),
        };
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
                    .child("binary"),
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
                .map(|hunk| (hunk, split_hunk(&hunk.lines)))
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
                for (r, row) in rows.iter().enumerate() {
                    body = body.child(self.diff_split_row(row, closing_row == Some((h, r)), cx));
                }
            }
            if let Some(reason) = file.truncated {
                let note = match reason {
                    Truncation::PerFile => format!(
                        "Diff truncated at {} lines — run `git diff` in the terminal for the rest.",
                        git_diff::MAX_LINES_PER_FILE
                    ),
                    Truncation::Budget => {
                        "Body not loaded — this working tree is past tty7's diff budget. \
                         Run `git diff` in the terminal for this file."
                            .to_string()
                    }
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
                    .child(format!("Untracked files ({total})")),
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
                            .text_color(cx.theme().success)
                            .child("A"),
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
                    .child(format!(
                        "… and {rest} more — run `git status` in the terminal to see them.",
                    )),
            );
        }
        section.into_any_element()
    }
}

fn focused_file(snap: &DiffSnapshot, overlay: &DiffOverlayState) -> Option<usize> {
    let path = overlay.focus.as_deref()?;
    snap.files.iter().position(|f| f.path == path)
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
    let mut parts = vec![format!(
        "{} changed file{}",
        snap.files.len(),
        if snap.files.len() == 1 { "" } else { "s" }
    )];
    let (added, removed) = stats.totals;
    let total_lines = (added + removed) as usize;
    let loaded = stats.retained_lines;
    let budget = stats.budget_exhausted;
    let per_file = stats.per_file_truncated;
    parts.push(match (budget, per_file) {
        (false, false) => format!("{total_lines} diff lines"),
        _ => {
            let cap = match (budget, per_file) {
                (true, true) => "tty7's budget and the per-file cap",
                (true, false) => "tty7's budget",
                _ => "the per-file cap",
            };
            format!(
                "{total_lines} changed lines, {loaded} diff rows loaded before {cap} cut the rest"
            )
        }
    });
    if stats.untracked_count > 0 {
        parts.push(format!("{} untracked", stats.untracked_count));
    }
    parts.join(", ")
}

#[derive(Clone, Copy)]
enum Side {
    Old,
    New,
}

struct SplitCell {
    no: Option<u32>,
    text: String,
    changed: bool,
}

struct SplitRow {
    left: Option<SplitCell>,
    right: Option<SplitCell>,
}

fn split_hunk(lines: &[git_diff::DiffLine]) -> Vec<SplitRow> {
    fn clean(text: &str) -> String {
        text.replace('\t', "    ")
    }
    fn flush(
        rows: &mut Vec<SplitRow>,
        rem: &mut Vec<&git_diff::DiffLine>,
        add: &mut Vec<&git_diff::DiffLine>,
    ) {
        for i in 0..rem.len().max(add.len()) {
            rows.push(SplitRow {
                left: rem.get(i).map(|l| SplitCell {
                    no: l.old_no,
                    text: clean(&l.text),
                    changed: true,
                }),
                right: add.get(i).map(|l| SplitCell {
                    no: l.new_no,
                    text: clean(&l.text),
                    changed: true,
                }),
            });
        }
        rem.clear();
        add.clear();
    }

    let mut rows = Vec::new();
    let mut rem: Vec<&git_diff::DiffLine> = Vec::new();
    let mut add: Vec<&git_diff::DiffLine> = Vec::new();
    for line in lines {
        match line.kind {
            LineKind::Removed => rem.push(line),
            LineKind::Added => add.push(line),
            LineKind::Context => {
                flush(&mut rows, &mut rem, &mut add);
                rows.push(SplitRow {
                    left: Some(SplitCell {
                        no: line.old_no,
                        text: clean(&line.text),
                        changed: false,
                    }),
                    right: Some(SplitCell {
                        no: line.new_no,
                        text: clean(&line.text),
                        changed: false,
                    }),
                });
            }
        }
    }
    flush(&mut rows, &mut rem, &mut add);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::git_diff::{DiffLine, LineKind};

    fn line(kind: LineKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old_no: old,
            new_no: new,
            text: text.to_string(),
        }
    }

    #[test]
    fn pairs_removed_and_added_side_by_side() {
        let lines = vec![
            line(LineKind::Context, Some(1), Some(1), "a"),
            line(LineKind::Removed, Some(2), None, "b"),
            line(LineKind::Removed, Some(3), None, "c"),
            line(LineKind::Added, None, Some(2), "B"),
            line(LineKind::Context, Some(4), Some(3), "d"),
        ];
        let rows = split_hunk(&lines);
        assert_eq!(rows.len(), 4);

        let l = rows[0].left.as_ref().unwrap();
        let r = rows[0].right.as_ref().unwrap();
        assert_eq!((l.no, l.text.as_str(), l.changed), (Some(1), "a", false));
        assert_eq!((r.no, r.text.as_str(), r.changed), (Some(1), "a", false));

        let l = rows[1].left.as_ref().unwrap();
        let r = rows[1].right.as_ref().unwrap();
        assert_eq!((l.no, l.text.as_str(), l.changed), (Some(2), "b", true));
        assert_eq!((r.no, r.text.as_str(), r.changed), (Some(2), "B", true));

        assert_eq!(rows[2].left.as_ref().unwrap().text, "c");
        assert!(rows[2].right.is_none());

        assert_eq!(rows[3].left.as_ref().unwrap().no, Some(4));
        assert_eq!(rows[3].right.as_ref().unwrap().no, Some(3));
    }

    #[test]
    fn expands_tabs_in_cell_text() {
        let lines = vec![line(LineKind::Added, None, Some(1), "\tindented")];
        let rows = split_hunk(&lines);
        assert_eq!(rows[0].right.as_ref().unwrap().text, "    indented");
        assert!(rows[0].left.is_none());
    }

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

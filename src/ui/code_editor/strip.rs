//! The header's file strip when there are more files than it has room for,
//! and the controls that share the header with it.
//!
//! **The strip keeps the files you use.** A docked column fits two or three
//! names, a filled window six or seven; past that the strip used to scroll
//! sideways, which put the file you were just in out of sight as often as
//! not. Now it holds the most recently fronted files — in their strip order,
//! so nothing jumps about as you switch between them — and a `+N` button at
//! its end lists everything, hidden files first, with a filter box.
//!
//! **Dock / Fill** is a two-cell switch rather than a menu entry, and a filled
//! editor leads with a pill naming the terminal it covers, which docks it back
//! beside that terminal.

use gpui::prelude::*;
use gpui::{
    Anchor, AnyElement, Context, Entity, FontWeight, SharedString, Subscription, Window, anchored,
    deferred, div, px, rems,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use super::{BufferId, TabCode};
use crate::ui::app::Tty7App;
use crate::ui::document_column::DocumentChrome;
use crate::ui::i18n::{L10nKey, t, t_fmt};
use tty7_core::core::config::DocumentLayout;

/// Height of a strip cell — a file, the `+N` button, the terminal pill.
pub(super) const CELL_H: f32 = 28.;
/// Their corner.
pub(super) const CELL_RADIUS: f32 = 7.;
/// The widest a file's name may make its cell before it is elided.
pub(super) const CELL_MAX_W: f32 = 168.;
/// Header text: a half step under the body size, like the rail's rows.
pub(super) const CELL_TEXT: f32 = 12.5 / 16.;

const PICKER_W: f32 = 340.;
const PICKER_LIST_MAX_H: f32 = 340.;
const PICKER_ROW_H: f32 = 28.;

impl TabCode {
    /// Every file in the strip, the one in front first and the rest by when
    /// they last were. Files that have never been in front follow in strip
    /// order.
    pub(crate) fn by_recency(&self) -> Vec<BufferId> {
        let front = self.active_id();
        let mut out: Vec<BufferId> = front.into_iter().collect();
        for id in self.recent.iter().chain(self.files.iter()) {
            if self.files.contains(id) && !out.contains(id) {
                out.push(*id);
            }
        }
        out
    }

    /// Moves the file in front to the head of `recent`. Run every frame the
    /// editor draws, so whatever brought a file forward — a click, a link,
    /// back/forward, a restore — is counted without a hook of its own. The
    /// other group's files keep their places, so moving the focus between
    /// groups of a split does not wipe either one's order.
    pub(crate) fn note_front(&mut self) {
        let order = self.by_recency();
        let rest: Vec<BufferId> = self
            .recent
            .iter()
            .copied()
            .filter(|id| !order.contains(id) && self.shows(*id))
            .collect();
        self.recent = order;
        self.recent.extend(rest);
    }

    /// Which files the strip shows when it has room for `cap`: the `cap` most
    /// recent, as `(position in files, id)` in strip order; and the rest,
    /// most recent first.
    pub(crate) fn strip_split(&self, cap: usize) -> (Vec<(usize, BufferId)>, Vec<BufferId>) {
        let order = self.by_recency();
        let cap = cap.max(1);
        let keep = &order[..cap.min(order.len())];
        let shown = self
            .files
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, id)| keep.contains(id))
            .collect();
        let hidden = order.iter().skip(cap).copied().collect();
        (shown, hidden)
    }
}

/// The `+N` list while it is open.
pub(crate) struct StripPicker {
    input: Entity<InputState>,
    /// The highlighted row, an index into the rows the query leaves.
    hi: usize,
    /// Whether the pointer is on the `+N` button. A press there is the
    /// button's to answer — it closes the list on click — so the list does
    /// not also close itself on the press and have the click reopen it.
    over_button: bool,
    _sub: Subscription,
}

/// One row of the list.
struct PickerRow {
    id: BufferId,
    name: SharedString,
    dir: SharedString,
    dirty: bool,
    front: bool,
}

impl Tty7App {
    /// How many names the strip holds. Fixed counts rather than a measure:
    /// names differ so much in width that a measured fit would reshuffle the
    /// strip whenever a long one came forward.
    pub(super) fn editor_strip_cap(&self, chrome: DocumentChrome, cx: &gpui::App) -> usize {
        let (left, right) = (self.left_panel_open(cx), self.right_panel_open(cx));
        match chrome {
            DocumentChrome::Fill if left && right => 5,
            DocumentChrome::Fill => 7,
            _ if right => 2,
            _ => 3,
        }
    }

    fn editor_strip_rows(&self, cx: &gpui::App) -> Vec<PickerRow> {
        let Some(code) = self.tab_code() else {
            return Vec::new();
        };
        let front = code.active_id();
        let roots = code.roots.as_slice();
        let local_host = self.spawn_host(cx);
        code.by_recency()
            .into_iter()
            .filter_map(|id| {
                let f = self.buffer(id)?;
                let dir = if f.untitled.is_some() {
                    String::new()
                } else {
                    let local = f.host.id() == local_host;
                    let mut segs =
                        super::nav::path_segments(&f.path, if local { roots } else { &[] });
                    segs.pop();
                    segs.join("/")
                };
                Some(PickerRow {
                    id,
                    name: f.label(),
                    dir: dir.into(),
                    dirty: f.dirty,
                    front: Some(id) == front,
                })
            })
            .collect()
    }

    /// The rows the query leaves, in the order Up/Down walks them.
    fn editor_strip_matches(&self, cap: usize, cx: &gpui::App) -> (Vec<PickerRow>, usize) {
        let q = self
            .editor
            .strip_picker
            .as_ref()
            .map(|p| p.input.read(cx).value().trim().to_lowercase())
            .unwrap_or_default();
        let rows = self.editor_strip_rows(cx);
        if !q.is_empty() {
            let hits: Vec<PickerRow> = rows
                .into_iter()
                .filter(|r| r.name.to_lowercase().contains(&q) || r.dir.to_lowercase().contains(&q))
                .collect();
            return (hits, 0);
        }
        // Hidden files first — they are what the list is for — then the ones
        // the strip already shows. Rows are in recency order, and the strip
        // shows the first `cap` of it.
        let cap = cap.max(1).min(rows.len());
        let mut rows = rows;
        let shown: Vec<PickerRow> = rows.drain(..cap).collect();
        let hidden = rows.len();
        rows.extend(shown);
        (rows, hidden)
    }

    pub(super) fn editor_toggle_strip_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor.strip_picker.is_some() {
            self.editor_close_strip_picker(window, cx);
            return;
        }
        let n = self.tab_code().map_or(0, |c| c.files.len());
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t_fmt(L10nKey::EditorStripSearch, &[("n", &n.to_string())]))
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        let _sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                if let Some(p) = this.editor.strip_picker.as_mut() {
                    p.hi = 0;
                }
                cx.notify();
            }
        });
        self.editor.strip_picker = Some(StripPicker {
            input,
            hi: 0,
            over_button: true,
            _sub,
        });
        cx.notify();
    }

    pub(super) fn editor_close_strip_picker(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor.strip_picker.take().is_some() {
            self.focus_editor(window, cx);
            cx.notify();
        }
    }

    fn editor_strip_pick(&mut self, id: BufferId, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.strip_picker = None;
        if let Some(pos) = self
            .tab_code()
            .and_then(|c| c.files.iter().position(|f| *f == id))
        {
            self.editor_activate(pos, window, cx);
        }
        cx.notify();
    }

    fn on_strip_picker_key(
        &mut self,
        ev: &gpui::KeyDownEvent,
        cap: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (rows, _) = self.editor_strip_matches(cap, cx);
        let Some(p) = self.editor.strip_picker.as_mut() else {
            return;
        };
        let n = rows.len();
        match ev.keystroke.key.as_str() {
            "escape" => self.editor_close_strip_picker(window, cx),
            "up" if n > 0 => {
                p.hi = p.hi.min(n - 1).saturating_sub(1);
                cx.notify();
            }
            "down" if n > 0 => {
                p.hi = (p.hi + 1).min(n - 1);
                cx.notify();
            }
            "enter" => {
                if let Some(row) = rows.get(p.hi.min(n.saturating_sub(1))) {
                    let id = row.id;
                    self.editor_strip_pick(id, window, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    /// The `+N` button at the end of the strip, with its list when open.
    pub(super) fn render_strip_overflow(
        &self,
        hidden: &[BufferId],
        cap: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if hidden.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let open = self.editor.strip_picker.is_some();
        let hidden_dirty = hidden
            .iter()
            .any(|id| self.buffer(*id).is_some_and(|f| f.dirty));
        let button = h_flex()
            .id("editor-strip-more")
            .occlude()
            .flex_none()
            .h(px(CELL_H))
            .px(px(8.))
            .gap(px(5.))
            .items_center()
            .rounded(px(CELL_RADIUS))
            .text_size(rems(12. / 16.))
            .font_features(crate::ui::theme::tabular_figures())
            .text_color(theme.muted_foreground)
            .when(open, |d| d.bg(theme.sidebar_accent))
            .hover(|s| s.bg(theme.sidebar_accent).text_color(theme.foreground))
            .when(hidden_dirty, |d| {
                d.child(
                    div()
                        .size(px(crate::ui::tab_strip::ROW_STATUS_DOT))
                        .rounded_full()
                        .bg(theme.muted_foreground),
                )
            })
            .child(format!("+{}", hidden.len()))
            .child(
                Icon::new(IconName::ChevronDown)
                    .xsmall()
                    .text_color(theme.muted_foreground),
            )
            .tooltip(|window, cx| {
                gpui_component::tooltip::Tooltip::new(t(L10nKey::EditorStripAllFiles))
                    .build(window, cx)
            })
            .on_hover(cx.listener(|this, over: &bool, _, _| {
                if let Some(p) = this.editor.strip_picker.as_mut() {
                    p.over_button = *over;
                }
            }))
            .on_click(cx.listener(|this, _, window, cx| {
                this.editor_toggle_strip_picker(window, cx);
            }));
        Some(
            div()
                .relative()
                .flex_none()
                .child(button)
                .children(self.render_strip_picker(cap, cx))
                .into_any_element(),
        )
    }

    fn render_strip_picker(&self, cap: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.editor.strip_picker.as_ref()?;
        let (rows, hidden) = self.editor_strip_matches(cap, cx);
        let querying = !picker.input.read(cx).value().trim().is_empty();
        let hi = picker.hi.min(rows.len().saturating_sub(1));
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let row_hover = gpui::rgb(cx.global::<crate::ui::presets::Surfaces>().popover.hover);

        let heading = |label: SharedString| {
            div()
                .h(px(24.))
                .pt(px(4.))
                .px(px(8.))
                .flex()
                .items_center()
                .text_size(rems(11. / 16.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(muted)
                .child(label)
        };
        let empty = rows.is_empty();
        let mut list = v_flex()
            .id("editor-strip-list")
            .max_h(px(PICKER_LIST_MAX_H))
            .overflow_y_scroll()
            .gap(px(1.));
        for (i, row) in rows.into_iter().enumerate() {
            if !querying && i == 0 && hidden > 0 {
                list = list.child(heading(
                    t_fmt(L10nKey::EditorStripHidden, &[("n", &hidden.to_string())]).into(),
                ));
            }
            if !querying && i == hidden {
                list = list.child(heading(t(L10nKey::EditorStripInBar).into()));
            }
            let id = row.id;
            let group: SharedString = format!("editor-strip-row-{i}").into();
            let slot = div()
                .id(("editor-strip-row-close", i))
                .relative()
                .flex_none()
                .size(px(18.))
                .rounded(px(4.))
                .hover(|s| s.bg(theme.muted))
                .when(row.dirty, |d| {
                    d.child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .group_hover(group.clone(), |s| s.invisible())
                            .child(div().size(px(6.)).rounded_full().bg(muted)),
                    )
                })
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(row.dirty, |d| d.invisible())
                        .group_hover(group.clone(), |s| s.visible())
                        .child(Icon::new(IconName::Close).xsmall().text_color(muted)),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.editor_close_buffer(id, window, cx);
                }));
            list = list.child(
                h_flex()
                    .id(("editor-strip-row", i))
                    .group(group)
                    .flex_none()
                    .h(px(PICKER_ROW_H))
                    .pl(px(8.))
                    .pr(px(4.))
                    .gap(px(8.))
                    .items_center()
                    .rounded(px(6.))
                    .when(i == hi, |d| d.bg(row_hover))
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if let Some(p) = this.editor.strip_picker.as_mut()
                            && p.hi != i
                        {
                            p.hi = i;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.editor_strip_pick(id, window, cx);
                    }))
                    .child(
                        div()
                            .min_w(px(40.))
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .overflow_hidden()
                            .text_color(fg)
                            .when(row.front, |d| d.font_weight(FontWeight::MEDIUM))
                            .child(row.name),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .overflow_hidden()
                            .text_size(rems(11.5 / 16.))
                            .text_color(muted)
                            .child(row.dir),
                    )
                    .child(slot),
            );
        }
        if empty {
            list = list.child(
                div()
                    .h(px(PICKER_ROW_H))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .text_color(muted)
                    .child(t(L10nKey::EditorStripNoMatch)),
            );
        }

        let rule = || div().h(px(1.)).mx(px(6.)).my(px(4.)).bg(theme.border);
        let footer_button = |key: &'static str, label: L10nKey| {
            div()
                .id(key)
                .text_color(muted)
                .hover(|s| s.text_color(fg))
                .child(t(label))
        };
        let card = v_flex()
            .occlude()
            .w(px(PICKER_W))
            .p(px(5.))
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .border_1()
            .border_color(theme.border)
            .rounded(px(10.))
            .shadow_lg()
            .text_size(rems(CELL_TEXT))
            .on_key_down(
                cx.listener(move |this, ev: &gpui::KeyDownEvent, window, cx| {
                    this.on_strip_picker_key(ev, cap, window, cx);
                }),
            )
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                if this
                    .editor
                    .strip_picker
                    .as_ref()
                    .is_some_and(|p| p.over_button)
                {
                    return;
                }
                this.editor_close_strip_picker(window, cx);
            }))
            .child(
                h_flex()
                    .h(px(30.))
                    .px(px(4.))
                    .gap(px(3.))
                    .items_center()
                    .child(Icon::new(IconName::Search).xsmall().text_color(muted))
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&picker.input)
                                .appearance(false)
                                .small()
                                .text_size(rems(CELL_TEXT)),
                        ),
                    ),
            )
            .child(rule())
            .child(list)
            .child(rule())
            .child(
                h_flex()
                    .h(px(28.))
                    .px(px(8.))
                    .gap(px(14.))
                    .items_center()
                    .text_size(rems(12. / 16.))
                    .child(
                        footer_button("editor-strip-close-saved", L10nKey::EditorStripCloseSaved)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.editor.strip_picker = None;
                                let ids = this.editor_strip_others(true);
                                this.editor_close_buffers(ids, window, cx);
                                this.focus_editor(window, cx);
                            })),
                    )
                    .child(
                        footer_button("editor-strip-close-others", L10nKey::EditorStripCloseOthers)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.editor.strip_picker = None;
                                let ids = this.editor_strip_others(false);
                                this.editor_close_buffers(ids, window, cx);
                                this.focus_editor(window, cx);
                            })),
                    ),
            );
        Some(
            div()
                .absolute()
                .right_0()
                .top(px(CELL_H + 4.))
                .child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopRight)
                            .snap_to_window_with_margin(px(8.))
                            .child(card),
                    )
                    .with_priority(1),
                )
                .into_any_element(),
        )
    }

    /// Every file in the strip but the one in front — only the saved ones
    /// when `saved_only`.
    fn editor_strip_others(&self, saved_only: bool) -> Vec<BufferId> {
        let Some(code) = self.tab_code() else {
            return Vec::new();
        };
        let front = code.active_id();
        code.files
            .iter()
            .copied()
            .filter(|id| Some(*id) != front)
            .filter(|id| !saved_only || self.buffer(*id).is_some_and(|f| !f.dirty))
            .collect()
    }

    /// Dock / Fill, as a two-cell switch: a raised cell on a sunken track.
    pub(super) fn render_layout_switch(
        &self,
        chrome: DocumentChrome,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let sf = cx.global::<crate::ui::presets::Surfaces>().window;
        let dark = theme.mode.is_dark();
        // What is on screen, not what was asked for: a window too narrow for
        // a column fills even when set to dock.
        let current = if chrome.is_dock() {
            DocumentLayout::Dock
        } else {
            DocumentLayout::Fill
        };
        let cell = |i: usize, layout: DocumentLayout, tip: L10nKey| {
            let live = layout == current;
            let ink = if live {
                theme.foreground
            } else {
                theme.muted_foreground
            };
            // The glyph: the window, with the column the editor takes in it.
            let frame = div()
                .relative()
                .w(px(14.))
                .h(px(11.))
                .border_1()
                .border_color(ink)
                .rounded(px(2.));
            let glyph = match layout {
                DocumentLayout::Dock => frame.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(4.))
                        .w(px(1.))
                        .bg(ink),
                ),
                DocumentLayout::Fill => frame.child(
                    div()
                        .absolute()
                        .top(px(1.5))
                        .bottom(px(1.5))
                        .left(px(1.5))
                        .right(px(1.5))
                        .rounded(px(0.8))
                        .bg(ink),
                ),
            };
            div()
                .id(("editor-layout", i))
                .w(px(24.))
                .h(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .when(live, |d| {
                    d.bg(if dark {
                        gpui::rgb(sf.selected).into()
                    } else {
                        theme.background
                    })
                    .shadow_xs()
                })
                .child(glyph)
                .tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(t(tip)).build(window, cx)
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_document_layout(layout, cx);
                }))
        };
        h_flex()
            .id("editor-layout-switch")
            .occlude()
            .flex_none()
            .ml(px(8.))
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(CELL_RADIUS))
            .bg(gpui::rgb(sf.hover))
            .child(cell(0, DocumentLayout::Dock, L10nKey::DocumentDock))
            .child(cell(1, DocumentLayout::Fill, L10nKey::DocumentFill))
            .into_any_element()
    }

    /// A filled editor's lead: the terminal it covers, by name, and a way back
    /// to it that keeps the file open beside it.
    pub(super) fn render_back_to_terminal(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let name = self
            .tabs
            .get(self.active)
            .map(|tab| self.tab_label(tab, self.active, Some(window), cx))
            .unwrap_or_default();
        let badge_bg = gpui::rgb(cx.global::<crate::ui::presets::Surfaces>().window.selected);
        h_flex()
            .flex_none()
            .items_center()
            .child(
                h_flex()
                    .id("editor-back-to-terminal")
                    .occlude()
                    .h(px(CELL_H))
                    .pl(px(8.))
                    .pr(px(10.))
                    .gap(px(7.))
                    .items_center()
                    .rounded(px(CELL_RADIUS))
                    .text_size(rems(CELL_TEXT))
                    .text_color(theme.muted_foreground)
                    .hover(|s| s.bg(theme.sidebar_accent).text_color(theme.foreground))
                    .child(
                        div()
                            .flex_none()
                            .size(px(16.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(badge_bg)
                            .font_family(theme.mono_font_family.clone())
                            .text_size(px(9.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(">_"),
                    )
                    .child(
                        div()
                            .max_w(px(CELL_MAX_W))
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .overflow_hidden()
                            .child(name),
                    )
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new(t(L10nKey::DocumentDock))
                            .build(window, cx)
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_document_layout(DocumentLayout::Dock, cx);
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .w(crate::ui::theme::hairline(window))
                    .h(px(16.))
                    .mx(px(6.))
                    .bg(theme.border),
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
    fn the_strip_keeps_the_files_last_in_front_in_their_strip_order() {
        let ids = ids(5);
        let mut code = code(&ids, 0);
        for front in [3, 1, 4] {
            code.active = front;
            code.note_front();
        }
        let (shown, hidden) = code.strip_split(3);
        // 4, 1 and 3 were in front last; they keep their strip positions.
        assert_eq!(shown, vec![(1, ids[1]), (3, ids[3]), (4, ids[4])]);
        // The rest by recency: 0 was in front before any of them, 2 never.
        assert_eq!(hidden, vec![ids[0], ids[2]]);
    }

    #[test]
    fn the_file_in_front_always_has_a_place() {
        let ids = ids(4);
        let mut code = code(&ids, 0);
        code.note_front();
        // Brought forward from outside the strip's view (a link, the list)
        // without a frame drawn in between.
        code.active = 3;
        let (shown, _) = code.strip_split(1);
        assert_eq!(shown, vec![(3, ids[3])]);
    }

    #[test]
    fn a_split_keeps_the_other_groups_order() {
        let ids = ids(4);
        let mut code = code(&ids[..2], 1);
        code.split = Some(super::super::split::OtherGroup {
            files: ids[2..].to_vec(),
            active: 0,
            focus_left: true,
        });
        code.note_front();
        code.swap_focus();
        code.note_front();
        code.swap_focus();
        code.note_front();
        assert_eq!(code.recent, vec![ids[1], ids[0], ids[2], ids[3]]);
    }

    #[test]
    fn closed_files_leave_the_order() {
        let ids = ids(3);
        let mut code = code(&ids, 2);
        code.note_front();
        code.files.remove(1);
        code.active = 1;
        code.note_front();
        assert_eq!(code.recent, vec![ids[2], ids[0]]);
    }
}

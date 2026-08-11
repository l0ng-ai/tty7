use gpui::{AnyElement, Context, Window, div, prelude::*, px, rems};
use gpui_component::button::Button;
use gpui_component::input::Input;
use gpui_component::{
    ActiveTheme as _, Icon, IconName, InteractiveElementExt as _, Sizable as _, h_flex, v_flex,
};
use std::path::PathBuf;

use crate::core::config::{Config, RightPanelTab};
use crate::daemon::protocol::PaneProcs;
use crate::ui::app::{
    CONTENT_INSET, TILE_GLYPH_SM, TILE_SIZE_SM, Tty7App, tile_trailing_inset,
    tile_trailing_inset_sm,
};
use crate::ui::i18n::{L10nKey, t};
use crate::ui::scrollbar::with_vertical_scrollbar;

pub(crate) const MIN_WIDTH: f32 = 216.;
pub(crate) const MAX_WIDTH_RATIO: f32 = 0.5;

/// How wide a panel edge is to grab. Both edges a window can drag — the tab
/// sidebar's and this panel's — are the same target, so they are one number.
pub(crate) const RESIZE_HANDLE_WIDTH: f32 = 8.;

/// The panel's type scale, in rems, on the same ladder as the rest of the
/// window.
///
/// This panel used to carry its own run of pixel sizes — 12 for body, 11.5/11
/// under it — which put its *primary* text at the size everything else uses
/// for *secondary* text, so the panel read a step smaller than the sidebar
/// beside it, and stayed that size when `ui_font_size` moved. In rems `TEXT`
/// and `META` are exactly `text_sm()` and `text_xs()`; they are spelled out
/// only because the mono variants have to be derived from them.
///
/// Mono sits a notch under the sans it pairs with: at an equal size its
/// x-height and stems read a size larger, which turns a label and its value
/// into two sizes instead of one line. The notch is a rem fraction rather than
/// a fixed pixel, so the correction scales with the text it is correcting.
const STEP: f32 = 1. / 16.;
pub(crate) const TEXT: f32 = 14. * STEP;
pub(crate) const TEXT_MONO: f32 = TEXT - STEP;
pub(crate) const META: f32 = 12. * STEP;
pub(crate) const META_MONO: f32 = META - STEP;

/// Uppercase section headings. Deliberately below `META` — it matches the tab
/// sidebar's group headings, which are the same thing one panel over.
const HEADING: f32 = 11. * STEP;

// The right panel's type ramp: four steps, half a point apart, that the Info
// and Source Control tabs draw from so switching between them does not change
// the apparent size of the panel. (The Files tab, in `file_tree.rs`, is the
// one holdout — it renders its rows with `text_sm()` = 14px.) The steps are
// close together on purpose: the panel is a dense aside next to the terminal,
// and the differences between them are meant to be felt as hierarchy rather
// than seen as different type sizes.
//
// (The px constants that used to live here — PANEL_TEXT and its steps — were
// superseded by the interface font scale's rems tokens; the Source Control
// panel still names its own px steps locally until it moves onto that scale.)

/// Height of the search strip.
///
/// gpui-component sizes an `Input` border-box, and `.xsmall()` is
/// `input_h(Size::XSmall)` = `h_5()` = 20px: one `LINE_HEIGHT` of `Rems(1.25)`
/// = 20px with `input_py(Size::XSmall)` = 0 above and below. (`.appearance(false)`
/// only drops the background, border and radius; the padding and the height
/// stay.) Thirty leaves that field 5px of slack top and bottom.
///
/// Load-bearing beyond this file: `scm/panel.rs` pins its commit box to the
/// same height with a `const _: () = assert!(…)`, so the two tabs' top strips
/// line up.
pub(crate) const SEARCH_H: f32 = 30.;

#[derive(Default)]
pub(crate) struct RightPanelState {
    pub(crate) procs_pane: Option<u64>,
    pub(crate) procs: Option<PaneProcs>,
    pub(crate) procs_loading: bool,
    pub(crate) procs_gen: u64,
    pub(crate) procs_forwards: Option<crate::ui::app::ForwardRoute>,
    pub(crate) scroll: gpui::ScrollHandle,
    pub(crate) tree_scroll: gpui::ScrollHandle,
}

const PROCS_POLL: std::time::Duration = std::time::Duration::from_millis(2000);

/// Widest of the labels actually on screen, so the values line up without a
/// fixed width guessing at them.
///
/// A hardcoded 46px fitted "cwd" and "shell" and nothing else: English
/// "changes" wrapped mid-word to "change / s", and in Chinese and Japanese
/// almost every label wrapped — ja "作業ディレクトリ" is eight glyphs. The
/// clamp keeps the longest of those from eating the panel; anything past it
/// runs into the gap rather than folding, which `whitespace_nowrap` on the
/// label guarantees.
fn info_label_column(
    rows: &[(&'static str, String)],
    window: &mut Window,
    cx: &gpui::App,
) -> gpui::Pixels {
    // Shaping needs real pixels, so this is the one place the rem has to be
    // resolved by hand. Both bounds were measured against a 12px label, so
    // they are carried as multiples of it rather than as pixels — otherwise
    // raising `ui_font_size` grows the labels into a clamp fitted to a
    // smaller face, and every one of them wraps.
    let label_px = TEXT * window.rem_size().as_f32();
    let min = 46. / 12. * label_px;
    let max = 108. / 12. * label_px;
    let font = gpui::Font {
        family: cx.theme().font_family.clone(),
        features: Default::default(),
        fallbacks: None,
        weight: Default::default(),
        style: Default::default(),
    };
    let widest = rows
        .iter()
        .map(|(k, _)| {
            window
                .text_system()
                .shape_line(
                    gpui::SharedString::from(*k),
                    px(label_px),
                    &[gpui::TextRun {
                        len: k.len(),
                        font: font.clone(),
                        color: gpui::Hsla::default(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                )
                .width
                .as_f32()
        })
        .fold(min, f32::max);
    px(widest.clamp(min, max).ceil())
}

impl Tty7App {
    pub(crate) fn right_panel_open(&self, _cx: &gpui::App) -> bool {
        self.right_panel_visible && !self.tabs.is_empty()
    }

    pub(crate) fn right_panel_px(&self, window: &Window, _cx: &gpui::App) -> f32 {
        let max = (window.viewport_size().width.as_f32() * MAX_WIDTH_RATIO).max(MIN_WIDTH);
        self.right_panel_width.get().clamp(MIN_WIDTH, max)
    }

    pub(crate) fn toggle_right_panel(&mut self, cx: &mut Context<Self>) {
        let next = !self.right_panel_visible;
        self.right_panel_visible = next;
        self.update_config(cx, |cfg| cfg.right_panel_visible = next);
        cx.notify();
    }

    pub(crate) fn set_right_panel_tab(&mut self, tab: RightPanelTab, cx: &mut Context<Self>) {
        self.right_panel_tab = tab;
        self.right_panel_visible = true;
        self.update_config(cx, |cfg| {
            cfg.right_panel_tab = tab;
            cfg.right_panel_visible = true;
        });
        cx.notify();
    }

    pub(crate) fn render_right_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let panel_open = self.right_panel_open(cx);
        if let Some(open) = self.sftp_panel.open_pane_id
            && (!panel_open || self.remote_files_pane(window, cx).map(|(id, _)| id) != Some(open))
        {
            self.sftp_close_browser(cx);
        }
        if !panel_open {
            return None;
        }
        let width = self.right_panel_px(window, cx);
        let tab = self.right_panel_tab;

        let body = match tab {
            RightPanelTab::Info => self.render_panel_info(window, cx),
            RightPanelTab::Scm => self.render_panel_scm(window, cx),
            RightPanelTab::Files => self.render_panel_files(window, cx),
        };
        let (backing, handle) = self.right_panel_resize(cx);

        Some(
            v_flex()
                .id("right-panel")
                .relative()
                .flex_none()
                .w(px(width))
                .h_full()
                .child(backing)
                .bg(crate::ui::theme::workspace_surface_color(cx))
                .border_l_1()
                .border_color(cx.theme().sidebar_border)
                .children(cfg!(target_os = "macos").then(|| {
                    let row = h_flex()
                        .id("right-panel-titlebar-drag")
                        .flex_none()
                        .h(px(crate::ui::app::TITLE_BAR_HEIGHT))
                        .border_b_1()
                        .border_color(cx.theme().transparent);
                    crate::ui::app::window_move_gesture(
                        row,
                        "right-panel-titlebar-drag",
                        window,
                        cx,
                    )
                    .on_double_click(|_, window, _| window.titlebar_double_click())
                    .items_center()
                    .gap(px(2.))
                    .pl(px(tile_trailing_inset()))
                    .children(self.right_panel_tabs(cx))
                    .child(div().flex_1())
                    .child(self.window_chrome(window, cx))
                }))
                .child(body)
                .children(self.sftp_transfers_footer(cx))
                .child(handle)
                .into_any_element(),
        )
    }

    fn right_panel_resize(&self, cx: &mut Context<Self>) -> (AnyElement, AnyElement) {
        use gpui::{Bounds, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, canvas};
        use std::cell::Cell as StdCell;
        use std::rc::Rc;

        let container: Rc<StdCell<Option<Bounds<Pixels>>>> = Rc::new(StdCell::new(None));
        let backing = canvas(
            {
                let container = container.clone();
                move |bounds, _window, _cx| container.set(Some(bounds))
            },
            {
                let container = container.clone();
                let width_cell = self.right_panel_width.clone();
                let dragging = self.right_panel_dragging.clone();
                move |_bounds, _state, window, _cx| {
                    window.on_mouse_event({
                        let container = container.clone();
                        let width_cell = width_cell.clone();
                        let dragging = dragging.clone();
                        move |ev: &MouseMoveEvent, _phase, window, _cx| {
                            if !dragging.get() {
                                return;
                            }
                            let Some(b) = container.get() else {
                                return;
                            };
                            let right = b.origin.x + b.size.width;
                            let raw = (right - ev.position.x).as_f32();
                            let max = (window.viewport_size().width.as_f32() * MAX_WIDTH_RATIO)
                                .max(MIN_WIDTH);
                            width_cell.set(raw.clamp(MIN_WIDTH, max));
                            window.refresh();
                        }
                    });
                    window.on_mouse_event({
                        let width_cell = width_cell.clone();
                        let dragging = dragging.clone();
                        move |_ev: &MouseUpEvent, _phase, window, cx| {
                            if !dragging.get() {
                                return;
                            }
                            dragging.set(false);
                            let w = width_cell.get();
                            let cfg = cx.global_mut::<Config>();
                            if cfg.right_panel_width != w {
                                cfg.right_panel_width = w;
                                cfg.save();
                            }
                            window.refresh();
                        }
                    });
                }
            },
        )
        .absolute()
        .size_full()
        .into_any_element();

        let active = self.right_panel_dragging.get();
        let handle = div()
            .group("right-panel-resize")
            .occlude()
            .absolute()
            .top_0()
            .left(px(-(RESIZE_HANDLE_WIDTH / 2.)))
            .w(px(RESIZE_HANDLE_WIDTH))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_col_resize()
            .child(
                div()
                    .w(px(1.))
                    .h_full()
                    .when(active, |d| d.bg(cx.theme().drag_border))
                    .group_hover("right-panel-resize", |s| s.bg(cx.theme().drag_border)),
            )
            .on_mouse_down(MouseButton::Left, {
                let dragging = self.right_panel_dragging.clone();
                move |_ev, window, _cx| {
                    dragging.set(true);
                    window.refresh();
                }
            })
            .into_any_element();

        (backing, handle)
    }

    pub(crate) fn panel_title(
        &self,
        text: &str,
        count: Option<String>,
        trailing: Option<AnyElement>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tabs = (!cfg!(target_os = "macos")).then(|| self.right_panel_tabs(cx));
        let has_trailing = trailing.is_some();
        if tabs.is_none() && !has_trailing {
            return div().flex_none().into_any_element();
        }
        let row = crate::ui::app::window_move_gesture(
            h_flex().id("panel-title"),
            "panel-title-drag",
            window,
            cx,
        );
        row.flex_none()
            .h(px(if tabs.is_some() {
                crate::ui::app::TITLE_BAR_HEIGHT
            } else {
                32.
            }))
            .items_center()
            .pl(px(CONTENT_INSET))
            .pr(px(match (&tabs, has_trailing) {
                (Some(_), _) => tile_trailing_inset(),
                (None, true) => tile_trailing_inset_sm(),
                (None, false) => CONTENT_INSET,
            }))
            .when(tabs.is_some(), |this| {
                this.border_b_1().border_color(cx.theme().sidebar_border)
            })
            .child(
                h_flex()
                    .flex_shrink_0()
                    .items_baseline()
                    .gap(px(7.))
                    .child(
                        // The title step of the panel ramp, SEMIBOLD and
                        // uppercased. It reads as a label rather than as
                        // content because of the weight and the caps.
                        div()
                            .text_size(rems(META))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(cx.theme().secondary_foreground)
                            .child(text.to_uppercase()),
                    )
                    .when_some(count, |this, c| {
                        this.child(
                            // A count is a token hanging off the heading, not
                            // part of it: one step down, mono, regular weight.
                            div()
                                .text_size(rems(META_MONO))
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_color(cx.theme().muted_foreground.opacity(0.75))
                                .child(c),
                        )
                    }),
            )
            .child(div().flex_1().min_w_0())
            .when_some(trailing, |this, t| this.child(t))
            .when_some(tabs, |this, tiles| {
                this.child(
                    h_flex()
                        .flex_shrink_0()
                        .items_center()
                        .gap(px(2.))
                        .when(has_trailing, |this| this.ml(px(6.)))
                        .children(tiles),
                )
            })
            .into_any_element()
    }

    pub(crate) fn panel_search(
        &self,
        input: &gpui::Entity<gpui_component::input::InputState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .flex_none()
            .items_center()
            // 8 here plus the `.xsmall()` field's own 4px of leading padding
            // is 12px of daylight between the glyph and the first character.
            .gap(px(8.))
            .h(px(SEARCH_H))
            .px(px(CONTENT_INSET))
            .child(
                Icon::new(IconName::Search)
                    .small()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(input).appearance(false).xsmall()),
            )
            .into_any_element()
    }

    pub(crate) fn panel_scroll(&self, inner: AnyElement, title: AnyElement) -> AnyElement {
        let body = div()
            .id("right-panel-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.right_panel.scroll)
            .child(inner);
        v_flex()
            .flex_1()
            .min_h_0()
            .child(title)
            .child(with_vertical_scrollbar(
                "right-panel-body-scrollbar",
                body,
                &self.right_panel.scroll,
            ))
            .into_any_element()
    }

    pub(crate) fn panel_empty(
        &self,
        text: &str,
        hint: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .px(px(CONTENT_INSET))
            .py(px(4.))
            .gap(px(3.))
            .text_size(rems(TEXT))
            .text_color(muted)
            .child(text.to_string())
            .children(hint.map(|h| {
                div()
                    .text_size(rems(META))
                    .text_color(muted.opacity(0.75))
                    .child(h.to_string())
            }))
            .into_any_element()
    }

    fn render_panel_info(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let title = self.panel_title(t(L10nKey::PanelInfoTitle), None, None, window, cx);
        let mut rows: Vec<(&'static str, String)> = Vec::new();
        let mut cwd_for_actions: Option<(PathBuf, bool)> = None;
        let mut pane_id: Option<u64> = None;
        let mut forwards_pane: Option<u64> = None;

        if let Some(tab) = self.tabs.get(self.active) {
            if let Some(leaf) = tab.detail_pane(window, cx) {
                let view = leaf.read(cx);
                pane_id = Some(view.pane_id);
                if let Some(cwd) = view.effective_cwd() {
                    rows.push((t(L10nKey::PanelCwd), compact_path(&cwd)));
                    // Copy Path is right either way; Reveal only means anything
                    // when the path is on the machine the file manager can see.
                    cwd_for_actions = Some((cwd, view.local_cwd().is_some()));
                }
                let shell = match view.shell_spec().map(|s| s.program.clone()) {
                    Some(program) => crate::core::shells::default_shell_name(Some(&program)),
                    None => self.default_shell_label(cx),
                };
                rows.push((t(L10nKey::PanelShell), shell));
                if let Some(ssh) = view.ssh_spec() {
                    rows.push((t(L10nKey::PanelSsh), ssh.host.clone()));
                }
                let connected_ssh = view
                    .remote_context()
                    .is_some_and(|c| c.kind == crate::daemon::protocol::RemoteKind::NativeSsh)
                    && matches!(
                        view.ssh_phase(),
                        Some(crate::daemon::protocol::SshPhase::Connected)
                    );
                if connected_ssh || view.workspace().is_some() {
                    forwards_pane = Some(view.pane_id);
                }
            }
            if let Some(git) = tab.git_status(Some(window), cx) {
                rows.push((t(L10nKey::PanelBranch), git.branch.clone()));
                rows.push((
                    t(L10nKey::PanelChangesRow),
                    format!("+{} −{}", git.added, git.removed),
                ));
            }
            if let Some(agent) = tab.agent(cx) {
                let name = agent.display_name();
                let status = match tab.agent_status(cx) {
                    Some(s) => format!("{name} · {}", agent_status_label(s)),
                    None => name.to_string(),
                };
                rows.push((t(L10nKey::PanelAgent), status));
            }
        }

        if rows.is_empty() {
            return self.panel_scroll(
                self.panel_empty(
                    t(L10nKey::PanelNoSession),
                    Some(t(L10nKey::PanelNoSessionHint)),
                    cx,
                ),
                title,
            );
        }

        let route = forwards_pane.map(|id| self.forward_route(id, cx));
        self.sync_procs(pane_id, route, cx);

        let mono = cx.theme().mono_font_family.clone();
        let cwd_label = t(L10nKey::PanelCwd);
        let label_w = info_label_column(&rows, window, cx);
        let mut list = v_flex().px(px(CONTENT_INSET)).py(px(2.)).gap(px(3.));
        for (k, v) in rows {
            list = list.child(
                h_flex()
                    .items_baseline()
                    .gap(px(9.))
                    .py(px(1.))
                    .text_size(rems(TEXT))
                    .child(
                        div()
                            .flex_none()
                            .w(label_w)
                            .whitespace_nowrap()
                            .text_color(cx.theme().muted_foreground)
                            .child(k),
                    )
                    .child(match k == cwd_label {
                        // A path identifies a pane by its last segment, and
                        // plain truncation eats exactly that: a deep checkout
                        // read "/private/tmp/claude-501…" and told you
                        // nothing. Let the head absorb the shrinking so the
                        // leaf survives, the way a file manager shows a path.
                        true => {
                            let (head, leaf) = split_path_leaf(&v);
                            h_flex()
                                .flex_1()
                                .min_w_0()
                                .text_size(rems(TEXT_MONO))
                                .font_family(mono.clone())
                                .text_color(cx.theme().foreground)
                                .child(div().min_w_0().flex_shrink(999.).truncate().child(head))
                                .child(div().min_w_0().flex_shrink(1.).truncate().child(leaf))
                                .into_any_element()
                        }
                        false => div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(rems(TEXT_MONO))
                            .font_family(mono.clone())
                            .text_color(cx.theme().foreground)
                            .child(v)
                            .into_any_element(),
                    }),
            );
        }

        let inner = v_flex()
            .child(self.panel_subtitle(t(L10nKey::PanelSessionSubtitle), false, None, cx))
            .child(list)
            .when_some(cwd_for_actions, |this, (cwd, local)| {
                this.child(self.cwd_actions(cwd, local, cx))
            })
            .children(self.procs_section(pane_id, cx))
            .children(self.ports_section(pane_id, cx))
            .children(self.forwards_section(forwards_pane, cx))
            .into_any_element();
        self.panel_scroll(inner, title)
    }

    fn cwd_actions(&self, cwd: PathBuf, local: bool, cx: &mut Context<Self>) -> AnyElement {
        let reveal_label = reveal_label();
        h_flex()
            .gap(px(2.))
            .px(px(tile_trailing_inset_sm()))
            .pt(px(6.))
            .when(local, |this| {
                this.child(
                    crate::ui::tab_strip::chrome_tile_sized(
                        Button::new("panel-info-reveal").icon(Icon::new(IconName::FolderOpen)),
                        TILE_SIZE_SM,
                        TILE_GLYPH_SM,
                        false,
                        cx,
                    )
                    .rounded_md()
                    .tooltip(reveal_label)
                    .on_click({
                        let cwd = cwd.clone();
                        move |_, _window, cx| cx.reveal_path(&cwd)
                    }),
                )
            })
            .child(
                crate::ui::tab_strip::chrome_tile_sized(
                    Button::new("panel-info-copy-path").icon(Icon::new(IconName::Copy)),
                    TILE_SIZE_SM,
                    TILE_GLYPH_SM,
                    false,
                    cx,
                )
                .rounded_md()
                .tooltip(t(L10nKey::FileTreeContextCopyPath))
                .on_click(move |_, _window, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                        cwd.display().to_string(),
                    ));
                }),
            )
            .into_any_element()
    }

    pub(crate) fn panel_subtitle(
        &self,
        text: &str,
        divider: bool,
        trailing: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .when(divider, |d| {
                d.mt(px(6.)).border_t_1().border_color(cx.theme().border)
            })
            .items_center()
            .justify_between()
            .pl(px(CONTENT_INSET))
            .pr(px(if trailing.is_some() {
                CONTENT_INSET - crate::ui::app::TILE_PAD
            } else {
                CONTENT_INSET
            }))
            .pt(px(match (divider, trailing.is_some()) {
                (true, false) => 12.,
                (true, true) => 8.,
                (false, false) => 10.,
                (false, true) => 6.,
            }))
            .pb(px(if trailing.is_some() { 0. } else { 4. }))
            .child(
                // A group header sits below the panel's own title in the
                // hierarchy, so it sits below it in the ramp too: the smallest
                // step, carried by weight and caps rather than by size.
                div()
                    .text_size(rems(HEADING))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .child(text.to_uppercase()),
            )
            .when_some(trailing, |this, t| this.child(t))
            .into_any_element()
    }

    fn procs_section(&self, pane_id: Option<u64>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let procs = &self.procs(pane_id)?.procs;
        if procs.len() < 2 {
            return None;
        }
        let mono = cx.theme().mono_font_family.clone();
        let mut list = v_flex().px(px(CONTENT_INSET)).py(px(1.)).gap(px(2.));
        for p in procs {
            list = list.child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .pl(px(f32::from(p.depth) * 10.))
                            .text_size(rems(TEXT_MONO))
                            .font_family(mono.clone())
                            .text_color(if p.foreground {
                                cx.theme().foreground
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(p.name.clone()),
                    )
                    .child(info_chip(
                        &p.pid.to_string(),
                        cx.theme().accent,
                        cx.theme().muted_foreground,
                        &mono,
                    )),
            );
        }
        Some(
            v_flex()
                .child(self.panel_subtitle(t(L10nKey::PanelProcessesSubtitle), true, None, cx))
                .child(list)
                .into_any_element(),
        )
    }

    fn ports_section(&self, pane_id: Option<u64>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ports = &self.procs(pane_id)?.ports;
        if ports.is_empty() {
            return None;
        }
        let mono = cx.theme().mono_font_family.clone();
        let mut list = v_flex().px(px(CONTENT_INSET)).py(px(1.)).gap(px(2.));
        for p in ports {
            list = list.child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(info_chip(
                        &p.port.to_string(),
                        cx.theme().accent,
                        cx.theme().foreground,
                        &mono,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(rems(TEXT_MONO))
                            .font_family(mono.clone())
                            .text_color(cx.theme().muted_foreground)
                            .child(p.name.clone()),
                    ),
            );
        }
        Some(
            v_flex()
                .child(self.panel_subtitle(t(L10nKey::PanelPortsSubtitle), true, None, cx))
                .child(list)
                .into_any_element(),
        )
    }

    fn procs(&self, pane_id: Option<u64>) -> Option<&PaneProcs> {
        (pane_id.is_some() && self.right_panel.procs_pane == pane_id)
            .then_some(self.right_panel.procs.as_ref())?
    }

    fn sync_procs(
        &mut self,
        pane_id: Option<u64>,
        forwards: Option<crate::ui::app::ForwardRoute>,
        cx: &mut Context<Self>,
    ) {
        let Some(pane_id) = pane_id else { return };
        self.right_panel.procs_forwards = forwards.clone();
        if self.right_panel.procs_pane != Some(pane_id) {
            self.right_panel.procs_pane = Some(pane_id);
            self.right_panel.procs = None;
            self.loopback_panel.managed.clear();
            self.right_panel.procs_gen += 1;
            self.right_panel.procs_loading = false;
        }
        if !self.right_panel.procs_loading {
            self.right_panel.procs_loading = true;
            let generation = self.right_panel.procs_gen;
            self.spawn_procs_query(pane_id, generation, forwards, cx);
        }
    }

    fn spawn_procs_query(
        &mut self,
        pane_id: u64,
        generation: u64,
        forwards: Option<crate::ui::app::ForwardRoute>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let route = forwards.clone();
            let (procs, managed) = cx
                .background_executor()
                .spawn(async move {
                    let procs = crate::terminal::RemoteTerminal::query_procs(pane_id);
                    let managed = route.map(|r| r.list()).unwrap_or_default();
                    (procs, managed)
                })
                .await;
            let keep_polling = this
                .update(cx, |app, cx| {
                    if app.right_panel.procs_gen != generation {
                        return false;
                    }
                    app.right_panel.procs = Some(procs);
                    if forwards.is_some() {
                        app.loopback_panel.managed = managed;
                    }
                    cx.notify();
                    let wanted =
                        app.right_panel_visible && app.right_panel_tab == RightPanelTab::Info;
                    if !wanted {
                        app.right_panel.procs_loading = false;
                    }
                    wanted
                })
                .unwrap_or(false);
            if !keep_polling {
                return;
            }
            cx.background_executor().timer(PROCS_POLL).await;
            let _ = this.update(cx, |app, cx| {
                if app.right_panel.procs_gen != generation {
                    return;
                }
                let wanted = app.right_panel_visible && app.right_panel_tab == RightPanelTab::Info;
                if wanted {
                    let forwards = app.right_panel.procs_forwards.clone();
                    app.spawn_procs_query(pane_id, generation, forwards, cx);
                } else {
                    app.right_panel.procs_loading = false;
                }
            });
        })
        .detach();
    }

    fn render_panel_files(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let remote = self.remote_files_pane(window, cx);
        let host = remote.as_ref().map(|(_, host)| host.clone());
        if self.sftp_sync_pane(remote.map(|(id, _)| id), window, cx) {
            return self.render_panel_sftp(host.unwrap_or_default(), window, cx);
        }

        let title = self.panel_title(t(L10nKey::PanelFilesTitle), None, None, window, cx);
        let search = self.panel_search(&self.file_search.clone(), cx);
        let rows = self.render_file_tree_rows(window, cx);
        v_flex()
            .flex_1()
            .min_h_0()
            .child(title)
            .child(search)
            .child(rows)
            .into_any_element()
    }

    /// The host label for whatever the Files panel is currently showing over
    /// SFTP, for copy that has to name the machine it is about to change.
    pub(crate) fn remote_files_host(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        self.remote_files_pane(window, cx).map(|(_, host)| host)
    }

    fn remote_files_pane(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(u64, String)> {
        use crate::daemon::protocol::{RemoteKind, SshPhase};
        let leaf = self.tabs.get(self.active)?.detail_pane(window, cx)?;
        let view = leaf.read(cx);
        let remote = view.remote_context()?;
        if remote.kind != RemoteKind::NativeSsh
            || !matches!(view.ssh_phase(), Some(SshPhase::Connected))
        {
            return None;
        }
        Some((view.pane_id, remote.target))
    }
}

/// Width of the fixed cell a git status letter is centred in.
///
/// Load-bearing beyond this function: `scm/panel.rs` gives its group-header
/// chevron box exactly this width so the group arrows and the status letters
/// stack into one vertical line down the right edge of the panel, and it keeps
/// its own `BADGE_W` in step. Changing it here without changing it there
/// breaks that column.
pub(crate) const BADGE_W: f32 = 14.;

/// A single-letter git status marker in a fixed-width cell.
///
/// Mono and SEMIBOLD so `M`, `A`, `D` and `U` all read as the same kind of
/// mark at a glance, and centred in a cell wide enough for the widest of them
/// at [`PANEL_TEXT_META`] — that is what makes a column of them line up
/// instead of drifting with the glyph widths.
pub(crate) fn git_badge(letter: &str, color: gpui::Hsla, mono: &gpui::SharedString) -> AnyElement {
    div()
        .flex_none()
        .w(px(BADGE_W))
        .text_center()
        .text_size(rems(META_MONO))
        .font_family(mono.clone())
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(color)
        .child(letter.to_string())
        .into_any_element()
}

/// A small filled pill around a mono token — a pid, a port number.
///
/// The padding and the radius are derived from the text size: at
/// [`PANEL_TEXT_META`] the line box is `round(10.5 × 1.618) = 17px`, so 1.5px
/// of vertical padding makes the pill 20px tall — one pixel more than the 19px
/// line of [`PANEL_TEXT`] beside it, which is what sets the height of a ports
/// row. Horizontal padding of 5px is about half an em of breathing room on
/// each side, and radius 4 is a fifth of the pill's height.
pub(crate) fn info_chip(
    text: &str,
    bg: gpui::Hsla,
    fg: gpui::Hsla,
    mono: &gpui::SharedString,
) -> AnyElement {
    div()
        .flex_none()
        .px(px(5.))
        .py(px(1.5))
        .rounded(px(4.))
        .bg(bg)
        .text_size(rems(META_MONO))
        .font_family(mono.clone())
        .text_color(fg)
        .child(text.to_string())
        .into_any_element()
}

pub fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        t(L10nKey::PanelRevealInFinder)
    } else {
        t(L10nKey::PanelOpenFolder)
    }
}

fn agent_status_label(status: crate::core::cli_agent::AgentStatus) -> &'static str {
    use crate::core::cli_agent::AgentStatus::*;
    match status {
        Idle => t(L10nKey::PanelAgentIdle),
        Working => t(L10nKey::PanelAgentWorking),
        Waiting => t(L10nKey::PanelAgentWaiting),
        Done => t(L10nKey::PanelAgentDone),
    }
}

/// Splits a path into everything-but-the-last-segment and the last segment,
/// so a row can shrink the first and keep the second.
fn split_path_leaf(s: &str) -> (String, String) {
    match s.rfind('/') {
        // Keep the separator with the head: "~/a/b/" + "c" rejoins exactly.
        Some(i) if i + 1 < s.len() => (s[..=i].to_string(), s[i + 1..].to_string()),
        _ => (String::new(), s.to_string()),
    }
}

fn compact_path(path: &std::path::Path) -> String {
    let s = path.to_string_lossy().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && s.starts_with(&home) => s.replacen(&home, "~", 1),
        _ => s,
    }
}

#[cfg(test)]
mod tests {
    use super::split_path_leaf;

    #[test]
    fn the_head_and_leaf_rejoin_into_the_path_they_came_from() {
        for p in [
            "~/repo/tty7",
            "/private/tmp/claude-501/a-very-long-directory/and-another-level",
            "/",
            "relative",
            "",
        ] {
            let (head, leaf) = split_path_leaf(p);
            assert_eq!(format!("{head}{leaf}"), p, "rejoining {p:?}");
        }
    }

    #[test]
    fn the_leaf_is_the_segment_that_names_the_directory() {
        let (head, leaf) = split_path_leaf("/a/b/c");
        assert_eq!((head.as_str(), leaf.as_str()), ("/a/b/", "c"));
        // A trailing slash has no leaf to keep, so the whole thing is head.
        let (head, leaf) = split_path_leaf("/a/b/");
        assert_eq!((head.as_str(), leaf.as_str()), ("", "/a/b/"));
        // Root is one segment with nothing before it.
        let (head, leaf) = split_path_leaf("/");
        assert_eq!((head.as_str(), leaf.as_str()), ("", "/"));
    }
}

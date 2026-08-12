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
    CONTENT_INSET, TILE_GLYPH_XS, TILE_SIZE_XS, Tty7App, tile_trailing_inset,
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
pub(crate) const HEADING: f32 = 11. * STEP;

// The right panel's type ramp: four steps, a point apart, that the Info and
// Source Control tabs both draw from so switching between them does not change
// the apparent size of the panel. The Files tab, in `file_tree.rs`, reaches the
// same 14px through `text_sm()`, which is the same rem under another name. The
// steps are close together on purpose: the panel is a dense aside next to the
// terminal, and the differences between them are meant to be felt as hierarchy
// rather than seen as different type sizes.
//
// Every tab of the panel is on this ladder now. The px constants that used to
// live here — PANEL_TEXT and its steps — went when the interface font scale
// landed, and `scm/` followed a branch later: it had been cut from main hours
// before that commit and had copied the ladder as it stood, which left the
// Source Control tab frozen at the *old* 12/11/10.5 while its neighbours moved
// to 14/13/12/11 and started tracking `ui_font_size`. Nothing in git conflicted
// — the two touched different files — so the only thing that would have caught
// it was a reader noticing that one tab was a step smaller than the rest.
//
// Which is the reason to keep reaching for these names rather than spelling a
// number: a size written as `px(12.)` anywhere in this panel is either a
// mistake or something that is not type.

/// Rows are laid out inside this inset and then pad themselves back out, so a
/// hovered row's background is wider than its text on both sides.
///
/// The text lands on `CONTENT_INSET` whatever this is — a list subtracts it
/// outside the row and the row adds it back inside — so all this number sets
/// is how far the hover fill bleeds past the text. It lives here rather than
/// in one tab because every tab of this panel is the same list of rows seen
/// from a different angle, and a fill that bleeds 4px under Source Control and
/// 6px under Info is a panel whose rows visibly do not belong to each other.
pub(crate) const ROW_INSET: f32 = 4.;

/// The strip the row and group action buttons live in, revealed by hovering
/// `row`.
///
/// Absolutely positioned and opaque, so it covers the tail of the row's text
/// rather than pushing it aside: hovering a row must not move a single pixel
/// of it, or the list crawls under the pointer.
///
/// It stops the mouse-down by hand instead of calling `occlude()`, which is
/// the obvious way to keep a click off the row underneath and was what made
/// the buttons vanish the moment the pointer reached them. `occlude()` is a
/// *hitbox* behaviour, and gpui inserts hitboxes in prepaint, which never
/// looks at `visibility` — so the strip blocked the mouse even while it was
/// invisible. Blocking cuts the hit test short at the blocking hitbox, and the
/// row's hitbox is behind this one because a parent prepaints before its
/// children; `group_hover` is nothing more than "is the group's hitbox
/// hovered", so the row stopped counting as hovered and the strip hid itself
/// — background, buttons and all — with the pointer sitting right on it. The
/// buttons' own hitboxes came from prepaint and outlived the paint, so they
/// went on answering tooltips for glyphs that were no longer drawn.
///
/// Stopping propagation buys the same "this click is ours, not the row's"
/// without lying to the hit test: children register their handlers after this
/// one and gpui bubbles back to front, so a button still gets its click first.
pub(crate) fn action_strip(row: &gpui::SharedString, backing: u32) -> gpui::Div {
    h_flex()
        .absolute()
        .right(px(ROW_INSET))
        .top_0()
        .bottom_0()
        .items_center()
        .gap(px(1.))
        .bg(gpui::rgb(backing))
        .invisible()
        .group_hover(row.clone(), |s| s.visible())
        .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
}

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

/// What a session row draws in its value column.
///
/// Every row used to be a `(&str, String)` pair rendered identically, and the
/// column paid for it twice: `changes` came out as an inert mono `+0 −0` —
/// the same fact the sidebar draws in green and red and opens the diff overlay
/// from — and the agent's state came out as a word where the sidebar has a
/// coloured dot. A row carries its own shape now, so one pane's facts read the
/// same whichever surface is showing them.
enum InfoValue {
    /// Mono text, truncated from the tail.
    Text(String),
    /// A filesystem path, shrunk from the head so the leaf survives.
    Path(String),
    /// `+N −M` in the sidebar's two colours, and a click into the diff
    /// overlay when the setting that governs the sidebar's counts allows it.
    Diff {
        added: u32,
        removed: u32,
        open: Option<(crate::ui::host_ops::HostId, PathBuf)>,
    },
    /// An agent and what it is doing, behind the status dot the sidebar draws
    /// on the tab — `hollow` for Waiting, which is a different *shape* rather
    /// than one more hue, for the same reason the tab's dot is.
    Agent {
        text: String,
        dot: Option<u32>,
        hollow: bool,
    },
}

/// One label/value line of the Session section.
struct InfoRow {
    label: &'static str,
    value: InfoValue,
    /// What this row's copy tile puts on the clipboard, where copying it is
    /// plausibly what someone wants — a path, a host, a branch. `None` on the
    /// rows where it is not ("zsh"), because a hover affordance that appears
    /// on every row teaches nothing about which rows can do something.
    copy: Option<String>,
    /// Set on the working-directory row when the path is on the machine the
    /// file manager can see, which is the only case Reveal means anything in.
    reveal: Option<PathBuf>,
}

impl InfoRow {
    fn text(label: &'static str, value: String) -> Self {
        Self {
            label,
            value: InfoValue::Text(value),
            copy: None,
            reveal: None,
        }
    }

    fn copyable(mut self) -> Self {
        self.copy = match &self.value {
            InfoValue::Text(v) | InfoValue::Path(v) => Some(v.clone()),
            _ => None,
        };
        self
    }

    /// Whether the row does anything if you click or hover it. It is what
    /// decides the hover fill, so the fill never promises an action the row
    /// does not have.
    fn interactive(&self) -> bool {
        self.copy.is_some()
            || self.reveal.is_some()
            || matches!(self.value, InfoValue::Diff { open: Some(_), .. })
    }
}

/// Widest of the labels actually on screen, so the values line up without a
/// fixed width guessing at them.
///
/// A hardcoded 46px fitted "cwd" and "shell" and nothing else: English
/// "changes" wrapped mid-word to "change / s", and in Chinese and Japanese
/// almost every label wrapped — ja "作業ディレクトリ" is eight glyphs. The
/// clamp keeps the longest of those from eating the panel; anything past it
/// runs into the gap rather than folding, which `whitespace_nowrap` on the
/// label guarantees.
fn info_label_column(rows: &[InfoRow], window: &mut Window, cx: &gpui::App) -> gpui::Pixels {
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
        .map(|row| {
            let k = row.label;
            window
                .text_system()
                .shape_line(
                    gpui::SharedString::from(k),
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
                    // A filter with no way out of it but selecting the text
                    // and deleting it is a filter people leave on and then
                    // wonder where their files went. The button only exists
                    // while there is something to clear, so an empty field
                    // still reads as one line of chrome.
                    .child(Input::new(input).appearance(false).xsmall().cleanable(true)),
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
        let mut rows: Vec<InfoRow> = Vec::new();
        let mut pane_id: Option<u64> = None;
        let mut forwards_pane: Option<u64> = None;
        // Whether the ports below are this machine's. They are listed by the
        // daemon that owns the pane, so what decides it is which machine that
        // daemon runs on — not whether the shell has since ssh'd somewhere,
        // which would hide the browser tile on a `ssh -L` pane whose forwarded
        // listener is on this machine and reachable.
        let mut local_pane = false;
        // Where the `changes` row's counts lead. Same source as the sidebar's,
        // and gated on the same setting, so turning the preview off turns it
        // off in both places rather than in one of them.
        let mut diff_target: Option<(crate::ui::host_ops::HostId, PathBuf)> = None;
        let mut git: Option<crate::terminal::git_status::GitStatus> = None;

        if let Some(tab) = self.tabs.get(self.active) {
            if let Some(leaf) = tab.detail_pane(window, cx) {
                let view = leaf.read(cx);
                pane_id = Some(view.pane_id);
                local_pane = view.host_id().is_local();
                diff_target = crate::ui::tab_sidebar::diff_click_cwd(
                    cx.global::<Config>(),
                    view.git_status_cwd()
                        .map(|cwd| (view.host_id(), cwd.to_path_buf())),
                );
                if let Some(cwd) = view.effective_cwd() {
                    rows.push(InfoRow {
                        label: t(L10nKey::PanelCwd),
                        value: InfoValue::Path(compact_path(&cwd)),
                        // The compacted `~/…` spelling is for reading; what
                        // goes on the clipboard is the path a shell can use.
                        copy: Some(cwd.display().to_string()),
                        // Reveal only means anything when the path is on the
                        // machine the file manager can see.
                        reveal: view.local_cwd().is_some().then(|| cwd.clone()),
                    });
                }
                let shell = match view.shell_spec().map(|s| s.program.clone()) {
                    Some(program) => crate::core::shells::default_shell_name(Some(&program)),
                    None => self.default_shell_label(cx),
                };
                rows.push(InfoRow::text(t(L10nKey::PanelShell), shell));
                if let Some(ssh) = view.ssh_spec() {
                    rows.push(InfoRow::text(t(L10nKey::PanelSsh), ssh.host.clone()).copyable());
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
                git = view.git_status(cx);
            }
            // Read off the same pane the rows above describe, rather than off
            // `Tab::git_status`, which resolves a split tab to its *first* leaf
            // while `detail_pane` resolves it to the *last focused* one. The
            // two agreed while the row was inert text; now that the counts open
            // a diff, disagreeing means a click that opens a repository other
            // than the one whose numbers were clicked.
            if let Some(git) = git {
                rows.push(InfoRow::text(t(L10nKey::PanelBranch), git.branch.clone()).copyable());
                rows.push(InfoRow {
                    label: t(L10nKey::PanelChangesRow),
                    value: InfoValue::Diff {
                        added: git.added,
                        removed: git.removed,
                        // A clean tree has no diff to open, so the row keeps
                        // its place in the table but stops being a button.
                        open: (git.added > 0 || git.removed > 0)
                            .then_some(diff_target.clone())
                            .flatten(),
                    },
                    copy: None,
                    reveal: None,
                });
            }
            if let Some(agent) = tab.agent(cx) {
                let name = agent.display_name();
                let status = tab.agent_status(cx);
                rows.push(InfoRow {
                    label: t(L10nKey::PanelAgent),
                    value: InfoValue::Agent {
                        text: match status {
                            Some(s) => format!("{name} · {}", agent_status_label(s)),
                            None => name.to_string(),
                        },
                        dot: status.and_then(|s| s.dot_rgb()),
                        hollow: status == Some(crate::core::cli_agent::AgentStatus::Waiting),
                    },
                    copy: None,
                    reveal: None,
                });
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

        let label_w = info_label_column(&rows, window, cx);
        // Rows pad themselves back out to `CONTENT_INSET`, so their hover fill
        // bleeds past the text on both sides — the geometry the Source Control
        // tab's rows are on, one tab over.
        let mut list = v_flex().px(px(CONTENT_INSET - ROW_INSET)).py(px(2.));
        for (i, row) in rows.into_iter().enumerate() {
            list = list.child(self.info_row(i, row, label_w, cx));
        }

        let inner = v_flex()
            .child(self.panel_subtitle(t(L10nKey::PanelSessionSubtitle), false, None, cx))
            .child(list)
            .children(self.procs_section(pane_id, cx))
            .children(self.ports_section(pane_id, local_pane, cx))
            .children(self.forwards_section(forwards_pane, cx))
            .into_any_element();
        self.panel_scroll(inner, title)
    }

    /// One label/value line, with whatever it can do revealed on hover.
    ///
    /// The two cwd buttons used to sit in a strip of their own under the whole
    /// table, unlabelled, four rows below the path they acted on and closer to
    /// the Processes heading than to it — "copy" and "open" with no stated
    /// object. Hanging them off the row they belong to is what makes them
    /// answerable, and it buys the panel the hover feedback it had none of.
    fn info_row(
        &self,
        i: usize,
        row: InfoRow,
        label_w: gpui::Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let mono = cx.theme().mono_font_family.clone();
        let id = gpui::SharedString::from(format!("panel-info-row-{i}"));
        let interactive = row.interactive();
        let tiles_wide = usize::from(row.reveal.is_some()) + usize::from(row.copy.is_some());
        // "Copy" is honest on a branch or a host, but on the working directory
        // it is the file tree's *Copy Path*, and the two live a right-click
        // apart from each other. Say the same words for the same act.
        let copy_label = match row.value {
            InfoValue::Path(_) => t(L10nKey::FileTreeContextCopyPath),
            _ => t(L10nKey::CmdCopy),
        };

        let value = match row.value {
            // A path identifies a pane by its last segment, and plain
            // truncation eats exactly that: a deep checkout read
            // "/private/tmp/claude-501…" and told you nothing. Let the head
            // absorb the shrinking so the leaf survives, the way a file
            // manager shows a path.
            InfoValue::Path(v) => {
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
            InfoValue::Text(v) => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(rems(TEXT_MONO))
                .font_family(mono.clone())
                .text_color(cx.theme().foreground)
                .child(v)
                .into_any_element(),
            InfoValue::Diff {
                added,
                removed,
                open,
            } => {
                let clean = added == 0 && removed == 0;
                // Sized to the two numbers, not to the row: `flex_1` here made
                // the whole rest of the line a button, so a click on the empty
                // half of the row opened the overlay and a pointer crossing it
                // underlined counts it was nowhere near. The slack belongs to
                // the value slot around this, which is what holds it.
                let counts = h_flex()
                    .flex_none()
                    .items_baseline()
                    .gap(px(6.))
                    .text_size(rems(TEXT_MONO))
                    .font_family(mono.clone())
                    // A clean tree said "+0 −0", which is two numbers to read
                    // before learning there was nothing to read. The dash is
                    // the table convention for an empty cell, and it needs no
                    // translating.
                    .when(clean, |this| {
                        this.child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child("—".to_string()),
                        )
                    })
                    .when(added > 0, |this| {
                        this.child(
                            div()
                                .text_color(cx.theme().success)
                                .child(format!("+{added}")),
                        )
                    })
                    .when(removed > 0, |this| {
                        this.child(
                            div()
                                .text_color(cx.theme().danger)
                                .child(format!("−{removed}")),
                        )
                    });
                match open {
                    // The row's hover fill says the line reacts; the underline
                    // says where the button inside it starts — the same pair
                    // the sidebar's counts wear.
                    Some((host, cwd)) => counts
                        .id(("panel-info-diff", i))
                        .cursor_pointer()
                        .hover(|s| s.underline())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.toggle_diff_overlay(host, cwd.clone(), window, cx);
                        }))
                        .into_any_element(),
                    None => counts.into_any_element(),
                }
            }
            // The dot hangs out of the flow rather than sitting in it. A
            // childless box has no baseline of its own, so as a flex item it
            // offers up its bottom edge instead — and the row, which aligns
            // its label and its value on their shared baseline, then hoisted
            // the whole value six pixels and left "agent" sitting under its
            // own value. Out of flow it cannot be mistaken for the thing that
            // sets the line.
            InfoValue::Agent { text, dot, hollow } => div()
                .flex_1()
                .min_w_0()
                .relative()
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .when(dot.is_some(), |d| d.pl(rems(PIP_SIZE + PIP_GAP)))
                        .text_size(rems(TEXT_MONO))
                        .font_family(mono.clone())
                        .text_color(cx.theme().foreground)
                        .child(text),
                )
                .children(dot.map(|rgb| {
                    status_pip(rgb, hollow, crate::ui::theme::workspace_surface_color(cx))
                }))
                .into_any_element(),
        };

        // The strip is opaque and pinned to the row's right edge, so whatever
        // sits under it is unreadable for as long as the pointer is on the row
        // — and on the working-directory row what sits there is the leaf, the
        // one segment the head-first elision exists to keep. Hold that much
        // width back from the value for good rather than only while hovered:
        // taking it on hover would re-elide the path under the pointer, which
        // is the pixel-shifting the strip is absolutely positioned to avoid.
        let value = h_flex()
            .flex_1()
            .min_w_0()
            .items_baseline()
            .when(tiles_wide > 0, |this| {
                this.pr(px(tiles_wide as f32 * (TILE_SIZE_XS + 1.) + 4.))
            })
            .child(value);

        let mut tiles = action_strip(&id, sf.hover);
        let mut has_tiles = false;
        if let Some(cwd) = row.reveal {
            has_tiles = true;
            tiles = tiles.child(
                self.info_tile(
                    "panel-info-reveal",
                    IconName::FolderOpen,
                    reveal_label(),
                    cx,
                )
                .on_click(move |_, _window, cx| cx.reveal_path(&cwd)),
            );
        }
        if let Some(text) = row.copy {
            has_tiles = true;
            tiles = tiles.child(
                self.info_tile(("panel-info-copy", i), IconName::Copy, copy_label, cx)
                    .on_click(move |_, _window, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
                    }),
            );
        }
        // A row with nothing to reveal gets no strip at all, rather than an
        // empty one carrying a hover subscription for a set of buttons that
        // does not exist.
        let actions = has_tiles.then_some(tiles);

        h_flex()
            .id(id.clone())
            .group(id)
            .relative()
            .items_baseline()
            .gap(px(9.))
            .px(px(ROW_INSET))
            .py(px(2.))
            .rounded(px(5.))
            .text_size(rems(TEXT))
            // Only rows that can do something light up, so the fill is never a
            // promise the row cannot keep.
            .when(interactive, |this| {
                this.hover(|s| s.bg(gpui::rgb(sf.hover)))
            })
            .child(
                div()
                    .flex_none()
                    .w(label_w)
                    .whitespace_nowrap()
                    .text_color(cx.theme().muted_foreground)
                    .child(row.label),
            )
            .child(value)
            .children(actions)
            .into_any_element()
    }

    /// The tile an Info row's hover strip is made of — the [`TILE_SIZE_XS`]
    /// box the Source Control rows use, because three `TILE_SIZE_SM` squares
    /// would eat a quarter of the width a path has to live in.
    fn info_tile(
        &self,
        id: impl Into<gpui::ElementId>,
        icon: IconName,
        tooltip: &'static str,
        cx: &mut Context<Self>,
    ) -> Button {
        crate::ui::tab_strip::chrome_tile_sized(
            Button::new(id).icon(Icon::new(icon)),
            TILE_SIZE_XS,
            TILE_GLYPH_XS,
            false,
            cx,
        )
        .rounded(px(4.))
        .tooltip(tooltip)
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
                            // Which of these has the terminal is the one thing
                            // the list is read for, and a hue apart from its
                            // neighbours was carrying it alone — a difference
                            // a light theme flattens and colour vision can
                            // miss. Weight says it a second way.
                            .when(p.foreground, |d| {
                                d.font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(cx.theme().foreground)
                            })
                            .when(!p.foreground, |d| d.text_color(cx.theme().muted_foreground))
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

    /// The listening ports of the pane's processes.
    ///
    /// `local` is whether the daemon that listed these ports is this machine's,
    /// and it is what decides whether the browser tile appears: a port on a
    /// remote host is not this machine's port, and opening it here is not a
    /// near miss, it is a different service. It is deliberately about the
    /// *host* and not about whether the shell has ssh'd somewhere — the ports
    /// come from the pane's own process tree either way, so a `ssh -L` pane's
    /// forwarded listener really is on this machine and really does open.
    fn ports_section(
        &self,
        pane_id: Option<u64>,
        local: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let ports = &self.procs(pane_id)?.ports;
        if ports.is_empty() {
            return None;
        }
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let mono = cx.theme().mono_font_family.clone();
        let mut list = v_flex().px(px(CONTENT_INSET - ROW_INSET)).py(px(1.));
        for (i, p) in ports.iter().enumerate() {
            // "What is this pane serving, and where" is the question the
            // section answers, and the next thing anyone does with the answer
            // is go there — so the row hands over an address instead of making
            // it something to read off the screen and retype.
            let authority = p.authority();
            // Keyed by the row, not by the port: `listening_ports` drops a
            // duplicate only when the port *and* the pid match, so a
            // pre-forking server — nginx, gunicorn, a node cluster — puts one
            // row per worker on screen, all on port 8000. Sharing an id makes
            // gpui hand them one interactive state between them, and a click on
            // the last row lights up the tooltip and the pressed fill on all
            // the others.
            let id = gpui::SharedString::from(format!("panel-port-{}-{}", p.port, p.pid));
            let mut tiles_wide = 1;
            let mut actions = action_strip(&id, sf.hover);
            if local {
                tiles_wide += 1;
                let url = format!("http://{authority}");
                actions = actions.child(
                    self.info_tile(
                        ("panel-port-open", i),
                        IconName::Globe,
                        t(L10nKey::PanelOpenInBrowser),
                        cx,
                    )
                    .on_click(move |_, _window, cx| cx.open_url(&url)),
                );
            }
            actions = actions.child(
                self.info_tile(
                    ("panel-port-copy", i),
                    IconName::Copy,
                    t(L10nKey::CmdCopy),
                    cx,
                )
                .on_click({
                    let authority = authority.clone();
                    move |_, _window, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(authority.clone()));
                    }
                }),
            );
            list = list.child(
                h_flex()
                    .id(id.clone())
                    .group(id)
                    .relative()
                    .items_center()
                    .gap(px(8.))
                    .px(px(ROW_INSET))
                    .py(px(1.))
                    .rounded(px(5.))
                    .hover(|s| s.bg(gpui::rgb(sf.hover)))
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
                            // Room held back for the strip, so the process name
                            // ends where the buttons begin instead of under
                            // them. Same reservation the Info rows make.
                            .pr(px(tiles_wide as f32 * (TILE_SIZE_XS + 1.) + 4.))
                            .text_size(rems(TEXT_MONO))
                            .font_family(mono.clone())
                            .text_color(cx.theme().muted_foreground)
                            .child(p.name.clone()),
                    )
                    .child(actions),
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

/// Diameter of the agent dot in the Info panel, and the gap between it and the
/// word it qualifies.
///
/// Seven sixteenths of a rem — seven pixels at the default interface size,
/// because a dot on a line of text has to survive being read at a glance
/// without becoming a bullet, and a rem rather than a pixel because the line it
/// sits in is sized in rems: pinned in pixels it slid towards the cap height of
/// its own row the moment the interface font scale moved off 100%.
const PIP_SIZE: f32 = 7. * STEP;
const PIP_GAP: f32 = 7. * STEP;

/// How far down the value box the dot starts, again as a fraction of the text
/// it is centred in rather than a pixel count.
const PIP_TOP: f32 = 6. * STEP;

/// The dot a tab wears for its agent's state, at the size a line of panel text
/// can carry it.
///
/// Same colours and the same hollow-for-Waiting rule as the sidebar's, because
/// it is the same fact: a reader who has learned that amber-with-a-hole means
/// "it wants you" on a tab must not have to learn it a second time here. Same
/// *shape*, too — [`Tty7App::status_dot`] punches a small hole out of a filled
/// dot, so drawing this one as a thin ring would have been a second dialect of
/// the one rule the doc above promises is shared. `hole` is the colour behind
/// the dot, which is what a hole in it has to be painted in; the agent row is
/// never interactive, so that colour is the panel's own and does not move
/// under the pointer.
fn status_pip(rgb: u32, hollow: bool, hole: gpui::Hsla) -> AnyElement {
    div()
        .absolute()
        .left_0()
        .top(rems(PIP_TOP))
        .size(rems(PIP_SIZE))
        .rounded_full()
        .bg(gpui::rgb(rgb))
        .when(hollow, |dot| {
            dot.flex()
                .items_center()
                .justify_center()
                .child(div().size(rems(PIP_SIZE * 0.36)).rounded_full().bg(hole))
        })
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
    use super::{InfoRow, InfoValue, split_path_leaf};

    fn diff(added: u32, removed: u32, open: bool) -> InfoRow {
        InfoRow {
            label: "changes",
            value: InfoValue::Diff {
                added,
                removed,
                open: open.then(|| {
                    (
                        crate::ui::host_ops::HostId::LOCAL,
                        std::path::PathBuf::from("/w/repo"),
                    )
                }),
            },
            copy: None,
            reveal: None,
        }
    }

    #[test]
    fn a_row_lights_up_only_when_there_is_something_behind_it() {
        // The hover fill is the panel's only "this line does something", so a
        // row that cannot do anything must not draw one.
        assert!(
            !InfoRow::text("shell", "zsh".into()).interactive(),
            "a plain readout is not a control"
        );
        assert!(
            InfoRow::text("branch", "main".into())
                .copyable()
                .interactive(),
            "a copy tile is something to hover for"
        );
        assert!(
            InfoRow {
                reveal: Some(std::path::PathBuf::from("/w/repo")),
                ..InfoRow::text("cwd", "/w/repo".into())
            }
            .interactive(),
            "so is Reveal, even with nothing else on the row"
        );
    }

    #[test]
    fn counts_are_a_button_only_when_there_is_a_diff_to_open() {
        assert!(
            diff(3, 1, true).interactive(),
            "changes with somewhere to go open the overlay"
        );
        // Both halves have to hold: a clean tree has no diff to show, and the
        // setting that governs the sidebar's counts can take the target away
        // from a dirty one.
        assert!(
            !diff(0, 0, false).interactive(),
            "a clean tree is a readout, not a link"
        );
        assert!(
            !diff(3, 1, false).interactive(),
            "no target means no link, however dirty the tree"
        );
    }

    #[test]
    fn copyable_takes_the_text_the_row_shows_and_nothing_else() {
        // `copyable()` reads the value it was given; rows built with an
        // explicit clipboard string (the cwd, which copies the real path
        // rather than the `~/…` spelling) set `copy` themselves.
        assert_eq!(
            InfoRow::text("ssh", "box".into())
                .copyable()
                .copy
                .as_deref(),
            Some("box")
        );
        assert_eq!(
            diff(3, 1, true).copyable().copy,
            None,
            "there is no sensible clipboard form of two coloured numbers"
        );
    }

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

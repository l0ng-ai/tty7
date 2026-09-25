use gpui::{AnyElement, Context, Window, div, prelude::*, px, rems};
use gpui_component::button::Button;
use gpui_component::input::Input;
use gpui_component::{
    ActiveTheme as _, Icon, IconName, InteractiveElementExt as _, Sizable as _, WindowExt as _,
    h_flex, v_flex,
};
use std::path::PathBuf;

use crate::core::config::{Config, RightPanelTab};
use crate::daemon::protocol::{ManagedForward, PaneProcs, PortProbe};
use crate::ui::app::{
    CONTENT_INSET, TILE_GLYPH_SM, TILE_GLYPH_XS, TILE_SIZE_SM, TILE_SIZE_XS, Tty7App,
    tile_trailing_inset, tile_trailing_inset_sm,
};
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::scrollbar::with_vertical_scrollbar;

/// Wide enough for the three word tabs, bare, beside the two chrome tiles at
/// the default interface size — the Changes count is dropped before a label is
/// ever cut. A larger UI font raises the floor past this; see
/// [`right_panel_tabs_floor`].
pub(crate) const MIN_WIDTH: f32 = 240.;

/// What the tab row needs with the count dropped: the labels, the row's lead,
/// its 2px gaps, and the chrome tiles at the trailing end.
fn right_panel_tabs_floor(window: &Window, cx: &gpui::App) -> f32 {
    let chrome = match cfg!(target_os = "macos") {
        true => crate::ui::tab_strip::trailing_chrome_tiles_w() + TAB_ROW_GAPS,
        false => tile_trailing_inset(),
    };
    (TAB_ROW_LEAD + crate::ui::tab_strip::right_panel_tab_labels_w(window, cx) + chrome).ceil()
}

/// The tab row's leading inset, and the 2px gaps it puts between the three
/// tabs, the spacer and the chrome.
const TAB_ROW_LEAD: f32 = 4.;

/// How tall the tab row is where it sits below the title bar (Windows and
/// Linux): the tab's 26px hover pill plus 2px either side. A full title-bar
/// height here stacked a second 40px band under the first and left the labels
/// sitting well down from the window's top edge.
const TAB_ROW_HEIGHT: f32 = TILE_SIZE_SM + 6.;
const TAB_ROW_GAPS: f32 = 4. * 2.;

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

/// Compact section headings share the sidebar group-label size.
pub(crate) const HEADING: f32 = META;

/// The leading glyph on a panel row — the file tree's folder and file marks.
///
/// Keep row glyphs on the same 16px grid as the toolbar.
pub(crate) const ROW_GLYPH: f32 = crate::ui::app::TILE_GLYPH;

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
pub(crate) const ROW_INSET: f32 = 6.;

/// Whether this forward is the one that reaches `port` on the far side.
///
/// Local forwards only, and only those aimed at the far host's own loopback:
/// a forward to some third machine happens to carry the same number, and
/// pairing it with the port row would claim it leads somewhere it does not.
/// Nor does a switched-off forward: it leads nowhere until it is switched back
/// on, and its own row is where that switch is.
pub(crate) fn forwards_port(m: &ManagedForward, port: u16) -> bool {
    m.enabled
        && m.kind == crate::daemon::protocol::SshForwardKind::Local
        && m.target_port == port
        && crate::daemon::protocol::PortEntry::reaches_loopback(&m.target_host)
}

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
    /// Who to ask about `procs_pane` — `None` means this machine's daemon.
    pub(crate) procs_host: Option<crate::ui::host_ops::SharedHost>,
    /// Whether the host that owns this pane cannot describe its processes at
    /// all: an older `tty7-server` on the far end, which does not know the
    /// request. Kept apart from an empty list, because "nothing is listening"
    /// and "nobody could tell us" are different sentences and the panel has to
    /// say which one it means.
    pub(crate) procs_unsupported: bool,
    /// The last round trip measured to the machine `procs_pane` lives on.
    /// `None` before the first ping comes back.
    pub(crate) link_rtt: Option<std::time::Duration>,
    /// Which host `link_rtt` was measured against, and — since only a remote
    /// pane has one — whether the latency row is drawn at all. Held per host
    /// rather than per pane so that moving between two panes of the same
    /// machine keeps the number on screen: it belongs to the link the two
    /// panes share, and blanking it per pane would empty the row for as long
    /// as the next poll takes to cross the network.
    pub(crate) link_host: Option<crate::ui::host_ops::HostId>,
    /// How `procs_pane`'s loopback ports can be reached from this machine.
    /// Read by the Ports list to decide what a click on a port does, and by
    /// the watch to decide whether it has to keep looking with the panel shut.
    pub(crate) port_route: crate::terminal::view::PortRoute,
    /// The remote ports already forwarded unasked, per set of forwards.
    ///
    /// Kept so that a forward the user then deletes is not immediately rebuilt
    /// by the next poll — the automatic offer is made once per port, and after
    /// that the port is theirs to forward or not.
    ///
    /// Keyed by owner rather than held for the pane in front, because a
    /// workspace's forwards are shared by all of its panes: switching tabs
    /// would otherwise re-announce every port the workspace had already
    /// forwarded, and switching back would re-offer what was just dismissed.
    /// A few `u16` per connection is not worth reclaiming.
    pub(crate) auto_forwarded:
        std::collections::HashMap<crate::ui::app::ForwardOwnerKey, std::collections::HashSet<u16>>,
    pub(crate) scroll: gpui::ScrollHandle,
    pub(crate) tree_scroll: gpui::ScrollHandle,
    /// A path the tree should scroll onto, and how many more renders it may
    /// take to get there. The row is usually not drawn yet when the request is
    /// made — its parents were only just expanded and their listings are still
    /// on their way — so the index has to be recomputed until it appears. The
    /// countdown is what stops a path that never arrives from being compared
    /// against every row forever.
    pub(crate) tree_reveal: Option<(PathBuf, u8)>,
}

/// How many renders a reveal waits for its row to show up. Generous: it costs
/// one path comparison per row, and a cold directory listing over SSH can take
/// a moment.
pub(crate) const TREE_REVEAL_RENDERS: u8 = 60;

/// How often the process and port list is re-read while it is on screen —
/// close enough that a process appearing feels immediate.
const PROCS_POLL: std::time::Duration = std::time::Duration::from_millis(2000);

/// How often it is re-read with the panel shut, where nobody is watching the
/// list and the only question is whether a new port has appeared. A couple of
/// extra seconds nobody can feel, against a query that crosses the network on
/// every remote pane.
const PORT_WATCH_POLL: std::time::Duration = std::time::Duration::from_millis(5000);

/// How many newly-seen ports one poll may forward. A dev server brings up one
/// or two; a number this side of a dozen is a process opening listeners in a
/// loop, and forwarding all of them helps nobody.
const AUTO_FORWARD_BURST: usize = 4;

/// What the Ports and Forwards sections are describing.
#[derive(Clone)]
pub(crate) struct PaneForwardCtx {
    pub(crate) pane_id: u64,
    /// `Some` when the pane has somewhere to hold managed forwards.
    pub(crate) route: Option<crate::ui::app::ForwardRoute>,
    pub(crate) port_route: crate::terminal::view::PortRoute,
    /// The host that owns this pane's processes, when that is not this
    /// machine. A remote workspace's panes live in the peer's registry, so the
    /// local daemon — which is what `query_procs` asks — has never heard of
    /// them and answers with an empty list.
    pub(crate) host: Option<crate::ui::host_ops::SharedHost>,
}

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
}

/// The table convention for a cell with nothing in it. Needs no translating,
/// and is shorter to read than any of the sentences it stands in for.
const EMPTY: &str = "—";

/// A round trip, at the precision the number is worth reading to.
///
/// Whole milliseconds up to a second: tenths of a millisecond on a link that
/// varies by whole ones is noise dressed as measurement. Past a second the
/// millisecond stops mattering and the second is the unit anyone would say it
/// in.
fn format_rtt(rtt: std::time::Duration) -> String {
    let ms = rtt.as_secs_f64() * 1000.;
    if ms < 1. {
        // Loopback and a peer on the same LAN both land here. Rounding to
        // "0 ms" would read as a failed measurement rather than a fast one.
        return "<1 ms".to_string();
    }
    // Rounded before the comparison, so 999.6 ms is not shown as "1000 ms" —
    // a millisecond reading that has run past the unit's own range.
    let rounded = ms.round() as u64;
    if rounded < 1000 {
        return format!("{rounded} ms");
    }
    format!("{:.1} s", rtt.as_secs_f64())
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

    /// What the sidebar has reserved, from this panel's point of view.
    pub(crate) fn sidebar_floor(&self, cx: &gpui::App) -> f32 {
        if self.sidebar_open(cx) {
            crate::ui::tab_sidebar::MIN_SIDEBAR_WIDTH
        } else {
            0.
        }
    }

    /// The narrowest the panel may be drawn: `MIN_WIDTH`, or wider when the
    /// interface font or the language makes the three tab labels and the two
    /// chrome tiles beside them need more than that.
    pub(crate) fn right_panel_min_px(&self, window: &Window, cx: &gpui::App) -> f32 {
        MIN_WIDTH.max(right_panel_tabs_floor(window, cx))
    }

    pub(crate) fn right_panel_max_px(&self, window: &Window, cx: &gpui::App) -> f32 {
        crate::ui::app::side_panel_max(
            window.viewport_size().width.as_f32(),
            self.right_panel_min_px(window, cx),
            self.sidebar_floor(cx) + self.document_floor(cx),
        )
    }

    pub(crate) fn right_panel_px(&self, window: &Window, cx: &gpui::App) -> f32 {
        self.right_panel_width.get().clamp(
            self.right_panel_min_px(window, cx),
            self.right_panel_max_px(window, cx),
        )
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
            self.sftp_panel.panel_was_closed = true;
            return None;
        }
        let width = self.right_panel_px(window, cx);
        let tab = self.right_panel_tab;

        let body = match tab {
            RightPanelTab::Info => self.render_panel_info(window, cx),
            RightPanelTab::Scm => self.render_panel_scm(window, cx),
            RightPanelTab::Files => self.render_panel_files(window, cx),
        };
        let (backing, handle) = self.right_panel_resize(window, cx);

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
                        .h(px(crate::ui::app::TITLE_BAR_HEIGHT));
                    crate::ui::app::window_move_gesture(
                        row,
                        "right-panel-titlebar-drag",
                        window,
                        cx,
                    )
                    .on_double_click(|_, window, _| window.titlebar_double_click())
                    .items_center()
                    .gap(px(2.))
                    .pl(px(TAB_ROW_LEAD))
                    // The tab bar is a strip of labels over a body, so it
                    // closes on a hairline the current tab's bar sits on.
                    .border_b_1()
                    .border_color(cx.theme().sidebar_border)
                    .relative()
                    .children(self.right_panel_tabs(
                        width
                            - TAB_ROW_LEAD
                            - crate::ui::tab_strip::trailing_chrome_tiles_w()
                            - TAB_ROW_GAPS,
                        window,
                        cx,
                    ))
                    .child(div().flex_1())
                    // Navigation controls stay visible on both sidebars.
                    .child(self.window_chrome(window, cx))
                }))
                .children(cfg!(target_os = "macos").then(|| div().flex_none().h(px(10.))))
                .child(body)
                .children(self.sftp_transfers_footer(cx))
                .child(handle)
                .into_any_element(),
        )
    }

    fn right_panel_resize(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> (AnyElement, AnyElement) {
        use gpui::{Bounds, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, canvas};
        use std::cell::Cell as StdCell;
        use std::rc::Rc;

        let container: Rc<StdCell<Option<Bounds<Pixels>>>> = Rc::new(StdCell::new(None));
        // Read while there is still a `cx` to read it from: the drag handler
        // below only ever sees a `Window`, and the cap it clamps against has to
        // be the same one the layout applies or the panel springs back from
        // wherever it was dropped.
        let others_floor = self.sidebar_floor(cx) + self.document_floor(cx);
        let own_floor = self.right_panel_min_px(window, cx);
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
                            let max = crate::ui::app::side_panel_max(
                                window.viewport_size().width.as_f32(),
                                own_floor,
                                others_floor,
                            );
                            width_cell.set(raw.clamp(own_floor, max));
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
        let has_trailing = trailing.is_some();
        let tabs = (!cfg!(target_os = "macos")).then(|| {
            let width = self.right_panel_px(window, cx);
            let trailing_w = match has_trailing {
                true => 2. * TILE_SIZE_SM + 6.,
                false => 0.,
            };
            let avail = width - TAB_ROW_LEAD - tile_trailing_inset() - trailing_w;
            self.right_panel_tabs(avail, window, cx)
        });
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
            .h(px(if tabs.is_some() { TAB_ROW_HEIGHT } else { 32. }))
            .items_center()
            .pl(px(CONTENT_INSET))
            .pr(px(match (&tabs, has_trailing) {
                (Some(_), _) => tile_trailing_inset(),
                (None, true) => tile_trailing_inset_sm(),
                (None, false) => CONTENT_INSET,
            }))
            // Where the tabs live in this row, they are the heading: the
            // current one already names the panel, and the Changes tab carries
            // the count. The row closes on the hairline their bar sits on.
            .when(tabs.is_some(), |row| {
                row.pl(px(TAB_ROW_LEAD))
                    .border_b_1()
                    .border_color(cx.theme().sidebar_border)
            })
            .when(tabs.is_none(), |row| {
                row.child(
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
                                .text_color(cx.theme().muted_foreground)
                                .child(text.to_string()),
                        )
                        .when_some(count, |this, c| {
                            this.child(
                                // A count is a token hanging off the heading, not
                                // part of it: one step down, mono, regular weight.
                                div()
                                    .text_size(rems(META_MONO))
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .text_color(cx.theme().muted_foreground)
                                    .child(c),
                            )
                        }),
                )
            })
            // Leading, as on macOS: the tabs open the row and whatever the
            // panel adds trails at the far end.
            .when_some(tabs, |this, tiles| {
                this.child(
                    h_flex()
                        .flex_shrink_0()
                        // Full height, so the current tile's underline — pinned
                        // to the bottom of its own box — lands on the rule that
                        // closes this row, the way it does on macOS. Without it
                        // the tiles are only as tall as a glyph and the bar
                        // floats a few pixels above the line.
                        .h_full()
                        .items_center()
                        .gap(px(2.))
                        .children(tiles),
                )
            })
            .child(div().flex_1().min_w_0())
            .when_some(trailing, |this, t| this.child(t))
            .into_any_element()
    }

    pub(crate) fn panel_search(
        &self,
        input: &gpui::Entity<gpui_component::input::InputState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.panel_search_with(input, None, cx)
    }

    /// [`Self::panel_search`] with a tile at its far end, inset the way the
    /// branch row's tile is so the two stack in one column.
    pub(crate) fn panel_search_with(
        &self,
        input: &gpui::Entity<gpui_component::input::InputState>,
        trailing: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_trailing = trailing.is_some();
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
            .when(has_trailing, |row| row.pr(px(tile_trailing_inset_sm())))
            .children(trailing)
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
                    .text_color(muted)
                    .child(h.to_string())
            }))
            .into_any_element()
    }

    fn render_panel_info(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let title = self.panel_title(t(L10nKey::PanelInfoTitle), None, None, window, cx);
        let mut rows: Vec<InfoRow> = Vec::new();
        // Which pane the sections below describe, and what can be done with
        // its ports. Worked out once, by the same call the watch uses.
        let ctx = self.pane_forward_ctx(window, cx);
        let mut pane_id: Option<u64> = None;
        // Where the `changes` row's counts lead. Same source as the sidebar's,
        // and gated on the same setting, so turning the preview off turns it
        // off in both places rather than in one of them.
        let mut diff_target: Option<(crate::ui::host_ops::HostId, PathBuf)> = None;
        let mut git: Option<crate::terminal::git_status::GitStatus> = None;

        if let Some(tab) = self.tabs.get(self.active) {
            if let Some(leaf) = tab.detail_pane(window, cx) {
                let view = leaf.read(cx);
                pane_id = Some(view.pane_id);
                diff_target = crate::ui::tab_sidebar::diff_click_cwd(
                    cx.global::<Config>(),
                    view.git_status_cwd()
                        .map(|cwd| (view.host_id(), cwd.to_path_buf())),
                );
                if let Some(cwd) = view.effective_cwd() {
                    let home = view.display_home(cx);
                    // Whether this pane's paths are this machine's decides
                    // both tiles: reveal only means anything on the machine
                    // the file manager can see, and only a local path may be
                    // re-spelled with this OS's separators — a remote one is
                    // already native where it lives.
                    let local = view.local_cwd().is_some();
                    rows.push(InfoRow {
                        label: t(L10nKey::PanelCwd),
                        value: InfoValue::Path(compact_path(&cwd, home.as_deref())),
                        // The compacted `~/…` spelling is for reading; what
                        // goes on the clipboard is the path a shell can use.
                        copy: Some(match local {
                            true => crate::ui::path_display::native_separators(&cwd)
                                .display()
                                .to_string(),
                            false => cwd.display().to_string(),
                        }),
                        reveal: local.then(|| cwd.clone()),
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
                // Only where there is a network between here and the shell. On
                // a pane of this machine's own the row would be reporting the
                // round trip to a Unix socket, which is a number with nothing
                // to compare it against.
                if self.right_panel.link_host.is_some() {
                    rows.push(InfoRow::text(
                        t(L10nKey::PanelLatency),
                        // A link whose first ping has not come back yet,
                        // rather than one measured at zero. The dash is the
                        // table's empty cell, the same one a clean working
                        // tree gets.
                        self.right_panel
                            .link_rtt
                            .map(format_rtt)
                            .unwrap_or_else(|| EMPTY.to_string()),
                    ));
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
            .children(self.ports_section(ctx.as_ref(), cx))
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
                let (head, leaf) = crate::ui::path_display::split_path_leaf(&v);
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .text_size(rems(TEXT))
                    .text_color(cx.theme().sidebar_foreground)
                    .child(
                        div()
                            .min_w_0()
                            .flex_shrink(999.)
                            .truncate()
                            .text_color(cx.theme().muted_foreground)
                            .child(head),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_shrink(1.)
                            .truncate()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(leaf),
                    )
                    .into_any_element()
            }
            InfoValue::Text(v) => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(rems(TEXT))
                .text_color(cx.theme().sidebar_foreground)
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
                                .child(EMPTY.to_string()),
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
                .on_click(move |_, _window, cx| {
                    cx.reveal_path(&crate::ui::path_display::native_separators(&cwd))
                }),
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
            .py(px(4.))
            .rounded(crate::ui::rounding::ROW_RADIUS)
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
                    .text_size(rems(TEXT))
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
                d.mt(px(6.))
                    .border_t_1()
                    .border_color(cx.theme().sidebar_border)
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
                // Weight and ink, not capitals, set a compact heading apart.
                div()
                    .text_size(rems(HEADING))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .child(text.to_string()),
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
                    .child(
                        div()
                            .flex_none()
                            .text_size(rems(META_MONO))
                            .font_family(mono.clone())
                            .text_color(cx.theme().muted_foreground)
                            .child(p.pid.to_string()),
                    ),
            );
        }
        Some(
            v_flex()
                .child(self.panel_subtitle(t(L10nKey::PanelProcessesSubtitle), true, None, cx))
                .child(list)
                .into_any_element(),
        )
    }

    /// The ports this pane is serving, and what it takes to reach them.
    ///
    /// One list, not two. Ports and forwards used to be separate sections, so
    /// a remote :3000 and the forward that reaches :3000 sat under different
    /// headings with nothing saying they were the same thing — and the ports
    /// half offered no way to build the forward the other half was for. A row
    /// is a port here, and the forward, when there is one, is where that row
    /// says it comes out.
    fn ports_section(
        &self,
        ctx: Option<&PaneForwardCtx>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let ctx = ctx?;
        let pane_id = ctx.pane_id;
        // No answer yet for this pane defaults to a probe that is fine, not a
        // broken one: the panel has nothing to doubt until it has been told
        // something.
        let (ports, probe) = self
            .procs(Some(pane_id))
            .map(|p| (p.ports.clone(), p.probe.clone()))
            .unwrap_or_default();
        let forwards: Vec<ManagedForward> = self
            .loopback_panel
            .managed
            .iter()
            .filter(|m| m.pane_id == pane_id)
            .cloned()
            .collect();
        let form_open = self.loopback_panel.form_pane_id == Some(pane_id);
        // A pane that cannot hold a forward and is serving nothing has no
        // section: the heading alone would be an empty promise.
        //
        // Unless the reason it is serving nothing is that nobody managed to
        // look. Then the heading and one muted line under it are the only
        // place the panel can admit it does not know, and a silently absent
        // section is the bug (#731): someone whose server is plainly up reads
        // the missing section as tty7 saying there is no server.
        if ports.is_empty() && forwards.is_empty() && ctx.route.is_none() && probe.is_ok() {
            return None;
        }

        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let mono = cx.theme().mono_font_family.clone();
        let openable = ctx.port_route != crate::terminal::view::PortRoute::Blocked;
        let mut list = v_flex().px(px(CONTENT_INSET - ROW_INSET)).py(px(1.));
        // Which forwards a port row has already accounted for; whatever is
        // left over gets a row of its own below.
        let mut paired: Vec<u64> = Vec::new();

        for (i, p) in ports.iter().enumerate() {
            let forward = forwards.iter().find(|m| forwards_port(m, p.port));
            if let Some(f) = forward {
                paired.push(f.id);
            }
            // What a click and a copy are about: the address that works from
            // here. Once a forward exists that is the local end of it, not the
            // far side's own spelling of the port.
            let here = forward.map(|f| f.bind_port);
            let authority = here
                .map(|local| format!("127.0.0.1:{local}"))
                .unwrap_or_else(|| p.authority());
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
            if openable {
                tiles_wide += 1;
                let port = p.port;
                let direct = p.authority();
                actions = actions.child(
                    self.info_tile(
                        ("panel-port-open", i),
                        IconName::Globe,
                        t(L10nKey::PanelOpenInBrowser),
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.open_pane_port(port, direct.clone(), cx)
                    })),
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
            if let Some(f) = forward {
                tiles_wide += 2;
                let forward_id = f.id;
                // Switched off, the forward stops speaking for this row and
                // comes out as a row of its own with the switch to turn it
                // back on.
                actions = actions.child(
                    crate::ui::tab_strip::chrome_tile_sized(
                        Button::new(("panel-port-switch-off", i))
                            .icon(Icon::empty().path("icons/power.svg")),
                        TILE_SIZE_XS,
                        TILE_GLYPH_XS,
                        false,
                        cx,
                    )
                    .rounded(px(4.))
                    .tooltip(t(L10nKey::ForwardTooltipTurnOff))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_managed_forward_enabled(pane_id, forward_id, false, window, cx)
                    })),
                );
                actions = actions.child(
                    self.info_tile(
                        ("panel-port-unforward", i),
                        IconName::Close,
                        t(L10nKey::ForwardTooltipRemove),
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.remove_managed_forward(pane_id, forward_id, cx)
                    })),
                );
            }
            list = list.child(
                h_flex()
                    .id(id.clone())
                    .group(id)
                    .relative()
                    .items_center()
                    .gap(px(8.))
                    .px(px(ROW_INSET))
                    .py(px(1.))
                    .rounded(crate::ui::rounding::ROW_RADIUS)
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
                    // Where the port comes out on this machine. Just the
                    // number: the host is always this machine's loopback, and
                    // spelling it out on every row would bury the one part
                    // that differs.
                    .children(here.map(|local| {
                        div()
                            .flex_none()
                            .text_size(rems(META_MONO))
                            .font_family(mono.clone())
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("→ :{local}"))
                    }))
                    .child(actions),
            );
        }

        // Forwards no port row spoke for: the remote and dynamic ones, and any
        // local forward pointed somewhere this pane is not itself serving.
        for forward in forwards.iter().filter(|m| !paired.contains(&m.id)) {
            list = list.child(self.forward_row(forward, &mono, cx));
        }

        let add = ctx.route.is_some().then(|| {
            crate::ui::tab_strip::chrome_tile_sized(
                Button::new(("ssh-forward-add-toggle", pane_id))
                    .icon(Icon::empty().path("icons/plus.svg")),
                TILE_SIZE_SM,
                TILE_GLYPH_SM,
                form_open,
                cx,
            )
            .rounded_md()
            .tooltip(if form_open {
                t(L10nKey::Cancel)
            } else {
                t(L10nKey::ForwardTooltipAdd)
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.toggle_managed_forward_form(pane_id, window, cx)
            }))
            .into_any_element()
        });

        Some(
            v_flex()
                .child(self.panel_subtitle(t(L10nKey::PanelPortsSubtitle), true, add, cx))
                .when(
                    ports.is_empty() && forwards.is_empty() && !form_open,
                    |this| {
                        // "Nothing is listening" and "nobody could tell us"
                        // look identical on screen unless the panel says which
                        // one it means — and every one of the second kind is a
                        // fixable thing: a far end running a server too old to
                        // answer, a probe that could not be run at all, a
                        // server started under `sudo` whose sockets this user
                        // is not allowed to see. Still one muted line in the
                        // place the word "None" would have gone; the panel is
                        // reporting what it knows, not raising an alarm.
                        let key = match (self.right_panel.procs_unsupported, &probe) {
                            (true, _) => L10nKey::PanelPortsUnsupported,
                            (false, PortProbe::Unavailable(_)) => L10nKey::PanelPortsProbeFailed,
                            (false, PortProbe::Restricted) => L10nKey::PanelPortsRestricted,
                            (false, PortProbe::Ok) => L10nKey::None,
                        };
                        this.child(
                            div()
                                .px(px(CONTENT_INSET))
                                .py(px(2.))
                                .text_size(rems(TEXT))
                                .text_color(cx.theme().muted_foreground)
                                .child(t(key)),
                        )
                    },
                )
                .child(list)
                .when(form_open, |this| this.child(self.forward_form(pane_id, cx)))
                .into_any_element(),
        )
    }

    /// Open one of this pane's ports in a browser, building the forward it
    /// needs first when it needs one.
    ///
    /// The forward is the part the user should not have to think about: they
    /// asked to see :3000, and where :3000 has to be tunnelled to be seen,
    /// that is this function's problem and not theirs.
    pub(crate) fn open_pane_port(
        &mut self,
        port: u16,
        direct_authority: String,
        cx: &mut Context<Self>,
    ) {
        use crate::terminal::view::PortRoute;
        match self.right_panel.port_route {
            PortRoute::Blocked => {}
            PortRoute::Direct => cx.open_url(&format!("http://{direct_authority}")),
            PortRoute::Forward => {
                if let Some(f) = self
                    .loopback_panel
                    .managed
                    .iter()
                    .find(|m| forwards_port(m, port))
                {
                    cx.open_url(&format!("http://127.0.0.1:{}", f.bind_port));
                    return;
                }
                let Some(route) = self.right_panel.procs_forwards.clone() else {
                    return;
                };
                let owner = route.owner_key();
                cx.spawn(async move |this, cx| {
                    let built = cx
                        .background_executor()
                        .spawn(async move { route.ensure_loopback("127.0.0.1", port) })
                        .await;
                    let _ = this.update_in(cx, |app, window, cx| match built {
                        Ok(f) => {
                            // Claimed, so the watch does not offer this port a
                            // second time after the user has just opened it.
                            app.right_panel
                                .auto_forwarded
                                .entry(owner)
                                .or_default()
                                .insert(port);
                            cx.open_url(&format!("http://127.0.0.1:{}", f.local_port));
                            if let Some(pane_id) = app.right_panel.procs_pane {
                                app.refresh_managed_forwards(pane_id, cx);
                            }
                        }
                        Err(e) => window.push_notification(
                            t_fmt(
                                L10nKey::LoopbackForwardFailed,
                                &[("port", &port.to_string()), ("error", &e.to_string())],
                            ),
                            cx,
                        ),
                    });
                })
                .detach();
            }
        }
    }

    fn procs(&self, pane_id: Option<u64>) -> Option<&PaneProcs> {
        (pane_id.is_some() && self.right_panel.procs_pane == pane_id)
            .then_some(self.right_panel.procs.as_ref())?
    }

    /// What the Ports and Forwards sections are describing: which pane, how
    /// forward requests about it reach a daemon, and whether the loopback
    /// ports it is serving can be opened from this machine.
    ///
    /// One answer for both readers. The panel draws from it and the watch polls
    /// from it, and when they were each working it out for themselves the panel
    /// could offer to open a port the watch had already given up on.
    pub(crate) fn pane_forward_ctx(
        &self,
        window: &Window,
        cx: &gpui::App,
    ) -> Option<PaneForwardCtx> {
        let leaf = self.tabs.get(self.active)?.detail_pane(window, cx)?;
        let view = leaf.read(cx);
        // A pane holds managed forwards once it has somewhere to hold them:
        // a live native-ssh connection, or the workspace's shared one.
        let connected_ssh = view
            .remote_context()
            .is_some_and(|c| c.kind == crate::daemon::protocol::RemoteKind::NativeSsh)
            && matches!(
                view.ssh_phase(),
                Some(crate::daemon::protocol::SshPhase::Connected)
            );
        Some(PaneForwardCtx {
            pane_id: view.pane_id,
            route: (connected_ssh || view.workspace().is_some()).then(|| view.forward_route()),
            port_route: view.port_route(cx),
            host: (!view.host_id().is_local())
                .then(|| view.host(cx))
                .flatten(),
        })
    }

    /// Point the port watch at whatever pane is in front, once a frame.
    ///
    /// Driven from the app's own render rather than the panel's: the watch has
    /// to run with the panel shut, which is exactly when a new port appearing
    /// is worth saying something about.
    pub(crate) fn sync_port_watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ctx = self.pane_forward_ctx(window, cx);
        self.right_panel.port_route = ctx
            .as_ref()
            .map_or(crate::terminal::view::PortRoute::Blocked, |c| c.port_route);
        let pane_id = ctx.as_ref().map(|c| c.pane_id);
        let host = ctx.as_ref().and_then(|c| c.host.clone());
        let route = ctx.and_then(|c| c.route);
        self.sync_procs(pane_id, route, host, cx);
    }

    /// Whether the process/port poll should run at all this round.
    fn procs_wanted(&self) -> bool {
        (self.right_panel_visible && self.right_panel_tab == RightPanelTab::Info)
            || self.watching_ports()
    }

    /// Whether the poll has to keep going with the panel closed: on a pane
    /// whose ports need forwarding, noticing a new listener *is* the feature,
    /// and a shut panel is not a reason to stop looking.
    fn watching_ports(&self) -> bool {
        self.right_panel.port_route == crate::terminal::view::PortRoute::Forward
            && self.right_panel.procs_forwards.is_some()
    }

    /// Forward the loopback ports this poll saw for the first time, and say so.
    ///
    /// Once per port, not once per poll: a forward the user then deletes stays
    /// deleted. Only ports bound somewhere this machine could reach through the
    /// tunnel — a listener pinned to one of the far host's own interfaces is a
    /// different service, and guessing at it would build a forward to nothing.
    fn auto_forward_ports(&mut self, cx: &mut Context<Self>) {
        if !self.watching_ports() {
            return;
        }
        let Some(route) = self.right_panel.procs_forwards.clone() else {
            return;
        };
        // Read out before the ledger is touched: the ports are behind the same
        // borrow the `seen` entry needs.
        let listening: Vec<u16> = match self.right_panel.procs.as_ref() {
            Some(procs) => procs
                .ports
                .iter()
                .filter(|p| crate::daemon::protocol::PortEntry::reaches_loopback(&p.addr))
                .map(|p| p.port)
                .collect(),
            None => return,
        };
        let owner = route.owner_key();
        let seen = self.right_panel.auto_forwarded.entry(owner).or_default();
        let mut fresh: Vec<u16> = Vec::new();
        for port in listening {
            if fresh.len() >= AUTO_FORWARD_BURST {
                break;
            }
            // Two rows may name one port — a pre-forking server puts one per
            // worker on screen — and they want one forward between them.
            if seen.contains(&port) || fresh.contains(&port) {
                continue;
            }
            fresh.push(port);
        }
        if fresh.is_empty() {
            return;
        }
        // Claimed before the request goes out, so the next poll — two seconds
        // away, and this round trip crosses a network — does not ask again.
        seen.extend(fresh.iter().copied());
        cx.spawn(async move |this, cx| {
            let built = cx
                .background_executor()
                .spawn(async move {
                    fresh
                        .into_iter()
                        .map(|port| {
                            let local = route
                                .ensure_loopback("127.0.0.1", port)
                                .map(|f| f.local_port);
                            (port, local)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update_in(cx, |app, window, cx| {
                for (port, local) in built {
                    match local {
                        Ok(local) => window.push_notification(
                            t_fmt(
                                L10nKey::PortAutoForwarded,
                                &[("port", &port.to_string()), ("local", &local.to_string())],
                            ),
                            cx,
                        ),
                        Err(e) => {
                            // Usually a connection that is not up yet. Let the
                            // next poll try again rather than writing the port
                            // off for the life of the pane.
                            log::debug!("could not auto-forward :{port}: {e}");
                            if let Some(seen) = app.right_panel.auto_forwarded.get_mut(&owner) {
                                seen.remove(&port);
                            }
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn sync_procs(
        &mut self,
        pane_id: Option<u64>,
        forwards: Option<crate::ui::app::ForwardRoute>,
        host: Option<crate::ui::host_ops::SharedHost>,
        cx: &mut Context<Self>,
    ) {
        let Some(pane_id) = pane_id else { return };
        self.right_panel.procs_forwards = forwards.clone();
        self.right_panel.procs_host = host.clone();
        // Per host, not per pane — see `link_host`. A pane of this machine's
        // own has no host at all, which is what clears the section rather than
        // leaving the last remote pane's numbers under a local one.
        let link_host = host.as_ref().map(|h| h.id());
        if self.right_panel.link_host != link_host {
            self.right_panel.link_host = link_host;
            self.right_panel.link_rtt = None;
        }
        if self.right_panel.procs_pane != Some(pane_id) {
            self.right_panel.procs_pane = Some(pane_id);
            self.right_panel.procs = None;
            self.loopback_panel.managed.clear();
            self.right_panel.procs_gen += 1;
            self.right_panel.procs_loading = false;
            self.right_panel.procs_unsupported = false;
        }
        // Asked before the first query as well as before every later one. This
        // used to be reached only from the Info panel's own render, where the
        // panel being open was implied; driven from the app's render it is not,
        // and starting a round trip per frame for a pane nobody is watching is
        // both wasted IPC and, under a test executor, a queue that never
        // empties.
        if !self.right_panel.procs_loading && self.procs_wanted() {
            self.right_panel.procs_loading = true;
            let generation = self.right_panel.procs_gen;
            self.spawn_procs_query(pane_id, generation, forwards, host, cx);
        }
    }

    fn spawn_procs_query(
        &mut self,
        pane_id: u64,
        generation: u64,
        forwards: Option<crate::ui::app::ForwardRoute>,
        host: Option<crate::ui::host_ops::SharedHost>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let route = forwards.clone();
            // Only while someone is looking. This poll also runs with the panel
            // shut, watching for ports to forward, and a round trip per round
            // for a row nobody can see is the far end's time spent on nothing.
            let want_link = this
                .read_with(cx, |app, _| {
                    app.right_panel_visible && app.right_panel_tab == RightPanelTab::Info
                })
                .unwrap_or(false);
            let (procs, managed, link) = cx
                .background_executor()
                .spawn(async move {
                    // A remote workspace's pane runs on the peer, so the peer
                    // is the only one that can walk its process tree; the local
                    // daemon does not have the pane at all and would answer
                    // with an empty list. `None` back from the host means it
                    // could not be asked, which is not the same as an empty
                    // answer — see `Host::pane_procs`.
                    let procs = match &host {
                        Some(host) => {
                            use crate::ui::host_ops::Host as _;
                            host.pane_procs(pane_id)
                        }
                        None => Some(crate::terminal::RemoteTerminal::query_procs(pane_id)),
                    };
                    let managed = route.map(|r| r.list()).unwrap_or_default();
                    let link = match (want_link, &host) {
                        (true, Some(host)) => host.link_rtt(),
                        _ => None,
                    };
                    (procs, managed, link)
                })
                .await;
            let keep_polling = this
                .update(cx, |app, cx| {
                    if app.right_panel.procs_gen != generation {
                        return false;
                    }
                    app.right_panel.procs_unsupported = procs.is_none();
                    // A host that could not answer leaves the last list it did
                    // answer with in place: blanking it on one failed poll
                    // would make the panel flicker on a link that hiccups.
                    if let Some(procs) = procs {
                        app.right_panel.procs = Some(procs);
                    }
                    if forwards.is_some() {
                        app.loopback_panel.managed = managed;
                    }
                    // Only when this round actually asked. A round that did not
                    // leaves the last answer in place, so reopening the panel
                    // shows the number it was closed on rather than a dash
                    // until the next poll lands.
                    if want_link {
                        app.right_panel.link_rtt = link;
                    }
                    cx.notify();
                    let wanted = app.procs_wanted();
                    if !wanted {
                        app.right_panel.procs_loading = false;
                    }
                    wanted
                })
                .unwrap_or(false);
            // After the list has landed, so the ports it forwards are the ones
            // this poll actually saw.
            let _ = this.update(cx, |app, cx| app.auto_forward_ports(cx));
            if !keep_polling {
                return;
            }
            let gap = this
                .read_with(cx, |app, _| {
                    match app.right_panel_visible && app.right_panel_tab == RightPanelTab::Info {
                        true => PROCS_POLL,
                        false => PORT_WATCH_POLL,
                    }
                })
                .unwrap_or(PORT_WATCH_POLL);
            cx.background_executor().timer(gap).await;
            let _ = this.update(cx, |app, cx| {
                if app.right_panel.procs_gen != generation {
                    return;
                }
                if app.procs_wanted() {
                    let forwards = app.right_panel.procs_forwards.clone();
                    let host = app.right_panel.procs_host.clone();
                    app.spawn_procs_query(pane_id, generation, forwards, host, cx);
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

/// `home` is the home directory of the machine `path` lives on. A remote
/// pane's cwd is measured against *its* host's home, never this machine's
/// (#580) — and against nothing at all while the host has not said.
fn compact_path(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    crate::ui::path_display::abbreviate_home(&path.to_string_lossy(), home).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{InfoRow, InfoValue, format_rtt, forwards_port};
    use crate::daemon::protocol::{ForwardStatus, ManagedForward, SshForwardKind};

    fn forward(kind: SshForwardKind, target_host: &str, target_port: u16) -> ManagedForward {
        ManagedForward {
            id: 1,
            pane_id: 7,
            kind,
            bind_host: "127.0.0.1".to_string(),
            bind_port: 51000,
            target_host: target_host.to_string(),
            target_port,
            description: None,
            status: ForwardStatus::Listening,
            enabled: true,
        }
    }

    /// A port row and the forward that reaches it are one line, so this is
    /// what decides whether a forward is *that* row's or a line of its own.
    #[test]
    fn a_port_row_claims_only_the_forward_that_reaches_it() {
        assert!(forwards_port(
            &forward(SshForwardKind::Local, "localhost", 3000),
            3000
        ));
        assert!(
            forwards_port(&forward(SshForwardKind::Local, "127.0.0.1", 3000), 3000),
            "the far side's loopback spells itself several ways"
        );
        assert!(
            !forwards_port(&forward(SshForwardKind::Local, "localhost", 3000), 8080),
            "a different port is a different row"
        );
        assert!(
            !forwards_port(&forward(SshForwardKind::Local, "10.0.0.5", 3000), 3000),
            "same number, another machine — pairing them would claim it leads \
             somewhere it does not"
        );
        assert!(
            !forwards_port(&forward(SshForwardKind::Remote, "localhost", 3000), 3000),
            "a remote forward listens on the far side, so it is not how this \
             port is reached from here"
        );
    }

    /// A switched-off forward reaches nothing, so it must not take over the
    /// port row — its own row is the only place its switch is.
    #[test]
    fn a_switched_off_forward_is_not_the_way_to_a_port() {
        let mut off = forward(SshForwardKind::Local, "localhost", 3000);
        off.enabled = false;
        assert!(!forwards_port(&off, 3000));
    }

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
    fn a_round_trip_is_read_at_the_precision_it_is_worth() {
        use std::time::Duration;
        // A peer on the same machine or the same LAN. "0 ms" would read as a
        // measurement that failed rather than one that was fast.
        assert_eq!(format_rtt(Duration::from_micros(120)), "<1 ms");
        assert_eq!(format_rtt(Duration::from_micros(999)), "<1 ms");
        assert_eq!(format_rtt(Duration::from_millis(1)), "1 ms");
        assert_eq!(format_rtt(Duration::from_micros(23_400)), "23 ms");
        assert_eq!(format_rtt(Duration::from_millis(999)), "999 ms");
        // Rounding up out of the millisecond's own range hands the number to
        // the unit above rather than printing a four-digit millisecond.
        assert_eq!(format_rtt(Duration::from_micros(999_600)), "1.0 s");
        // Past a second the millisecond has stopped carrying information, and
        // the second is the unit anyone would say the number in.
        assert_eq!(format_rtt(Duration::from_millis(1_450)), "1.4 s");
        assert_eq!(format_rtt(Duration::from_secs(4)), "4.0 s");
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
}

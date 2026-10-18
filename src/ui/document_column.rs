//! The slot the code panel and the diff overlay are drawn in.
//!
//! Both surfaces used to be full-workspace overlays — `absolute`, `inset_0`,
//! `occlude` — so opening a file to read it hid the agent that told you to read
//! it, and reviewing one turned into a toggle loop. They now dock as a column
//! beside the terminal instead, on the same pattern the right panel has always
//! used: a flex sibling with a drag handle and a persisted share of the width.
//!
//! A sibling column, rather than a narrower overlay, is the whole point: the
//! terminal element's laid-out bounds are what drive `set_grid_size`, so a
//! column takes width *away* from the grid and the PTY reflows to what is left.
//! An overlay painted over half the workspace would leave the grid full width
//! with half of it under a card.
//!
//! The overlay is not gone — [`DocumentLayout::Fill`] is exactly the old paint,
//! one command away, and a window too narrow to seat both a terminal and a
//! document falls back to it for that frame without touching what the user
//! saved.

use gpui::{AnyElement, Context, FontWeight, Window, div, prelude::*, px, rems};
use gpui_component::{ActiveTheme as _, InteractiveElementExt as _, h_flex, v_flex};
use std::cell::Cell as StdCell;
use std::rc::Rc;

use crate::core::config::{
    Config, DOCUMENT_RATIO_MAX, DOCUMENT_RATIO_MIN, DOCUMENT_RATIO_STOPS, DocumentLayout,
};
use crate::ui::app::{DOCUMENT_MIN_W, OverlayTop, TERMINAL_MIN_W, Tty7App, document_column_px};
use crate::ui::i18n::{L10nKey, t};
use crate::ui::right_panel::RESIZE_HANDLE_WIDTH;

/// Which wrapper a document surface is being asked to paint itself in.
///
/// The *content* of the code panel and of the diff overlay is the same in all
/// three; only the box around it changes, and with it where the header sits and
/// what its gestures mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DocumentChrome {
    /// The historical full-workspace overlay.
    Fill,
    /// A column beside the terminal, carrying its own header as its first row.
    /// macOS only: there the title bar lives inside the terminal column, so the
    /// document column runs to the top of the window and its header lands in
    /// the title strip by itself.
    Dock,
    /// A column beside the terminal whose header has been lifted into the
    /// spanning title bar above it.
    ///
    /// Windows and Linux put the window controls at the right end of a title
    /// bar that spans the workspace, which leaves the strip directly above the
    /// document column empty — a title bar's height of nothing, with the file
    /// name one row below it. The header goes there instead, and the column
    /// renders its body alone.
    DockHoisted,
}

impl DocumentChrome {
    /// Whether this is one of the two column layouts.
    pub(crate) fn is_dock(self) -> bool {
        !matches!(self, DocumentChrome::Fill)
    }

    /// Whether the surface draws its own header, or has had it lifted away.
    pub(crate) fn renders_own_header(self) -> bool {
        !matches!(self, DocumentChrome::DockHoisted)
    }

    /// Whether the header is the strip along the top of the window, and so has
    /// to behave like a title bar — dragging the window, zooming on a
    /// double-click.
    ///
    /// True for the overlay, whose header stands in for the title bar. True for
    /// a hoisted header, which is drawn *into* the title bar. True for a docked
    /// column only on macOS, where the column reaches the top of the window
    /// anyway. False for a plain docked header, which sits inside the workspace
    /// and would be a second, fake title bar if it moved the window.
    pub(crate) fn header_is_title_strip(self) -> bool {
        match self {
            DocumentChrome::Fill | DocumentChrome::DockHoisted => true,
            DocumentChrome::Dock => cfg!(target_os = "macos"),
        }
    }
}

/// Document header cells share the editor file strip's dimensions, so the
/// terminal button looks the same beside file tabs and a diff's subject.
/// Height of a header cell — a file, the `+N` button, the terminal pill.
pub(crate) const HEADER_CELL_H: f32 = 28.;
/// Their corner.
pub(crate) const HEADER_CELL_RADIUS: f32 = 7.;
/// The widest a name may make its cell before it is elided.
pub(crate) const HEADER_CELL_MAX_W: f32 = 168.;
/// Header text: a half step under the body size, like the rail's rows.
pub(crate) const HEADER_CELL_TEXT: f32 = 12.5 / 16.;

/// The two-cell icon switch a document header carries.
///
/// One builder for the two switches the headers hold — Dock / Fill for the box a
/// document sits in, and Split / Unified for how a diff lays out — because they
/// stand side by side in the same row, and a cell size, track fill or raised
/// shadow that differs between them reads as one of the two being drawn wrong.
const SWITCH_CELL_W: f32 = 24.;
const SWITCH_CELL_H: f32 = 22.;
/// The window every glyph is drawn inside. 14 × 11 is the frame the Dock / Fill
/// pair has always used, so a new glyph joins that family rather than starting a
/// second one beside it.
const SWITCH_FRAME_W: f32 = 14.;
const SWITCH_FRAME_H: f32 = 11.;
const SWITCH_EDGE: f32 = 2.;
const SWITCH_GAP: f32 = 2.;
const SWITCH_LEAD: f32 = 8.;
const SWITCH_CELL_RADIUS: f32 = 5.;
const SWITCH_TRACK_RADIUS: f32 = 7.;

/// Which cell a document's layout switch raises: the box the surface is
/// actually in, which a window too narrow to seat a column decides rather than
/// the config does. A switch that raised what was *asked* would have the raised
/// cell and the screen disagree, and the cell it pointed at would be the one
/// clicking cannot reach.
pub(crate) fn layout_switch_cell(chrome: DocumentChrome) -> DocumentLayout {
    match chrome.is_dock() {
        true => DocumentLayout::Dock,
        false => DocumentLayout::Fill,
    }
}

/// The ink a cell's glyph is drawn in: the live one in body ink, the rest one
/// rung down, so the raised cell is the only dark mark on the track.
pub(crate) fn switch_ink(live: bool, cx: &gpui::App) -> gpui::Hsla {
    let theme = cx.theme();
    match live {
        true => theme.foreground,
        false => theme.muted_foreground,
    }
}

/// The frame of a switch glyph, empty.
fn switch_frame(ink: gpui::Hsla) -> gpui::Div {
    div()
        .relative()
        .w(px(SWITCH_FRAME_W))
        .h(px(SWITCH_FRAME_H))
        .border_1()
        .border_color(ink)
        .rounded(px(2.))
}

/// Dock: the window with the column the document takes beside the terminal.
pub(crate) fn glyph_dock(ink: gpui::Hsla) -> gpui::AnyElement {
    switch_frame(ink)
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(4.))
                .w(px(1.))
                .bg(ink),
        )
        .into_any_element()
}

/// Fill: the window, filled — the document is the whole workspace.
pub(crate) fn glyph_fill(ink: gpui::Hsla) -> gpui::AnyElement {
    switch_frame(ink)
        .child(
            div()
                .absolute()
                .top(px(1.5))
                .bottom(px(1.5))
                .left(px(1.5))
                .right(px(1.5))
                .rounded(px(0.8))
                .bg(ink),
        )
        .into_any_element()
}

/// Split / Unified: the patch's two sides as two frames, side by side or
/// stacked.
///
/// Two separate frames rather than one frame with a stroke through it: Dock is
/// already a frame with a vertical stroke, and the two switches stand next to
/// each other in the diff header, so a divided frame read as Dock drawn twice.
fn glyph_two_panes(ink: gpui::Hsla, side_by_side: bool) -> gpui::AnyElement {
    const GAP: f32 = 2.;
    let (w, h) = match side_by_side {
        true => ((SWITCH_FRAME_W - GAP) / 2., SWITCH_FRAME_H),
        false => (SWITCH_FRAME_W, (SWITCH_FRAME_H - GAP) / 2.),
    };
    let pane = || {
        div()
            .w(px(w))
            .h(px(h))
            .border_1()
            .border_color(ink)
            .rounded(px(1.5))
    };
    div()
        .flex()
        .when(!side_by_side, |d| d.flex_col())
        .w(px(SWITCH_FRAME_W))
        .h(px(SWITCH_FRAME_H))
        .gap(px(GAP))
        .child(pane())
        .child(pane())
        .into_any_element()
}

/// Split: the two sides next to each other.
pub(crate) fn glyph_split(ink: gpui::Hsla) -> gpui::AnyElement {
    glyph_two_panes(ink, true)
}

/// Unified: the two sides stacked, one above the other.
pub(crate) fn glyph_unified(ink: gpui::Hsla) -> gpui::AnyElement {
    glyph_two_panes(ink, false)
}

/// The sunken track the two cells sit on.
pub(crate) fn switch_track(
    id: &'static str,
    cells: Vec<gpui::AnyElement>,
    cx: &gpui::App,
) -> gpui::AnyElement {
    let sf = cx.global::<crate::ui::presets::Surfaces>().window;
    h_flex()
        .id(id)
        .occlude()
        .flex_none()
        .ml(px(SWITCH_LEAD))
        .p(px(SWITCH_EDGE))
        .gap(px(SWITCH_GAP))
        .rounded(px(SWITCH_TRACK_RADIUS))
        .bg(gpui::rgb(sf.hover))
        .children(cells)
        .into_any_element()
}

/// One cell of a [`switch_track`]: its glyph, raised when it is what is on
/// screen, and the tooltip and a11y label that name it — a divider down a frame
/// cannot say "side by side" on its own.
pub(crate) fn switch_cell<H>(
    id: impl Into<gpui::ElementId>,
    glyph: gpui::AnyElement,
    live: bool,
    tip: L10nKey,
    on_pick: H,
    cx: &gpui::App,
) -> gpui::AnyElement
where
    H: Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
{
    let theme = cx.theme();
    let sf = cx.global::<crate::ui::presets::Surfaces>().window;
    let dark = theme.mode.is_dark();
    div()
        .id(id)
        // The glyph has no text for assistive technology to read. Use the
        // tooltip's localized name and expose the raised cell as a pressed
        // button; GPUI also routes accessible clicks to the on_click handler.
        .role(gpui::Role::Button)
        .aria_label(t(tip))
        .aria_toggled(if live {
            gpui::Toggled::True
        } else {
            gpui::Toggled::False
        })
        .w(px(SWITCH_CELL_W))
        .h(px(SWITCH_CELL_H))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(SWITCH_CELL_RADIUS))
        .cursor_pointer()
        .when(live, |cell| {
            cell.bg(match dark {
                true => gpui::rgb(sf.selected).into(),
                false => theme.background,
            })
            .shadow_xs()
        })
        .child(glyph)
        .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(t(tip)).build(window, cx))
        .on_click(on_pick)
        .into_any_element()
}

impl Tty7App {
    /// A filled document's lead: the terminal it covers, by name, and a way
    /// back that keeps the file or diff open beside it. Both surfaces use this
    /// button so returning to the terminal never depends on which document is
    /// in front. A docked header already has the terminal beside it.
    pub(crate) fn render_document_terminal_button(
        &self,
        chrome: DocumentChrome,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if chrome != DocumentChrome::Fill {
            return None;
        }
        let theme = cx.theme();
        let name = self
            .tabs
            .get(self.active)
            .map(|tab| self.tab_label(tab, self.active, Some(window), cx))
            .unwrap_or_default();
        let badge_bg = gpui::rgb(cx.global::<crate::ui::presets::Surfaces>().window.selected);
        Some(
            h_flex()
                .flex_none()
                .items_center()
                .child(
                    h_flex()
                        .id("document-back-to-terminal")
                        .when(cfg!(test), |button| {
                            button.debug_selector(|| "document-back-to-terminal".into())
                        })
                        .role(gpui::Role::Button)
                        .aria_label(format!("{}: {name}", t(L10nKey::DocumentDock)))
                        .occlude()
                        .h(px(HEADER_CELL_H))
                        .pl(px(8.))
                        .pr(px(10.))
                        .gap(px(7.))
                        .items_center()
                        .rounded(px(HEADER_CELL_RADIUS))
                        .text_size(rems(HEADER_CELL_TEXT))
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
                                .max_w(px(HEADER_CELL_MAX_W))
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
                .into_any_element(),
        )
    }

    /// The layout the active tab's document was asked for, which is not always
    /// the one it gets — see [`Tty7App::document_dock_px`].
    ///
    /// A tab that has never been told follows the config, which holds the last
    /// explicit choice anyone made and is therefore what a fresh tab starts
    /// from. Telling one tab never moves another.
    pub(crate) fn document_layout(&self, cx: &gpui::App) -> DocumentLayout {
        self.tabs
            .get(self.active)
            .and_then(|t| t.document_layout)
            .unwrap_or_else(|| cx.global::<Config>().document_layout)
    }

    /// Which of the two surfaces the docked column shows.
    ///
    /// `overlay_top` orders a *pair* of overlays in fill mode, where both are
    /// painted and the front one wins on paint order. A column has one child,
    /// so the ordering has to become a choice: the surface on top, unless it is
    /// closed, in which case there is no front and the survivor is it.
    pub(crate) fn document_front(&self) -> Option<OverlayTop> {
        let tab = self.tabs.get(self.active)?;
        let code = tab.code.as_ref().is_some_and(|c| c.visible);
        let diff = tab.diff_overlay.is_some();
        match (tab.overlay_top, code, diff) {
            (_, false, false) => None,
            (OverlayTop::Code, true, _) | (OverlayTop::Diff, true, false) => Some(OverlayTop::Code),
            (OverlayTop::Diff, _, true) | (OverlayTop::Code, false, true) => Some(OverlayTop::Diff),
        }
    }

    /// What the document column has reserved, from a side panel's point of
    /// view.
    ///
    /// Read from the user's intent and from whether a surface is open — never
    /// from the *effective* layout, which is derived from the widths this feeds
    /// and would close the loop on itself.
    pub(crate) fn document_floor(&self, cx: &gpui::App) -> f32 {
        if self.document_layout(cx) == DocumentLayout::Dock && self.document_front().is_some() {
            DOCUMENT_MIN_W
        } else {
            0.
        }
    }

    /// The width the terminal and a docked document share: the window less
    /// whichever side panels are open, at the widths they are actually drawn
    /// at rather than at their floors. A sidebar someone dragged wider is width
    /// the terminal no longer has.
    pub(crate) fn document_body_px(&self, window: &Window, cx: &gpui::App) -> f32 {
        let viewport = window.viewport_size().width.as_f32();
        let sidebar = if self.sidebar_open(cx) {
            self.sidebar_px(window, cx)
        } else {
            0.
        };
        let panel = if self.right_panel_open(cx) {
            self.right_panel_px(window, cx)
        } else {
            0.
        };
        viewport - sidebar - panel
    }

    /// How wide the document column is drawn this frame, or `None` when the
    /// surface is closed, the user chose fill, or the window is too narrow to
    /// seat both.
    pub(crate) fn document_dock_px(&self, window: &Window, cx: &gpui::App) -> Option<f32> {
        if self.document_layout(cx) != DocumentLayout::Dock || self.document_front().is_none() {
            return None;
        }
        document_column_px(self.document_body_px(window, cx), self.document_ratio.get())
    }

    /// Fill ↔ dock, for the active tab. One of the two writers of the layout;
    /// the narrow window fallback is not, on purpose — running this while the
    /// fallback is showing is the user saying they meant the overlay, and that
    /// is worth keeping.
    pub(crate) fn toggle_document_fill(&mut self, cx: &mut Context<Self>) {
        let next = match self.document_layout(cx) {
            DocumentLayout::Dock => DocumentLayout::Fill,
            DocumentLayout::Fill => DocumentLayout::Dock,
        };
        self.set_document_layout(next, cx);
    }

    /// Snap the column to a named share of the terminal column. Docks first if
    /// the surface is filling the window: asking for a third of the width is
    /// asking for a column.
    pub(crate) fn set_document_ratio(&mut self, ratio: f32, cx: &mut Context<Self>) {
        self.document_ratio.set(ratio);
        self.update_config(cx, |cfg| cfg.document_ratio = ratio);
        self.set_document_layout(DocumentLayout::Dock, cx);
    }

    /// Third → half → two thirds → third, the double-click on the divider.
    /// Starts from whichever named width the current one is nearest, so a
    /// dragged column joins the cycle where it looks like it is.
    pub(crate) fn cycle_document_ratio(&mut self, cx: &mut Context<Self>) {
        let current = self.document_ratio.get();
        let nearest = next_ratio_stop(current);
        self.set_document_ratio(nearest, cx);
    }

    /// Point the active tab at a layout, and only that tab.
    ///
    /// Deliberately not written back to `document_layout` in the config: that
    /// key is the value a tab starts from, and a tab that has not been told is
    /// still reading it. Writing it here would reach every one of those at
    /// once, which is the window-wide switch this is not.
    ///
    /// The narrow-window fallback does not come through here either — it is
    /// derived at render time and stored nowhere.
    pub(crate) fn set_document_layout(&mut self, layout: DocumentLayout, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.document_layout = Some(layout);
        }
        cx.notify();
    }

    /// Dock / Fill, as a two-cell icon switch: a raised cell on a sunken track.
    ///
    /// On the header rather than in Settings because this is where the question
    /// comes up — the moment a file covers the terminal is the moment you want it
    /// not to, and a preference three pages into a settings panel is not an answer
    /// to that.
    ///
    /// It used to be a right-click menu, which only answered for someone who
    /// already knew the menu was there: the code panel's switch was the one anyone
    /// found, so the diff panel — which had the menu and nothing else — looked to
    /// have no say in the matter at all. Both headers now carry this one.
    pub(crate) fn document_layout_switch(
        &self,
        chrome: DocumentChrome,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = layout_switch_cell(chrome);
        let cell = |i: usize,
                    layout: DocumentLayout,
                    glyph: fn(gpui::Hsla) -> AnyElement,
                    tip: L10nKey| {
            let live = layout == current;
            let ink = switch_ink(live, cx);
            switch_cell(
                ("document-layout", i),
                glyph(ink),
                live,
                tip,
                cx.listener(move |this, _, _, cx| this.set_document_layout(layout, cx)),
                cx,
            )
        };
        switch_track(
            "document-layout-switch",
            vec![
                cell(0, DocumentLayout::Dock, glyph_dock, L10nKey::DocumentDock),
                cell(1, DocumentLayout::Fill, glyph_fill, L10nKey::DocumentFill),
            ],
            cx,
        )
    }

    /// The column itself: the surface `overlay_top` selects, sized, bordered,
    /// and given the divider on its left edge.
    /// The header on its own, for the strip above the column — see
    /// [`DocumentChrome::DockHoisted`]. Returns `None` for the layouts that
    /// keep their header inside the surface.
    pub(crate) fn render_document_header(
        &mut self,
        chrome: DocumentChrome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if chrome.renders_own_header() {
            return None;
        }
        match self.document_front()? {
            OverlayTop::Code => Some(self.render_editor_header_only(chrome, window, cx)),
            OverlayTop::Diff => self.render_diff_header_only(chrome, window, cx),
        }
    }

    pub(crate) fn render_document_column(
        &mut self,
        width: f32,
        chrome: DocumentChrome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let body = self.document_body_px(window, cx);
        let surface = match self.document_front()? {
            OverlayTop::Code => self.render_code_overlay(chrome, window, cx),
            OverlayTop::Diff => self.render_diff_overlay(chrome, window, cx),
        }?;
        let (backing, handle) = self.document_resize(body, cx);
        Some(
            v_flex()
                .id("document-column")
                .relative()
                .flex_none()
                .w(px(width))
                .h_full()
                .bg(crate::ui::theme::workspace_surface_color(cx))
                .border_l(px(DOCUMENT_RULE))
                .border_color(cx.theme().sidebar_border)
                .child(backing)
                // The clip belongs to the content, not to the column: the
                // divider hangs half a handle past the left edge, the way the
                // panels' do, and clipping the column would have taken that
                // half — and the grab with it — away.
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .w(px(document_content_px(width)))
                        .overflow_hidden()
                        .child(surface),
                )
                .child(handle)
                .into_any_element(),
        )
    }

    /// The divider, on the same contract as the right panel's: the cell moves
    /// on every mouse move, the config is written once, on mouse up.
    ///
    /// `body` is read here, while there is still a `cx` to read it from — the
    /// drag handler only ever sees a `Window`, and a limit that disagreed with
    /// the one the layout applies would spring the column back from wherever it
    /// was dropped.
    fn document_resize(&self, body: f32, cx: &mut Context<Self>) -> (AnyElement, AnyElement) {
        use gpui::{Bounds, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, canvas};

        let container: Rc<StdCell<Option<Bounds<Pixels>>>> = Rc::new(StdCell::new(None));
        let backing = canvas(
            {
                let container = container.clone();
                move |bounds, _window, _cx| container.set(Some(bounds))
            },
            {
                let container = container.clone();
                let ratio_cell = self.document_ratio.clone();
                let dragging = self.document_dragging.clone();
                move |_bounds, _state, window, _cx| {
                    window.on_mouse_event({
                        let container = container.clone();
                        let ratio_cell = ratio_cell.clone();
                        let dragging = dragging.clone();
                        move |ev: &MouseMoveEvent, _phase, window, _cx| {
                            if !dragging.get() || body <= 0. {
                                return;
                            }
                            let Some(b) = container.get() else {
                                return;
                            };
                            let right = b.origin.x + b.size.width;
                            let raw = (right - ev.position.x).as_f32();
                            ratio_cell.set(dragged_ratio(body, raw));
                            window.refresh();
                        }
                    });
                    window.on_mouse_event({
                        let ratio_cell = ratio_cell.clone();
                        let dragging = dragging.clone();
                        move |_ev: &MouseUpEvent, _phase, window, cx| {
                            if !dragging.get() {
                                return;
                            }
                            dragging.set(false);
                            let r = ratio_cell.get();
                            let cfg = cx.global_mut::<Config>();
                            if cfg.document_ratio != r {
                                cfg.document_ratio = r;
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

        let active = self.document_dragging.get();
        let handle = div()
            .id("document-resize")
            .group("document-resize")
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
                    .group_hover("document-resize", |s| s.bg(cx.theme().drag_border)),
            )
            .on_mouse_down(MouseButton::Left, {
                let dragging = self.document_dragging.clone();
                move |_ev, window, _cx| {
                    dragging.set(true);
                    window.refresh();
                }
            })
            // A double-click lands a mouse-down first, which arms the drag; the
            // mouse-up disarms it without having moved, so the cycle below is
            // the only thing that ends up happening.
            .on_double_click(cx.listener(|this, _, _window, cx| {
                this.document_dragging.set(false);
                this.cycle_document_ratio(cx);
            }))
            .into_any_element();

        (backing, handle)
    }
}

/// The rule on the docked column's left edge, in points.
///
/// Named rather than written twice: the content box below is the column less
/// this, and the two have to move together.
pub(crate) const DOCUMENT_RULE: f32 = 1.;

/// The width the docked column's content box is drawn at.
///
/// The column is sized in points here, and its content has to be sized the
/// same way. It used to be `w_full()` — a share of whatever box the flex
/// ancestor chain resolved to — and on a fractional display scale (a 1.75x
/// Wayland surface, GNOME's `scale-monitor-framebuffer`) that share came out
/// against the wrong box entirely: a 404-point column handed its file a
/// 123-point content box, so the editor laid out at a fifth of the space it
/// was given and soft-wrapped every few characters (#1150). A point count has
/// no ancestor to be resolved against.
///
/// [`DOCUMENT_RULE`] is taken off the content because the column draws it as a
/// border: sizing the content at the full width instead would push it a point
/// past the column, and the clip would eat that off the right.
pub(crate) fn document_content_px(column: f32) -> f32 {
    (column - DOCUMENT_RULE).max(0.)
}

/// The ratio a divider dropped `raw` points from the right edge of a `body`
/// wide area settles on.
///
/// Two clamps, and both are load-bearing. The pixel one keeps the terminal and
/// the document each above their floor, and is what binds on a narrow window.
/// The ratio one is the band the *file* keeps — `Config::sanitize` holds
/// `document_ratio` to it, so a drag that wrote outside it would be moved on
/// the next launch, and a column dropped against the edge of a wide window
/// would reopen hundreds of points from where it was left.
pub(crate) fn dragged_ratio(body: f32, raw: f32) -> f32 {
    let w = raw.clamp(DOCUMENT_MIN_W, (body - TERMINAL_MIN_W).max(DOCUMENT_MIN_W));
    (w / body).clamp(DOCUMENT_RATIO_MIN, DOCUMENT_RATIO_MAX)
}

/// The width the divider's double-click moves to from `current`: the one after
/// whichever named stop `current` is nearest, wrapping round.
pub(crate) fn next_ratio_stop(current: f32) -> f32 {
    let nearest = DOCUMENT_RATIO_STOPS
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| (*a - current).abs().total_cmp(&(*b - current).abs()))
        .map(|(i, _)| i)
        .unwrap_or(1);
    DOCUMENT_RATIO_STOPS[(nearest + 1) % DOCUMENT_RATIO_STOPS.len()]
}

#[cfg(test)]
mod tests {
    use super::{DocumentChrome, dragged_ratio, layout_switch_cell, next_ratio_stop};
    use crate::core::config::{
        DOCUMENT_RATIO_HALF, DOCUMENT_RATIO_MAX, DOCUMENT_RATIO_MIN, DOCUMENT_RATIO_THIRD,
        DOCUMENT_RATIO_TWO_THIRDS, DocumentLayout,
    };
    use crate::ui::app::{DOCUMENT_MIN_W, TERMINAL_MIN_W, document_column_px};

    /// Both document headers raise the cell that matches what is on screen, so
    /// the narrow-window fallback — where a tab asked to dock is drawn filled —
    /// cannot leave the switch pointing at a box the surface is not in.
    #[test]
    fn the_layout_switch_raises_the_box_the_document_is_actually_in() {
        assert_eq!(
            layout_switch_cell(DocumentChrome::Fill),
            DocumentLayout::Fill,
            "the overlay is the whole workspace"
        );
        for column in [DocumentChrome::Dock, DocumentChrome::DockHoisted] {
            assert_eq!(
                layout_switch_cell(column),
                DocumentLayout::Dock,
                "a column is a column, hoisted header or not"
            );
        }
    }

    /// Nothing the divider can write is something the next launch moves.
    /// `Config::sanitize` clamps `document_ratio` into a band, so a drag that
    /// left the band was saved and then quietly relocated: on a 2560-point body
    /// the narrowest the divider went was a ratio of 0.11, which came back as
    /// 0.2 — a 232-point jump the user never asked for. The drag clamps to the
    /// file's band as well as to the two floors now, and this is what says so.
    #[test]
    fn a_dragged_width_is_one_the_config_can_keep() {
        for body in [640., 900., 1440., 2560., 5120.] {
            for raw in [-500., 0., 1., DOCUMENT_MIN_W, body / 2., body, body + 900.] {
                let r = dragged_ratio(body, raw);
                assert!(
                    (DOCUMENT_RATIO_MIN..=DOCUMENT_RATIO_MAX).contains(&r),
                    "body {body} dropped at {raw} wrote {r}, which the file would not keep"
                );
                // And the width that ratio draws still holds the invariant the
                // whole budget exists for.
                let drawn = document_column_px(body, r).expect("a body that seats both");
                assert!(body - drawn >= TERMINAL_MIN_W - f32::EPSILON);
                assert!(drawn >= DOCUMENT_MIN_W - f32::EPSILON);
            }
        }
    }

    /// #1150: the content box is the column's own width, less the rule on its
    /// left edge — never a share of a box resolved further up the flex chain.
    /// A fractional display scale made that share resolve against the wrong
    /// ancestor, and a 404-point column gave its file 123 points, so a docked
    /// document soft-wrapped every few characters.
    #[test]
    fn the_docked_content_is_the_columns_own_width_less_its_rule() {
        use super::{DOCUMENT_RULE, document_content_px};
        for column in [DOCUMENT_MIN_W, 320., 404.54688, 542., 1920.] {
            let content = document_content_px(column);
            assert_eq!(content, column - DOCUMENT_RULE, "column {column}");
            assert!(
                content > 0.,
                "a column of {column} still has to have room for a file"
            );
        }
        // A column narrower than its own rule is not a layout; it must not
        // answer with a negative width for taffy to reject.
        assert_eq!(document_content_px(0.), 0.);
        assert_eq!(document_content_px(0.5), 0.);
    }

    /// Third, half, two thirds, round again — and a dragged width joins at
    /// whichever stop it looks nearest to rather than always restarting.
    #[test]
    fn the_divider_cycles_the_named_widths() {
        assert_eq!(next_ratio_stop(DOCUMENT_RATIO_THIRD), DOCUMENT_RATIO_HALF);
        assert_eq!(
            next_ratio_stop(DOCUMENT_RATIO_HALF),
            DOCUMENT_RATIO_TWO_THIRDS
        );
        assert_eq!(
            next_ratio_stop(DOCUMENT_RATIO_TWO_THIRDS),
            DOCUMENT_RATIO_THIRD
        );
        // Dragged to just under half: nearest stop is half, so the cycle goes
        // on to two thirds rather than back to a third.
        assert_eq!(next_ratio_stop(0.47), DOCUMENT_RATIO_TWO_THIRDS);
        assert_eq!(next_ratio_stop(0.8), DOCUMENT_RATIO_THIRD);
    }

    /// The default is half of what the terminal and the document share, and
    /// what they share is the window less the panels — not the window.
    #[test]
    fn half_of_the_body_is_half_of_the_body() {
        let body = 1440. - 220. - 260.;
        assert_eq!(document_column_px(body, 0.5), Some(body / 2.));
    }

    /// Two thirds is allowed past the half-window cap the side panels obey.
    /// Only the terminal's floor binds it.
    #[test]
    fn two_thirds_is_two_thirds_until_the_terminal_floor_says_otherwise() {
        let wide = 1200.;
        assert_eq!(document_column_px(wide, 2. / 3.), Some(wide * 2. / 3.));

        // 800 * 2/3 is 533, which would leave the terminal 267 — under its
        // floor — so the column stops where the terminal starts.
        let tight = 800.;
        assert_eq!(
            document_column_px(tight, 2. / 3.),
            Some(tight - TERMINAL_MIN_W)
        );
    }

    /// A column narrower than it can be read at is not a column. The ratio
    /// floors out rather than shrinking with the window.
    #[test]
    fn a_thin_share_still_gets_the_documents_floor() {
        let body = 700.;
        assert_eq!(document_column_px(body, 0.2), Some(DOCUMENT_MIN_W));
    }

    /// Below the width where both fit there is no docked layout to draw, and
    /// the caller falls back to the overlay for the frame. The threshold is
    /// exact so that widening by a point re-docks.
    #[test]
    fn a_window_too_narrow_for_both_has_no_docked_width() {
        let floor = TERMINAL_MIN_W + DOCUMENT_MIN_W;
        assert_eq!(document_column_px(floor - 1., 0.5), None);
        assert_eq!(document_column_px(floor, 0.5), Some(DOCUMENT_MIN_W));
        assert_eq!(document_column_px(f32::NAN, 0.5), None);
    }

    /// Whatever the terminal is left, it is never less than its floor. That is
    /// the invariant the whole budget exists for.
    #[test]
    fn the_terminal_keeps_its_floor_at_every_share() {
        for body in [640., 700., 900., 1200., 2400.] {
            for ratio in [0.2, 1. / 3., 0.5, 2. / 3., 0.8] {
                let Some(doc) = document_column_px(body, ratio) else {
                    continue;
                };
                assert!(
                    body - doc >= TERMINAL_MIN_W - f32::EPSILON,
                    "body {body} ratio {ratio} left the terminal {}",
                    body - doc
                );
                assert!(doc >= DOCUMENT_MIN_W - f32::EPSILON);
            }
        }
    }
}

#[cfg(test)]
mod gpui_tests {
    use super::*;
    use crate::core::config::{DOCUMENT_RATIO_TWO_THIRDS, DocumentLayout};
    use crate::ui::app::test_window;
    use crate::ui::pane::{Pane, PaneSlot};
    use crate::ui::pending_pane::{PendingPane, PendingSpawn};
    use gpui::{Entity, TestAppContext, VisualTestContext, px, size};

    /// One quiet tab. The pane is a *connecting* one so the harness needs no
    /// PTY and runs on every platform.
    fn push_tab(app: &mut Tty7App, cx: &mut Context<Tty7App>) {
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
                    run_on_land: None,
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
        cx.notify();
    }

    /// A window with `tabs` quiet tabs, active on the first, sized to order.
    fn window_with(
        cx: &mut TestAppContext,
        w: f32,
        tabs: usize,
    ) -> (Entity<Tty7App>, VisualTestContext) {
        let (app, mut vcx) = test_window::harness(cx);
        app.update_in(&mut vcx, |app, _, cx| {
            for _ in 0..tabs {
                push_tab(app, cx);
            }
            app.active = 0;
        });
        vcx.simulate_resize(size(px(w), px(900.)));
        vcx.run_until_parked();
        (app, vcx)
    }

    fn window(cx: &mut TestAppContext, w: f32) -> (Entity<Tty7App>, VisualTestContext) {
        window_with(cx, w, 1)
    }

    fn dock_px(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> Option<f32> {
        app.update_in(vcx, |app, window, cx| app.document_dock_px(window, cx))
    }

    /// The config value: the default under tabs that were never told, not
    /// necessarily what the active tab is doing.
    fn layout(vcx: &mut VisualTestContext) -> DocumentLayout {
        vcx.update(|_, cx| cx.global::<Config>().document_layout)
    }

    /// What the active tab is actually doing.
    fn tab_layout(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> DocumentLayout {
        app.update_in(vcx, |app, _, cx| app.document_layout(cx))
    }

    /// Fill is one tab's answer, not the window's. Reading a long file in one
    /// tab while an agent works in another wants the whole window here and half
    /// of it there, and a global switch made each of those flip the other.
    #[gpui::test]
    fn one_tabs_fill_leaves_the_other_docked(cx: &mut TestAppContext) {
        let (app, mut vcx) = window_with(cx, 1440., 2);

        // A file open in each tab, both docked to start with.
        for i in [0, 1] {
            app.update_in(&mut vcx, |app, window, cx| {
                app.active = i;
                app.toggle_code_panel(window, cx);
            });
        }
        vcx.run_until_parked();

        app.update_in(&mut vcx, |app, _, cx| {
            app.active = 1;
            app.toggle_document_fill(cx);
        });
        vcx.run_until_parked();
        assert_eq!(dock_px(&app, &mut vcx), None, "the tab that asked, fills");

        app.update_in(&mut vcx, |app, _, _cx| app.active = 0);
        vcx.run_until_parked();
        assert!(
            dock_px(&app, &mut vcx).is_some(),
            "the tab that did not ask, does not"
        );

        // The config is untouched — it is what every tab that was never told is
        // still reading, so writing it would have reached all of them at once.
        assert_eq!(layout(&mut vcx), DocumentLayout::Dock);
        app.update_in(&mut vcx, |app, window, cx| {
            push_tab(app, cx);
            app.active = 2;
            app.toggle_code_panel(window, cx);
        });
        vcx.run_until_parked();
        assert!(
            dock_px(&app, &mut vcx).is_some(),
            "a fresh tab starts from the config, not from what tab 1 chose"
        );
    }

    /// A docked column takes width off the tab strip too, and the strip has to
    /// be told. Where the header is hoisted — Windows, Linux — it is drawn over
    /// the trailing end of the spanning title bar with no fill of its own, so a
    /// chip left under it shows through the file name and stays clickable
    /// through it. On macOS the strip sits inside the terminal column, and
    /// sizing it to the window rather than to that column is what pushed the
    /// New Tab button off the end before the detail panel got the same
    /// reservation.
    #[gpui::test]
    fn the_tab_chips_stop_where_the_document_column_starts(cx: &mut TestAppContext) {
        let (app, mut vcx) = window_with(cx, 1200., 10);
        // Chips only exist with the bar along the top; with the rail up the
        // strip is the sidebar's and has nothing in this row to collide with.
        vcx.update(|_, cx| {
            cx.global_mut::<Config>().tab_bar_position = crate::core::config::TabBarPosition::Top;
        });
        app.update_in(&mut vcx, |app, window, cx| {
            for (i, tab) in app.tabs.iter_mut().enumerate() {
                tab.name = Some(format!("a tab with a long enough name {i}"));
            }
            app.active = 0;
            app.toggle_code_panel(window, cx);
        });
        vcx.run_until_parked();

        let (viewport, panel, docked) = app.update_in(&mut vcx, |app, window, cx| {
            (
                window.viewport_size().width.as_f32(),
                if app.right_panel_open(cx) {
                    app.right_panel_px(window, cx)
                } else {
                    0.
                },
                app.document_dock_px(window, cx)
                    .expect("a 1200 window seats both"),
            )
        });
        let column_left = viewport - panel - docked;
        let chips = app.update_in(&mut vcx, |app, _, _| app.strip_slots.borrow().clone());
        let far = chips
            .iter()
            .map(|b| (b.origin.x + b.size.width).as_f32())
            .fold(0., f32::max);
        assert!(far > 0., "the strip has to have drawn chips to measure");
        assert!(
            far <= column_left + 0.5,
            "a chip reached {far}, past the column's left edge at {column_left}"
        );
    }

    /// The palette row has to name what running it will do. Fill is per tab, so
    /// a tab told to fill must be offered "Dock" — even though the config, which
    /// every tab that was never told is still reading, still says dock. Reading
    /// the config here offered "Fill" to a tab that was already filling.
    #[gpui::test]
    fn the_palette_offers_the_active_tabs_layout_not_the_configs(cx: &mut TestAppContext) {
        use crate::ui::i18n::L10nKey;
        use crate::ui::search::CommandKind;

        let (app, mut vcx) = window_with(cx, 1440., 2);
        crate::ui::i18n::set_locale("en");
        let row = |app: &Entity<Tty7App>, vcx: &mut VisualTestContext| {
            app.update_in(vcx, |app, window, cx| {
                app.search_actions(window, cx)
                    .into_iter()
                    .find(|c| c.kind == CommandKind::ToggleDocumentFill)
                    .expect("the search offers the fill toggle")
                    .title
            })
        };
        assert_eq!(row(&app, &mut vcx), t(L10nKey::CmdDocumentFill).to_string());

        app.update_in(&mut vcx, |app, _, cx| {
            app.active = 1;
            app.toggle_document_fill(cx);
        });
        vcx.run_until_parked();
        assert_eq!(
            row(&app, &mut vcx),
            t(L10nKey::CmdDocumentDock).to_string(),
            "a tab that is filling is offered the way back"
        );
        assert_eq!(
            layout(&mut vcx),
            DocumentLayout::Dock,
            "and the config it did not write still reads dock"
        );

        app.update_in(&mut vcx, |app, _, _| app.active = 0);
        vcx.run_until_parked();
        assert_eq!(
            row(&app, &mut vcx),
            t(L10nKey::CmdDocumentFill).to_string(),
            "the other tab is untouched, and is still offered fill"
        );
    }

    /// The complaint in #625: opening a file must leave the terminal on screen.
    /// The column is half of what the terminal and the document share, and the
    /// terminal's own laid-out area gives up exactly that width — which is what
    /// makes the PTY reflow rather than hide half its columns under a card.
    #[gpui::test]
    fn opening_the_code_panel_docks_a_column_beside_the_terminal(cx: &mut TestAppContext) {
        let (app, mut vcx) = window(cx, 1440.);

        app.update_in(&mut vcx, |app, window, cx| {
            app.toggle_code_panel(window, cx);
        });
        vcx.run_until_parked();

        let body = app.update_in(&mut vcx, |app, window, cx| app.document_body_px(window, cx));
        let docked = dock_px(&app, &mut vcx).expect("a 1440 window seats both");
        assert!(
            (docked - body / 2.).abs() < 0.5,
            "half the terminal column, not half the window: {docked} of {body}"
        );

        // The terminal is laid out at what is left, not at the full width with
        // a card over half of it. `pane_area` is the rectangle the grid sizes
        // itself from, so this is the assertion the PTY reflow rests on.
        let pane = app
            .update_in(&mut vcx, |app, _, _| app.pane_area.get())
            .expect("the body area painted");
        assert!(
            (pane.size.width.as_f32() - (body - docked)).abs() < 1.5,
            "terminal laid out at {} of a {body} body beside a {docked} column",
            pane.size.width.as_f32()
        );
        assert!(pane.size.width.as_f32() >= TERMINAL_MIN_W);
    }

    /// Esc — which is what `toggle_code_panel` runs — puts the width back.
    #[gpui::test]
    fn closing_the_surface_gives_the_width_back(cx: &mut TestAppContext) {
        let (app, mut vcx) = window(cx, 1440.);
        app.update_in(&mut vcx, |app, window, cx| {
            app.toggle_code_panel(window, cx);
        });
        vcx.run_until_parked();
        assert!(dock_px(&app, &mut vcx).is_some());

        app.update_in(&mut vcx, |app, window, cx| {
            app.toggle_code_panel(window, cx);
        });
        vcx.run_until_parked();
        assert_eq!(
            app.update_in(&mut vcx, |app, _, _| app.document_front()),
            None
        );
        assert_eq!(dock_px(&app, &mut vcx), None);
        assert_eq!(
            app.update_in(&mut vcx, |app, _, cx| app.document_floor(cx)),
            0.,
            "a closed surface reserves nothing from the panels"
        );
    }

    /// A window with no room for both falls back to the overlay for the frame
    /// and leaves the saved layout alone. Widening re-docks with no command run
    /// in between — the fallback is derived, not stored.
    #[gpui::test]
    fn a_narrow_window_falls_back_without_saving_it(cx: &mut TestAppContext) {
        let (app, mut vcx) = window(cx, 560.);
        app.update_in(&mut vcx, |app, window, cx| {
            app.toggle_code_panel(window, cx);
        });
        vcx.run_until_parked();

        assert_eq!(dock_px(&app, &mut vcx), None, "no room for both");
        assert_eq!(
            layout(&mut vcx),
            DocumentLayout::Dock,
            "the fallback must never write the user's choice"
        );

        vcx.simulate_resize(size(px(1440.), px(900.)));
        vcx.run_until_parked();
        assert!(
            dock_px(&app, &mut vcx).is_some(),
            "widening re-docks on the next frame"
        );
        assert_eq!(layout(&mut vcx), DocumentLayout::Dock);
    }

    /// Filling is still there, and asking for it *is* a choice worth keeping —
    /// including from inside the narrow-window fallback, where the user is
    /// looking at an overlay and saying they want it.
    #[gpui::test]
    fn asking_to_fill_is_kept_and_a_named_width_docks_again(cx: &mut TestAppContext) {
        let (app, mut vcx) = window(cx, 560.);
        app.update_in(&mut vcx, |app, window, cx| {
            app.toggle_code_panel(window, cx);
        });
        app.update_in(&mut vcx, |app, _, cx| app.toggle_document_fill(cx));
        vcx.run_until_parked();
        assert_eq!(tab_layout(&app, &mut vcx), DocumentLayout::Fill);

        vcx.simulate_resize(size(px(1440.), px(900.)));
        vcx.run_until_parked();
        assert_eq!(
            dock_px(&app, &mut vcx),
            None,
            "a wide window does not overrule a chosen fill"
        );

        // Asking for two thirds of the width is asking for a column.
        app.update_in(&mut vcx, |app, _, cx| {
            app.set_document_ratio(DOCUMENT_RATIO_TWO_THIRDS, cx)
        });
        vcx.run_until_parked();
        assert_eq!(tab_layout(&app, &mut vcx), DocumentLayout::Dock);
        let body = app.update_in(&mut vcx, |app, window, cx| app.document_body_px(window, cx));
        let docked = dock_px(&app, &mut vcx).expect("docked again");
        // Two thirds, or as near as the terminal's floor allows — with both
        // side panels open a 1440 window has 960 to share, and two thirds of
        // that would leave the terminal 320.
        let want = (body * 2. / 3.).min(body - TERMINAL_MIN_W);
        assert!((docked - want).abs() < 0.5, "{docked} of {body}");
        assert!(docked > body / 2., "wider than the half it started at");
        assert_eq!(
            vcx.update(|_, cx| cx.global::<Config>().document_ratio),
            DOCUMENT_RATIO_TWO_THIRDS
        );

        // The header menu writes the layout outright rather than toggling it.
        app.update_in(&mut vcx, |app, _, cx| {
            app.set_document_layout(DocumentLayout::Fill, cx)
        });
        vcx.run_until_parked();
        assert_eq!(tab_layout(&app, &mut vcx), DocumentLayout::Fill);
        assert_eq!(dock_px(&app, &mut vcx), None);
        assert_eq!(
            vcx.update(|_, cx| cx.global::<Config>().document_ratio),
            DOCUMENT_RATIO_TWO_THIRDS,
            "filling does not forget the width to come back to"
        );
    }
}

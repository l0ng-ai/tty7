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

use gpui::{AnyElement, Context, Window, div, prelude::*, px};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use gpui_component::{ActiveTheme as _, InteractiveElementExt as _, v_flex};
use std::cell::Cell as StdCell;
use std::rc::Rc;

use crate::core::config::{Config, DOCUMENT_RATIO_STOPS, DocumentLayout};
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

impl Tty7App {
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

    /// What right-clicking a document's header offers: where it sits.
    ///
    /// On the header rather than in Settings because this is where the question
    /// comes up — the moment a file covers the terminal is the moment you want
    /// it not to, and a preference three pages into a settings panel is not an
    /// answer to that. On the header rather than on a tile beside the close
    /// button because the row already carries a file name, a dirty dot and, for
    /// a diff, a view toggle; a fifth control in a column's width is one too
    /// many for something you set once.
    ///
    pub(crate) fn document_header_menu(
        menu: PopupMenu,
        app: &gpui::WeakEntity<Self>,
        cx: &gpui::App,
    ) -> PopupMenu {
        let docked = app
            .upgrade()
            .is_none_or(|this| this.read(cx).document_layout(cx) == DocumentLayout::Dock);
        let mut menu = menu.min_w(px(220.));

        for (label, layout) in [
            (L10nKey::DocumentDock, DocumentLayout::Dock),
            (L10nKey::DocumentFill, DocumentLayout::Fill),
        ] {
            menu = menu.item(
                PopupMenuItem::new(t(label))
                    .checked(docked == (layout == DocumentLayout::Dock))
                    .on_click({
                        let app = app.clone();
                        move |_, _window, cx| {
                            let _ = app.update(cx, |this, cx| this.set_document_layout(layout, cx));
                        }
                    }),
            );
        }
        menu
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
                .border_l_1()
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
                        .w_full()
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
                            let w = raw
                                .clamp(DOCUMENT_MIN_W, (body - TERMINAL_MIN_W).max(DOCUMENT_MIN_W));
                            ratio_cell.set(w / body);
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
    use super::next_ratio_stop;
    use crate::core::config::{
        DOCUMENT_RATIO_HALF, DOCUMENT_RATIO_THIRD, DOCUMENT_RATIO_TWO_THIRDS,
    };
    use crate::ui::app::{DOCUMENT_MIN_W, TERMINAL_MIN_W, document_column_px};

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
        // a card over half of it.  is the rectangle the grid sizes
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

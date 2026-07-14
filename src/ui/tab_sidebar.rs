//! The vertical tab sidebar: the left-side alternative to the horizontal
//! [`tab_strip`](crate::ui::tab_strip), shown when `tab_bar_position` is `left`.
//! One full-width row per tab — label, inline rename, drag-to-reorder, hover
//! close — under a search + new-tab control bar at the top of the rail.
//!
//! Split out of `app.rs` as an `impl Tty7App` block, exactly like `tab_strip`.
//! It shares the model wholesale: the same `self.tabs`/`self.active` state, the
//! same `tab_label`, the same `activate`/`close_tab`/`move_tab`/`start_rename`
//! operations, the same `DragTab` payload, and the same theme tokens the chips
//! use — so the vertical list stays pixel-consistent with the strip and adds no
//! new state or business logic, only a new set of click targets in a new shape.

use gpui::{
    Bounds, Context, FontWeight, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
    SharedString, Window, canvas, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use std::cell::Cell;
use std::rc::Rc;

use crate::core::config::Config;
use crate::ui::app::{TITLE_BAR_HEIGHT, Tty7App};
use crate::ui::hints::tab_badge_label;
use crate::ui::tab_strip::DragTab;

/// Minimum sidebar width, and the maximum as a fraction of the window width, so
/// a resize drag can't collapse the rail or let it swallow the terminal.
const MIN_SIDEBAR_WIDTH: f32 = 180.;
const MAX_SIDEBAR_WIDTH_RATIO: f32 = 0.5;

/// Width (px) of the draggable resize handle's invisible hit-area, centered on
/// the rail's right border; it holds a 1px hairline that brightens on hover /
/// drag. Centered (half overhangs the body) so it clears the row close buttons.
const RESIZE_HANDLE_WIDTH: f32 = 8.;

impl Tty7App {
    /// The vertical tab sidebar rendered down the left edge of the body in
    /// `tab_bar_position: left` mode. Only reached when at least one tab is open
    /// (the caller keeps the horizontal layout for the zero-tab home page), so
    /// there's no empty state to render.
    pub(crate) fn tab_sidebar(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let active = self.active;
        // While the bare ⌘/Ctrl hold is armed (see `ui::hints`), each of the
        // first nine rows swaps its close affordance for a ⌘N badge — same slot
        // and footprint as the chips, so the vertical list gets the identical
        // "hold to see switch digits" hint the horizontal strip has.
        let show_badges = self.mod_hint_badges;
        // Width from the persisted/drag-updated cell, re-clamped to the live
        // window so a saved value never exceeds half the (possibly smaller)
        // viewport. The floor wins if half the window is somehow narrower.
        let max_width = (window.viewport_size().width.as_f32() * MAX_SIDEBAR_WIDTH_RATIO)
            .max(MIN_SIDEBAR_WIDTH);
        let width = self.sidebar_width.get().clamp(MIN_SIDEBAR_WIDTH, max_width);
        // Live filter query from the top-bar search box; empty matches all.
        let query = self.sidebar_search.read(cx).value().trim().to_lowercase();

        let mut list = v_flex()
            // An id + `overflow_y_scroll` makes the row column scroll on its own
            // when the tabs outgrow the window height, leaving the "+" footer
            // pinned (same pattern the settings panel uses). `track_scroll` lets
            // `activate` pull the selected row into view.
            .id("tab-sidebar-list")
            .track_scroll(&self.sidebar_scroll)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_1p5()
            .gap_0p5();

        for (i, tab) in self.tabs.iter().enumerate() {
            let is_active = i == active;
            let label = self.tab_label(tab, i, Some(window), cx);
            // SSH status dot (PRD FR-E2).
            let ssh_dot = self.tab_ssh_dot(tab, cx);
            // Filter by the search box; matching is on the visible label. The row
            // keeps its real index `i`, so activate/close/move still hit the right
            // tab even when the list is narrowed.
            if !query.is_empty() && !label.to_lowercase().contains(&query) {
                continue;
            }
            let drag_label: SharedString = label.clone().into();

            // Inline rename input for this tab, if it's the one being renamed —
            // the same `self.renaming` branch the strip uses, so a double-click
            // rename works identically in either layout.
            let rename_input = self
                .renaming
                .as_ref()
                .filter(|r| r.index == i)
                .map(|r| r.input.clone());

            let label_region = match rename_input {
                Some(input) => div()
                    .id(("sidebar-rename", i))
                    .flex_1()
                    .min_w_0()
                    // Swallow the mouse-down (incl. double-click word-select) so
                    // it doesn't reach the row's activate handler below.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(Input::new(&input).appearance(false))
                    .into_any_element(),
                None => div()
                    .id(("sidebar-label", i))
                    .flex_1()
                    .min_w_0()
                    // Ellipsis-truncate so a long label degrades gracefully in the
                    // fixed-width rail rather than hard-clipping mid-glyph.
                    .truncate()
                    .text_sm()
                    // Active row carries a hair more weight, matching the chip.
                    .when(is_active, |d| d.font_weight(FontWeight::MEDIUM))
                    .child(label)
                    // Single click activates; double click starts a rename.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            if ev.click_count >= 2 {
                                this.start_rename(i, window, cx);
                            } else {
                                this.activate(i, window, cx);
                            }
                        }),
                    )
                    // Drag the row by its label to reorder it (shared `DragTab`).
                    .on_drag(
                        DragTab {
                            index: i,
                            label: drag_label.clone(),
                        },
                        |drag, _, _, cx| {
                            cx.stop_propagation();
                            cx.new(|_| drag.clone())
                        },
                    )
                    .into_any_element(),
            };

            let row = h_flex()
                .id(("tab-row", i))
                // A per-row group so this row's close affordance reveals on its own
                // hover without touching siblings (same trick as the chip).
                .group(SharedString::from(format!("tab-row-{i}")))
                .w_full()
                .h(px(34.))
                .items_center()
                .justify_between()
                .gap_1p5()
                .pl_3()
                .pr_1p5()
                .rounded_lg()
                // Sidebar-surface token scheme (gpui-component's Sidebar
                // semantics), so the rows sit cohesively on the sunk rail rather
                // than reading as chips: active = the sidebar-accent fill + its
                // paired foreground; inactive = the muted sidebar foreground with
                // a half-strength accent on hover (a natural hover→active ramp).
                .when(is_active, |s| {
                    s.bg(cx.theme().sidebar_accent)
                        .text_color(cx.theme().sidebar_accent_foreground)
                })
                .when(!is_active, |s| {
                    s.text_color(cx.theme().sidebar_foreground)
                        .hover(|s| s.bg(cx.theme().sidebar_accent.opacity(0.5)))
                })
                // Drop target: dropping a dragged row here moves it to this slot.
                .drag_over::<DragTab>(|s, _, _, cx| s.bg(cx.theme().drag_border.opacity(0.2)))
                .on_drop(cx.listener(move |this, drag: &DragTab, _window, cx| {
                    this.move_tab(drag.index, i, cx);
                }))
                // A click anywhere on the row (padding, gaps) activates it; the
                // label and close children stop propagation for their own actions.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.activate(i, window, cx);
                    }),
                )
                // Leading SSH status dot when this tab hosts an SSH session.
                .when_some(ssh_dot, |c, color| {
                    c.child(div().flex_shrink_0().size(px(6.)).rounded_full().bg(color))
                })
                .child(label_region)
                // Trailing slot: while the shortcut hints are armed it shows the
                // row's ⌘N switch digit; otherwise the close affordance — always
                // shown on the active row, opacity-0-until-hover on the others so
                // a column of tabs reads clean. Space is reserved either way.
                .child(if show_badges && i < 9 {
                    // Bare digit, no keycap box — matches the chip badge exactly.
                    div()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(20.))
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if is_active {
                            cx.theme().sidebar_accent_foreground
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(tab_badge_label(i))
                        .into_any_element()
                } else {
                    div()
                        .flex_shrink_0()
                        .when(!is_active, |s| {
                            s.opacity(0.)
                                .group_hover(SharedString::from(format!("tab-row-{i}")), |s| {
                                    s.opacity(1.)
                                })
                        })
                        .child(
                            Button::new(("sidebar-close", i))
                                .icon(IconName::Close)
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.close_tab(i, window, cx);
                                })),
                        )
                        .into_any_element()
                });

            list = list.child(row);
        }

        // Top control bar: a right-aligned "+" new-tab button (the same shell
        // picker the strip uses), with new-tab at the top of the rail rather than
        // in a bottom button. A hairline under it separates the control row from
        // the tab list.
        let add_button = self.attach_new_tab_menu(
            Button::new("sidebar-add")
                .icon(Icon::new(IconName::Plus).size(px(15.)))
                .ghost()
                .xsmall()
                .w(px(30.))
                .h(px(30.))
                .rounded_lg(),
            cx,
        );
        // Borderless "Search tabs…" that sits directly on the sunk surface: a
        // leading magnifier + an appearance-less input, no box and no divider
        // under the bar, so the control row and list read as one continuous rail
        // rather than stacked panels.
        let top_bar = h_flex()
            .flex_shrink_0()
            .items_center()
            .gap_1()
            .h(px(44.))
            .px_3()
            .child(
                Icon::new(IconName::Search)
                    .small()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.sidebar_search).appearance(false)),
            )
            .child(add_button);

        // ── Resize drag (mirrors the split divider in `pane.rs`) ──────────────
        // A backing canvas measures the rail's bounds into a per-frame cell and,
        // while the handle is held, installs window-level mouse listeners so the
        // drag keeps tracking even when the pointer outruns the thin handle.
        let container: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));
        let backing = canvas(
            {
                let container = container.clone();
                move |bounds, _window, _cx| container.set(Some(bounds))
            },
            {
                let container = container.clone();
                let width_cell = self.sidebar_width.clone();
                let dragging = self.sidebar_dragging.clone();
                move |_bounds, _state, window, _cx| {
                    // Track the pointer while the handle is held: width = pointer
                    // x minus the rail's left edge, clamped to the live bounds.
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
                            let raw = (ev.position.x - b.origin.x).as_f32();
                            let max = (window.viewport_size().width.as_f32()
                                * MAX_SIDEBAR_WIDTH_RATIO)
                                .max(MIN_SIDEBAR_WIDTH);
                            width_cell.set(raw.clamp(MIN_SIDEBAR_WIDTH, max));
                            window.refresh();
                        }
                    });
                    // On release, end the drag and persist the final width so it
                    // survives a restart (the config observer re-syncs the cell).
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
                            if cfg.sidebar_width != w {
                                cfg.sidebar_width = w;
                                cfg.save();
                            }
                            window.refresh();
                        }
                    });
                }
            },
        )
        .absolute()
        .size_full();

        // The draggable handle at the right edge: a comfortable invisible hit-area
        // centered over the border, holding a 1px line that brightens on hover /
        // drag (the border stays visible underneath when idle).
        let handle_active = self.sidebar_dragging.get();
        let handle = div()
            .group("sidebar-resize")
            .absolute()
            .top_0()
            .right(px(-(RESIZE_HANDLE_WIDTH / 2.)))
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
                    .when(handle_active, |d| d.bg(cx.theme().drag_border))
                    .group_hover("sidebar-resize", |s| s.bg(cx.theme().drag_border)),
            )
            .on_mouse_down(MouseButton::Left, {
                let dragging = self.sidebar_dragging.clone();
                move |_ev, window, _cx| {
                    dragging.set(true);
                    window.refresh();
                }
            });

        div()
            .relative()
            .flex_shrink_0()
            .w(px(width))
            .h_full()
            // The whole rail is the sunk `sidebar` surface (a few % off the body),
            // so the color contrast — not hard lines — separates it from the
            // terminal. A single hairline right edge in the paired border token
            // delineates the seam, so the rail reads as one cohesive surface.
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            // The measurer/listener sits behind the content, the handle on top.
            .child(backing)
            .child(
                v_flex()
                    .size_full()
                    // A title-bar-height top zone: on macOS the traffic lights
                    // sit on the rail's surface here, and it aligns the search box
                    // with the terminal's top (which starts below the title bar),
                    // so the rail reads as one panel from the very top edge.
                    .child(div().h(px(TITLE_BAR_HEIGHT)).flex_shrink_0())
                    .child(top_bar)
                    .child(list),
            )
            .child(handle)
    }
}

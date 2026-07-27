//! The vertical tab sidebar: the left-side alternative to the horizontal
//! [`tab_strip`](crate::ui::tab_strip), shown when `tab_bar_position` is `left`.
//! One full-width row per tab — label, inline rename, drag-to-reorder, hover
//! close — under a search + new-tab control bar at the top of the rail.
//!
//! Split out of `app.rs` as an `impl Tty7App` block, exactly like `tab_strip`.
//! It shares the model wholesale: the same `self.tabs`/`self.active` state, the
//! same `tab_label`, the same `activate`/`close_tab`/`start_rename` operations,
//! the same `DragTab` payload and reorder machinery, and the same theme tokens
//! the chips use — so the vertical list stays pixel-consistent with the strip
//! and adds no new state or business logic, only a new set of click targets in
//! a new shape.

use gpui::{
    Animation, AnimationExt as _, AnyElement, Axis, Bounds, Context, Div, FontWeight, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, SharedString, Stateful, Window, canvas,
    deferred, div, ease_out_quint, linear_color_stop, linear_gradient, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::menu::{ContextMenu, ContextMenuExt as _};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use std::path::{Path, PathBuf};

use crate::core::config::{Config, SidebarGrouping};
use crate::terminal::git_status::GitStatusCache;
use crate::ui::app::{TITLE_BAR_HEIGHT, Tty7App};
use crate::ui::hints::tab_badge_label;
use crate::ui::reorder::{self, Reorder, Surface};
use crate::ui::tab_strip::{DragTab, REORDER_SLIDE_MS};

/// Minimum sidebar width, and the maximum as a fraction of the window width, so
/// a resize drag can't collapse the rail or let it swallow the terminal.
const MIN_SIDEBAR_WIDTH: f32 = 180.;
const MAX_SIDEBAR_WIDTH_RATIO: f32 = 0.5;

/// Width (px) of the draggable resize handle's invisible hit-area, centered on
/// the rail's right border; it holds a 1px hairline that brightens on hover /
/// drag. Centered (half overhangs the body) so it clears the row close buttons.
const RESIZE_HANDLE_WIDTH: f32 = 8.;

/// Gap between rows in the rail, and between the group blocks — the distance a
/// row or block travels on top of its own height when a drag passes it.
const ROW_GAP: f32 = 2.;

/// Marks a live drag as a *group* drag — the sidebar counterpart to
/// [`DragTab`], and like it a stateless marker that renders nothing: the block
/// being dragged never leaves the rail, so there is no card floating over the
/// window. Its type is what tells the rail's drop handlers "this is a group,
/// not a tab". Scratch never starts one: it's pinned last by
/// [`sidebar_sections`], so it has no slot to move to.
#[derive(Clone)]
pub(crate) struct DragGroup;

impl Render for DragGroup {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // gpui always paints *something* at the cursor for an active drag;
        // an empty, zero-sized element is how this drag paints nothing.
        div()
    }
}

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
        // The rail is a sunk column, so its rows read the sidebar ladder.
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
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
            // `activate` pull the selected row into view — and feeds the overlay
            // scrollbar the wrapper below hangs over this column.
            .id("tab-sidebar-list")
            .track_scroll(&self.sidebar_scroll)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            // 4px horizontal, so a row's own `pl_2` puts its content on the rail's
            // 12px content inset — the same line the search row and the top
            // controls use. The 8px the capsule stops short of the rail edge is
            // what makes the active row read as inset rather than full-bleed.
            .px_1()
            .py_1p5()
            // Tight row-to-row spacing so the tabs read as one dense list, not a
            // set of far-apart cards (each row already has its own padding).
            .gap_0p5();

        // ── Repo grouping ─────────────────────────────────────────────────────
        // Per-tab sticky group keys and the sections derived from them (both
        // documented on their functions). The same pair drives
        // `visual_tab_order`, so the ⌘N digits painted below and the ⌘N
        // actions always agree on which row is "tab 3".
        let keys: Rc<Vec<Option<PathBuf>>> = Rc::new(self.sidebar_group_keys(cx));
        let sections = sidebar_sections(&keys);

        // Each row's position in display order — the digit its ⌘N badge shows.
        // Claimed for every tab, filtered-out ones included, and read off this
        // map rather than counted as rows are emitted, so neither the search
        // box nor a drag's live reflow can renumber the shortcuts under you.
        let badge_pos: Vec<usize> = {
            let mut pos = vec![0usize; self.tabs.len()];
            for (n, i) in sections.iter().flat_map(|s| s.tabs.iter()).enumerate() {
                pos[*i] = n;
            }
            pos
        };

        // Which tabs each section actually lists, and their labels. Settled
        // before anything is laid out, because both drag surfaces are keyed to
        // what is *visible*: a group the search box has emptied isn't rendered,
        // so it must not claim a slot in the group geometry either — a phantom
        // zero-sized slot would sit at the origin and swallow every crossing.
        // (Matching is on the visible label; a row keeps its real tab index, so
        // activate/close/reorder still hit the right tab when the list is
        // narrowed.)
        let visible_by_section: Vec<Vec<(usize, String)>> = sections
            .iter()
            .map(|s| {
                s.tabs
                    .iter()
                    .map(|&i| (i, self.tab_label(&self.tabs[i], i, Some(window), cx)))
                    .filter(|(_, label)| query.is_empty() || label.to_lowercase().contains(&query))
                    .collect()
            })
            .collect();

        // ── Live drag-reorder, part 1: the group blocks ───────────────────────
        // Repo groups can be dragged by their header to reorder the whole
        // block; Scratch can't (it's pinned last, so it has neither a slot to
        // move to nor one to give up), so the draggable slots are exactly the
        // rendered repo groups. See [`crate::ui::reorder`] for the machinery.
        let pointer = window.mouse_position();
        let rendered = |ix: &usize| !visible_by_section[*ix].is_empty();
        let repo_slots: Vec<usize> = (0..sections.len())
            .filter(|&ix| sections[ix].key.is_some())
            .filter(rendered)
            .collect();
        let repo_groups = repo_slots.len();
        let group_slots: Rc<RefCell<Vec<Bounds<Pixels>>>> =
            Rc::new(RefCell::new(vec![Bounds::default(); repo_groups]));
        let group_preview =
            reorder::preview(&self.reorder, &Surface::SidebarGroups, repo_groups, pointer);
        let repo_roots: Vec<PathBuf> = repo_slots
            .iter()
            .filter_map(|&ix| sections[ix].key.clone())
            .collect();
        let slot_display: Vec<usize> = match &group_preview {
            Some(p) => {
                // Same as the rows below: record what releasing right now would
                // produce, so the commit doesn't depend on where the cursor is.
                if let (Some(from), Some(to)) = (repo_roots.get(p.from), repo_roots.get(p.target))
                    && let Some(order) = regrouped_order(&keys, from, to)
                {
                    reorder::set_pending(&self.reorder, &Surface::SidebarGroups, order);
                }
                p.order.clone()
            }
            None => (0..repo_groups).collect(),
        };
        // The blocks to lay out, as `(drag slot, section)`. Repo groups lead in
        // the previewed slot order; Scratch trails them with no slot of its own.
        let mut blocks: Vec<(Option<usize>, usize)> = slot_display
            .into_iter()
            .map(|slot| (Some(slot), repo_slots[slot]))
            .collect();
        blocks.extend(
            (0..sections.len())
                .filter(|&ix| sections[ix].key.is_none())
                .filter(rendered)
                .map(|ix| (None, ix)),
        );

        for (group_slot, group_ix) in blocks {
            let section = &sections[group_ix];
            let group_key = section.key.clone();
            // Kept as concrete elements, not `AnyElement`s: a live drag
            // restyles them (the slide-in offset), which can only be applied
            // once every row of the group has been built.
            let mut rows: Vec<ContextMenu<Stateful<Div>>> = Vec::new();
            let visible = visible_by_section[group_ix].clone();
            let visible_tabs: Vec<usize> = visible.iter().map(|(i, _)| *i).collect();
            let row_slots: Rc<RefCell<Vec<Bounds<Pixels>>>> =
                Rc::new(RefCell::new(vec![Bounds::default(); visible.len()]));
            // ── Live drag-reorder, part 2: the rows of this group ─────────────
            let row_preview = reorder::preview(
                &self.reorder,
                &Surface::SidebarRows(group_key.clone()),
                visible.len(),
                pointer,
            );
            for (slot, (i, label)) in visible.into_iter().enumerate() {
                let badge_pos = badge_pos[i];
                let tab = &self.tabs[i];
                let is_active = i == active;
                // No status/cwd text under the title: the avatar's status dot
                // already carries working/waiting/done, and the group header + the
                // trailing branch tag carry the location — a "Working…" or cwd
                // line would just be noise. One line per row, nothing else.
                // Leading avatar inputs: the SSH connection-status colour (PRD
                // FR-E2) and the coding agent running in the tab, if any — the
                // avatar brands the row by whichever applies.
                let ssh_dot = self.tab_ssh_dot(tab, cx);
                let agent = tab.agent(cx);
                let agent_status = tab.agent_status(cx);
                let agent_unread = tab.agent_unread_count(cx);
                // Second line, when the pane is inside a git work tree: the
                // branch (flexes + truncates) with the working-tree diff pinned
                // to the row's right. Kept *off* the title line on purpose — a
                // long branch or a big `+426 −238` would otherwise crowd the
                // title into an ellipsis. The branch is also the row's most
                // volatile text (checkouts, rebases), so isolating it here means
                // a change never disturbs the title; grouping keys on the repo
                // root only, so a branch switch never relocates the row either
                // (see `Tab::sidebar_group`). The diff counts are a quiet
                // green/red readout and double as the diff-overlay toggle: click
                // them to peek another session's changes in an overlay without
                // activating this row's tab. The cwd they probe is the same one
                // the status resolved through, so overlay and counts always
                // describe the same repo.
                let git_cwd = tab
                    .pane
                    .focused_or_first(window, cx)
                    .and_then(|leaf| leaf.read(cx).git_status_cwd().map(|p| p.to_path_buf()));
                let git_line = tab.git_status(Some(window), cx).map(|g| {
                    let mut line = h_flex()
                        .id(("sidebar-git", i))
                        .w_full()
                        .items_center()
                        .gap_1p5()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(
                            gpui::svg()
                                .path("icons/git-branch.svg")
                                .flex_shrink_0()
                                .size(px(11.))
                                .text_color(cx.theme().muted_foreground),
                        )
                        // Branch name flexes and truncates; the counts stay
                        // pinned right (a long branch ellipsizes, counts don't).
                        .child(div().flex_1().min_w_0().truncate().child(g.branch.clone()));
                    if g.added > 0 || g.removed > 0 {
                        let mut counts = h_flex()
                            .id(("sidebar-diff", i))
                            .flex_shrink_0()
                            .items_center()
                            .gap_1p5()
                            .when_some(git_cwd, |counts, cwd| {
                                counts.cursor_pointer().on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                        // Swallow the press so the row/label
                                        // handlers don't also activate the tab.
                                        cx.stop_propagation();
                                        this.toggle_diff_overlay(cwd.clone(), window, cx);
                                    }),
                                )
                            });
                        if g.added > 0 {
                            counts = counts.child(
                                div()
                                    .text_color(cx.theme().success)
                                    .child(format!("+{}", g.added)),
                            );
                        }
                        if g.removed > 0 {
                            counts = counts.child(
                                div()
                                    .text_color(cx.theme().danger)
                                    .child(format!("−{}", g.removed)),
                            );
                        }
                        line = line.child(counts);
                    }
                    line
                });
                // Inline rename input for this tab, if it's the one being renamed —
                // the same `self.renaming` branch the strip uses, so a context-menu
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
                    None => v_flex()
                        .id(("sidebar-label", i))
                        .flex_1()
                        .min_w_0()
                        // A hair of air between the title and branch lines.
                        .gap(px(2.))
                        // Title line — ellipsis-truncate so a long label degrades
                        // gracefully in the fixed-width rail rather than hard-clipping.
                        .child(
                            div()
                                .w_full()
                                .truncate()
                                .text_sm()
                                // Active row carries a hair more weight, matching the chip.
                                .when(is_active, |d| d.font_weight(FontWeight::MEDIUM))
                                .child(label),
                        )
                        // Branch + diff line, when the pane sits in a git repo.
                        .children(git_line)
                        // No mouse handler of its own: activation *and* the
                        // reorder drag both live on the row, and a child that
                        // swallowed the press would take the label — the
                        // largest part of the row — out of both. (Only the
                        // diff counts inside the branch line stop the press,
                        // deliberately: they're their own click target.)
                        .into_any_element(),
                };

                let row = h_flex()
                    .id(("tab-row", i))
                    // A per-row group so this row's close affordance reveals on its own
                    // hover without touching siblings (same trick as the chip).
                    .group(SharedString::from(format!("tab-row-{i}")))
                    // A row is first of all a switch target, so the hover
                    // cursor says "click me"; picking it up swaps in the
                    // closed hand (see `Tty7App::render`).
                    .cursor_pointer()
                    // Drag anywhere on the row to reorder it (shared `DragTab`).
                    // On the row, not on its label: the drag's frame of
                    // reference is where the *row* was grabbed, which is what
                    // the frozen geometry below measures — hang it off the
                    // label and the held row rides a few pixels off the cursor,
                    // skewing every crossing by that much. `slot` is the row's
                    // position among the *visible* rows of its group; a drag
                    // never leaves the group, so that's the whole world it
                    // needs. The builder runs once, when gpui promotes the
                    // press into a drag, freezing the geometry as of the last
                    // painted frame.
                    .on_drag(DragTab, {
                        let state = self.reorder.clone();
                        let slots = row_slots.clone();
                        let group_key = group_key.clone();
                        move |_drag, grab, _window, cx| {
                            cx.stop_propagation();
                            *state.borrow_mut() = Some(Reorder::new(
                                Surface::SidebarRows(group_key.clone()),
                                slot,
                                slots.borrow().clone(),
                                Axis::Vertical,
                                px(ROW_GAP),
                                grab,
                            ));
                            cx.new(|_| DragTab)
                        }
                    })
                    .w_full()
                    // Size to content with a small, uniform vertical padding: a
                    // one-line shell tab is a short row, a two-line git tab
                    // (title + branch) is a taller one. The *padding* is what
                    // stays consistent, so rows read as harmonious even though
                    // heights differ.
                    .py_1()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .pl_2()
                    .pr_2()
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
                            .hover(|s| s.bg(gpui::rgb(sf.hover)))
                    })
                    // Held: a light dimming so the row under your cursor reads
                    // as picked up. Not a lift — it stays in the rail's plane.
                    .when(row_preview.as_ref().is_some_and(|p| p.from == slot), |s| {
                        s.opacity(0.75)
                    })
                    // Measures this row into its group's slot table — the
                    // geometry a drag starting on a later frame freezes.
                    // Absolute and empty, so it costs the layout nothing.
                    .child(
                        canvas(
                            {
                                let slots = row_slots.clone();
                                move |bounds, _window, _cx| {
                                    if let Some(s) = slots.borrow_mut().get_mut(slot) {
                                        *s = bounds;
                                    }
                                }
                            },
                            |_, _, _, _| {},
                        )
                        // `inset_0`, not `size_full` — see the strip's copy of
                        // this canvas for why the distinction matters.
                        .absolute()
                        .inset_0(),
                    )
                    // A click anywhere on the row (padding, gaps) activates it; the
                    // label and close children stop propagation for their own actions.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.activate(i, window, cx);
                        }),
                    )
                    // Leading avatar: agent brand mark, SSH status, or shell glyph.
                    .child(self.tab_avatar(agent, agent_status, agent_unread, ssh_dot, 22., cx))
                    .child(label_region)
                    // Trailing ⌘N badge: while the shortcut hints are armed the
                    // row shows its switch digit in an in-flow 20px slot (an
                    // all-rows-at-once modal reflow, same as the strip). The digit
                    // is the row's *display* position (`activate_visual` speaks the
                    // same order), so under grouping the rail still reads 1…9 top
                    // to bottom instead of scattering the tab-vector indices.
                    .when(show_badges && badge_pos < 9, |row| {
                        // Bare digit, no keycap box — matches the chip badge exactly.
                        row.child(
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
                                .child(tab_badge_label(badge_pos)),
                        )
                    })
                    // Close affordance: out of flow, so the label runs the full
                    // rail width instead of always reserving a slot for a button
                    // that's invisible until hover (same Safari-style float as
                    // the strip's chips). On hover the ✕ sits over the *title
                    // line's* right end — pinned to the row top, not centered:
                    // on a two-line row a centered ✕ would straddle both lines
                    // and cover the branch line's `+n −n` counts, which are a
                    // click target of their own (the diff-overlay toggle). A
                    // solid backing in the row's hover fill plus a short
                    // gradient run-in fades covered title text out instead of
                    // hard-cutting mid-glyph. Nothing reflows on hover.
                    .when(!(show_badges && badge_pos < 9), |row| {
                        // The row fills are composited over the rail (the accent
                        // carries alpha; the inactive hover is a half-strength
                        // wash), so flatten them against `sidebar` to get the
                        // opaque colour the float must match.
                        // Both rungs are opaque ladder colours, so the float's
                        // backing is just the rung itself — no flattening needed,
                        // and no alpha to drift out of step with the row it copies.
                        let backing: gpui::Hsla = if is_active {
                            gpui::rgb(sf.selected).into()
                        } else {
                            gpui::rgb(sf.hover).into()
                        };
                        let mut fade_from = backing;
                        fade_from.a = 0.;
                        row.child(
                            h_flex()
                                .absolute()
                                // `py_1` row padding: the 20px button covers the
                                // title line exactly.
                                .top(px(4.))
                                .right(px(6.))
                                .opacity(0.)
                                .group_hover(SharedString::from(format!("tab-row-{i}")), |s| {
                                    s.opacity(1.)
                                })
                                .child(div().w(px(10.)).h(px(20.)).bg(linear_gradient(
                                    90.,
                                    linear_color_stop(fade_from, 0.),
                                    linear_color_stop(backing, 1.),
                                )))
                                .child(
                                    div().bg(backing).child(
                                        Button::new(("sidebar-close", i))
                                            .icon(IconName::Close)
                                            .ghost()
                                            .xsmall()
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.close_tab(i, window, cx);
                                            })),
                                    ),
                                ),
                        )
                    });

                // Per-tab right-click menu, shared with the strip's chips;
                // `below_wording` flips the trailing close to "Close Tabs Below"
                // to match the vertical layout.
                let menu_app = cx.entity().downgrade();
                rows.push(row.context_menu(move |menu, window, cx| {
                    Tty7App::tab_context_menu(menu, i, true, &menu_app, window, cx)
                }));
            }

            if rows.is_empty() {
                continue;
            }

            let row_display: Vec<usize> = match &row_preview {
                Some(p) => {
                    // Record the tab order releasing right now would produce, so
                    // letting go applies exactly what's on screen no matter where
                    // the cursor ended up (see `reorder::set_pending`).
                    if let Some(order) =
                        reordered_rows(&keys, &group_key, &visible_tabs, p.from, p.target)
                    {
                        reorder::set_pending(
                            &self.reorder,
                            &Surface::SidebarRows(group_key.clone()),
                            order,
                        );
                    }
                    p.order.clone()
                }
                None => (0..rows.len()).collect(),
            };
            let row_count = rows.len();
            let mut rows: Vec<Option<ContextMenu<Stateful<Div>>>> =
                rows.into_iter().map(Some).collect();
            let rows: Vec<AnyElement> = row_display
                .into_iter()
                .map(|slot| match &row_preview {
                    // The row in hand: drawn wherever the cursor is holding it,
                    // pixel for pixel, with no animation in the way. `deferred`
                    // keeps its slot in the layout but paints it after its
                    // siblings, so it passes *over* the rows it's crossing
                    // instead of being clipped behind them.
                    Some(p) if p.from == slot => deferred(
                        rows[slot]
                            .take()
                            .expect("each slot emitted once")
                            .relative()
                            .top(p.held),
                    )
                    .into_any_element(),
                    // Slide into place rather than teleporting. `offset` is
                    // zero for every row the last crossing left alone, so one
                    // row moves at a time instead of the group re-animating.
                    Some(p) => {
                        let offset = p.offsets[slot].as_f32();
                        rows[slot]
                            .take()
                            .expect("each slot emitted once")
                            .with_animation(
                                (
                                    SharedString::from(format!("row-slide-{}", p.generation)),
                                    slot,
                                ),
                                Animation::new(std::time::Duration::from_millis(REORDER_SLIDE_MS))
                                    .with_easing(ease_out_quint()),
                                move |el, delta| el.top(px(offset * (1. - delta))),
                            )
                            .into_any_element()
                    }
                    None => rows[slot]
                        .take()
                        .expect("each slot emitted once")
                        .into_any_element(),
                })
                .collect();
            // Group header: the repo's directory name (or "Scratch"), small
            // and muted so it labels without competing with the rows, plus the
            // visible-row count. Not a click target — rows do the activating —
            // but it *is* the whole group's drag handle: drag one project name
            // and the block moves, tabs and all, with the other groups sliding
            // around it exactly as rows do inside one. Scratch (`group_key ==
            // None`) sits out: it's pinned last, so it has nowhere to go.
            let header = section.name.clone().map(|name| {
                let label: SharedString = name.to_uppercase().into();
                h_flex()
                    .id(("sidebar-group", group_ix))
                    .w_full()
                    .items_center()
                    .gap_1p5()
                    .pl_2()
                    .pr_1p5()
                    .pt_1p5()
                    .pb_0p5()
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .when_some(group_slot, |header, slot| {
                        // Unlike a row, a header does nothing on click — its
                        // only affordance is the drag, so the open hand is the
                        // honest hover cursor (it closes once you pick it up).
                        // Windows has no open-hand system cursor, and gpui's
                        // Windows backend falls every unmapped `CursorStyle`
                        // through to `IDC_ARROW` — which reads as "nothing to
                        // do here". Point there instead: it's the same cursor
                        // the rows use, and it at least says "interactive".
                        let header = if cfg!(target_os = "windows") {
                            header.cursor_pointer()
                        } else {
                            header.cursor_grab()
                        };
                        header.on_drag(DragGroup, {
                            let state = self.reorder.clone();
                            let slots = group_slots.clone();
                            move |_drag, grab, _window, cx| {
                                cx.stop_propagation();
                                *state.borrow_mut() = Some(Reorder::new(
                                    Surface::SidebarGroups,
                                    slot,
                                    slots.borrow().clone(),
                                    Axis::Vertical,
                                    px(ROW_GAP),
                                    // The header is the handle, but the *block*
                                    // is what moves. The header leads the block,
                                    // so the grab point inside the header is
                                    // also the grab point inside the block —
                                    // it passes through unchanged.
                                    grab,
                                ));
                                cx.new(|_| DragGroup)
                            }
                        })
                    })
                    // Count sits right next to the name (not pushed to the
                    // rail's right edge): the name shrinks and truncates if
                    // long, the count trails it as a quiet tally.
                    .child(
                        div()
                            .flex_shrink(1.)
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(cx.theme().muted_foreground.opacity(0.7))
                            .child(row_count.to_string()),
                    )
            });

            // One block per group — header plus its rows — so a header drag can
            // move the whole thing as a unit and measure it as one slot.
            let block = v_flex()
                .w_full()
                .gap(px(ROW_GAP))
                // Held: the block you're dragging dims, exactly as a held row
                // does — nothing lifts off the rail.
                .when(
                    group_preview
                        .as_ref()
                        .is_some_and(|p| Some(p.from) == group_slot),
                    |b| b.opacity(0.75),
                )
                .children(header)
                .children(rows)
                // Measures the block for the group-drag geometry. Only the
                // rendered repo groups hold a slot — Scratch is pinned last and
                // never moves, and a group the search box emptied isn't here at
                // all — so `group_slot` indexes that list, not `sections`.
                .when_some(group_slot, |block, slot| {
                    block.child(
                        canvas(
                            {
                                let slots = group_slots.clone();
                                move |bounds, _window, _cx| {
                                    if let Some(s) = slots.borrow_mut().get_mut(slot) {
                                        *s = bounds;
                                    }
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                });

            list = list.child(match (&group_preview, group_slot) {
                // The block in hand tracks the cursor, painted over the ones it
                // crosses (same treatment a held row gets inside a group).
                (Some(p), Some(slot)) if p.from == slot => {
                    deferred(block.relative().top(p.held)).into_any_element()
                }
                // Everything else slides; a slotless block (Scratch) never
                // moves, so it falls through to the plain block below.
                (Some(p), Some(slot)) => {
                    let offset = p.offsets[slot].as_f32();
                    block
                        .with_animation(
                            (
                                SharedString::from(format!("group-slide-{}", p.generation)),
                                slot,
                            ),
                            Animation::new(std::time::Duration::from_millis(REORDER_SLIDE_MS))
                                .with_easing(ease_out_quint()),
                            move |el, delta| el.top(px(offset * (1. - delta))),
                        )
                        .into_any_element()
                }
                _ => block.into_any_element(),
            });
        }

        // The rail's own controls — new tab, and collapse — live in the top zone
        // beside the traffic lights, right-aligned to the rail's content edge
        // rather than sitting in the search row. Two reasons: the search row is
        // for searching, and a collapse button that lives *inside* the rail would
        // disappear along with it (its counterpart then appears in the title
        // strip, see `tab_strip`). Right-aligned, they ride the rail's right edge,
        // which is what says "these belong to this panel" when it's resized.
        let controls = h_flex()
            .flex_shrink_0()
            .h(px(TITLE_BAR_HEIGHT))
            // Same box as the real title bar this row stands in for, hairline
            // included: gpui-component's `TitleBar` draws a `border_b_1` inside its
            // own `TITLE_BAR_HEIGHT` (tty7 paints it transparent, but it still takes
            // its pixel), so the bar centres its contents on 19.5 while an
            // unbordered 40px row centres them on 20. Half a pixel is invisible on
            // the line-art tiles, and *not* on the solid brand mark: collapsing the
            // rail hands the mark from this row to the bar, and it visibly hopped up
            // as it went. Reserve the same pixel here and the handover is still.
            .border_b_1()
            .border_color(cx.theme().transparent)
            .items_center()
            .justify_end()
            .gap(px(2.))
            // Glyph's ink, not hit box, on the content edge — see `TILE_PAD`.
            .pr(px(crate::ui::app::tile_trailing_inset()))
            // The brand mark leads the row, on the rail's own content inset — the
            // line the search magnifier and every row label below it start on, so
            // it reads as the head of this column rather than a floating badge.
            // The spacer is what keeps the controls pinned right once the row has
            // a leading child (`justify_end` alone no longer does it).
            .when_some(crate::ui::app::window_mark(), |row, mark| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .pl(px(crate::ui::app::CONTENT_INSET))
                        .child(mark),
                )
                .child(div().flex_1())
            })
            // Both tiles are wrapped in an `occlude()` div, exactly like the
            // title-strip chrome. This row is a `WindowControlArea::Drag` (set
            // below), which on Windows maps to HTCAPTION — the OS claims the click
            // as a window-drag before gpui ever hit-tests, so a bare button never
            // fires its `on_click`. `occlude()` gives each a BlockMouse hitbox so
            // hit-testing stops on the button. (No-op on macOS, where titlebar
            // dragging doesn't gate child hit-testing — which is why this worked
            // there and silently did nothing on Windows.)
            .child(
                div().occlude().flex_shrink_0().child(
                    self.attach_new_tab_menu(
                        // `chrome_tile`, not `ghost()`: this "+" sits beside the
                        // collapse tile and the title bar's own "+", and ghost's
                        // hover is a heavier, differently-derived grey.
                        crate::ui::tab_strip::chrome_tile_sized(
                            Button::new("sidebar-add").icon(Icon::new(IconName::Plus)),
                            crate::ui::app::TILE_SIZE,
                            crate::ui::app::TILE_GLYPH_LINE,
                            false,
                            cx,
                        )
                        .rounded_lg(),
                        cx,
                    ),
                ),
            )
            .child(
                div().occlude().flex_shrink_0().child(
                    crate::ui::tab_strip::chrome_tile(
                        Button::new("sidebar-collapse")
                            .icon(Icon::empty().path("icons/panel-left.svg")),
                        false,
                        cx,
                    )
                    .rounded_lg()
                    .tooltip("Hide Sidebar")
                    .on_click(cx.listener(|this, _, _window, cx| this.toggle_left_panel(cx))),
                ),
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
            .px(px(crate::ui::app::CONTENT_INSET))
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
            );

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
                    // so the rail reads as one panel from the very top edge. The
                    // rail's controls ride its right end, on the title bar's own
                    // center line — same row as the "⋯" across the window.
                    //
                    // The real `TitleBar` — which carries the window's drag region
                    // — only spans the *right* column in this layout, so this strip
                    // would be dead space you can't grab the window by. `title_bar_drag`
                    // makes the controls' row act like the bar it sits level with:
                    // drag to move, double-click to zoom, while the buttons on the
                    // right keep taking their own clicks (they're `occlude()`d).
                    .child(crate::ui::app::title_bar_drag(
                        controls.id("sidebar-titlebar-drag"),
                    ))
                    .child(top_bar)
                    .child(crate::ui::scrollbar::with_vertical_scrollbar(
                        "tab-sidebar-scrollbar",
                        list,
                        &self.sidebar_scroll,
                    )),
            )
            .child(handle)
    }

    /// Each tab's sidebar group key, in tab order: the *repository home* of
    /// its *first* pane's cwd (the main checkout's root — linked worktrees of
    /// one repo share a group), resolved through the tab's sticky
    /// `sidebar_group` cell — only a landed probe answer moves a tab (see the
    /// field's doc), so an in-flight cd never reshuffles the list. The first
    /// pane rather than the focused one (which the branch line follows), so
    /// switching focus between splits in different repos never relocates the
    /// row — the group answers "where does this tab live", not "what am I
    /// touching". `None` = the Scratch group. With grouping configured off
    /// every key is `None`, which collapses the list to one flat section and
    /// makes the same-group drop check a no-op.
    fn sidebar_group_keys(&self, cx: &gpui::App) -> Vec<Option<PathBuf>> {
        let grouping = cx.global::<Config>().sidebar_grouping == SidebarGrouping::Repo;
        self.tabs
            .iter()
            .map(|tab| {
                if !grouping {
                    return None;
                }
                let cwd = tab
                    .pane
                    .first_leaf()
                    .and_then(|leaf| leaf.read(cx).git_status_cwd().map(|p| p.to_path_buf()));
                if let Some(known) =
                    cwd.and_then(|cwd| cx.global::<GitStatusCache>().known_repo_for(&cwd))
                {
                    *tab.sidebar_group.borrow_mut() = known;
                }
                tab.sidebar_group.borrow().clone()
            })
            .collect()
    }

    /// Tab indices in the order the tab UI displays them: the grouped order
    /// when the vertical sidebar is grouping by repo, plain tab order in
    /// every other layout. ⌘N and the hint digits both go through this, so
    /// "press 3 for the third row you see" stays true under grouping.
    fn visual_tab_order(&self, cx: &gpui::App) -> Vec<usize> {
        if cx.global::<Config>().tab_bar_position != crate::core::config::TabBarPosition::Left {
            return (0..self.tabs.len()).collect();
        }
        let keys = self.sidebar_group_keys(cx);
        sidebar_sections(&keys)
            .into_iter()
            .flat_map(|s| s.tabs)
            .collect()
    }

    /// Activate the `n`-th tab *as displayed* (see
    /// [`visual_tab_order`](Self::visual_tab_order)) — the ⌘N actions land
    /// here so the shortcut always matches the digit badge on the row.
    pub(crate) fn activate_visual(
        &mut self,
        n: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(&i) = self.visual_tab_order(cx).get(n) {
            self.activate(i, window, cx);
        }
    }
}

/// One block of the sidebar: a header and the tabs under it.
#[derive(Debug, PartialEq)]
struct Section {
    /// The repo root this group is keyed on, `None` for Scratch. Sections with
    /// a key are the draggable ones (Scratch is pinned last), and it doubles as
    /// the group's identity in a drag.
    key: Option<PathBuf>,
    /// The header text, or `None` for "render flat, no header".
    name: Option<String>,
    /// The group's tabs, in tab order.
    tabs: Vec<usize>,
}

/// Partition per-tab group keys into the sidebar's sections: groups in
/// first-appearance order (a new repo appends, the existing ones never
/// reshuffle) with the Scratch group pinned last. A nameless single section
/// means "render flat, no headers" — used when no tab is in any repo, where a
/// lone Scratch header over everything would be noise.
fn sidebar_sections(keys: &[Option<PathBuf>]) -> Vec<Section> {
    let mut group_order: Vec<&PathBuf> = Vec::new();
    for k in keys.iter().flatten() {
        if !group_order.iter().any(|g| *g == k) {
            group_order.push(k);
        }
    }
    if group_order.is_empty() {
        return vec![Section {
            key: None,
            name: None,
            tabs: (0..keys.len()).collect(),
        }];
    }
    let names = group_names(&group_order);
    let mut sections: Vec<Section> = group_order
        .iter()
        .zip(names)
        .map(|(root, name)| Section {
            key: Some((*root).clone()),
            name: Some(name),
            tabs: (0..keys.len())
                .filter(|&i| keys[i].as_ref() == Some(*root))
                .collect(),
        })
        .collect();
    let scratch: Vec<usize> = (0..keys.len()).filter(|&i| keys[i].is_none()).collect();
    if !scratch.is_empty() {
        sections.push(Section {
            key: None,
            name: Some("Scratch".into()),
            tabs: scratch,
        });
    }
    sections
}

/// The tab permutation for a row dropped at a new place inside its own group:
/// `visible` are the group's rows as the rail currently lists them (the search
/// box may be hiding others), and the row at `from` lands where the row at `to`
/// is now.
///
/// Like [`regrouped_order`] this returns a whole-vector permutation rather than
/// a single move, because "third row in this group" only means something once
/// the vector is laid out the way the rail draws it: groups in their existing
/// order, each one contiguous, Scratch last. Rows the filter is hiding keep
/// their place in the group, and no other group is disturbed.
fn reordered_rows(
    keys: &[Option<PathBuf>],
    group: &Option<PathBuf>,
    visible: &[usize],
    from: usize,
    to: usize,
) -> Option<Vec<usize>> {
    let (&moved, &anchor) = (visible.get(from)?, visible.get(to)?);
    if moved == anchor {
        return None;
    }
    let mut members: Vec<usize> = (0..keys.len()).filter(|&i| keys[i] == *group).collect();
    members.retain(|&i| i != moved);
    // Land it on the far side of the row it was dropped onto, so dragging down
    // ends up below that row and dragging up above it.
    let at = members.iter().position(|&i| i == anchor)? + usize::from(to > from);
    members.insert(at, moved);

    let mut out: Vec<usize> = Vec::with_capacity(keys.len());
    for g in sidebar_sections(keys).iter().map(|s| &s.key) {
        if g == group {
            out.extend_from_slice(&members);
        } else {
            out.extend((0..keys.len()).filter(|&i| keys[i] == *g));
        }
    }
    Some(out)
}

/// The tab permutation that moves the group rooted at `from` into `to`'s slot,
/// as old indices in their new order — or `None` when the move is a no-op (same
/// group, or either root no longer has any tab).
///
/// Groups are ordered by first appearance in the tab vector, so the move is
/// "reorder the group list, then lay the tabs back out group by group". Each
/// group therefore comes out *contiguous*, with Scratch last, matching exactly
/// what the sidebar renders — which also settles the old caveat that a tab drag
/// inside an interleaved group could shuffle other groups' headers: after any
/// header drag the vector is compacted and interleaving is gone. Relative order
/// within a group is preserved.
fn regrouped_order(keys: &[Option<PathBuf>], from: &Path, to: &Path) -> Option<Vec<usize>> {
    if from == to {
        return None;
    }
    let mut order: Vec<&PathBuf> = Vec::new();
    for k in keys.iter().flatten() {
        if !order.iter().any(|g| *g == k) {
            order.push(k);
        }
    }
    let fi = order.iter().position(|g| g.as_path() == from)?;
    let ti = order.iter().position(|g| g.as_path() == to)?;
    let moved = order.remove(fi);
    order.insert(ti, moved);

    let mut out: Vec<usize> = Vec::with_capacity(keys.len());
    for g in &order {
        out.extend((0..keys.len()).filter(|&i| keys[i].as_ref() == Some(*g)));
    }
    // Scratch tabs trail the repo groups, which is where the sidebar draws them.
    out.extend((0..keys.len()).filter(|&i| keys[i].is_none()));
    Some(out)
}

/// Display names for the group roots: each root's directory name, extended
/// upward by parent components only while it collides with another root's
/// (`app` stays `app` on its own; two checkouts both named `app` become
/// `work/app` and `fork/app`). Distinct roots must differ somewhere, so the
/// loop settles; a root that exhausts its components while still colliding
/// just keeps its longest suffix.
fn group_names(roots: &[&PathBuf]) -> Vec<String> {
    // Only the normal components — no root-dir "/" entry, so a joined suffix
    // never renders as "//work/app".
    let comps: Vec<Vec<String>> = roots
        .iter()
        .map(|r| {
            r.components()
                .filter(|c| matches!(c, std::path::Component::Normal(_)))
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect()
        })
        .collect();
    let mut depth = vec![1usize; roots.len()];
    loop {
        let names: Vec<String> = comps
            .iter()
            .zip(&depth)
            .enumerate()
            .map(|(i, (c, &d))| {
                if c.is_empty() {
                    // Degenerate root with no normal components (e.g. "/").
                    roots[i].display().to_string()
                } else {
                    c[c.len().saturating_sub(d)..].join("/")
                }
            })
            .collect();
        let mut grew = false;
        for i in 0..names.len() {
            let collides = names
                .iter()
                .enumerate()
                .any(|(j, n)| j != i && *n == names[i]);
            if collides && depth[i] < comps[i].len() {
                depth[i] += 1;
                grew = true;
            }
        }
        if !grew {
            return names;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// Groups appear in first-appearance order with Scratch pinned last, and
    /// an all-`None` key set renders as one headerless flat section.
    #[test]
    fn sections_order_groups_by_first_appearance_scratch_last() {
        let keys = vec![
            Some(p("/w/beta")),
            None,
            Some(p("/w/alpha")),
            Some(p("/w/beta")),
        ];
        let sections = sidebar_sections(&keys);
        let shape: Vec<(Option<PathBuf>, Option<String>, Vec<usize>)> = sections
            .into_iter()
            .map(|s| (s.key, s.name, s.tabs))
            .collect();
        assert_eq!(
            shape,
            vec![
                (Some(p("/w/beta")), Some("beta".into()), vec![0, 3]),
                (Some(p("/w/alpha")), Some("alpha".into()), vec![2]),
                (None, Some("Scratch".into()), vec![1]),
            ]
        );

        // No tab in any repo: one headerless section over everything.
        let flat = sidebar_sections(&[None, None]);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].name, None);
        assert_eq!(flat[0].tabs, vec![0, 1]);
    }

    /// A row dropped inside its group lands on the far side of the row it was
    /// dropped onto, leaves every other group alone, and comes out with the
    /// groups laid out contiguously the way the rail draws them.
    #[test]
    fn reordered_rows_moves_within_the_group_only() {
        // alpha owns 0 and 2, interleaved with beta's 1; scratch is 3.
        let keys = vec![
            Some(p("/w/alpha")),
            Some(p("/w/beta")),
            Some(p("/w/alpha")),
            None,
        ];
        let alpha = Some(p("/w/alpha"));
        // alpha's first row dragged onto its second: alpha reads [2, 0], and
        // the vector comes out grouped — alpha, beta, scratch.
        assert_eq!(
            reordered_rows(&keys, &alpha, &[0, 2], 0, 1),
            Some(vec![2, 0, 1, 3])
        );
        // Back the other way.
        assert_eq!(
            reordered_rows(&keys, &alpha, &[0, 2], 1, 0),
            Some(vec![2, 0, 1, 3])
        );
        // Dropping a row on itself changes nothing.
        assert_eq!(reordered_rows(&keys, &alpha, &[0, 2], 1, 1), None);
    }

    /// Rows the search box is hiding aren't dragged along: the visible rows
    /// reorder among themselves and the hidden one keeps its place in the group.
    #[test]
    fn reordered_rows_leaves_filtered_out_rows_alone() {
        let keys = vec![Some(p("/w/a")), Some(p("/w/a")), Some(p("/w/a"))];
        let a = Some(p("/w/a"));
        // Only rows 0 and 2 are listed; dragging 0 past 2 puts it after row 2,
        // and row 1 stays between… where it was relative to the others.
        assert_eq!(
            reordered_rows(&keys, &a, &[0, 2], 0, 1),
            Some(vec![1, 2, 0])
        );
    }

    /// A header drag moves the whole group into the target's slot and lays
    /// every group out contiguously, Scratch last, keeping intra-group order.
    #[test]
    fn regrouped_order_moves_the_group_into_the_target_slot() {
        // Groups by first appearance: alpha (0, 3), beta (2), gamma (4).
        let keys = vec![
            Some(p("/w/alpha")),
            None,
            Some(p("/w/beta")),
            Some(p("/w/alpha")),
            Some(p("/w/gamma")),
        ];
        // gamma dropped on alpha → gamma, alpha, beta, then Scratch.
        assert_eq!(
            regrouped_order(&keys, &p("/w/gamma"), &p("/w/alpha")),
            Some(vec![4, 0, 3, 2, 1])
        );
        // alpha dropped on gamma (a move down) → beta, gamma, alpha.
        assert_eq!(
            regrouped_order(&keys, &p("/w/alpha"), &p("/w/gamma")),
            Some(vec![2, 4, 0, 3, 1])
        );
    }

    /// Dropping a group on itself, or naming a root no tab lives in, is a
    /// no-op rather than a re-shuffle.
    #[test]
    fn regrouped_order_ignores_self_and_unknown_roots() {
        let keys = vec![Some(p("/w/alpha")), Some(p("/w/beta"))];
        assert_eq!(regrouped_order(&keys, &p("/w/alpha"), &p("/w/alpha")), None);
        assert_eq!(regrouped_order(&keys, &p("/w/gone"), &p("/w/beta")), None);
        assert_eq!(regrouped_order(&keys, &p("/w/alpha"), &p("/w/gone")), None);
    }

    /// Same-named roots grow a parent prefix until distinct; unrelated names
    /// stay short even alongside the colliding pair.
    #[test]
    fn group_names_disambiguate_only_the_collisions() {
        let (a, b, c) = (
            p("/home/u/work/app"),
            p("/home/u/fork/app"),
            p("/home/u/tty7"),
        );
        let names = group_names(&[&a, &b, &c]);
        assert_eq!(names, vec!["work/app", "fork/app", "tty7"]);
    }

    /// A root that runs out of components keeps its longest suffix instead of
    /// looping forever, and the other side still grows past it to distinctness.
    #[test]
    fn group_names_handle_suffix_roots() {
        let (short, long) = (p("/app"), p("/x/app"));
        let names = group_names(&[&short, &long]);
        assert_eq!(names, vec!["app", "x/app"]);
    }
}

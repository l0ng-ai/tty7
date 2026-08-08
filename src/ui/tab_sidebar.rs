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
use crate::ui::i18n::{L10nKey, t};
use crate::ui::reorder::{self, Reorder, Surface};
use crate::ui::tab_strip::{DragTab, REORDER_SLIDE_MS};

const MIN_SIDEBAR_WIDTH: f32 = 180.;

const GRAB_HANDLE_W: f32 = 48.;
const MAX_SIDEBAR_WIDTH_RATIO: f32 = 0.5;

const RESIZE_HANDLE_WIDTH: f32 = 8.;

const ROW_GAP: f32 = 2.;

#[derive(Clone)]
pub(crate) struct DragGroup;

impl Render for DragGroup {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl Tty7App {
    pub(crate) fn tab_sidebar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let active = self.active;
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let show_badges = self.mod_hint_badges;
        let max_width = (window.viewport_size().width.as_f32() * MAX_SIDEBAR_WIDTH_RATIO)
            .max(MIN_SIDEBAR_WIDTH);
        let width = self.sidebar_width.get().clamp(MIN_SIDEBAR_WIDTH, max_width);
        let query = self.sidebar_search.read(cx).value().trim().to_lowercase();

        let mut list = v_flex()
            .id("tab-sidebar-list")
            .track_scroll(&self.sidebar_scroll)
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_1()
            .py_1p5()
            .gap_0p5();

        let keys: Rc<Vec<Option<PathBuf>>> = Rc::new(self.sidebar_group_keys(cx));
        let sections = sidebar_sections(&keys);

        let badge_pos: Vec<usize> = {
            let mut pos = vec![0usize; self.tabs.len()];
            for (n, i) in sections.iter().flat_map(|s| s.tabs.iter()).enumerate() {
                pos[*i] = n;
            }
            pos
        };

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
                if let (Some(from), Some(to)) = (repo_roots.get(p.from), repo_roots.get(p.target))
                    && let Some(order) = regrouped_order(&keys, from, to)
                {
                    reorder::set_pending(&self.reorder, &Surface::SidebarGroups, order);
                }
                p.order.clone()
            }
            None => (0..repo_groups).collect(),
        };
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
            let mut rows: Vec<ContextMenu<Stateful<Div>>> = Vec::new();
            let visible = visible_by_section[group_ix].clone();
            let visible_tabs: Vec<usize> = visible.iter().map(|(i, _)| *i).collect();
            let row_slots: Rc<RefCell<Vec<Bounds<Pixels>>>> =
                Rc::new(RefCell::new(vec![Bounds::default(); visible.len()]));
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
                let ssh_dot = self.tab_ssh_dot(tab, cx);
                let agent = tab.agent(cx);
                let agent_status = tab.agent_status(cx);
                let agent_unread = tab.agent_unread_count(cx);
                let git_cwd = diff_click_cwd(
                    cx.global::<Config>(),
                    tab.pane.focused_or_first(window, cx).and_then(|leaf| {
                        let view = leaf.read(cx);
                        let cwd = view.git_status_cwd()?.to_path_buf();
                        Some((view.host_id(), cwd))
                    }),
                );
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
                        .child(div().flex_1().min_w_0().truncate().child(g.branch.clone()));
                    if g.added > 0 || g.removed > 0 {
                        let mut counts = h_flex()
                            .id(("sidebar-diff", i))
                            .flex_shrink_0()
                            .items_center()
                            .gap_1p5()
                            .when_some(git_cwd, |counts, (host, cwd)| {
                                counts.cursor_pointer().on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                        cx.stop_propagation();
                                        this.toggle_diff_overlay(host, cwd.clone(), window, cx);
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
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(Input::new(&input).appearance(false))
                        .into_any_element(),
                    None => v_flex()
                        .id(("sidebar-label", i))
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(
                            div()
                                .w_full()
                                .truncate()
                                .text_sm()
                                .when(is_active, |d| d.font_weight(FontWeight::MEDIUM))
                                .child(label),
                        )
                        .children(git_line)
                        .into_any_element(),
                };

                let row = h_flex()
                    .id(("tab-row", i))
                    .group(SharedString::from(format!("tab-row-{i}")))
                    .cursor_pointer()
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
                    .py_1()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .pl_2()
                    .pr_2()
                    .rounded_lg()
                    .when(is_active, |s| {
                        s.bg(cx.theme().sidebar_accent)
                            .text_color(cx.theme().sidebar_accent_foreground)
                    })
                    .when(!is_active, |s| {
                        s.text_color(cx.theme().sidebar_foreground)
                            .hover(|s| s.bg(gpui::rgb(sf.hover)))
                    })
                    .when(row_preview.as_ref().is_some_and(|p| p.from == slot), |s| {
                        s.opacity(0.75)
                    })
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
                        .absolute()
                        .inset_0(),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.activate(i, window, cx);
                        }),
                    )
                    .child(self.tab_avatar(agent, agent_status, agent_unread, ssh_dot, 22., cx))
                    .child(label_region)
                    .when(show_badges && badge_pos < 9, |row| {
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
                    .when(!(show_badges && badge_pos < 9), |row| {
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
                    Some(p) if p.from == slot => deferred(
                        rows[slot]
                            .take()
                            .expect("each slot emitted once")
                            .relative()
                            .top(p.held),
                    )
                    .into_any_element(),
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
                                    grab,
                                ));
                                cx.new(|_| DragGroup)
                            }
                        })
                    })
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

            let block = v_flex()
                .w_full()
                .gap(px(ROW_GAP))
                .when(
                    group_preview
                        .as_ref()
                        .is_some_and(|p| Some(p.from) == group_slot),
                    |b| b.opacity(0.75),
                )
                .children(header)
                .children(rows)
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
                (Some(p), Some(slot)) if p.from == slot => {
                    deferred(block.relative().top(p.held)).into_any_element()
                }
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

        let controls = h_flex()
            .flex_shrink_0()
            .h(px(TITLE_BAR_HEIGHT))
            .border_b_1()
            .border_color(cx.theme().transparent)
            .items_center()
            .justify_end()
            .gap(px(2.))
            .pr(px(crate::ui::app::tile_trailing_inset()))
            .when_some(crate::ui::app::window_mark(), |row, mark| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .pl(px(crate::ui::app::CONTENT_INSET))
                        .child(mark),
                )
                .child(div().flex_1().min_w(px(GRAB_HANDLE_W)))
            })
            .child(
                div().occlude().flex_shrink_0().child(
                    self.attach_new_tab_menu(
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
                    .tooltip(t(L10nKey::TabTooltipHideSidebar))
                    .on_click(cx.listener(|this, _, _window, cx| this.toggle_left_panel(cx))),
                ),
            );
        let workspace_head = h_flex()
            .flex_shrink_0()
            .px(px(crate::ui::app::CONTENT_INSET - 7.))
            .pt(px(4.))
            .child(self.workspace_head(cx));

        let chip_inset = crate::ui::app::CONTENT_INSET - 7. + 4.;
        let top_bar = h_flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(6.))
            .h(px(44.))
            .pl(px(chip_inset))
            .pr(px(crate::ui::app::CONTENT_INSET))
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(Self::AVATAR_PX))
                    .child(
                        Icon::new(IconName::Search)
                            .size(px(14.))
                            .text_color(cx.theme().muted_foreground),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.sidebar_search).appearance(false).pl_0()),
            );

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

        let handle_active = self.sidebar_dragging.get();
        let handle = div()
            .group("sidebar-resize")
            .occlude()
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
            .bg(crate::ui::theme::workspace_surface_color(cx))
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .child(backing)
            .child(
                v_flex()
                    .size_full()
                    .child(crate::ui::app::title_bar_drag(
                        controls.id("sidebar-titlebar-drag"),
                        "sidebar-titlebar-drag",
                        window,
                        cx,
                    ))
                    .child(workspace_head)
                    .child(top_bar)
                    .child(crate::ui::scrollbar::with_vertical_scrollbar(
                        "tab-sidebar-scrollbar",
                        list,
                        &self.sidebar_scroll,
                    )),
            )
            .child(handle)
    }

    fn sidebar_group_keys(&self, cx: &gpui::App) -> Vec<Option<PathBuf>> {
        let grouping = cx.global::<Config>().sidebar_grouping == SidebarGrouping::Repo;
        self.tabs
            .iter()
            .map(|tab| {
                if !grouping {
                    return None;
                }
                let cwd = tab.pane.first_leaf().and_then(|leaf| {
                    let view = leaf.terminal()?.read(cx);
                    Some((view.host_id(), view.git_status_cwd()?.to_path_buf()))
                });
                if let Some(known) =
                    cwd.and_then(|(id, cwd)| cx.global::<GitStatusCache>().known_repo_for(id, &cwd))
                {
                    *tab.sidebar_group.borrow_mut() = known;
                }
                tab.sidebar_group.borrow().clone()
            })
            .collect()
    }

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

    /// Which group a tab about to be spawned in `cwd` belongs to, when the
    /// repo probe for that directory has already landed. `Some(None)` means
    /// the cache knows it is not a repo; a bare `None` means it never looked.
    ///
    /// A tab's group otherwise starts empty and only fills in once its shell
    /// has started and reported a cwd, which parks every new tab in the
    /// scratch group at the bottom of the sidebar until then. A tab spawned
    /// from one already sitting in a repo inherits a warm cache, so seeding
    /// it here lands the tab in its group on the first frame.
    pub(crate) fn spawn_group(
        &self,
        cwd: Option<&Path>,
        cx: &gpui::App,
    ) -> Option<Option<PathBuf>> {
        let host = self
            .window_workspace(cx)
            .as_ref()
            .map_or(crate::ui::host_ops::HostId::LOCAL, |ws| ws.target.host_id());
        cx.try_global::<GitStatusCache>()?
            .known_repo_for(host, cwd?)
    }
}

#[derive(Debug, PartialEq)]
struct Section {
    key: Option<PathBuf>,
    name: Option<String>,
    tabs: Vec<usize>,
}

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
            name: Some(t(L10nKey::SidebarScratchGroup).to_string()),
            tabs: scratch,
        });
    }
    sections
}

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
    out.extend((0..keys.len()).filter(|&i| keys[i].is_none()));
    Some(out)
}

fn group_names(roots: &[&PathBuf]) -> Vec<String> {
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

fn diff_click_cwd<T>(cfg: &Config, target: Option<T>) -> Option<T> {
    cfg.sidebar_diff_preview.then_some(target).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn diff_preview_setting_gates_the_click_target() {
        let mut cfg = Config::default();
        assert!(cfg.sidebar_diff_preview, "default is today's behaviour");
        assert_eq!(
            diff_click_cwd(&cfg, Some(p("/w/repo"))),
            Some(p("/w/repo")),
            "enabled: the counts are a click target"
        );

        cfg.sidebar_diff_preview = false;
        assert_eq!(
            diff_click_cwd(&cfg, Some(p("/w/repo"))),
            None,
            "disabled: no cwd, so no cursor and no toggle_diff_overlay"
        );
    }

    #[test]
    fn diff_click_target_needs_a_repo_either_way() {
        let mut cfg = Config::default();
        assert_eq!(diff_click_cwd::<PathBuf>(&cfg, None), None);
        cfg.sidebar_diff_preview = false;
        assert_eq!(diff_click_cwd::<PathBuf>(&cfg, None), None);
    }

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

        let flat = sidebar_sections(&[None, None]);
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].name, None);
        assert_eq!(flat[0].tabs, vec![0, 1]);
    }

    #[test]
    fn reordered_rows_moves_within_the_group_only() {
        let keys = vec![
            Some(p("/w/alpha")),
            Some(p("/w/beta")),
            Some(p("/w/alpha")),
            None,
        ];
        let alpha = Some(p("/w/alpha"));
        assert_eq!(
            reordered_rows(&keys, &alpha, &[0, 2], 0, 1),
            Some(vec![2, 0, 1, 3])
        );
        assert_eq!(
            reordered_rows(&keys, &alpha, &[0, 2], 1, 0),
            Some(vec![2, 0, 1, 3])
        );
        assert_eq!(reordered_rows(&keys, &alpha, &[0, 2], 1, 1), None);
    }

    #[test]
    fn reordered_rows_leaves_filtered_out_rows_alone() {
        let keys = vec![Some(p("/w/a")), Some(p("/w/a")), Some(p("/w/a"))];
        let a = Some(p("/w/a"));
        assert_eq!(
            reordered_rows(&keys, &a, &[0, 2], 0, 1),
            Some(vec![1, 2, 0])
        );
    }

    #[test]
    fn regrouped_order_moves_the_group_into_the_target_slot() {
        let keys = vec![
            Some(p("/w/alpha")),
            None,
            Some(p("/w/beta")),
            Some(p("/w/alpha")),
            Some(p("/w/gamma")),
        ];
        assert_eq!(
            regrouped_order(&keys, &p("/w/gamma"), &p("/w/alpha")),
            Some(vec![4, 0, 3, 2, 1])
        );
        assert_eq!(
            regrouped_order(&keys, &p("/w/alpha"), &p("/w/gamma")),
            Some(vec![2, 4, 0, 3, 1])
        );
    }

    #[test]
    fn regrouped_order_ignores_self_and_unknown_roots() {
        let keys = vec![Some(p("/w/alpha")), Some(p("/w/beta"))];
        assert_eq!(regrouped_order(&keys, &p("/w/alpha"), &p("/w/alpha")), None);
        assert_eq!(regrouped_order(&keys, &p("/w/gone"), &p("/w/beta")), None);
        assert_eq!(regrouped_order(&keys, &p("/w/alpha"), &p("/w/gone")), None);
    }

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

    #[test]
    fn group_names_handle_suffix_roots() {
        let (short, long) = (p("/app"), p("/x/app"));
        let names = group_names(&[&short, &long]);
        assert_eq!(names, vec!["app", "x/app"]);
    }
}

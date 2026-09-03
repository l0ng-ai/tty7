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

use tty7_core::core::machine::ProjectId;

use crate::core::config::{Config, SidebarGrouping};
use crate::terminal::git_status::GitStatusCache;
use crate::ui::app::{TITLE_BAR_HEIGHT, Tty7App};
use crate::ui::hints::tab_badge_label;
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::reorder::{self, Reorder, Surface};
use crate::ui::right_panel::RESIZE_HANDLE_WIDTH;
use crate::ui::tab_strip::{
    DragTab, REORDER_SLIDE_MS, abbreviate_home, elide_keep_edges, elide_label,
    elide_path_keep_tail, measure_text, strip_host_prefix,
};

pub(crate) const MIN_SIDEBAR_WIDTH: f32 = 180.;

const GRAB_HANDLE_W: f32 = 48.;

/// The hover group the whole rail belongs to, so the two add buttons can wait
/// until the pointer is somewhere in the sidebar before showing themselves.
const SIDEBAR_GROUP: &str = "tab-sidebar";

const ROW_GAP: f32 = 2.;

/// The row chrome the text budget has to be measured around. These are the
/// numbers the layout below is built from, not a second guess at it — a row
/// that elides against a budget wider than it really has falls back to CSS
/// truncation, which drops the tail this whole module exists to keep.
mod row_metrics {
    /// `border_r_1` on the sidebar itself.
    pub(super) const BORDER: f32 = 1.;
    /// `px_1` on the scrolling list that holds the rows.
    pub(super) const LIST_PAD: f32 = 4.;
    /// `pl_2` + `pr_2` on the row.
    pub(super) const ROW_PAD: f32 = 8.;
    /// The avatar handed to `tab_avatar`.
    pub(super) const AVATAR: f32 = 22.;
    /// `gap_2` between the row's children.
    pub(super) const GAP: f32 = 8.;
    /// The ⌘N badge, when one is shown.
    pub(super) const BADGE: f32 = 20.;
    /// `gap_1p5`, between the branch icon and its text and before the counts.
    pub(super) const META_GAP: f32 = 6.;
    /// The branch icon.
    pub(super) const BRANCH_ICON: f32 = 11.;

    /// What a row can spend on text, before the badge is taken out.
    pub(super) const fn text_budget(width: f32) -> f32 {
        width - BORDER - 2. * LIST_PAD - 2. * ROW_PAD - AVATAR - GAP
    }
}

/// What a sidebar row rendered, next to what it had to leave out, so the
/// hover card can be built by comparison instead of deriving the same strings
/// a second time — the two derivations have to agree, and the shortest way to
/// guarantee that is to only ever have one.
struct SidebarRowShown {
    /// The elided title, and the full string it came from. `None` when the
    /// row is showing a placeholder (`Shell 3`) rather than a real title,
    /// which nothing can expand.
    title: Option<(SharedString, SharedString)>,
    branch: Option<(SharedString, SharedString, u32, u32)>,
    cwd: Option<(SharedString, SharedString)>,
}

#[derive(Clone)]
pub(crate) struct DragGroup;

impl Render for DragGroup {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Every detail a sidebar row could not fit, collected so the hover card can
/// be rendered from cloneable data (an `AnyElement` cannot be cloned, but the
/// tooltip closure has to rebuild its content on every hover).
#[derive(Clone)]
struct SidebarInfo {
    /// Full path, when the row's title was elided.
    title: Option<SharedString>,
    /// Full branch plus diff counts, when the row's branch was elided.
    branch: Option<(SharedString, u32, u32)>,
    /// Full working directory, when the row's second line was elided.
    cwd: Option<SharedString>,
    /// Remote host, when the avatar only shows a dot for it.
    host: Option<SharedString>,
}

impl Tty7App {
    /// Whether the tab rail is on screen — the same three conditions `render`
    /// assembles the layout from, in one place the panel opposite can ask.
    pub(crate) fn sidebar_open(&self, cx: &gpui::App) -> bool {
        cx.global::<Config>().tab_bar_position == crate::core::config::TabBarPosition::Left
            && !self.tabs.is_empty()
            && !self.sidebar_collapsed
    }

    /// What the right panel has reserved, from the sidebar's point of view.
    pub(crate) fn right_panel_floor(&self, cx: &gpui::App) -> f32 {
        if self.right_panel_open(cx) {
            crate::ui::right_panel::MIN_WIDTH
        } else {
            0.
        }
    }

    pub(crate) fn sidebar_max_px(&self, window: &Window, cx: &gpui::App) -> f32 {
        crate::ui::app::side_panel_max(
            window.viewport_size().width.as_f32(),
            MIN_SIDEBAR_WIDTH,
            self.right_panel_floor(cx) + self.document_floor(cx),
        )
    }

    /// How wide the sidebar is drawn, given the live cell and the cap the rest
    /// of the window leaves it. Read here rather than clamped at each caller so
    /// the document column's budget and the sidebar itself can never disagree
    /// about how much width is already spoken for.
    pub(crate) fn sidebar_px(&self, window: &Window, cx: &gpui::App) -> f32 {
        self.sidebar_width
            .get()
            .clamp(MIN_SIDEBAR_WIDTH, self.sidebar_max_px(window, cx))
    }

    pub(crate) fn tab_sidebar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let active = self.active;
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let show_badges = self.mod_hint_badges;
        let width = self.sidebar_px(window, cx);
        let query = self.sidebar_search.read(cx).value().trim().to_lowercase();
        // Blanked here, written again from paint: a row filtered out by the
        // search — or hidden with its collapsed group — must leave no rectangle
        // behind for a pane to be dropped between.
        *self.sidebar_slots.borrow_mut() = vec![Bounds::default(); self.tabs.len()];
        // Every group drops itself when its rows filter out, so a query that
        // matches nothing left the sidebar showing only its own search box.
        let mut any_rows = false;

        let mut list = v_flex()
            .id("tab-sidebar-list")
            .track_scroll(&self.sidebar_scroll)
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .px_1()
            .py_1p5()
            .gap_0p5();

        let keys: Rc<Vec<Option<PathBuf>>> = Rc::new(self.sidebar_group_keys(cx));
        let members: Vec<Option<ProjectId>> = self.tabs.iter().map(|t| t.project.get()).collect();
        let sections = Rc::new(sidebar_sections(&keys, &members, &self.named_projects()));

        // ⌘N runs ActivateTabN, which goes through `activate_visual` — the
        // Nth row as the sidebar lays it out, not the Nth tab in `self.tabs`.
        // The badge has to be read off the same order or it names a chord that
        // opens a different tab, so take it from `visual_tab_order` rather than
        // flattening `sections` a second time here.
        //
        let badge_pos = badge_positions(&self.visual_tab_order(cx), self.tabs.len());

        // The row shows an elided title and a branch; the filter used to read
        // only the elided title, so typing the branch you can see, or the part
        // of the path the row dropped, matched nothing. The label is built
        // here only when there is a query to match it against — the rows
        // themselves elide against measured width and no longer need it.
        let visible_by_section: Vec<Vec<usize>> = sections
            .iter()
            .map(|s| {
                s.tabs
                    .iter()
                    .copied()
                    .filter(|&i| {
                        query.is_empty()
                            || self
                                .tab_label(&self.tabs[i], i, Some(window), cx)
                                .to_lowercase()
                                .contains(&query)
                            || self.tabs[i]
                                .leaf_title(Some(window), cx)
                                .to_lowercase()
                                .contains(&query)
                            || self.tabs[i]
                                .git_status(Some(window), cx)
                                .is_some_and(|g| g.branch.to_lowercase().contains(&query))
                    })
                    .collect()
            })
            .collect();

        let pointer = window.mouse_position();
        // The row text is measured against real glyphs before it is elided:
        // `text_sm` is 0.875rem and `text_xs` 0.75rem, resolved here so the
        // measurement and the render use the same sizes and family.
        let font = gpui::Font {
            family: cx.theme().font_family.clone(),
            features: Default::default(),
            fallbacks: None,
            weight: Default::default(),
            style: Default::default(),
        };
        // The active row renders its title at `FontWeight::MEDIUM`, which is
        // wider than the regular weight in any proportional face. Measuring
        // it as regular would let the one row the user is looking at overflow
        // into the truncation this is here to avoid.
        let title_font_active = gpui::Font {
            weight: FontWeight::MEDIUM,
            ..font.clone()
        };
        let rem = window.rem_size().as_f32();
        let rendered = |ix: &usize| !visible_by_section[*ix].is_empty();
        // A project is on screen because someone declared it, not because a
        // tab landed in it, so an empty one still draws its header — that row
        // is where its first tab comes from. Under a live search it drops out
        // like anything else that matched nothing.
        let project_rendered = |ix: &usize| query.is_empty() || !visible_by_section[*ix].is_empty();
        let repo_slots: Vec<usize> = (0..sections.len())
            .filter(|&ix| sections[ix].key.repo().is_some())
            .filter(rendered)
            .collect();
        let repo_groups = repo_slots.len();
        let group_slots: Rc<RefCell<Vec<Bounds<Pixels>>>> =
            Rc::new(RefCell::new(vec![Bounds::default(); repo_groups]));
        let group_preview =
            reorder::preview(&self.reorder, &Surface::SidebarGroups, repo_groups, pointer);
        let slot_display: Vec<usize> = match &group_preview {
            Some(p) => {
                if let (Some(&from), Some(&to)) = (repo_slots.get(p.from), repo_slots.get(p.target))
                    && let Some(order) =
                        regrouped_order(&sections, &sections[from].key, &sections[to].key)
                {
                    reorder::set_pending(&self.reorder, &Surface::SidebarGroups, order);
                }
                p.order.clone()
            }
            None => (0..repo_groups).collect(),
        };
        let mut blocks: Vec<(Option<usize>, usize)> = (0..sections.len())
            .filter(|&ix| sections[ix].key.project().is_some())
            .filter(project_rendered)
            .map(|ix| (None, ix))
            .collect();
        blocks.extend(
            slot_display
                .into_iter()
                .map(|slot| (Some(slot), repo_slots[slot])),
        );
        blocks.extend(
            (0..sections.len())
                .filter(|&ix| sections[ix].key == SectionKey::Scratch)
                .filter(rendered)
                .map(|ix| (None, ix)),
        );

        let projects_shown = blocks
            .iter()
            .filter(|(_, ix)| sections[*ix].key.project().is_some())
            .count();
        // The two headings are what say which half of the rail you are
        // looking at: above the second one everything was declared, below it
        // everything was derived. Both are drawn whether or not anything is
        // under them — an empty PROJECTS is the invitation to declare one, and
        // TABS carries the new-tab button.
        //
        // A live search hides them — the query is asking about tabs, and
        // chrome that always matches would keep "nothing matches" from ever
        // showing.
        let headings = query.is_empty();
        let projects_folded = self.is_folded(&SidebarFold::Projects);
        let tabs_folded = self.is_folded(&SidebarFold::Tabs);
        if headings {
            list = list.child(self.projects_heading(cx));
        }

        let block_count = blocks.len();
        for (block_at, (group_slot, group_ix)) in blocks.into_iter().enumerate() {
            if headings && block_at == projects_shown {
                list = list.child(self.tabs_heading(projects_shown > 0 && !projects_folded, cx));
            }
            let section = &sections[group_ix];
            let group_key = section.key.clone();
            let project = group_key.project();
            // A folded heading keeps its own half off the rail entirely,
            // headers and all. Only a heading can do that; folding one block
            // leaves its header behind, because that header is the way back.
            let half_folded = match project {
                Some(_) => projects_folded,
                None => tabs_folded,
            };
            if headings && half_folded {
                continue;
            }
            let folded = self.is_folded(&SidebarFold::Section(group_key.clone()));
            // A folded block draws its header and nothing else, so the rows
            // are never built — and never write a rectangle a pane could be
            // dropped onto, which is what `sidebar_slots` being blanked every
            // frame is for.
            let mut rows: Vec<ContextMenu<Stateful<Div>>> = Vec::new();
            let visible = if folded {
                Vec::new()
            } else {
                visible_by_section[group_ix].clone()
            };
            let visible_tabs: Vec<usize> = visible.clone();
            let row_slots: Rc<RefCell<Vec<Bounds<Pixels>>>> =
                Rc::new(RefCell::new(vec![Bounds::default(); visible.len()]));
            let row_preview = reorder::preview(
                &self.reorder,
                &Surface::SidebarRows(group_key.clone()),
                visible.len(),
                pointer,
            );
            for (slot, i) in visible.into_iter().enumerate() {
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
                // The number this row actually wears, or `None` for no badge:
                // hints turned off, past ⌘9, or a row the chord order left out.
                let badge_n = badge_pos.filter(|_| show_badges).filter(|&n| n < 9);
                let badge_extra = if badge_n.is_some() {
                    row_metrics::BADGE + row_metrics::GAP
                } else {
                    0.
                };
                // Elision is measured against this budget so the label and
                // branch never wrap or overflow into CSS truncation.
                let label_avail = (row_metrics::text_budget(width) - badge_extra).max(48.);
                let title_size = 0.875 * rem;
                let meta_size = 0.75 * rem;
                let title_font = if is_active { &title_font_active } else { &font };
                // Title: elide the *full* label against the row budget, so a
                // wide sidebar shows the whole thing and a narrow one keeps
                // whichever end identifies it — the tail for a path, both
                // edges for anything else. A fixed segment cap
                // (`short_title`) would elide even when the row has room, so
                // only the width may decide here.
                //
                // `full_title` is the unelided string the card can expand
                // back to; `None` means the row is showing a placeholder that
                // no card can improve on.
                let (shown_title, full_title) =
                    if let Some(name) = tab.name.as_ref().filter(|n| !n.trim().is_empty()) {
                        // A renamed tab is elided like anything else — and so
                        // the card has to be able to spell the name back out.
                        let full = SharedString::from(name.trim().to_string());
                        let shown = elide_label(
                            &window.text_system(),
                            title_font,
                            title_size,
                            &full,
                            label_avail,
                        );
                        (shown, Some(full))
                    } else {
                        let (raw_title, home) = tab.leaf_title_and_home(Some(window), cx);
                        let title = strip_host_prefix(raw_title.trim());
                        let raw = abbreviate_home(title, home.as_deref());
                        if raw.trim().is_empty() {
                            // Nothing to expand: the row is naming an unnamed
                            // shell, not hiding a title behind an ellipsis.
                            let placeholder = SharedString::from(t_fmt(
                                L10nKey::TabUnnamedShell,
                                &[("n", &((i + 1).to_string()))],
                            ));
                            (placeholder, None)
                        } else {
                            let full = SharedString::from(raw.as_ref());
                            let shown = elide_label(
                                &window.text_system(),
                                title_font,
                                title_size,
                                &full,
                                label_avail,
                            );
                            (shown, Some(full))
                        }
                    };
                let mut branch_shown: Option<(SharedString, SharedString, u32, u32)> = None;
                let mut cwd_shown: Option<(SharedString, SharedString)> = None;
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
                                .size(px(row_metrics::BRANCH_ICON))
                                .text_color(cx.theme().muted_foreground),
                        );
                    // The diff counts are measured against real glyphs so the
                    // branch can be elided to exactly the space they leave;
                    // the counts themselves never wrap or shrink. They render
                    // as two children of a `gap_1p5` row, so the gap between
                    // them is measured rather than a space that stands in for
                    // it.
                    let mut counts_w = 0.;
                    if g.added > 0 {
                        counts_w += measure_text(
                            &window.text_system(),
                            &font,
                            meta_size,
                            &format!("+{}", g.added),
                        );
                    }
                    if g.removed > 0 {
                        counts_w += measure_text(
                            &window.text_system(),
                            &font,
                            meta_size,
                            &format!("−{}", g.removed),
                        );
                    }
                    if g.added > 0 && g.removed > 0 {
                        counts_w += row_metrics::META_GAP;
                    }
                    if counts_w > 0. {
                        // The gap between the branch and the counts.
                        counts_w += row_metrics::META_GAP;
                    }
                    // Branch: keep both ends (`window-…backdrop`) so its
                    // identifying tail survives a narrow sidebar.
                    let branch_avail =
                        (label_avail - row_metrics::BRANCH_ICON - row_metrics::META_GAP - counts_w)
                            .max(0.);
                    let shown = elide_keep_edges(
                        &window.text_system(),
                        &font,
                        meta_size,
                        &g.branch,
                        branch_avail,
                    );
                    branch_shown = Some((
                        shown.clone(),
                        SharedString::from(g.branch.clone()),
                        g.added,
                        g.removed,
                    ));
                    line = line.child(div().flex_1().min_w_0().truncate().child(shown));
                    if g.added > 0 || g.removed > 0 {
                        let mut counts = h_flex()
                            .id(("sidebar-diff", i))
                            .flex_shrink_0()
                            .items_center()
                            .gap_1p5()
                            .when_some(git_cwd, |counts, (host, cwd)| {
                                // A click target inside a click target: the row
                                // highlights as a whole, which says nothing
                                // about the counts being their own button. The
                                // underline the SFTP breadcrumb uses for
                                // clickable text says where this one starts.
                                counts
                                    .cursor_pointer()
                                    .hover(|s| s.underline())
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                            cx.stop_propagation();
                                            // Swallowing the press also swallows the
                                            // row's click, the only thing that
                                            // activates a tab — so this row has to
                                            // activate itself, or the overlay lands
                                            // in whichever tab was already on
                                            // screen, carrying this row's repo (#706).
                                            this.activate(i, window, cx);
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
                // Outside a repo there is no branch line; the second line then
                // carries the compressed cwd with its root marker, so a tab
                // whose title is just a shell name still says where it lives.
                if git_line.is_none() {
                    cwd_shown = tab
                        .pane
                        .focused_or_first(window, cx)
                        .and_then(|leaf| {
                            let leaf = leaf.read(cx);
                            Some((leaf.effective_cwd()?, leaf.display_home(cx)))
                        })
                        .map(|(cwd, home)| {
                            let text = cwd.display().to_string();
                            let full = SharedString::from(
                                abbreviate_home(&text, home.as_deref()).into_owned(),
                            );
                            let shown = elide_path_keep_tail(
                                &window.text_system(),
                                &font,
                                meta_size,
                                &full,
                                label_avail,
                            );
                            (shown, full)
                        })
                        // The title already carries the whole path; a second
                        // copy adds noise, not information.
                        .filter(|(shown, _)| shown.as_ref() != shown_title.as_ref());
                }
                let rename_input = self
                    .renaming
                    .as_ref()
                    .filter(|r| r.tab == tab.tree_id.get())
                    .map(|r| r.input.clone());

                let shown = SidebarRowShown {
                    title: full_title.map(|full| (shown_title.clone(), full)),
                    branch: branch_shown.clone(),
                    cwd: cwd_shown.clone(),
                };
                let info = self.sidebar_info(tab, window, cx, &shown);
                // Colors are captured by value so the tooltip builder (which
                // borrows no app state) can style the card on its own.
                let muted = cx.theme().muted_foreground;
                let success = cx.theme().success;
                let danger = cx.theme().danger;

                let label_region = match rename_input {
                    Some(input) => div()
                        .id(("sidebar-rename", i))
                        .flex_1()
                        .min_w_0()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        // The row switches tabs on the *release* now, so
                        // holding the press back is no longer enough: a click
                        // landing in the field would reach the row behind it
                        // and switch away from the name being typed, taking
                        // the focus with it.
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(Input::new(&input).appearance(false))
                        .into_any_element(),
                    None => v_flex()
                        .id(("sidebar-label", i))
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .when_some(info, |col, info| {
                            col.tooltip(move |window, cx| {
                                // `Tooltip::element` rebuilds its content on
                                // every hover, so the captured info is cloned
                                // per call instead of being moved out.
                                let info = info.clone();
                                gpui_component::tooltip::Tooltip::element(move |_window, _cx| {
                                    let card = v_flex()
                                        .gap_1()
                                        // The card is the one place that
                                        // promised the whole string, so a long
                                        // path wraps here rather than being
                                        // truncated a second time.
                                        .when_some(info.title.clone(), |c, title| {
                                            c.child(
                                                div()
                                                    .max_w(px(420.))
                                                    .text_sm()
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .child(title),
                                            )
                                        })
                                        .when_some(
                                            info.branch.clone(),
                                            |c, (branch, added, removed)| {
                                                let mut line = h_flex()
                                                    .items_center()
                                                    .gap_1p5()
                                                    .text_xs()
                                                    .text_color(muted)
                                                    .child(
                                                        gpui::svg()
                                                            .path("icons/git-branch.svg")
                                                            .flex_shrink_0()
                                                            .size(px(11.))
                                                            .text_color(muted),
                                                    )
                                                    .child(div().child(branch));
                                                if added > 0 {
                                                    line = line.child(
                                                        div()
                                                            .text_color(success)
                                                            .child(format!("+{added}")),
                                                    );
                                                }
                                                if removed > 0 {
                                                    line = line.child(
                                                        div()
                                                            .text_color(danger)
                                                            .child(format!("−{removed}")),
                                                    );
                                                }
                                                c.child(line)
                                            },
                                        )
                                        .when_some(info.cwd.clone(), |c, cwd| {
                                            c.child(
                                                div()
                                                    .max_w(px(420.))
                                                    .text_xs()
                                                    .text_color(muted)
                                                    .child(cwd),
                                            )
                                        })
                                        .when_some(info.host.clone(), |c, host| {
                                            c.child(
                                                h_flex()
                                                    .items_center()
                                                    .gap_1p5()
                                                    .text_xs()
                                                    .text_color(muted)
                                                    .child(
                                                        gpui::svg()
                                                            .path("icons/machine-remote.svg")
                                                            .flex_shrink_0()
                                                            .size(px(11.))
                                                            .text_color(muted),
                                                    )
                                                    .child(div().truncate().child(host)),
                                            )
                                        });
                                    card
                                })
                                .build(window, cx)
                            })
                        })
                        .child(
                            div()
                                .w_full()
                                .truncate()
                                .text_sm()
                                .when(is_active, |d| d.font_weight(FontWeight::MEDIUM))
                                .child(shown_title),
                        )
                        .children(git_line)
                        .when_some(cwd_shown, |col, (cwd, _)| {
                            col.child(
                                h_flex()
                                    .id(("sidebar-cwd", i))
                                    .w_full()
                                    .items_center()
                                    .gap_1p5()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground.opacity(0.8))
                                    .child(div().flex_1().min_w_0().truncate().child(cwd)),
                            )
                        })
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
                        let id = tab.tree_id.get();
                        move |_drag, grab, _window, cx| {
                            cx.stop_propagation();
                            *state.borrow_mut() = Some(
                                Reorder::new(
                                    Surface::SidebarRows(group_key.clone()),
                                    slot,
                                    slots.borrow().clone(),
                                    Axis::Vertical,
                                    px(ROW_GAP),
                                    grab,
                                )
                                .of_tab(id),
                            );
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
                                // The row by tab as well as by slot: reordering
                                // reads the slots of one group, a pane dropped
                                // on the sidebar reads every row there is.
                                let by_tab = self.sidebar_slots.clone();
                                move |bounds, _window, _cx| {
                                    if let Some(s) = slots.borrow_mut().get_mut(slot) {
                                        *s = bounds;
                                    }
                                    if let Some(s) = by_tab.borrow_mut().get_mut(i) {
                                        *s = bounds;
                                    }
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                    // Switched on the release, not the press: a press that turns
                    // into a drag is the tab being picked up, and a tab on its
                    // way into another tab's layout must not put itself on
                    // screen on the way there.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.activate(i, window, cx);
                    }))
                    .child(self.tab_avatar(
                        ("sidebar-avatar", i),
                        agent,
                        agent_status,
                        agent_unread,
                        ssh_dot,
                        22.,
                        cx,
                    ))
                    .child(label_region)
                    .when_some(badge_n, |row, n| {
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
                                .child(tab_badge_label(n)),
                        )
                    })
                    .when(badge_n.is_none(), |row| {
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
                                .child(div().w(px(10.)).h(px(crate::ui::tab_strip::MIN_TARGET)).bg(
                                    linear_gradient(
                                        90.,
                                        linear_color_stop(fade_from, 0.),
                                        linear_color_stop(backing, 1.),
                                    ),
                                ))
                                .child(
                                    div().bg(backing).child(
                                        crate::ui::tab_strip::hit_target(
                                            Button::new(("sidebar-close", i))
                                                .icon(IconName::Close)
                                                .ghost()
                                                .xsmall(),
                                        )
                                        .tooltip(t(L10nKey::TabContextCloseTab))
                                        // Held here, because the row behind it
                                        // switches tabs on the release too:
                                        // without this the same click closes
                                        // tab `i` and then activates whichever
                                        // tab slid into its place.
                                        .on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.close_tab(i, window, cx);
                                            }),
                                        ),
                                    ),
                                ),
                        )
                    });

                let menu_app = cx.entity().downgrade();
                rows.push(row.context_menu(move |menu, window, cx| {
                    Tty7App::tab_context_menu(menu, i, true, &menu_app, window, cx)
                }));
            }

            if rows.is_empty() && project.is_none() && !folded {
                continue;
            }

            let row_display: Vec<usize> = match &row_preview {
                Some(p) => {
                    if let Some(order) =
                        reordered_rows(&sections, &group_key, &visible_tabs, p.from, p.target)
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
            let row_count = if folded {
                visible_by_section[group_ix].len()
            } else {
                rows.len()
            };
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
            // The header a project is being renamed under is a text box, so
            // the name is edited where it is read rather than in a dialog
            // somewhere else.
            let renaming = project.and_then(|id| {
                self.project_rename
                    .as_ref()
                    .filter(|r| r.project == id)
                    .map(|r| r.input.clone())
            });
            let header: Option<AnyElement> = section.name.clone().map(|name| {
                // Left in the case it came in. These names are directories
                // (`tty7`, `025/1inch`) and translated words, and capitalising
                // them made a Latin one shout while a Chinese one — where
                // there is no case to raise — stayed exactly as it was: one
                // style, two different readings. Sentence case also puts a
                // block header a clear step below the heading above it, which
                // is the whole of the hierarchy in this rail.
                let label: SharedString = name.into();
                let head = h_flex()
                    .id(("sidebar-group", group_ix))
                    .group(SharedString::from(format!("sidebar-head-{group_ix}")))
                    .w_full()
                    .items_center()
                    .gap_1()
                    .pl_1p5()
                    .pr_1p5()
                    .pt_1p5()
                    .pb_0p5()
                    .text_size(px(12.))
                    .text_color(cx.theme().sidebar_foreground)
                    .cursor_pointer()
                    .on_click({
                        let key = group_key.clone();
                        cx.listener(move |this, _, _window, cx| {
                            cx.stop_propagation();
                            this.toggle_fold(SidebarFold::Section(key.clone()));
                            cx.notify();
                        })
                    })
                    .child(fold_chevron(folded, 12.))
                    .when_some(group_slot, |header, slot| {
                        crate::ui::reorder::cursor_grab(header).on_drag(DragGroup, {
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
                    .when(project.is_some(), |head| {
                        head.child(
                            Icon::empty()
                                .path("icons/folder.svg")
                                .size(px(12.))
                                .flex_shrink_0()
                                .text_color(cx.theme().muted_foreground),
                        )
                    })
                    .child(match renaming {
                        Some(input) => div()
                            .flex_1()
                            .min_w_0()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(Input::new(&input).appearance(false).xsmall())
                            .into_any_element(),
                        None => div()
                            .flex_shrink(1.)
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(label)
                            .into_any_element(),
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(row_count.to_string()),
                    )
                    .when_some(project, |head, id| {
                        head.child(div().flex_1()).child(
                            div()
                                .flex_shrink_0()
                                .opacity(0.)
                                .group_hover(
                                    SharedString::from(format!("sidebar-head-{group_ix}")),
                                    |s| s.opacity(1.),
                                )
                                .child(
                                    crate::ui::tab_strip::hit_target(
                                        Button::new(("project-new-tab", group_ix))
                                            .icon(IconName::Plus)
                                            .ghost()
                                            .xsmall(),
                                    )
                                    .tooltip(t(L10nKey::ProjectNewTab))
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.new_tab_in_project(id, window, cx);
                                        },
                                    )),
                                ),
                        )
                    });
                match project {
                    Some(id) => {
                        let menu_app = cx.entity().downgrade();
                        head.context_menu(move |menu, window, cx| {
                            Tty7App::project_context_menu(menu, id, &menu_app, window, cx)
                        })
                        .into_any_element()
                    }
                    None => head.into_any_element(),
                }
            });

            // A project with nothing in it says so, and the line that says it
            // is also the way to put something in it.
            let empty_hint = (project.is_some() && rows.is_empty() && !folded).then(|| {
                let id = project.expect("checked just above");
                h_flex()
                    .id(("project-empty", group_ix))
                    .w_full()
                    .items_center()
                    .gap_1p5()
                    .pl_2()
                    .pr_2()
                    .py_1()
                    .rounded_lg()
                    .cursor_pointer()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground.opacity(0.8))
                    .hover(|s| s.bg(gpui::rgb(sf.hover)))
                    .child(Icon::new(IconName::Plus).size(px(11.)).flex_shrink_0())
                    .child(div().truncate().child(t(L10nKey::ProjectEmpty)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.new_tab_in_project(id, window, cx);
                    }))
                    .into_any_element()
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
                .children(empty_hint)
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

            // What the search found, not what was drawn: a block folded shut
            // still matched, and "nothing matches" would be a lie.
            any_rows |= !visible_by_section[group_ix].is_empty();
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

        // The derived half can be empty — every tab filed under a project —
        // and its heading still has to be there: it is the boundary between
        // the two halves, and it carries the new-tab button.
        if headings && block_count == projects_shown {
            list = list.child(self.tabs_heading(projects_shown > 0 && !projects_folded, cx));
        }

        if !any_rows && !query.is_empty() {
            list = list.child(
                div()
                    .px_2()
                    .py_3()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(crate::ui::i18n::t_fmt(
                        crate::ui::i18n::L10nKey::SettingsNothingMatches,
                        &[("query", &query)],
                    )),
            );
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
            // No new-tab button here: it lives on the TABS heading, beside
            // the half of the rail it adds to.
            .child(
                div().occlude().flex_shrink_0().child(
                    crate::ui::tab_strip::chrome_tile(
                        Button::new("sidebar-collapse")
                            .icon(Icon::empty().path("icons/panel-left.svg")),
                        false,
                        cx,
                    )
                    .rounded_lg()
                    .tooltip(crate::ui::tab_strip::chord_hint(
                        t(L10nKey::TabTooltipHideSidebar),
                        "ToggleLeftPanel",
                        cx,
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| this.toggle_left_panel(cx))),
                ),
            );
        // The tile inside asks for `w_full`, and a percentage is only a width
        // while some box above it has a real one. This row used to have none of
        // its own and borrowed the column's by cross-axis stretch, which did not
        // always hold; `w_full` here swapped that for a second percentage, and a
        // row whose width is `Percent` is no longer `auto`, so it lost stretch
        // as well — on the passes that size the column from its content there
        // was still nothing to resolve against and the tile fell back to hugging
        // the workspace name. Hand the row real pixels: the rail is
        // `w(px(width))` and layout is border-box, so its content is one pixel
        // narrower than that because of the right border.
        let workspace_head = h_flex()
            .w(px(width - 1.))
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
        // Read while there is still a `cx` to read it from: the drag handler
        // below only ever sees a `Window`, and the cap it clamps against has to
        // be the same one the layout applies or the sidebar springs back from
        // wherever it was dropped.
        let others_floor = self.right_panel_floor(cx) + self.document_floor(cx);
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
                            let max = crate::ui::app::side_panel_max(
                                window.viewport_size().width.as_f32(),
                                MIN_SIDEBAR_WIDTH,
                                others_floor,
                            );
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
            .group(SIDEBAR_GROUP)
            .relative()
            .flex_shrink_0()
            .w(px(width))
            .h_full()
            .bg(crate::ui::theme::workspace_surface_color(cx))
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .child(backing)
            .child(
                // Real pixels, not `size_full`: the rail's own width is a
                // definite `px`, but a percentage off it is still a percentage,
                // and on the passes that size this column from its content it
                // resolves against nothing. Everything below asks for `w_full`
                // — the tab rows, their group blocks, the scroll area — so one
                // unresolved link here collapsed the whole chain and every row
                // fell back to hugging the longest tab name. Border-box takes
                // the rail's 1px right border off the content width.
                v_flex()
                    .w(px(width - 1.))
                    .h_full()
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

    /// What the sidebar row hid: the full title, the full branch and diff
    /// counts, the working directory, and the remote host the avatar only
    /// dots. `None` when the row showed everything — a card would add noise,
    /// not information. The host is included even for an untruncated row,
    /// because the title strips the `user@host:` prefix the avatar cannot
    /// spell out.
    ///
    /// Every line is decided by comparing what the row rendered against the
    /// string it was elided from. Both come from the row itself: deriving
    /// them here a second time is how a renamed tab ended up with a name the
    /// row shortened and the card refused to expand.
    fn sidebar_info(
        &self,
        tab: &crate::ui::app::Tab,
        window: &mut Window,
        cx: &gpui::App,
        shown: &SidebarRowShown,
    ) -> Option<SidebarInfo> {
        let elided = |pair: &Option<(SharedString, SharedString)>| {
            pair.as_ref()
                .filter(|(shown, full)| shown != full)
                .map(|(_, full)| full.clone())
        };
        let mut info = SidebarInfo {
            title: elided(&shown.title),
            branch: shown
                .branch
                .as_ref()
                .filter(|(shown, full, _, _)| shown != full)
                .map(|(_, full, added, removed)| (full.clone(), *added, *removed)),
            // The cwd only earns a card line when it was rendered *and*
            // elided: a repo row already shows the full path as its title, so
            // repeating the cwd under it would be noise, not information.
            cwd: elided(&shown.cwd),
            host: None,
        };
        // The host is read off the same leaf the title and cwd came from; a
        // split tab whose panes sit on different machines would otherwise
        // name whichever one happens to be first.
        if let Some(target) = tab.pane.focused_or_first(window, cx).and_then(|leaf| {
            leaf.read(cx)
                .remote_context()
                .map(|r| SharedString::from(r.target.clone()))
        }) {
            info.host = Some(target);
        }
        (info.title.is_some() || info.branch.is_some() || info.cwd.is_some() || info.host.is_some())
            .then_some(info)
    }

    /// Each project's id beside the name its header reads — an explicit name
    /// when it has one, otherwise the folder title with the same collision
    /// walk-up a repo group header does.
    pub(crate) fn named_projects(&self) -> Vec<(ProjectId, String)> {
        self.projects
            .iter()
            .map(|p| p.id)
            .zip(crate::ui::projects::project_names(&self.projects))
            .collect()
    }

    pub(crate) fn is_folded(&self, what: &SidebarFold) -> bool {
        self.sidebar_folded.borrow().contains(what)
    }

    pub(crate) fn toggle_fold(&self, what: SidebarFold) {
        let mut folded = self.sidebar_folded.borrow_mut();
        if !folded.remove(&what) {
            folded.insert(what);
        }
    }

    /// One of the rail's two top-level headings — the only thing that says
    /// which half of the sidebar you are looking at: above the second one
    /// everything was declared, below it everything was derived.
    ///
    /// Deliberately the quietest text in the rail, and the only text in it set
    /// in capitals. That pair is what makes it read as chrome rather than as
    /// one more block header: a heading that shared the block headers' size,
    /// case and weight — which is what this was at first — left eight rows
    /// that all looked alike and no way to tell what was inside what.
    ///
    /// The two halves are told apart by the space above the second heading
    /// and nothing else. A hairline there was tried and looked like what it
    /// was — a rule drawn edge to edge across a 200px rail, sitting right
    /// under whatever the block above ended with. Every sidebar on this
    /// platform separates its sections with whitespace and a quiet label;
    /// none of them draws a line.
    ///
    /// `leading_gap` is the caller's answer to "is there anything up there to
    /// be separated from". Whitespace between two headings with nothing
    /// between them is not a separator, it is a hole.
    fn rail_heading<A: IntoElement>(
        &self,
        id: &'static str,
        label: SharedString,
        fold: SidebarFold,
        leading_gap: bool,
        add: A,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<A> {
        let folded = self.is_folded(&fold);
        h_flex()
            .id(id)
            .group(SharedString::from(id))
            .w_full()
            .items_center()
            .gap_1()
            .pl_1()
            .pr_1p5()
            .pb_1()
            .when(leading_gap, |h| h.mt_2())
            .pt_1p5()
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().muted_foreground)
            .child(fold_chevron(folded, 11.))
            .child(div().flex_1().min_w_0().truncate().child(label))
            .child(
                div()
                    .flex_shrink_0()
                    .opacity(0.)
                    .group_hover(SIDEBAR_GROUP, |s| s.opacity(1.))
                    .child(add),
            )
            .cursor_pointer()
            .hover(|s| s.text_color(cx.theme().sidebar_foreground))
            .on_click(cx.listener(move |this, _, _window, cx| {
                cx.stop_propagation();
                this.toggle_fold(fold.clone());
                cx.notify();
            }))
    }

    fn projects_heading(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let add = crate::ui::tab_strip::hit_target(
            Button::new("sidebar-project-add")
                .icon(IconName::Plus)
                .ghost()
                .xsmall(),
        )
        .tooltip(t(L10nKey::ProjectNewTooltip))
        .on_click(cx.listener(|this, _, window, cx| {
            cx.stop_propagation();
            this.new_project(window, cx);
        }));
        self.rail_heading(
            "sidebar-projects-heading",
            SharedString::from(t(L10nKey::SidebarProjectsHeading).to_uppercase()),
            SidebarFold::Projects,
            // The first thing in the list; the gap above it is the list's own
            // top padding.
            false,
            add,
            cx,
        )
    }

    fn tabs_heading(&self, leading_gap: bool, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        self.rail_heading(
            "sidebar-tabs-heading",
            SharedString::from(t(L10nKey::SidebarTabsHeading).to_uppercase()),
            SidebarFold::Tabs,
            leading_gap,
            self.new_tab_heading_button("sidebar-add", cx),
            cx,
        )
    }

    fn sidebar_group_keys(&self, cx: &gpui::App) -> Vec<Option<PathBuf>> {
        let grouping = cx.global::<Config>().sidebar_grouping;
        self.tabs
            .iter()
            .map(|tab| {
                if grouping == SidebarGrouping::None {
                    return None;
                }
                let cwd = tab.pane.first_leaf().and_then(|leaf| {
                    let view = leaf.terminal()?.read(cx);
                    Some((view.host_id(), view.git_status_cwd()?.to_path_buf()))
                });
                if let Some((id, cwd)) = cwd {
                    let known = cx.global::<GitStatusCache>().known_repo_for(id, &cwd);
                    if let Some(group) = resolved_group(grouping, known, &cwd) {
                        *tab.sidebar_group.borrow_mut() = group;
                    }
                }
                tab.sidebar_group.borrow().clone()
            })
            .collect()
    }

    /// The tabs the rail is showing, in the order it shows them — which is
    /// what ⌘N counts and what a row's badge names.
    ///
    /// Rows inside a folded block are not on screen, so they are not in the
    /// count: leaving them in would number the visible rows ⌘1, ⌘4, ⌘5, and
    /// ⌘2 would open something nobody can see.
    fn visual_tab_order(&self, cx: &gpui::App) -> Vec<usize> {
        if cx.global::<Config>().tab_bar_position != crate::core::config::TabBarPosition::Left {
            return (0..self.tabs.len()).collect();
        }
        let keys = self.sidebar_group_keys(cx);
        let members: Vec<Option<ProjectId>> = self.tabs.iter().map(|t| t.project.get()).collect();
        let sections = sidebar_sections(&keys, &members, &self.named_projects());
        on_screen(&sections, &self.sidebar_folded.borrow())
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
    /// repo probe for that directory has already landed. A bare `None` means
    /// the cache never looked; `Some` is the group [`resolved_group`] reached
    /// — the repo home, the cwd itself under repo-or-directory grouping, or
    /// `Some(None)` for Scratch.
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
        let cwd = cwd?;
        let host = self
            .window_workspace(cx)
            .as_ref()
            .map_or(crate::ui::host_ops::HostId::LOCAL, |ws| ws.target.host_id());
        let known = cx.try_global::<GitStatusCache>()?.known_repo_for(host, cwd);
        resolved_group(cx.global::<Config>().sidebar_grouping, known, cwd)
    }
}

/// The group a probed cwd resolves to under `grouping`: the repo home when
/// the cache found one, otherwise Scratch — or the cwd itself under
/// repo-or-directory grouping, so a shell in a plain folder still gets a
/// header. `known` is the cache's three-valued answer; a probe that never
/// ran resolves to `None`, no decision, and the tab keeps whatever group it
/// already has rather than bouncing through Scratch mid-probe.
fn resolved_group(
    grouping: SidebarGrouping,
    known: Option<Option<PathBuf>>,
    cwd: &Path,
) -> Option<Option<PathBuf>> {
    Some(match known? {
        Some(root) => Some(root),
        None if grouping == SidebarGrouping::RepoOrDirectory => Some(cwd.to_path_buf()),
        None => None,
    })
}

/// What a block of rows in the sidebar is grouped by.
///
/// Two of these are derived from where the tabs happen to be — a repo the
/// probe found, or nothing it could place — and one is declared. They share a
/// type because a row belongs to exactly one block whichever kind it is, and
/// everything that reads the sidebar's shape (⌘N order, in-block reordering,
/// which rows a drag may shuffle) wants that one answer.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SectionKey {
    /// A project someone declared, by its id.
    Project(ProjectId),
    /// A repo home the probe derived from a tab's cwd.
    Repo(PathBuf),
    /// The tabs the probe could not place.
    Scratch,
}

impl SectionKey {
    fn repo(&self) -> Option<&PathBuf> {
        match self {
            SectionKey::Repo(root) => Some(root),
            _ => None,
        }
    }

    fn project(&self) -> Option<ProjectId> {
        match self {
            SectionKey::Project(id) => Some(*id),
            _ => None,
        }
    }
}

/// The tabs `sections` actually puts on screen, in row order, with every
/// folded block left out — the order ⌘N counts.
fn on_screen(sections: &[Section], folded: &std::collections::HashSet<SidebarFold>) -> Vec<usize> {
    sections
        .iter()
        .filter(|s| {
            let half = match s.key {
                SectionKey::Project(_) => SidebarFold::Projects,
                _ => SidebarFold::Tabs,
            };
            !folded.contains(&half) && !folded.contains(&SidebarFold::Section(s.key.clone()))
        })
        .flat_map(|s| s.tabs.clone())
        .collect()
}

/// The chord number each tab wears, indexed the way `self.tabs` is, read off
/// the same [`on_screen`] order `activate_visual` walks — a badge taken from
/// anywhere else names a chord that opens a different tab.
///
/// `None` for a tab that order left out, rather than a `0` that reads as ⌘1.
/// The rail can draw a row that order does not count: a live search deliberately
/// ignores a folded heading, so it shows rows the chord order has taken out. A
/// row with no number is the only honest thing to draw there — the alternative
/// is every one of those rows claiming ⌘1 while ⌘1 opens something else.
fn badge_positions(order: &[usize], tabs: usize) -> Vec<Option<usize>> {
    let mut pos = vec![None; tabs];
    for (n, &i) in order.iter().enumerate() {
        if let Some(slot) = pos.get_mut(i) {
            *slot = Some(n);
        }
    }
    pos
}

/// The twist that says whether a block is open, drawn where a disclosure
/// triangle goes — pointing down when what is under it is showing.
fn fold_chevron(folded: bool, size: f32) -> impl IntoElement {
    Icon::new(if folded {
        IconName::ChevronRight
    } else {
        IconName::ChevronDown
    })
    .size(px(size))
    .flex_shrink_0()
}

/// Something in the rail that folds shut, named by the header you click.
///
/// Held in memory rather than in the config: re-opening a block is one click,
/// and the alternative is a map keyed by repo path — the orphaned-key problem
/// this whole feature was written to get away from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SidebarFold {
    /// The PROJECTS heading, and with it every project under it.
    Projects,
    /// The TABS heading, and with it the whole derived grouping.
    Tabs,
    /// One block, header left showing.
    Section(SectionKey),
}

#[derive(Debug, PartialEq)]
struct Section {
    key: SectionKey,
    name: Option<String>,
    tabs: Vec<usize>,
}

/// The sidebar's blocks, top to bottom: every declared project in the order
/// the workspace holds them, then the derived grouping over whatever tabs no
/// project has claimed.
///
/// The second half is the whole of today's sidebar, unchanged — the only
/// difference is the tabs it is fed. `projects` carries each project's id
/// beside the name its header reads, resolved by the caller so this can be
/// tested without one.
fn sidebar_sections(
    keys: &[Option<PathBuf>],
    members: &[Option<ProjectId>],
    projects: &[(ProjectId, String)],
) -> Vec<Section> {
    let member_of = |i: usize| members.get(i).copied().flatten();
    let mut out: Vec<Section> = projects
        .iter()
        .map(|(id, name)| Section {
            key: SectionKey::Project(*id),
            name: Some(name.clone()),
            tabs: (0..keys.len())
                .filter(|&i| member_of(i) == Some(*id))
                .collect(),
        })
        .collect();
    let loose: Vec<usize> = (0..keys.len())
        .filter(|&i| member_of(i).is_none())
        .collect();

    let mut group_order: Vec<&PathBuf> = Vec::new();
    for k in loose.iter().filter_map(|&i| keys[i].as_ref()) {
        if !group_order.iter().any(|g| *g == k) {
            group_order.push(k);
        }
    }
    if group_order.is_empty() {
        if loose.is_empty() && !out.is_empty() {
            return out;
        }
        out.push(Section {
            key: SectionKey::Scratch,
            // The only derived block there is, so it wears no header of its
            // own: its rows sit directly under TABS, which is what a
            // workspace with no repos has always looked like. A name would
            // only be a second heading saying the same thing.
            name: None,
            tabs: loose,
        });
        return out;
    }
    let names = group_names(&group_order);
    out.extend(group_order.iter().zip(names).map(|(root, name)| {
        Section {
            key: SectionKey::Repo((*root).clone()),
            name: Some(name),
            tabs: loose
                .iter()
                .copied()
                .filter(|&i| keys[i].as_ref() == Some(*root))
                .collect(),
        }
    }));
    let scratch: Vec<usize> = loose
        .iter()
        .copied()
        .filter(|&i| keys[i].is_none())
        .collect();
    if !scratch.is_empty() {
        out.push(Section {
            key: SectionKey::Scratch,
            name: Some(t(L10nKey::SidebarScratchGroup).to_string()),
            tabs: scratch,
        });
    }
    out
}

/// The whole tab order with one row moved within its own block. Rows filtered
/// out by the search are not in `visible` and are left where they are.
fn reordered_rows(
    sections: &[Section],
    key: &SectionKey,
    visible: &[usize],
    from: usize,
    to: usize,
) -> Option<Vec<usize>> {
    let (&moved, &anchor) = (visible.get(from)?, visible.get(to)?);
    if moved == anchor {
        return None;
    }
    let mut members = sections.iter().find(|s| s.key == *key)?.tabs.clone();
    members.retain(|&i| i != moved);
    let at = members.iter().position(|&i| i == anchor)? + usize::from(to > from);
    members.insert(at, moved);

    Some(
        sections
            .iter()
            .flat_map(|s| {
                if s.key == *key {
                    members.clone()
                } else {
                    s.tabs.clone()
                }
            })
            .collect(),
    )
}

/// The whole tab order with one block moved into another block's slot.
fn regrouped_order(sections: &[Section], from: &SectionKey, to: &SectionKey) -> Option<Vec<usize>> {
    if from == to {
        return None;
    }
    let fi = sections.iter().position(|s| s.key == *from)?;
    let ti = sections.iter().position(|s| s.key == *to)?;
    let mut order: Vec<usize> = (0..sections.len()).collect();
    let moved = order.remove(fi);
    order.insert(ti, moved);
    Some(
        order
            .into_iter()
            .flat_map(|i| sections[i].tabs.clone())
            .collect(),
    )
}

pub(crate) fn group_names(roots: &[&PathBuf]) -> Vec<String> {
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

/// Whether a `+N −M` is a button, and what it opens if it is.
///
/// One function because the setting is one setting: the sidebar's counts and
/// the Info panel's `changes` row are the same number about the same working
/// tree, and "Open diff preview from sidebar counts" turning one of them into
/// plain text while the other stayed clickable would be a setting that half
/// works.
pub(crate) fn diff_click_cwd<T>(cfg: &Config, target: Option<T>) -> Option<T> {
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
    fn a_probed_non_repo_groups_by_folder_only_in_the_fallback_mode() {
        let cwd = p("/w/plain");
        assert_eq!(
            resolved_group(SidebarGrouping::RepoOrDirectory, Some(None), &cwd),
            Some(Some(p("/w/plain")))
        );
        assert_eq!(
            resolved_group(SidebarGrouping::Repo, Some(None), &cwd),
            Some(None),
            "under Repo a probed non-repo still falls to Scratch"
        );
        // Never probed: no decision in either mode, so the tab keeps the
        // group it already has instead of bouncing through Scratch.
        assert_eq!(resolved_group(SidebarGrouping::Repo, None, &cwd), None);
        assert_eq!(
            resolved_group(SidebarGrouping::RepoOrDirectory, None, &cwd),
            None
        );
    }

    #[test]
    fn a_known_repo_home_wins_over_the_folder_in_both_modes() {
        for mode in [SidebarGrouping::Repo, SidebarGrouping::RepoOrDirectory] {
            assert_eq!(
                resolved_group(mode, Some(Some(p("/w/repo"))), &p("/w/repo/sub")),
                Some(Some(p("/w/repo")))
            );
        }
    }

    /// The two-argument shape every test below is written against: no
    /// projects declared, so nothing is filed under one.
    fn loose(n: usize) -> Vec<Option<ProjectId>> {
        vec![None; n]
    }

    fn sections_of(keys: &[Option<PathBuf>]) -> Vec<Section> {
        sidebar_sections(keys, &loose(keys.len()), &[])
    }

    fn flatten(sections: &[Section]) -> Vec<usize> {
        sections.iter().flat_map(|s| s.tabs.clone()).collect()
    }

    #[test]
    fn sections_order_groups_by_first_appearance_scratch_last() {
        let keys = vec![
            Some(p("/w/beta")),
            None,
            Some(p("/w/alpha")),
            Some(p("/w/beta")),
        ];
        let shape: Vec<(SectionKey, Option<String>, Vec<usize>)> = sections_of(&keys)
            .into_iter()
            .map(|s| (s.key, s.name, s.tabs))
            .collect();
        assert_eq!(
            shape,
            vec![
                (
                    SectionKey::Repo(p("/w/beta")),
                    Some("beta".into()),
                    vec![0, 3]
                ),
                (
                    SectionKey::Repo(p("/w/alpha")),
                    Some("alpha".into()),
                    vec![2]
                ),
                (SectionKey::Scratch, Some("Scratch".into()), vec![1]),
            ]
        );

        let flat = sections_of(&[None, None]);
        assert_eq!(flat.len(), 1);
        assert_eq!(
            flat[0].name, None,
            "one block and nothing above it: no header"
        );
        assert_eq!(flat[0].tabs, vec![0, 1]);
    }

    /// The whole point of the additive shape: the derived half is fed only the
    /// tabs no project claimed, and is otherwise exactly what it was.
    #[test]
    fn a_project_takes_its_tabs_out_of_the_derived_grouping() {
        let arb = ProjectId::new();
        let keys = vec![
            Some(p("/w/beta")),
            Some(p("/w/alpha")),
            Some(p("/w/beta")),
            None,
        ];
        // Tab 0 is filed under the project; the other tab in /w/beta is not.
        let members = vec![Some(arb), None, None, None];
        let sections = sidebar_sections(&keys, &members, &[(arb, "套利研究".into())]);
        let shape: Vec<(SectionKey, Option<String>, Vec<usize>)> = sections
            .into_iter()
            .map(|s| (s.key, s.name, s.tabs))
            .collect();
        assert_eq!(
            shape,
            vec![
                (SectionKey::Project(arb), Some("套利研究".into()), vec![0]),
                (
                    SectionKey::Repo(p("/w/alpha")),
                    Some("alpha".into()),
                    vec![1]
                ),
                (SectionKey::Repo(p("/w/beta")), Some("beta".into()), vec![2]),
                (SectionKey::Scratch, Some("Scratch".into()), vec![3]),
            ],
            "the repo group keeps the tab the project did not take, and \
             /w/alpha now leads because first appearance is read over the \
             tabs that are left"
        );
    }

    /// A project stays when the last tab in it closes — that is the whole
    /// difference between it and a group.
    #[test]
    fn a_project_with_no_tabs_is_still_a_section() {
        let empty = ProjectId::new();
        let sections = sidebar_sections(&[None], &[None], &[(empty, "做市实验".into())]);
        assert_eq!(sections[0].key, SectionKey::Project(empty));
        assert!(sections[0].tabs.is_empty());
        assert_eq!(
            sections[1].name, None,
            "the only derived block wears no header: its rows sit under TABS"
        );
    }

    #[test]
    fn every_tab_in_a_project_leaves_no_block_below() {
        let only = ProjectId::new();
        let sections = sidebar_sections(
            &[None, None],
            &[Some(only), Some(only)],
            &[(only, "p".into())],
        );
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].tabs, vec![0, 1]);
    }

    fn folds(of: &[SidebarFold]) -> std::collections::HashSet<SidebarFold> {
        of.iter().cloned().collect()
    }

    /// Folding is what is on screen, and ⌘N counts what is on screen. A row
    /// inside a folded block is not a row, so it takes no number — otherwise
    /// the visible rows would badge ⌘1, ⌘4, ⌘5 and ⌘2 would open something
    /// nobody can see.
    #[test]
    fn a_folded_block_takes_its_rows_out_of_the_chord_order() {
        let arb = ProjectId::new();
        let keys = vec![None, Some(p("/w/beta")), Some(p("/w/alpha")), None];
        let members = vec![Some(arb), None, None, None];
        let sections = sidebar_sections(&keys, &members, &[(arb, "arb".into())]);
        assert_eq!(on_screen(&sections, &folds(&[])), vec![0, 1, 2, 3]);
        assert_eq!(
            on_screen(
                &sections,
                &folds(&[SidebarFold::Section(SectionKey::Repo(p("/w/beta")))])
            ),
            vec![0, 2, 3]
        );
        assert_eq!(
            on_screen(&sections, &folds(&[SidebarFold::Projects])),
            vec![1, 2, 3],
            "folding a heading takes its whole half, headers and all"
        );
        assert_eq!(
            on_screen(&sections, &folds(&[SidebarFold::Tabs])),
            vec![0],
            "and the derived half is one heading, however many blocks it holds"
        );
        assert!(
            on_screen(
                &sections,
                &folds(&[SidebarFold::Projects, SidebarFold::Tabs])
            )
            .is_empty()
        );
    }

    /// The badge on a row and the tab ⌘N opens are two readings of one order,
    /// taken in two places. Grouping makes them diverge from `self.tabs`
    /// order — tab 3 sits in the second row here — so if they are ever read
    /// off different things, the badge names a chord that opens another tab.
    #[test]
    fn a_row_badge_names_the_chord_that_opens_that_row() {
        let keys = vec![
            Some(p("/w/beta")),
            None,
            Some(p("/w/alpha")),
            Some(p("/w/beta")),
        ];
        // What `visual_tab_order` returns for a left tab bar.
        let order = flatten(&sections_of(&keys));
        assert_eq!(order, vec![0, 3, 2, 1]);

        let badge_pos = badge_positions(&order, keys.len());
        for (row, tab) in order.iter().copied().enumerate() {
            // ActivateTabN → activate_visual(N - 1) → order[N - 1].
            let n = badge_pos[tab].expect("every tab in the order is badged");
            let chord = tab_badge_label(n);
            let opens = order[n];
            assert_eq!(
                opens, tab,
                "row {row} badges ⌘{chord}, which opens tab {opens}"
            );
        }
    }

    /// A live search draws rows a folded heading has taken out of the chord
    /// order — that is deliberate, the query is asking about tabs. So the two
    /// disagree, and the badge has to say "no number" rather than fall back to
    /// a `0` that reads as ⌘1 while ⌘1 opens something else entirely.
    #[test]
    fn a_row_the_chord_order_left_out_wears_no_badge() {
        let arb = ProjectId::new();
        let keys = vec![None, Some(p("/w/beta")), None];
        let members = vec![Some(arb), None, Some(arb)];
        let sections = sidebar_sections(&keys, &members, &[(arb, "arb".into())]);

        let order = on_screen(&sections, &folds(&[SidebarFold::Projects]));
        assert_eq!(order, vec![1], "the folded half is out of the order");

        let badge_pos = badge_positions(&order, keys.len());
        assert_eq!(badge_pos, vec![None, Some(0), None]);
        assert!(
            badge_pos[0].is_none() && badge_pos[2].is_none(),
            "a project row a search still draws must not claim ⌘1"
        );
    }

    #[test]
    fn reordered_rows_moves_within_the_group_only() {
        let keys = vec![
            Some(p("/w/alpha")),
            Some(p("/w/beta")),
            Some(p("/w/alpha")),
            None,
        ];
        let sections = sections_of(&keys);
        let alpha = SectionKey::Repo(p("/w/alpha"));
        assert_eq!(
            reordered_rows(&sections, &alpha, &[0, 2], 0, 1),
            Some(vec![2, 0, 1, 3])
        );
        assert_eq!(
            reordered_rows(&sections, &alpha, &[0, 2], 1, 0),
            Some(vec![2, 0, 1, 3])
        );
        assert_eq!(reordered_rows(&sections, &alpha, &[0, 2], 1, 1), None);
    }

    /// Rows inside a project reorder the same way rows inside a group do.
    #[test]
    fn reordered_rows_works_inside_a_project_too() {
        let arb = ProjectId::new();
        let keys = vec![None, Some(p("/w/beta")), None];
        let members = vec![Some(arb), None, Some(arb)];
        let sections = sidebar_sections(&keys, &members, &[(arb, "arb".into())]);
        assert_eq!(
            reordered_rows(&sections, &SectionKey::Project(arb), &[0, 2], 0, 1),
            Some(vec![2, 0, 1])
        );
    }

    #[test]
    fn reordered_rows_leaves_filtered_out_rows_alone() {
        let keys = vec![Some(p("/w/a")), Some(p("/w/a")), Some(p("/w/a"))];
        let sections = sections_of(&keys);
        assert_eq!(
            reordered_rows(&sections, &SectionKey::Repo(p("/w/a")), &[0, 2], 0, 1),
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
        let sections = sections_of(&keys);
        let g = |path: &str| SectionKey::Repo(p(path));
        assert_eq!(
            regrouped_order(&sections, &g("/w/gamma"), &g("/w/alpha")),
            Some(vec![4, 0, 3, 2, 1])
        );
        assert_eq!(
            regrouped_order(&sections, &g("/w/alpha"), &g("/w/gamma")),
            Some(vec![2, 4, 0, 3, 1])
        );
    }

    #[test]
    fn regrouped_order_ignores_self_and_unknown_roots() {
        let keys = vec![Some(p("/w/alpha")), Some(p("/w/beta"))];
        let sections = sections_of(&keys);
        let g = |path: &str| SectionKey::Repo(p(path));
        assert_eq!(
            regrouped_order(&sections, &g("/w/alpha"), &g("/w/alpha")),
            None
        );
        assert_eq!(
            regrouped_order(&sections, &g("/w/gone"), &g("/w/beta")),
            None
        );
        assert_eq!(
            regrouped_order(&sections, &g("/w/alpha"), &g("/w/gone")),
            None
        );
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

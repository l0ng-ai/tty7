use gpui::{
    Animation, AnimationExt as _, AnyElement, Axis, Bounds, Context, Div, FontWeight, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, SharedString, Stateful, Window, canvas,
    deferred, div, ease_out_quint, linear_color_stop, linear_gradient, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent};
use gpui_component::menu::{ContextMenu, ContextMenuExt as _, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use std::path::{Path, PathBuf};

use crate::core::config::Config;
use crate::core::group_key::{
    AutoKey, GroupId, GroupKey, PinnedGroup, WorkspaceGroups, auto_key, place,
};
use crate::terminal::git_status::GitStatusCache;
use crate::ui::app::{TITLE_BAR_HEIGHT, Tab, Tty7App};
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

const ROW_GAP: f32 = 2.;

/// A single-line tab row: one line of `text_sm` and a little air, the same
/// 28px the search field and the workspace chip above it stand at.
const ROW_HEIGHT: f32 = 28.;

/// The row chrome the text budget has to be measured around. These are the
/// numbers the layout below is built from, not a second guess at it — a row
/// that elides against a budget wider than it really has falls back to CSS
/// truncation, which drops the tail this whole module exists to keep.
mod row_metrics {
    /// `border_r_1` on the sidebar itself.
    pub(super) const BORDER: f32 = 1.;
    /// `px_2` on the scrolling list that holds the rows.
    pub(super) const LIST_PAD: f32 = 8.;
    /// `pl_2` + `pr_2` on the row.
    pub(super) const ROW_PAD: f32 = 8.;
    /// The avatar handed to `tab_avatar`.
    pub(super) const AVATAR: f32 = 18.;
    /// `gap_2` between the row's children.
    pub(super) const GAP: f32 = 8.;
    /// The ⌘N badge, when one is shown.
    pub(super) const BADGE: f32 = 20.;
    /// The zoom mark, when the tab has a pane zoomed over the others.
    pub(super) const ZOOM: f32 = 16.;
    /// `gap_1p5`, between the branch icon and its text and before the counts.
    pub(super) const META_GAP: f32 = 6.;
    /// The branch icon.
    pub(super) const BRANCH_ICON: f32 = 11.;
    /// `pl_2` + `pr_1p5` on a group header.
    pub(super) const HEADER_PAD: f32 = 8. + 6.;
    /// The chevron a header opens with, and the asterisk that marks a custom
    /// group: both `xsmall` icons, which resolve to 12px.
    pub(super) const HEADER_ICON: f32 = 12.;

    /// What a row can spend on text, before the badge is taken out.
    pub(super) const fn text_budget(width: f32) -> f32 {
        width - BORDER - 2. * LIST_PAD - 2. * ROW_PAD - AVATAR - GAP
    }

    /// What a group header can spend on its name and the branch beside it,
    /// with the chevron and its gap already taken out. The pin and the folded
    /// row count come off at the call site, which knows whether they are drawn.
    pub(super) const fn header_budget(width: f32) -> f32 {
        width - BORDER - 2. * LIST_PAD - HEADER_PAD - HEADER_ICON - META_GAP
    }
}

/// The narrowest a group's heading is allowed to get: three or four capitals
/// and an ellipsis, which is still a name and not a stub.
const HEADER_NAME_FLOOR: f32 = 40.;

/// The narrowest a row's title is allowed to get before the working directory
/// beside it stops taking room, and the narrowest that path may be drawn at:
/// below this it is an ellipsis and a slash, which names no directory.
const ROW_TITLE_FLOOR: f32 = 48.;
const ROW_CWD_FLOOR: f32 = 24.;

/// How a group header divides its line between the heading and the branch its
/// rows share. The branch takes what it wants up to half the line, and the
/// heading keeps the rest — so a long branch can no longer crush the name
/// (flex used to hand the overflow to them in proportion to what each asked
/// for, which gave the longer string the smaller cut), and a long custom name
/// cannot crush the branch in return. `git_want` is `None` for a header with
/// no shared branch on it, which then owns the whole line.
fn header_name_avail(avail: f32, git_want: Option<f32>) -> f32 {
    match git_want {
        Some(want) => (avail - want.min(avail * 0.5)).max(HEADER_NAME_FLOOR),
        None => avail,
    }
}

/// What a diff's counts occupy on a line, measured against real glyphs: the
/// two numbers, the gap between them when both are drawn, and the gap that
/// separates them from the branch. They never wrap and never shrink, so this
/// is the width a branch has to be elided around — on a row and on the group
/// header that lifts the branch off its rows alike.
fn counts_width(
    ts: &gpui::WindowTextSystem,
    font: &gpui::Font,
    size: f32,
    status: &crate::terminal::git_status::GitStatus,
) -> f32 {
    let mut w = 0.;
    if status.added > 0 {
        w += measure_text(ts, font, size, &format!("+{}", status.added));
    }
    if status.removed > 0 {
        w += measure_text(ts, font, size, &format!("−{}", status.removed));
    }
    if status.added > 0 && status.removed > 0 {
        w += row_metrics::META_GAP;
    }
    if w > 0. {
        w += row_metrics::META_GAP;
    }
    w
}

/// The branch a whole group shares, lifted off its rows and onto its header.
struct SharedGit {
    status: crate::terminal::git_status::GitStatus,
    /// Where a click on the counts opens the diff overlay, if the setting
    /// allows one.
    click: Option<(crate::ui::host_ops::HostId, PathBuf)>,
    /// Every row the group counts, drawn or folded away.
    rows: Vec<usize>,
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
    pub(crate) fn right_panel_floor(&self, window: &Window, cx: &gpui::App) -> f32 {
        if self.right_panel_open(cx) {
            self.right_panel_min_px(window, cx)
        } else {
            0.
        }
    }

    pub(crate) fn sidebar_max_px(&self, window: &Window, cx: &gpui::App) -> f32 {
        crate::ui::app::side_panel_max(
            window.viewport_size().width.as_f32(),
            MIN_SIDEBAR_WIDTH,
            self.right_panel_floor(window, cx) + self.document_floor(cx),
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
        // Read before it is blanked: the rectangles a tab held over the
        // sidebar is measured against are the ones drawn last frame, the
        // same way a pane dropped here is. Blanked and written again below
        // so a group that folds or filters away stops accepting drops.
        let over_group = self.sidebar_regroup_target(window);
        let lifting_row = crate::ui::reorder::dragged_sidebar_tab(&self.reorder).is_some();
        // Offered every frame the pointer is over a group, and cleared with
        // the rest of the drag's pending state on the frames it is not — so
        // letting go anywhere else drops on nothing.
        if let Some(target) = over_group {
            crate::ui::reorder::set_regroup(&self.reorder, target);
        }
        self.sidebar_group_slots.borrow_mut().clear();
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
            .px_2()
            .py_1p5()
            .gap_0p5();

        let keys: Rc<Vec<Option<GroupKey>>> = Rc::new(self.sidebar_group_keys(cx));
        let groups = &self.sidebar_groups;
        let sections = sidebar_sections(&keys, groups);
        // A search outranks a fold. Typing something that matches a row inside
        // a folded group has to show that row — a box that says nothing
        // matches while the match sits behind a chevron is just lying.
        let folds_apply = query.is_empty();

        // ⌘N runs ActivateTabN, which goes through `activate_visual` — the
        // Nth row as the sidebar lays it out, not the Nth tab in `self.tabs`.
        // The badge has to be read off the same order or it names a chord that
        // opens a different tab, so take it from `visual_tab_order` rather than
        // flattening `sections` a second time here.
        let badge_pos: Vec<usize> = {
            let mut pos = vec![0usize; self.tabs.len()];
            for (n, i) in self.visual_tab_order(cx).into_iter().enumerate() {
                pos[i] = n;
            }
            pos
        };

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
            weight: FontWeight::SEMIBOLD,
            ..font.clone()
        };
        let rem = window.rem_size().as_f32();
        // Measure with the same interface scale used to paint the header.
        let header_size = rem * 0.75;
        let header_font = gpui::Font {
            weight: FontWeight::SEMIBOLD,
            ..font.clone()
        };
        // The diff counts in their resting weight: green still means added,
        // but twelve of them down a column no longer outshout the titles.
        let added_ink = crate::ui::presets::resting_ink(
            cx.theme().success,
            cx.theme().muted_foreground,
            cx.theme().sidebar,
        );
        let removed_ink = crate::ui::presets::resting_ink(
            cx.theme().danger,
            cx.theme().muted_foreground,
            cx.theme().sidebar,
        );
        let rendered = |ix: &usize| !visible_by_section[*ix].is_empty();
        // Every auto group draws a header, and a header is what there is to
        // grab, so these are the slots the group-reorder surface runs over.
        // Ungrouped is excluded: it is where the keyless tabs fall, and it
        // always sits last. Pinned groups sit above all of them, in an order
        // of their own.
        let keyed_slots: Vec<usize> = (0..sections.len())
            .filter(|&ix| {
                sections[ix]
                    .key
                    .as_ref()
                    .is_some_and(|k| k.auto().is_some())
            })
            .filter(rendered)
            .collect();
        let keyed_groups = keyed_slots.len();
        let group_slots: Rc<RefCell<Vec<Bounds<Pixels>>>> =
            Rc::new(RefCell::new(vec![Bounds::default(); keyed_groups]));
        let group_preview = reorder::preview(
            &self.reorder,
            &Surface::SidebarGroups,
            keyed_groups,
            pointer,
        );
        let keyed_keys: Vec<AutoKey> = keyed_slots
            .iter()
            .filter_map(|&ix| sections[ix].key.as_ref()?.auto().cloned())
            .collect();
        let slot_display: Vec<usize> = match &group_preview {
            Some(p) => {
                if let (Some(from), Some(to)) = (keyed_keys.get(p.from), keyed_keys.get(p.target))
                    && let Some(order) = regrouped_order(&keys, from, to)
                {
                    reorder::set_pending(&self.reorder, &Surface::SidebarGroups, order);
                }
                p.order.clone()
            }
            None => (0..keyed_groups).collect(),
        };
        let mut blocks: Vec<(Option<usize>, usize)> = (0..sections.len())
            .filter(|&ix| sections[ix].pinned().is_some())
            .filter(rendered)
            .map(|ix| (None, ix))
            .collect();
        blocks.extend(
            slot_display
                .into_iter()
                .map(|slot| (Some(slot), keyed_slots[slot])),
        );
        blocks.extend(
            (0..sections.len())
                .filter(|&ix| sections[ix].key.is_none())
                .filter(rendered)
                .map(|ix| (None, ix)),
        );

        for (group_slot, group_ix) in blocks {
            let section = &sections[group_ix];
            let group_key = section.key.clone();
            // Only a group that draws a header can be folded — there is
            // nothing to click otherwise, and the one headerless section (the
            // list below the pinned groups, when nothing else is there) must
            // never answer to Ungrouped's fold.
            let folded =
                section.name.is_some() && folds_apply && groups.is_folded(group_key.as_ref());
            let mut rows: Vec<ContextMenu<Stateful<Div>>> = Vec::new();
            // The header keeps counting every row the group has; folding only
            // stops them being drawn. Nothing downstream then registers a
            // rectangle for them, which is what keeps a pane from being
            // dropped into a group that is shut.
            //
            // No exception for the active tab. A fold that leaves one row
            // hanging under a shut chevron, with the header counting rows
            // that are not there, reads as a list that failed to load. The
            // cost is that ⌘T inside a folded group — `spawn_group` seeds
            // the new tab with the group it came from — puts the new tab
            // behind the chevron: the pane area shows the fresh shell and the
            // header count goes up, but the row waits for the group to open.
            let row_count = visible_by_section[group_ix].len();
            let visible: Vec<usize> = match folded {
                true => Vec::new(),
                false => visible_by_section[group_ix].clone(),
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
            // A group whose rows all sit on the same branch with the same
            // diff says so once, on its header, instead of once per row.
            // Four copies of `pr-818 +94 −26` under one heading describe the
            // repo, not the tabs, and being the only coloured text in the
            // column they were also the loudest thing in it. Read off every
            // row the group counts rather than the ones it draws, so a folded
            // group still names its branch.
            //
            // A lone row is no exception. Its branch describes the same repo
            // the heading above it names, and leaving it down there gave a
            // one-tab group a shape no other group in the column has: a
            // bare heading over a two-line row. It lifts like any other.
            let shared_git: Option<SharedGit> = section.name.as_ref().and_then(|_| {
                let rows = &visible_by_section[group_ix];
                if rows.is_empty() {
                    return None;
                }
                // Only rows that *have* a status get a vote. A tab that was
                // just opened has none until its shell reports a directory
                // and the poll comes back; counting it as a disagreement
                // pulled the branch off the header and grew a branch line
                // under every sibling for the half second it took, then
                // folded them all back — the column jumped twice for every
                // ⌘T. Unknown is not different; it is not yet known.
                let mut known = rows
                    .iter()
                    .filter_map(|&i| Some((i, self.tabs[i].git_status(Some(window), cx)?)));
                let (first, status) = known.next()?;
                let same = known.all(|(_, other)| other == status);
                same.then(|| SharedGit {
                    status,
                    click: git_click(&self.tabs[first], window, cx),
                    rows: rows.clone(),
                })
            });
            for (slot, i) in visible.into_iter().enumerate() {
                let badge_pos = badge_pos[i];
                let tab = &self.tabs[i];
                let is_active = i == active;
                let ssh_dot = self.tab_ssh_dot(tab, cx);
                let agent = tab.agent(cx);
                let agent_status = tab.agent_status(cx);
                let agent_unread = tab.agent_unread_count(cx);
                let git_cwd = git_click(tab, window, cx);
                let badge_extra = if show_badges && badge_pos < 9 {
                    row_metrics::BADGE + row_metrics::GAP
                } else {
                    0.
                };
                let zoomed = self.tab_is_zoomed(i);
                let zoom_extra = if zoomed {
                    row_metrics::ZOOM + row_metrics::GAP
                } else {
                    0.
                };
                // Elision is measured against this budget so the label and
                // branch never wrap or overflow into CSS truncation.
                let label_avail =
                    (row_metrics::text_budget(width) - badge_extra - zoom_extra).max(48.);
                let title_size = 0.875 * rem;
                let meta_size = 0.75 * rem;
                let title_font = if is_active { &title_font_active } else { &font };
                // Title: the *full* label, elided further down once the
                // working directory beside it has said how much of the line
                // it wants. A wide sidebar shows the whole thing and a narrow
                // one keeps whichever end identifies it — the tail for a
                // path, both edges for anything else. A fixed segment cap
                // (`short_title`) would elide even when the row has room, so
                // only the width may decide here.
                //
                // `full_title` is the unelided string the card can expand
                // back to; `None` means the row is showing a placeholder that
                // no card can improve on.
                let (title_text, full_title) =
                    if let Some(name) = tab.name.as_ref().filter(|n| !n.trim().is_empty()) {
                        // A renamed tab is elided like anything else — and so
                        // the card has to be able to spell the name back out.
                        let full = SharedString::from(name.trim().to_string());
                        (full.clone(), Some(full))
                    } else {
                        // The ladder the strip and the switcher climb, read
                        // here for the name and not for the shortening: this
                        // column measures in pixels and lets a card expand the
                        // row back to the whole string, so it wants what
                        // `label_of` would have cut down rather than the cut.
                        use crate::ui::machine_mirror::TabLabel;
                        let (view, home) = tab.label_view(Some(window), cx);
                        let raw = match view.label() {
                            TabLabel::Osc(title) | TabLabel::Cwd(title) => {
                                abbreviate_home(strip_host_prefix(title.trim()), home.as_deref())
                                    .into_owned()
                            }
                            TabLabel::Agent(agent) => agent.display_name().to_string(),
                            // A tab holding a name got one above.
                            TabLabel::Named(name) => name.to_string(),
                            TabLabel::Process(title) => title.to_string(),
                            TabLabel::Unknown => String::new(),
                        };
                        if raw.trim().is_empty() {
                            // Nothing to expand: the row is naming an unnamed
                            // shell, not hiding a title behind an ellipsis.
                            let placeholder = SharedString::from(t_fmt(
                                L10nKey::TabUnnamedShell,
                                &[("n", &((i + 1).to_string()))],
                            ));
                            (placeholder, None)
                        } else {
                            let full = SharedString::from(raw);
                            (full.clone(), Some(full))
                        }
                    };
                let mut branch_shown: Option<(SharedString, SharedString, u32, u32)> = None;
                let mut cwd_shown: Option<(SharedString, SharedString)> = None;
                let git_line = match shared_git.is_some() {
                    true => None,
                    false => tab.git_status(Some(window), cx),
                }
                .map(|g| {
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
                    let counts_w = counts_width(&window.text_system(), &font, meta_size, &g);
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
                            counts = counts
                                .child(div().text_color(added_ink).child(format!("+{}", g.added)));
                        }
                        if g.removed > 0 {
                            counts = counts.child(
                                div()
                                    .text_color(removed_ink)
                                    .child(format!("−{}", g.removed)),
                            );
                        }
                        line = line.child(counts);
                    }
                    line
                });
                // Outside a repo there is no branch line, and the working
                // directory rides on the title's own line rather than growing
                // a second one under it: a group of plain shells was a column
                // of two-line rows describing paths that mostly agree, which
                // is twice the height for a line of small grey text nobody
                // was reading. A row keeps its second line only for a branch.
                let cwd_full: Option<SharedString> = match git_line.is_none()
                    && shared_git.is_none()
                {
                    false => None,
                    true => tab
                        .pane
                        .focused_or_first(window, cx)
                        .and_then(|leaf| {
                            let leaf = leaf.read(cx);
                            Some((leaf.effective_cwd()?, leaf.display_home(cx)))
                        })
                        .map(|(cwd, home)| {
                            let text = cwd.display().to_string();
                            SharedString::from(abbreviate_home(&text, home.as_deref()).into_owned())
                        })
                        // The title already carries the whole path; a second
                        // copy adds noise, not information.
                        .filter(|full| full.as_ref() != title_text.as_ref()),
                };
                // The path takes what it needs up to half the line and the
                // title keeps the rest — the same split a group header makes
                // with the branch beside its heading. Flex would hand the
                // overflow to the two of them in proportion to what each
                // asked for, which cuts the longer string hardest.
                let cwd_want = cwd_full.as_ref().map(|full| {
                    row_metrics::META_GAP
                        + measure_text(&window.text_system(), &font, meta_size, full)
                });
                let title_avail = match cwd_want {
                    Some(want) => (label_avail - want.min(label_avail * 0.5)).max(ROW_TITLE_FLOOR),
                    None => label_avail,
                };
                let shown_title = elide_label(
                    &window.text_system(),
                    title_font,
                    title_size,
                    &title_text,
                    title_avail,
                );
                if let Some(full) = cwd_full {
                    // Measured against what the title actually took, not what
                    // it was allowed to: a short title hands the slack back
                    // instead of leaving the path elided around a gap.
                    let avail = (label_avail
                        - measure_text(
                            &window.text_system(),
                            title_font,
                            title_size,
                            &shown_title,
                        )
                        - row_metrics::META_GAP)
                        .max(0.);
                    if avail >= ROW_CWD_FLOOR {
                        let shown = elide_path_keep_tail(
                            &window.text_system(),
                            &font,
                            meta_size,
                            &full,
                            avail,
                        );
                        cwd_shown = Some((shown, full));
                    }
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
                let success = added_ink;
                let danger = removed_ink;

                // A row that grows a branch line under its title pads itself
                // out; a one-line row sits at `ROW_HEIGHT`.
                let two_line = git_line.is_some();
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
                            h_flex()
                                .w_full()
                                .items_center()
                                .gap_1p5()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_sm()
                                        .when(is_active, |d| d.font_weight(FontWeight::SEMIBOLD))
                                        .child(shown_title),
                                )
                                // The path is elided to the room the title
                                // left, so it may not shrink again here — a
                                // second cut would come out of its tail, the
                                // half that says which directory this is.
                                .when_some(cwd_shown.map(|(cwd, _)| cwd), |line, cwd| {
                                    line.child(
                                        div()
                                            .flex_shrink_0()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(cwd),
                                    )
                                }),
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
                    .min_h(px(ROW_HEIGHT))
                    .when(two_line, |s| s.py_1p5())
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .pl_2()
                    .pr_2()
                    .rounded(crate::ui::rounding::CARD_RADIUS)
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
                        row_metrics::AVATAR,
                        cx,
                    ))
                    // Leading, like the chip's: the trailing end of a row is
                    // the badge's, and the close button fades in over it.
                    .when(zoomed, |row| {
                        row.child(self.zoom_mark(("sidebar-zoom", i), cx))
                    })
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
                            cx.theme().sidebar_accent
                        } else {
                            gpui::rgb(sf.hover).into()
                        };
                        let mut fade_from = backing;
                        fade_from.a = 0.;
                        row.child(
                            h_flex()
                                .absolute()
                                .top(px(match two_line {
                                    true => 4.,
                                    false => (ROW_HEIGHT - crate::ui::tab_strip::MIN_TARGET) / 2.,
                                }))
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

            if row_count == 0 {
                continue;
            }

            let row_display: Vec<usize> = match &row_preview {
                Some(p) => {
                    if let Some(order) =
                        reordered_rows(&keys, groups, &group_key, &visible_tabs, p.from, p.target)
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
            // A pinned group carries a mark. It is the only thing separating
            // it on sight from a derived one — they behave differently (a
            // `cd` moves a tab out of a repo group and never out of this
            // one), and a group pinned on a real repo would otherwise print a
            // header identical to that repo's.
            let pinned_id = section.pinned();
            let pinned = pinned_id.is_some();
            let renaming_group = self
                .group_rename
                .as_ref()
                .filter(|r| Some(r.group) == pinned_id)
                .map(|r| r.input.clone());
            let header = section.name.clone().map(|name| {
                // The header packs a heading and the branch its whole group
                // shares onto one 11px line, and the branch is the unbounded
                // half of it: beside `fix/rpc-proxy-and-error-classification`,
                // `DELTA-NEUTRAL-BOT` came out as `DEL…`. Flex splits an
                // overflow between the two in proportion to how much room each
                // asked for, which is backwards here — the name is what the
                // group *is*, the branch only what it happens to be sitting
                // on. So both are measured against the header's real chrome:
                // the branch gets what it needs up to half the line, the name
                // keeps the rest, and each is elided into its share the way a
                // row already elides its own.
                let ts = window.text_system();
                let mut avail = row_metrics::header_budget(width);
                if pinned {
                    avail -= row_metrics::HEADER_ICON + row_metrics::META_GAP;
                }
                let count_label = row_count.to_string();
                if folded {
                    avail -=
                        measure_text(&ts, &font, header_size, &count_label) + row_metrics::META_GAP;
                }
                let avail = avail.max(HEADER_NAME_FLOOR);
                // What the shared branch would take if nothing were in its
                // way: the icon, the gap after it, the branch itself, the
                // counts, and the two gaps the spacer between the name and
                // the branch sits in.
                let git_want = shared_git.as_ref().map(|shared| {
                    let counts = counts_width(&ts, &font, header_size, &shared.status);
                    row_metrics::BRANCH_ICON
                        + 3. * row_metrics::META_GAP
                        + measure_text(&ts, &font, header_size, &shared.status.branch)
                        + counts
                });
                let name_avail = header_name_avail(avail, git_want);
                let label = elide_label(&ts, &header_font, header_size, &name, name_avail);
                let name_w = measure_text(&ts, &header_font, header_size, &label);
                let bar = h_flex()
                    .id(("sidebar-group", group_ix))
                    .w_full()
                    .items_center()
                    .gap_1p5()
                    .pl_2()
                    .pr_1p5()
                    // More above a heading than below it: the 12px is the
                    // generous interval in a column whose rows sit 2px apart,
                    // and it is what makes a group a group without a box.
                    .pt(px(12.))
                    .pb_1()
                    .text_size(px(header_size))
                    .text_color(cx.theme().muted_foreground)
                    .hover(|s| s.text_color(cx.theme().foreground))
                    .on_click(cx.listener({
                        let key = group_key.clone();
                        move |this, _, _window, cx| this.toggle_sidebar_group(key.as_ref(), cx)
                    }))
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
                    // The heading names the group; the chevron only says
                    // something when there is something behind it. An open
                    // group shows its rows, which is its own answer.
                    .when(folded, |header| {
                        header.child(
                            div()
                                .flex_shrink_0()
                                .child(Icon::new(IconName::ChevronRight).xsmall()),
                        )
                    })
                    .when(pinned, |header| {
                        header.child(
                            div()
                                .flex_shrink_0()
                                .child(Icon::new(IconName::Asterisk).xsmall()),
                        )
                    })
                    .child(match renaming_group {
                        Some(input) => div()
                            .id(("sidebar-group-rename", group_ix))
                            .flex_1()
                            .min_w_0()
                            // The header folds the group on click, so a click
                            // landing in the field would shut the very group
                            // whose name is being typed — and take the focus
                            // with it.
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(|_, _, cx| cx.stop_propagation())
                            .child(Input::new(&input).appearance(false))
                            .into_any_element(),
                        // Elided above, so the truncation here is only the
                        // backstop for a face that measures wider than it
                        // paints; the name no longer gives room to the branch.
                        None => div()
                            .flex_shrink_0()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            // Body ink, not caption grey: the heading is the
                            // name of what sits under it, and the branch and
                            // counts beside it are the metadata.
                            .text_color(cx.theme().foreground)
                            .child(label)
                            .into_any_element(),
                    })
                    .when_some(shared_git, |bar, shared| {
                        let SharedGit {
                            status,
                            click,
                            rows,
                        } = shared;
                        let branch_avail = (avail
                            - name_w
                            - row_metrics::BRANCH_ICON
                            - 3. * row_metrics::META_GAP
                            - counts_width(&ts, &font, header_size, &status))
                        .max(0.);
                        // Both ends, like a row's: the tail is what tells two
                        // branches off the same prefix apart.
                        let branch =
                            elide_keep_edges(&ts, &font, header_size, &status.branch, branch_avail);
                        let mut line = h_flex()
                            .id(("sidebar-group-git", group_ix))
                            .flex_shrink(1.)
                            .min_w_0()
                            .items_center()
                            .gap_1p5()
                            .child(
                                gpui::svg()
                                    .path("icons/git-branch.svg")
                                    .flex_shrink_0()
                                    .size(px(row_metrics::BRANCH_ICON))
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .child(div().min_w_0().truncate().child(branch));
                        if status.added > 0 || status.removed > 0 {
                            let mut counts = h_flex()
                                .id(("sidebar-group-diff", group_ix))
                                .flex_shrink_0()
                                .items_center()
                                .gap_1p5()
                                .when_some(click, |counts, (host, cwd)| {
                                    counts
                                        .cursor_pointer()
                                        .hover(|s| s.underline())
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(
                                                move |this, _: &MouseDownEvent, window, cx| {
                                                    cx.stop_propagation();
                                                    // The overlay opens over the
                                                    // active tab; make sure that
                                                    // is one of this group's,
                                                    // the same way a row's counts
                                                    // activate their row first.
                                                    if !rows.contains(&this.active)
                                                        && let Some(&first) = rows.first()
                                                    {
                                                        this.activate(first, window, cx);
                                                    }
                                                    this.toggle_diff_overlay(
                                                        host,
                                                        cwd.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                },
                                            ),
                                        )
                                });
                            if status.added > 0 {
                                counts = counts.child(
                                    div()
                                        .text_color(added_ink)
                                        .child(format!("+{}", status.added)),
                                );
                            }
                            if status.removed > 0 {
                                counts = counts.child(
                                    div()
                                        .text_color(removed_ink)
                                        .child(format!("−{}", status.removed)),
                                );
                            }
                            line = line.child(counts);
                        }
                        bar.child(div().flex_1()).child(line)
                    })
                    // The count is redundant while the rows are on screen; it
                    // is what a shut group has instead of them.
                    .when(folded, |bar| {
                        bar.child(div().flex_shrink_0().child(count_label))
                    });
                // Renaming is offered on a menu rather than a double click:
                // the first click of a double would fold the group, so the
                // name would be edited on a box that just shut. A repo group
                // gets no menu — it is named after its root, and a rename
                // there could only lie about where its tabs are.
                //
                // Attached last and erased to `AnyElement`, because the menu
                // wrapper changes the element's type and the two arms have to
                // agree.
                match pinned_id {
                    Some(id) => {
                        let app = cx.entity().downgrade();
                        bar.context_menu(move |menu, _window, _cx| {
                            let app = app.clone();
                            menu.item(PopupMenuItem::new(t(L10nKey::SidebarRenameGroup)).on_click(
                                move |_, window, cx| {
                                    let _ = app.update(cx, |this, cx| {
                                        this.start_group_rename(id, window, cx)
                                    });
                                },
                            ))
                        })
                        .into_any_element()
                    }
                    None => bar.into_any_element(),
                }
            });

            // A tab in the air makes the difference between the two kinds of
            // group visible: the ones that can take it stay lit, the ones
            // that cannot fade back. Until a drag is under way they look
            // alike, and this is where a user finds out which is which
            // without being told.
            let takes_drops = pinned;
            let block = v_flex()
                .w_full()
                .gap(px(ROW_GAP))
                .when(
                    group_preview
                        .as_ref()
                        .is_some_and(|p| Some(p.from) == group_slot),
                    |b| b.opacity(0.75),
                )
                .when(lifting_row && !takes_drops, |b| b.opacity(0.4))
                .when(
                    pinned_id.is_some_and(|id| over_group == Some(reorder::Regroup::Into(id))),
                    |b| b.rounded_md().bg(cx.theme().drag_border.opacity(0.15)),
                )
                .children(header)
                .children(rows)
                .child(
                    canvas(
                        {
                            let slots = group_slots.clone();
                            let landing = self.sidebar_group_slots.clone();
                            // Only a pinned group is recorded, so a drag
                            // looking for somewhere to land finds nothing over
                            // an auto group or over Ungrouped.
                            move |bounds, _window, _cx| {
                                if let Some(slot) = group_slot
                                    && let Some(s) = slots.borrow_mut().get_mut(slot)
                                {
                                    *s = bounds;
                                }
                                if let Some(id) = pinned_id {
                                    landing.borrow_mut().push((id, bounds));
                                }
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .inset_0(),
                );

            any_rows = true;
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

        // Keep navigation discoverable without requiring a hover over the rail.
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
                div()
                    .occlude()
                    .flex_shrink_0()
                    .child(self.new_tab_button("sidebar-add", cx)),
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
                    .tooltip_element(crate::ui::tab_strip::chord_tooltip(
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

        let top_bar = h_flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(6.))
            .h(px(ROW_HEIGHT))
            .mx_2()
            .mt_1p5()
            .mb_1()
            .pl(px(6.))
            .pr_1()
            .rounded_lg()
            .bg(cx.theme().muted)
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
                div().flex_1().min_w_0().child(
                    Input::new(&self.sidebar_search)
                        .appearance(false)
                        .cleanable(true)
                        .pl_0(),
                ),
            );

        let container: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));
        // Read while there is still a `cx` to read it from: the drag handler
        // below only ever sees a `Window`, and the cap it clamps against has to
        // be the same one the layout applies or the sidebar springs back from
        // wherever it was dropped.
        let others_floor = self.right_panel_floor(window, cx) + self.document_floor(cx);
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

    /// Where a tab being dragged in the sidebar would land if let go now,
    /// when that is somewhere other than a new place in its own group.
    ///
    /// Over a pinned group it did not come from, into that group; anywhere
    /// below the divider, back to auto grouping — but only for a tab that is
    /// in a pinned group, since one that is not already is auto grouped and
    /// a drag that would change nothing offers nothing, falling back to plain
    /// reordering.
    ///
    /// No auto group is a target. Its membership is decided by its tabs'
    /// cwds, so "put this tab in tty7" is a request the sidebar has no honest
    /// way to honour; the only thing a drop below the divider can mean is
    /// "stop keeping this tab by hand".
    fn sidebar_regroup_target(&self, window: &Window) -> Option<reorder::Regroup> {
        let dragged = crate::ui::reorder::dragged_sidebar_tab(&self.reorder)?;
        let here = self
            .tabs
            .iter()
            .find(|t| t.tree_id.get() == dragged)
            .and_then(|t| t.group.get())
            .filter(|g| self.sidebar_groups.contains(*g));
        let pointer = window.mouse_position();
        if let Some((id, _)) = self
            .sidebar_group_slots
            .borrow()
            .iter()
            .find(|(_, bounds)| bounds.contains(&pointer))
        {
            return (Some(*id) != here).then_some(reorder::Regroup::Into(*id));
        }
        let below = self
            .sidebar_divider
            .get()
            .is_some_and(|d| pointer.y >= d.origin.y);
        (below && here.is_some()).then_some(reorder::Regroup::ToAuto)
    }

    /// The pinned groups there are, in sidebar order, with the name each
    /// header reads — for menus that offer them.
    pub(crate) fn pinned_group_names(&self) -> Vec<(GroupId, String)> {
        let names = pinned_names(&self.sidebar_groups.pinned);
        self.sidebar_groups
            .pinned
            .iter()
            .map(|g| g.id)
            .zip(names)
            .collect()
    }

    /// Change this workspace's groups, and send the change to the machine so
    /// every other window onto the workspace draws it too.
    pub(crate) fn edit_groups(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut WorkspaceGroups),
    ) {
        edit(&mut self.sidebar_groups);
        // Up as one `WorkspaceSetGroups`, queued ahead of any `TabSetGroup`
        // the same edit made — which the save below sends.
        crate::ui::tree_sync::push_groups(cx, self.workspace, self.sidebar_groups.clone());
        self.save_session(cx);
        cx.notify();
    }

    /// Put the dragged tab where it was dropped.
    ///
    /// By id rather than index: a drag is several frames long, and a tab
    /// closing anywhere else in that time would shift every index after it.
    pub(crate) fn regroup_tab(
        &mut self,
        tab: tty7_core::core::machine::TabId,
        target: reorder::Regroup,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tabs.iter().position(|t| t.tree_id.get() == tab) else {
            return;
        };
        let group = match target {
            reorder::Regroup::Into(id) => Some(id),
            reorder::Regroup::ToAuto => None,
        };
        self.set_tab_group(index, group, cx);
    }

    /// Put tab `index` in pinned group `group`, or hand it back to auto
    /// grouping when `group` is `None`.
    pub(crate) fn set_tab_group(
        &mut self,
        index: usize,
        group: Option<GroupId>,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        tab.group.set(group);
        // Carries the move to the daemon as a `TabSetGroup`, so another
        // window on the same workspace sees it too.
        self.save_session(cx);
        cx.notify();
    }

    /// A name no pinned group is using yet, for a group about to be made.
    ///
    /// The placeholder only has to be unique — the rename box opens on it
    /// selected, so the first keystroke replaces it. Unique still matters:
    /// two headers reading the same name are two groups nobody can tell
    /// apart.
    fn fresh_group_name(&self) -> String {
        let taken: Vec<String> = self
            .pinned_group_names()
            .into_iter()
            .map(|(_, n)| n)
            .collect();
        let base = t(L10nKey::SidebarNewGroupName).to_string();
        (1..)
            .map(|n| match n {
                1 => base.clone(),
                n => format!("{base} {n}"),
            })
            .find(|candidate| !taken.contains(candidate))
            .expect("an unbounded range always reaches an untaken name")
    }

    /// Make a new label group, put tab `index` in it, and open its header for
    /// renaming.
    ///
    /// No dialog: the tab is in the group before a character is typed, so
    /// what the name is being given to is on screen while it is chosen.
    pub(crate) fn new_tab_group(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let group = PinnedGroup::label(self.fresh_group_name());
        let id = group.id;
        if let Some(tab) = self.tabs.get(index) {
            tab.group.set(Some(id));
        }
        self.edit_groups(cx, |groups| groups.pinned.push(group));
        self.start_group_rename(id, window, cx);
    }

    /// Make a new, empty label group and open its header for renaming — the
    /// palette's "New Group". It stays on screen empty until a tab is dragged
    /// into it or opened from its header.
    pub(crate) fn new_empty_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let group = PinnedGroup::label(self.fresh_group_name());
        let id = group.id;
        self.edit_groups(cx, |groups| groups.pinned.push(group));
        self.start_group_rename(id, window, cx);
    }

    /// Keep the auto group `key`: pin it, with the tabs in it now.
    ///
    /// A repo group becomes a folder group on the repo's home, so tabs that
    /// walk into the repo later join it too, and it reads the repo's name
    /// until renamed. A host group has no folder on this workspace's machine
    /// to keep, so it becomes a label group named after the host.
    ///
    /// The fold comes along: a group that was shut stays shut, rather than
    /// springing open in its new place.
    pub(crate) fn pin_auto_group(&mut self, key: AutoKey, cx: &mut Context<Self>) {
        let mut group = match &key {
            AutoKey::Repo(root) => PinnedGroup::folder(root),
            AutoKey::SshHost(host) => PinnedGroup::label(host.clone()),
        };
        group.collapsed = self.sidebar_groups.auto_collapsed.contains(&key);
        let id = group.id;
        let wanted = Some(GroupKey::Auto(key.clone()));
        for (tab, place) in self.tabs.iter().zip(self.sidebar_group_keys(cx)) {
            if place == wanted {
                tab.group.set(Some(id));
            }
        }
        self.edit_groups(cx, |groups| {
            groups.auto_collapsed.retain(|k| *k != key);
            groups.pinned.push(group);
        });
    }

    /// Pin `folder` as a group of its own — a folder dropped from Finder, or
    /// "Pin as Group" in the file tree. Pinning one that is already pinned
    /// does nothing: two groups keeping one folder would split its tabs
    /// between them by nothing but list order.
    ///
    /// Tabs already sitting in the folder are gathered in on the next frame,
    /// the way a tab walking in would be: the folder is new, so each of them
    /// has just "entered" it as far as [`EntryWatch`](crate::core::group_key::EntryWatch) can tell.
    pub(crate) fn pin_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        let spelled = folder.to_string_lossy();
        if self
            .sidebar_groups
            .pinned
            .iter()
            .any(|g| g.folder.as_deref() == Some(&*spelled))
        {
            return;
        }
        let group = PinnedGroup::folder(&folder);
        self.edit_groups(cx, |groups| groups.pinned.push(group));
    }

    /// Delete pinned group `id`. Its tabs are not closed — they go back to
    /// auto grouping, which is where a tab nobody filed by hand belongs.
    ///
    /// Unpinning a folder group is the same act: the group stops being kept,
    /// and its tabs fall back to the auto groups their cwds resolve to.
    pub(crate) fn delete_group(&mut self, id: GroupId, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            if tab.group.get() == Some(id) {
                tab.group.set(None);
            }
        }
        if self.group_rename.as_ref().is_some_and(|r| r.group == id) {
            self.group_rename = None;
        }
        self.edit_groups(cx, |groups| groups.pinned.retain(|g| g.id != id));
    }

    /// Point pinned group `id` at `folder`, or make it a label group when
    /// `folder` is `None`.
    ///
    /// A folder group nobody renamed reads its folder's name, and would read
    /// nothing once the folder is gone — so clearing the folder keeps the
    /// name it was showing.
    pub(crate) fn set_group_folder(
        &mut self,
        id: GroupId,
        folder: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        let shown = self
            .pinned_group_names()
            .into_iter()
            .find(|(g, _)| *g == id)
            .map(|(_, n)| n);
        self.edit_groups(cx, |groups| {
            let Some(group) = groups.get_mut(id) else {
                return;
            };
            if folder.is_none() && group.given_name().is_none() {
                group.name = shown;
            }
            group.folder = folder.map(|f| f.to_string_lossy().into_owned());
        });
    }

    /// Ask for a folder for pinned group `id` with the system picker. Only
    /// for a workspace on this machine: the picker browses this computer, and
    /// a path picked here means nothing to a remote one.
    pub(crate) fn pick_group_folder(&mut self, id: GroupId, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update(cx, |this, cx| this.set_group_folder(id, Some(path), cx));
            }
        })
        .detach();
    }

    /// The directory the active tab is working in, on the workspace's host —
    /// what "Use Current Tab's Folder" pins a group to.
    pub(crate) fn active_tab_folder(&self, window: &Window, cx: &gpui::App) -> Option<PathBuf> {
        let leaf = self
            .tabs
            .get(self.active)?
            .pane
            .focused_or_first(window, cx)?;
        leaf.read(cx).effective_host_cwd()
    }

    /// Open a tab in the group drawn under `key` — a header's "+" and its
    /// menu's "New Tab".
    ///
    /// A folder group opens in its folder, a repo group in its repo; a label
    /// group has no directory of its own and opens where ⌘T would. The tab
    /// joins a pinned group outright rather than waiting for its cwd to walk
    /// in: a label group has nothing to walk into, and "new tab here" should
    /// not depend on a probe.
    pub(crate) fn new_tab_in_group(
        &mut self,
        key: GroupKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let before = self.tabs.len();
        let folder = match &key {
            GroupKey::Pinned(id) => self
                .sidebar_groups
                .get(*id)
                .and_then(|g| g.folder_path().map(Path::to_path_buf)),
            GroupKey::Auto(AutoKey::Repo(root)) => Some(root.clone()),
            GroupKey::Auto(AutoKey::SshHost(_)) => None,
        };
        match folder {
            Some(folder) => self.new_tab_at(folder, window, cx),
            None => self.new_tab(window, cx),
        }
        if self.tabs.len() == before {
            return;
        }
        if let GroupKey::Pinned(id) = key
            && let Some(tab) = self.tabs.get(self.active)
        {
            tab.group.set(Some(id));
            self.save_session(cx);
        }
    }

    /// Put the pinned groups in `order` — indices into the list as it stands.
    pub(crate) fn apply_pinned_order(&mut self, order: &[usize], cx: &mut Context<Self>) {
        let pinned = &self.sidebar_groups.pinned;
        if order.len() != pinned.len() || order.iter().enumerate().all(|(i, &o)| i == o) {
            return;
        }
        let reordered: Vec<PinnedGroup> = order
            .iter()
            .filter_map(|&i| pinned.get(i).cloned())
            .collect();
        if reordered.len() != pinned.len() {
            return;
        }
        self.edit_groups(cx, |groups| groups.pinned = reordered);
    }

    /// Open the header of pinned group `id` for renaming.
    pub(crate) fn start_group_rename(
        &mut self,
        id: GroupId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self
            .pinned_group_names()
            .into_iter()
            .find(|(g, _)| *g == id)
            .map(|(_, n)| n)
        else {
            return;
        };
        let input = Self::rename_box(current, window, cx);
        let subs = vec![cx.subscribe_in(
            &input,
            window,
            |this, _input, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    this.commit_group_rename(window, cx)
                }
                _ => {}
            },
        )];
        self.group_rename = Some(crate::ui::app::GroupRename {
            group: id,
            input,
            _subs: subs,
        });
        cx.notify();
    }

    /// Write the typed name onto the group being renamed.
    pub(crate) fn commit_group_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.group_rename.take() else {
            return;
        };
        let value = rename.input.read(cx).value().trim().to_string();
        self.edit_groups(cx, |groups| {
            let Some(group) = groups.get_mut(rename.group) else {
                return;
            };
            match (value.is_empty(), group.folder.is_some()) {
                // A folder group handed a blank name goes back to reading
                // its folder's — which is how a rename is undone.
                (true, true) => group.name = None,
                // A label group has nothing to fall back on: a blank name
                // would draw an unlabelled header, so it keeps the one it had.
                (true, false) => {}
                (false, _) => group.name = Some(value),
            }
        });
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Fold the group drawn under `key` (`None`: Ungrouped), or unfold it if
    /// it is already shut. Stored with the workspace: a group folded away is a
    /// statement about work you are done with for now, and every window onto
    /// the workspace — and the next launch — should still show it shut.
    pub(crate) fn toggle_sidebar_group(&mut self, key: Option<&GroupKey>, cx: &mut Context<Self>) {
        self.edit_groups(cx, |groups| groups.toggle_folded(key));
    }

    /// Rule 2: a tab whose cwd walks into a pinned folder joins that group.
    ///
    /// Run once per frame, before anything asks where a tab is drawn. Every
    /// tab's [`EntryWatch`](crate::core::group_key::EntryWatch) is fed whether or not the tab is in a pinned group
    /// — that is what lets a tab dragged out of a folder group while it is
    /// still inside the folder stay out: the watch already knows it is
    /// inside, so staying there is no entry. Only a tab not already kept in a
    /// pinned group acts on an entry; a kept tab never leaves on its own.
    ///
    /// A tab is only looked at once its repo probe has answered. Before that
    /// the question "is it inside" has half an answer — the cwd without the
    /// repo home a worktree is matched by — and recording that half as where
    /// the tab *was* would read the other half landing a frame later as an
    /// entry, pulling every restored worktree tab into its group at launch.
    ///
    /// SSH panes are never looked at: their cwd is on another machine, and a
    /// folder pinned in this workspace names a directory on this one.
    pub(crate) fn settle_sidebar_groups(&mut self, cx: &mut Context<Self>) {
        let Some(cache) = cx.try_global::<GitStatusCache>() else {
            return;
        };
        let mut joined = false;
        for tab in &self.tabs {
            let Some(leaf) = tab.pane.first_leaf() else {
                continue;
            };
            let Some(view) = leaf.terminal() else {
                continue;
            };
            let view = view.read(cx);
            if view.remote_context().is_some() {
                continue;
            }
            let Some(cwd) = view.git_status_cwd() else {
                continue;
            };
            let Some(home) = cache.known_repo_for(view.host_id(), cwd) else {
                continue;
            };
            let inside = self.sidebar_groups.folder_for(Some(cwd), home.as_deref());
            let mut watch = tab.folder_watch.get();
            let entered = watch.observe(inside);
            tab.folder_watch.set(watch);
            let kept = tab
                .group
                .get()
                .is_some_and(|g| self.sidebar_groups.contains(g));
            if let Some(id) = entered
                && !kept
            {
                tab.group.set(Some(id));
                joined = true;
            }
        }
        if joined {
            self.save_session(cx);
        }
    }

    /// The auto group tab `tab` resolves to (rule 3): the host for an SSH
    /// pane, otherwise the repo its cwd is in, otherwise none (Ungrouped).
    ///
    /// Remembered on the tab, and the memory is what answers while there is
    /// nothing to go on — a probe in flight, or a pane (a WSL one, say) whose
    /// paths no probe reaches — so a tab does not bounce through Ungrouped
    /// between one answer and the next.
    fn tab_auto_group(&self, tab: &Tab, cx: &gpui::App) -> Option<AutoKey> {
        let resolved = tab.pane.first_leaf().and_then(|leaf| {
            let view = leaf.terminal()?.read(cx);
            let host = ssh_host(view.remote_context().as_ref());
            let known = view.git_status_cwd().and_then(|cwd| {
                cx.try_global::<GitStatusCache>()?
                    .known_repo_for(view.host_id(), cwd)
            });
            auto_key(host.as_deref(), known)
        });
        if let Some(key) = resolved {
            *tab.auto_group.borrow_mut() = key;
        }
        tab.auto_group.borrow().clone()
    }

    /// Where each tab is drawn, in `self.tabs` order: its pinned group, its
    /// auto group, or `None` for Ungrouped (or for the flat list below the
    /// pinned groups, with auto grouping off).
    pub(crate) fn sidebar_group_keys(&self, cx: &gpui::App) -> Vec<Option<GroupKey>> {
        let auto_grouping = cx.global::<Config>().sidebar_auto_grouping;
        self.tabs
            .iter()
            .map(|tab| {
                // Resolved even with auto grouping off, so the remembered
                // answer is warm the moment it is turned back on.
                let auto = self.tab_auto_group(tab, cx);
                place(tab.group.get(), &self.sidebar_groups, auto_grouping, auto)
            })
            .collect()
    }

    pub(crate) fn visual_tab_order(&self, cx: &gpui::App) -> Vec<usize> {
        if cx.global::<Config>().tab_bar_position != crate::core::config::TabBarPosition::Left {
            return (0..self.tabs.len()).collect();
        }
        let keys = self.sidebar_group_keys(cx);
        sidebar_sections(&keys, &self.sidebar_groups)
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

    /// Where a tab about to be spawned in `cwd` goes.
    ///
    /// A tab spawned from one kept in a pinned group joins it, before the cwd
    /// is consulted at all — the cwd says nothing about a group the user
    /// stated by hand. Every caller here spawns from the active tab (the ones
    /// that start from a named tab activate it first), so that is the one to
    /// inherit from. Without this, ⌘T inside a folded group would draw
    /// nothing but the header's count going up by one.
    ///
    /// Otherwise a cwd inside a pinned folder joins that folder's group, and
    /// the auto group is seeded from the repo cache when its probe for that
    /// directory has already landed. A tab's auto group otherwise starts
    /// empty and only fills in once its shell has reported a cwd, which parks
    /// every new tab in Ungrouped at the bottom of the sidebar until then; a
    /// tab spawned from one already in a repo inherits a warm cache, so this
    /// lands it in its group on the first frame.
    pub(crate) fn spawn_group(&self, cwd: Option<&Path>, cx: &gpui::App) -> SpawnPlace {
        let kept = self
            .tabs
            .get(self.active)
            .and_then(|t| t.group.get())
            .filter(|g| self.sidebar_groups.contains(*g));
        let known = cwd.and_then(|cwd| {
            let host = self
                .window_workspace(cx)
                .as_ref()
                .map_or(crate::ui::host_ops::HostId::LOCAL, |ws| ws.target.host_id());
            cx.try_global::<GitStatusCache>()?.known_repo_for(host, cwd)
        });
        let entered = cwd.and_then(|cwd| {
            let home = known.clone().flatten();
            self.sidebar_groups.folder_for(Some(cwd), home.as_deref())
        });
        SpawnPlace {
            group: kept.or(entered),
            auto: auto_key(None, known),
        }
    }
}

/// Where a tab about to be spawned goes, worked out before it exists — see
/// [`Tty7App::spawn_group`].
pub(crate) struct SpawnPlace {
    group: Option<GroupId>,
    auto: Option<Option<AutoKey>>,
}

impl SpawnPlace {
    /// Put `tab` where this says.
    pub(crate) fn seat(&self, tab: &Tab) {
        if let Some(id) = self.group {
            tab.group.set(Some(id));
        }
        if let Some(auto) = &self.auto {
            *tab.auto_group.borrow_mut() = auto.clone();
        }
    }
}

/// The host an SSH pane is on, when it is one.
///
/// A native SSH pane and a shell that has ssh'd onward from a local prompt
/// are grouped alike, by the target they name: in neither case does a `Host`
/// of ours reach the far side, so neither has a repo to probe — the first
/// reports a remote cwd that names nothing here, the second only the local
/// directory the `ssh` was typed in, which says nothing about where its shell
/// now is. The host is the one thing about either that is certain. A WSL pane
/// is not an SSH one: its distro is on this machine, and it keeps the group it
/// had.
fn ssh_host(remote: Option<&crate::daemon::protocol::RemoteContext>) -> Option<String> {
    use crate::daemon::protocol::RemoteKind;
    remote
        .filter(|r| matches!(r.kind, RemoteKind::Ssh | RemoteKind::NativeSsh))
        .map(|r| r.target.clone())
}

#[derive(Debug, PartialEq)]
struct Section {
    key: Option<GroupKey>,
    name: Option<String>,
    tabs: Vec<usize>,
}

impl Section {
    fn pinned(&self) -> Option<GroupId> {
        self.key.as_ref().and_then(GroupKey::pinned)
    }
}

/// The sidebar's sections, top to bottom: every pinned group in the user's
/// order — an empty one too, since a kept group stays until it is deleted —
/// then the auto groups in the order their first tab appears, then
/// Ungrouped.
///
/// Ungrouped only draws a header beside other groups below the divider. With
/// no auto group to set it apart from — grouping off, or nothing resolved
/// yet — it is just the list, and a header over the whole of it would be a
/// label on nothing. The same goes for a sidebar with no groups at all, which
/// comes out as one headerless section holding every tab.
fn sidebar_sections(keys: &[Option<GroupKey>], groups: &WorkspaceGroups) -> Vec<Section> {
    let members = |key: &GroupKey| -> Vec<usize> {
        (0..keys.len())
            .filter(|&i| keys[i].as_ref() == Some(key))
            .collect()
    };
    let mut auto_order: Vec<&AutoKey> = Vec::new();
    for k in keys.iter().flatten().filter_map(GroupKey::auto) {
        if !auto_order.contains(&k) {
            auto_order.push(k);
        }
    }
    let rest: Vec<usize> = (0..keys.len()).filter(|&i| keys[i].is_none()).collect();
    if groups.pinned.is_empty() && auto_order.is_empty() {
        return vec![Section {
            key: None,
            name: None,
            tabs: rest,
        }];
    }
    let pinned = pinned_names(&groups.pinned);
    let auto = auto_names(&auto_order);
    let mut sections: Vec<Section> = groups
        .pinned
        .iter()
        .zip(pinned)
        .map(|(g, name)| {
            let key = GroupKey::Pinned(g.id);
            Section {
                tabs: members(&key),
                key: Some(key),
                name: Some(name),
            }
        })
        .collect();
    sections.extend(auto_order.iter().zip(auto).map(|(k, name)| {
        let key = GroupKey::Auto((*k).clone());
        Section {
            tabs: members(&key),
            key: Some(key),
            name: Some(name),
        }
    }));
    if !rest.is_empty() {
        sections.push(Section {
            key: None,
            name: (!auto_order.is_empty()).then(|| t(L10nKey::SidebarUngroupedGroup).to_string()),
            tabs: rest,
        });
    }
    sections
}

fn reordered_rows(
    keys: &[Option<GroupKey>],
    groups: &WorkspaceGroups,
    group: &Option<GroupKey>,
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
    for g in sidebar_sections(keys, groups).iter().map(|s| &s.key) {
        if g == group {
            out.extend_from_slice(&members);
        } else {
            out.extend((0..keys.len()).filter(|&i| keys[i] == *g));
        }
    }
    Some(out)
}

/// The tab order that moves auto group `from` to where auto group `to` is.
///
/// Auto groups are ordered by where their first tab sits, so moving one is
/// moving its tabs. Pinned groups keep an order of their own and take no part:
/// their tabs follow the auto groups' here, where they draw no differently.
fn regrouped_order(keys: &[Option<GroupKey>], from: &AutoKey, to: &AutoKey) -> Option<Vec<usize>> {
    if from == to {
        return None;
    }
    let mut order: Vec<&AutoKey> = Vec::new();
    for k in keys.iter().flatten().filter_map(GroupKey::auto) {
        if !order.contains(&k) {
            order.push(k);
        }
    }
    let fi = order.iter().position(|g| *g == from)?;
    let ti = order.iter().position(|g| *g == to)?;
    let moved = order.remove(fi);
    order.insert(ti, moved);

    let auto_of = |i: usize| keys[i].as_ref().and_then(GroupKey::auto);
    let mut out: Vec<usize> = Vec::with_capacity(keys.len());
    for g in &order {
        out.extend((0..keys.len()).filter(|&i| auto_of(i) == Some(*g)));
    }
    out.extend((0..keys.len()).filter(|&i| auto_of(i).is_none()));
    Some(out)
}

/// The order of all pinned groups after the header in visible slot `from` is
/// dropped on slot `to`, as indices into `all`. `shown` is the pinned groups
/// on screen, in order — a search can hide some, and those keep their places.
fn reordered_pinned(
    all: &[GroupId],
    shown: &[GroupId],
    from: usize,
    to: usize,
) -> Option<Vec<usize>> {
    let (&moved, &anchor) = (shown.get(from)?, shown.get(to)?);
    if moved == anchor {
        return None;
    }
    let mut order: Vec<GroupId> = all.to_vec();
    order.retain(|g| *g != moved);
    let at = order.iter().position(|g| *g == anchor)? + usize::from(to > from);
    order.insert(at, moved);
    order
        .iter()
        .map(|g| all.iter().position(|a| a == g))
        .collect()
}

/// What each pinned group's header reads, in list order: the name the user
/// gave it, or its folder's last component — lengthened, like a repo's, when
/// two folders end the same way. A label group always has a given name.
fn pinned_names(pinned: &[PinnedGroup]) -> Vec<String> {
    let unnamed: Vec<PathBuf> = pinned
        .iter()
        .filter(|g| g.given_name().is_none())
        .filter_map(|g| g.folder_path().map(Path::to_path_buf))
        .collect();
    let mut from_folders = group_names(&unnamed.iter().collect::<Vec<_>>()).into_iter();
    pinned
        .iter()
        .map(|g| match (g.given_name(), g.folder_path()) {
            (Some(name), _) => name.to_string(),
            (None, Some(_)) => from_folders
                .next()
                .expect("group_names answers one name per folder"),
            (None, None) => String::new(),
        })
        .collect()
}

/// What each auto group's header reads, in `keys` order.
///
/// Only repo roots go through [`group_names`]. They are paths, so two of them
/// can perfectly well end in the same component and need lengthening until
/// they differ. A host group reads its `user@host` target as it is.
fn auto_names(keys: &[&AutoKey]) -> Vec<String> {
    let roots: Vec<&PathBuf> = keys
        .iter()
        .filter_map(|k| match k {
            AutoKey::Repo(p) => Some(p),
            AutoKey::SshHost(_) => None,
        })
        .collect();
    let mut disambiguated = group_names(&roots).into_iter();
    keys.iter()
        .map(|k| match k {
            AutoKey::Repo(_) => disambiguated
                .next()
                .expect("group_names answers one name per root"),
            AutoKey::SshHost(host) => host.clone(),
        })
        .collect()
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

/// Where a click on a tab's diff counts opens the overlay: the focused pane's
/// repo, when the setting allows a preview at all.
fn git_click(
    tab: &Tab,
    window: &Window,
    cx: &gpui::App,
) -> Option<(crate::ui::host_ops::HostId, PathBuf)> {
    diff_click_cwd(
        cx.global::<Config>(),
        tab.pane.focused_or_first(window, cx).and_then(|leaf| {
            let view = leaf.read(cx);
            let cwd = view.git_status_cwd()?.to_path_buf();
            Some((view.host_id(), cwd))
        }),
    )
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
mod fold_tests {
    use super::*;
    use crate::ui::app::test_window::harness_with_tabs;
    use gpui::TestAppContext;

    /// Bounds a row registered for itself while it was on screen. A folded
    /// row leaves the default rectangle behind, and that is what stops a pane
    /// being dropped into a group that is shut.
    fn drawn(app: &Tty7App, i: usize) -> bool {
        app.sidebar_slots.borrow()[i].size.height > px(0.)
    }

    /// Put tab `i` in a directory and tell the cache that directory is in the
    /// repo `root`, whose home is `home` — the same as `root` unless the
    /// checkout is a linked worktree. Both halves are needed: the sidebar
    /// reads the tab's cwd and looks it up in the cache, and either one
    /// missing makes it return "no decision", which would leave every group
    /// below untouched and every assertion about moving vacuous.
    fn plant(app: &Tty7App, i: usize, cwd: &str, root: &str, home: &str, cx: &mut gpui::App) {
        use crate::terminal::git_status::{GitStatusCache, RepoSnapshot};
        use crate::ui::host_ops::HostId;

        let cwd = PathBuf::from(cwd);
        let leaf = app.tabs[i].pane.first_leaf().expect("test tab has a pane");
        leaf.terminal()
            .expect("test pane is a terminal")
            .update(cx, |view, _| {
                view.set_git_status_cwd_for_test(Some(cwd.clone()))
            });
        cx.update_global::<GitStatusCache, _>(|cache, _| {
            cache.finish_probe(
                HostId::LOCAL,
                &cwd,
                Some(RepoSnapshot {
                    root: PathBuf::from(root),
                    home: PathBuf::from(home),
                    branch: "main".into(),
                    counts: Some((0, 0)),
                }),
            );
        });
    }

    fn plant_repo(app: &Tty7App, i: usize, cwd: &str, root: &str, cx: &mut gpui::App) {
        plant(app, i, cwd, root, root, cx);
    }

    /// Put tab `i` somewhere the cache knows is in no repo at all.
    fn plant_plain(app: &Tty7App, i: usize, cwd: &str, cx: &mut gpui::App) {
        use crate::terminal::git_status::GitStatusCache;
        use crate::ui::host_ops::HostId;

        let cwd = PathBuf::from(cwd);
        let leaf = app.tabs[i].pane.first_leaf().expect("test tab has a pane");
        leaf.terminal()
            .expect("test pane is a terminal")
            .update(cx, |view, _| {
                view.set_git_status_cwd_for_test(Some(cwd.clone()))
            });
        cx.update_global::<GitStatusCache, _>(|cache, _| {
            cache.finish_probe(HostId::LOCAL, &cwd, None);
        });
    }

    fn repo(s: &str) -> Option<GroupKey> {
        Some(GroupKey::Auto(AutoKey::Repo(PathBuf::from(s))))
    }

    /// Pin a label group called `name`, answering its id.
    fn label(app: &mut Tty7App, name: &str, cx: &mut Context<Tty7App>) -> GroupId {
        let group = PinnedGroup::label(name);
        let id = group.id;
        app.edit_groups(cx, |groups| groups.pinned.push(group));
        id
    }

    /// Pin `folder`, answering the group's id.
    fn folder(app: &mut Tty7App, folder: &str, cx: &mut Context<Tty7App>) -> GroupId {
        app.pin_folder(PathBuf::from(folder), cx);
        app.sidebar_groups
            .pinned
            .iter()
            .find(|g| g.folder.as_deref() == Some(folder))
            .expect("just pinned")
            .id
    }

    /// Rule 1. A tab kept in a pinned group is not the probe's to move:
    /// without this the cwd would drag a hand-placed tab back into its repo
    /// on the very next frame, and no amount of clicking would keep it where
    /// it was put.
    ///
    /// Tab 1 is the control. It is kept nowhere, so the same probe that must
    /// leave tab 0 alone has to file tab 1 — otherwise this test would pass
    /// just as well with the probe switched off entirely.
    #[gpui::test]
    fn a_probe_files_an_auto_tab_and_never_a_pinned_one(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 2);

        let work = app.update(&mut vcx, |app, cx| {
            let work = label(app, "work", cx);
            app.set_tab_group(0, Some(work), cx);
            for i in 0..2 {
                plant_repo(app, i, "/w/probed/sub", "/w/probed", cx);
            }
            cx.notify();
            work
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, cx| {
            let keys = app.sidebar_group_keys(cx);
            assert_eq!(
                keys[0],
                Some(GroupKey::Pinned(work)),
                "the pinned group survived a probe that had a real answer"
            );
            assert_eq!(
                keys[1],
                repo("/w/probed"),
                "and that same probe did file the tab that was kept nowhere"
            );
        });
    }

    /// Dropping a tab below the divider is the way out of a pinned group: the
    /// cwd decides again.
    #[gpui::test]
    fn a_tab_handed_back_to_auto_follows_its_cwd_again(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);

        let (work, dragged) = app.update(&mut vcx, |app, cx| {
            plant_repo(app, 0, "/w/probed/sub", "/w/probed", cx);
            let work = label(app, "work", cx);
            app.set_tab_group(0, Some(work), cx);
            (work, app.tabs[0].tree_id.get())
        });
        vcx.run_until_parked();
        app.update(&mut vcx, |app, cx| {
            assert_eq!(app.sidebar_group_keys(cx)[0], Some(GroupKey::Pinned(work)));
        });

        app.update(&mut vcx, |app, cx| {
            app.regroup_tab(dragged, reorder::Regroup::ToAuto, cx)
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, cx| {
            assert_eq!(
                app.sidebar_group_keys(cx)[0],
                repo("/w/probed"),
                "dropped below the divider, so the probe takes the tab back"
            );
        });
    }

    /// A drag is several frames long, so the tab it is carrying is named by
    /// id. Were it an index, any tab closing before the drop — in another
    /// window on the same workspace, or by a shell exiting — would shift it,
    /// and the drop would land on whichever tab slid into that slot.
    #[gpui::test]
    fn a_drop_finds_its_tab_after_the_indexes_shift(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 3);

        let dragged = app.update(&mut vcx, |app, _| app.tabs[2].tree_id.get());
        let work = app.update_in(&mut vcx, |app, window, cx| {
            let work = label(app, "work", cx);
            app.close_tab(0, window, cx);
            app.regroup_tab(dragged, reorder::Regroup::Into(work), cx);
            work
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, _| {
            let moved = app
                .tabs
                .iter()
                .find(|t| t.tree_id.get() == dragged)
                .expect("the dragged tab is still open");
            assert_eq!(
                moved.group.get(),
                Some(work),
                "the tab that was picked up is the tab that moved"
            );
            assert!(
                app.tabs
                    .iter()
                    .filter(|t| t.tree_id.get() != dragged)
                    .all(|t| t.group.get().is_none()),
                "and no bystander was regrouped in its place"
            );
        });
    }

    /// ⌘T inside a pinned group has to land in it. Otherwise the new tab
    /// goes wherever its cwd says, and if the group it was opened from is
    /// folded, the only thing that happens on screen is the header's count
    /// going up by one — the symptom #804 fixed for repo groups.
    #[gpui::test]
    fn a_tab_spawned_inside_a_pinned_group_joins_it(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);

        let work = app.update(&mut vcx, |app, cx| {
            // A probe that would send the tab somewhere else if it were
            // consulted, so this cannot pass by there being no answer.
            plant_repo(app, 0, "/w/probed/sub", "/w/probed", cx);
            let work = label(app, "work", cx);
            app.set_tab_group(0, Some(work), cx);
            app.active = 0;
            work
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, cx| {
            let place = app.spawn_group(Some(&PathBuf::from("/w/probed/sub")), cx);
            assert_eq!(
                place.group,
                Some(work),
                "the pinned group is inherited ahead of anything the cwd says"
            );
        });

        app.update(&mut vcx, |app, cx| app.set_tab_group(0, None, cx));
        vcx.run_until_parked();

        app.update(&mut vcx, |app, cx| {
            let place = app.spawn_group(Some(&PathBuf::from("/w/probed/sub")), cx);
            assert_eq!(place.group, None, "with nothing kept, nothing inherited");
            assert_eq!(
                place.auto,
                Some(Some(AutoKey::Repo(PathBuf::from("/w/probed")))),
                "and the cwd seeds the auto group from the warm cache"
            );
        });
    }

    /// A tab opened in a pinned folder is filed there before its shell has
    /// said a word — from the cwd it was opened in.
    #[gpui::test]
    fn a_tab_spawned_in_a_pinned_folder_joins_it(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);

        app.update(&mut vcx, |app, cx| {
            let tty7 = folder(app, "/w/tty7", cx);
            let place = app.spawn_group(Some(&PathBuf::from("/w/tty7/src")), cx);
            assert_eq!(place.group, Some(tty7));
            let place = app.spawn_group(Some(&PathBuf::from("/w/else")), cx);
            assert_eq!(place.group, None);
        });
    }

    /// A rename lands on the group itself: its tabs point at it by id, so
    /// nothing about them has to change.
    #[gpui::test]
    fn renaming_a_group_keeps_its_tabs_in_it(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 2);

        let work = app.update_in(&mut vcx, |app, window, cx| {
            let work = label(app, "work", cx);
            app.set_tab_group(0, Some(work), cx);
            app.set_tab_group(1, Some(work), cx);
            app.start_group_rename(work, window, cx);
            app.group_rename
                .as_ref()
                .expect("the box is up")
                .input
                .update(cx, |state, cx| state.set_value("urgent", window, cx));
            app.commit_group_rename(window, cx);
            work
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, _| {
            assert_eq!(app.pinned_group_names(), vec![(work, "urgent".to_string())]);
            assert!(app.tabs.iter().all(|t| t.group.get() == Some(work)));
        });
    }

    /// Clearing the box and dismissing it must not leave a label group with
    /// no name to draw; a folder group, which has its folder's name to fall
    /// back on, goes back to reading it.
    #[gpui::test]
    fn a_blank_rename_keeps_a_label_and_resets_a_folder(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);

        app.update_in(&mut vcx, |app, window, cx| {
            let work = label(app, "work", cx);
            let tty7 = folder(app, "/w/tty7", cx);
            app.edit_groups(cx, |groups| {
                groups.get_mut(tty7).expect("pinned").name = Some("mine".into())
            });
            for id in [work, tty7] {
                app.start_group_rename(id, window, cx);
                app.group_rename
                    .as_ref()
                    .expect("the box is up")
                    .input
                    .update(cx, |state, cx| state.set_value("   ", window, cx));
                app.commit_group_rename(window, cx);
            }
            assert_eq!(
                app.pinned_group_names(),
                vec![(work, "work".to_string()), (tty7, "tty7".to_string())],
                "the label kept its name; the folder went back to its own"
            );
        });
    }

    /// The placeholder only has to be unique — the box opens selected, so
    /// the first keystroke replaces it. But two headers with one name are two
    /// groups nobody can tell apart.
    #[gpui::test]
    fn a_second_new_group_does_not_land_on_the_first(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 2);

        app.update_in(&mut vcx, |app, window, cx| {
            app.new_tab_group(0, window, cx);
            app.commit_group_rename(window, cx);
            app.new_tab_group(1, window, cx);
            app.commit_group_rename(window, cx);
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, _| {
            let names = app.pinned_group_names();
            assert_eq!(names.len(), 2, "two groups, not one shared by both tabs");
            assert_ne!(names[0].1, names[1].1);
            assert_ne!(app.tabs[0].group.get(), app.tabs[1].group.get());
        });
    }

    /// "Auto grouping off" hides the groups the sidebar works out, and
    /// Ungrouped's header with them — but a pinned group is the user's, and
    /// still stands.
    #[gpui::test]
    fn auto_grouping_off_hides_auto_groups_and_keeps_pinned_ones(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 3);

        let work = app.update(&mut vcx, |app, cx| {
            let work = label(app, "work", cx);
            app.set_tab_group(0, Some(work), cx);
            plant_repo(app, 1, "/w/alpha", "/w/alpha", cx);
            plant_plain(app, 2, "/tmp", cx);
            let mut cfg = cx.global::<Config>().clone();
            cfg.sidebar_auto_grouping = false;
            cx.set_global(cfg);
            cx.notify();
            work
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, cx| {
            let keys = app.sidebar_group_keys(cx);
            assert_eq!(keys, vec![Some(GroupKey::Pinned(work)), None, None]);
            let sections = sidebar_sections(&keys, &app.sidebar_groups);
            assert_eq!(
                sections.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
                vec![Some("work".into()), None],
                "the pinned header, then a flat list with no header over it"
            );
        });

        app.update(&mut vcx, |_, cx| {
            let mut cfg = cx.global::<Config>().clone();
            cfg.sidebar_auto_grouping = true;
            cx.set_global(cfg);
            cx.notify();
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, cx| {
            let keys = app.sidebar_group_keys(cx);
            assert_eq!(
                keys,
                vec![Some(GroupKey::Pinned(work)), repo("/w/alpha"), None]
            );
            let sections = sidebar_sections(&keys, &app.sidebar_groups);
            assert_eq!(
                sections.last().and_then(|s| s.name.clone()),
                Some("Ungrouped".into()),
                "beside an auto group, the rest is Ungrouped again"
            );
        });
    }

    /// Rule 2: a tab walking into a pinned folder joins it.
    #[gpui::test]
    fn a_tab_walking_into_a_pinned_folder_joins_it(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 2);

        let probed = app.update(&mut vcx, |app, cx| {
            plant_plain(app, 0, "/w/elsewhere", cx);
            plant_plain(app, 1, "/w/elsewhere", cx);
            let probed = folder(app, "/w/probed", cx);
            cx.notify();
            probed
        });
        vcx.run_until_parked();
        app.update(&mut vcx, |app, cx| {
            plant_repo(app, 0, "/w/probed/sub", "/w/probed", cx);
            cx.notify();
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, cx| {
            assert_eq!(app.tabs[0].group.get(), Some(probed), "walked in: joined");
            assert_eq!(app.tabs[1].group.get(), None, "stayed out: did not");
            assert_eq!(
                app.sidebar_group_keys(cx)[0],
                Some(GroupKey::Pinned(probed)),
                "and the pinned folder beats the repo root it sits in"
            );
        });
    }

    /// Nested folders: the deepest one a tab is inside wins, so pinning a
    /// monorepo and a package in it files the package's tabs under the
    /// package.
    #[gpui::test]
    fn nested_pinned_folders_file_a_tab_under_the_deepest(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 2);

        let (mono, pkg) = app.update(&mut vcx, |app, cx| {
            let mono = folder(app, "/w/mono", cx);
            let pkg = folder(app, "/w/mono/pkg", cx);
            plant_repo(app, 0, "/w/mono/pkg/src", "/w/mono", cx);
            plant_repo(app, 1, "/w/mono/docs", "/w/mono", cx);
            cx.notify();
            (mono, pkg)
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, _| {
            assert_eq!(app.tabs[0].group.get(), Some(pkg));
            assert_eq!(app.tabs[1].group.get(), Some(mono));
        });
    }

    /// A linked worktree lives outside its main checkout, but its repo home
    /// is that checkout — so pinning the repo keeps its worktrees too.
    #[gpui::test]
    fn a_worktree_joins_the_folder_its_repo_home_is_pinned_as(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);

        let tty7 = app.update(&mut vcx, |app, cx| {
            let tty7 = folder(app, "/w/tty7", cx);
            plant(
                app,
                0,
                "/tmp/wt/feature/src",
                "/tmp/wt/feature",
                "/w/tty7",
                cx,
            );
            cx.notify();
            tty7
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, _| {
            assert_eq!(app.tabs[0].group.get(), Some(tty7));
        });
    }

    /// The edge the whole rule turns on. A tab dragged out of a folder group
    /// while it is still in the folder has been told where to go; it must not
    /// be pulled straight back, and only walking out and in again rejoins it.
    #[gpui::test]
    fn a_tab_dragged_out_is_not_pulled_back_until_it_re_enters(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);

        let probed = app.update(&mut vcx, |app, cx| {
            let probed = folder(app, "/w/probed", cx);
            plant_repo(app, 0, "/w/probed/sub", "/w/probed", cx);
            cx.notify();
            probed
        });
        vcx.run_until_parked();
        app.update(&mut vcx, |app, cx| {
            assert_eq!(
                app.tabs[0].group.get(),
                Some(probed),
                "joined on the way in"
            );
            app.set_tab_group(0, None, cx);
        });
        for _ in 0..3 {
            app.update(&mut vcx, |_, cx| cx.notify());
            vcx.run_until_parked();
        }
        app.update(&mut vcx, |app, cx| {
            assert_eq!(app.tabs[0].group.get(), None, "still inside: stays out");
            plant_plain(app, 0, "/w/elsewhere", cx);
            cx.notify();
        });
        vcx.run_until_parked();
        app.update(&mut vcx, |app, cx| {
            assert_eq!(app.tabs[0].group.get(), None, "left: nothing to join");
            plant_repo(app, 0, "/w/probed/sub", "/w/probed", cx);
            cx.notify();
        });
        vcx.run_until_parked();
        app.update(&mut vcx, |app, _| {
            assert_eq!(app.tabs[0].group.get(), Some(probed), "re-entered: joined");
        });
    }

    /// A tab restored from the machine tree was filed by whoever had it
    /// last. Sitting in a folder at launch is not walking into it.
    #[gpui::test]
    fn a_restored_tab_is_not_pulled_in_by_where_it_already_is(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);

        app.update(&mut vcx, |app, cx| {
            app.tabs[0]
                .folder_watch
                .set(crate::core::group_key::EntryWatch::baseline());
            folder(app, "/w/probed", cx);
            plant_repo(app, 0, "/w/probed/sub", "/w/probed", cx);
            cx.notify();
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, _| {
            assert_eq!(app.tabs[0].group.get(), None);
        });
    }

    /// Deleting a group closes nothing: its tabs go back to auto grouping.
    #[gpui::test]
    fn deleting_a_group_returns_its_tabs_to_auto_grouping(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 2);

        let work = app.update(&mut vcx, |app, cx| {
            let work = label(app, "work", cx);
            plant_repo(app, 0, "/w/alpha", "/w/alpha", cx);
            app.set_tab_group(0, Some(work), cx);
            app.set_tab_group(1, Some(work), cx);
            work
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, cx| app.delete_group(work, cx));
        vcx.run_until_parked();

        app.update(&mut vcx, |app, cx| {
            assert_eq!(app.tabs.len(), 2, "no tab was closed");
            assert!(app.sidebar_groups.pinned.is_empty());
            assert!(app.tabs.iter().all(|t| t.group.get().is_none()));
            assert_eq!(app.sidebar_group_keys(cx)[0], repo("/w/alpha"));
        });
    }

    /// Pinning an auto group keeps the tabs in it now, keeps its fold, and —
    /// for a repo — keeps its folder, so tabs walking in later join too.
    #[gpui::test]
    fn pinning_an_auto_group_keeps_its_tabs_and_its_folder(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 3);

        app.update(&mut vcx, |app, cx| {
            plant_repo(app, 0, "/w/alpha", "/w/alpha", cx);
            plant_repo(app, 1, "/w/alpha/src", "/w/alpha", cx);
            plant_repo(app, 2, "/w/beta", "/w/beta", cx);
            app.toggle_sidebar_group(repo("/w/alpha").as_ref(), cx);
            cx.notify();
        });
        vcx.run_until_parked();
        app.update(&mut vcx, |app, cx| {
            app.pin_auto_group(AutoKey::Repo(PathBuf::from("/w/alpha")), cx)
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, _| {
            let group = app.sidebar_groups.pinned.first().expect("pinned");
            assert_eq!(group.folder.as_deref(), Some("/w/alpha"));
            assert!(group.collapsed, "the fold came along");
            assert!(app.sidebar_groups.auto_collapsed.is_empty());
            assert_eq!(app.tabs[0].group.get(), Some(group.id));
            assert_eq!(app.tabs[1].group.get(), Some(group.id));
            assert_eq!(app.tabs[2].group.get(), None, "beta stays auto");
        });
    }

    /// A pinned group with no tabs is still drawn — it is kept until it is
    /// deleted — while an auto group with none simply is not there.
    #[gpui::test]
    fn an_empty_pinned_group_still_has_a_section(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);

        app.update(&mut vcx, |app, cx| {
            let work = label(app, "work", cx);
            let keys = app.sidebar_group_keys(cx);
            let sections = sidebar_sections(&keys, &app.sidebar_groups);
            assert_eq!(sections[0].key, Some(GroupKey::Pinned(work)));
            assert!(sections[0].tabs.is_empty());
        });
    }

    #[gpui::test]
    fn folding_a_group_takes_its_rows_off_the_sidebar(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 3);
        let alpha = AutoKey::Repo(PathBuf::from("/w/alpha"));
        let beta = AutoKey::Repo(PathBuf::from("/w/beta"));

        app.update(&mut vcx, |app, cx| {
            for (i, root) in [(0, &alpha), (1, &alpha), (2, &beta)] {
                *app.tabs[i].auto_group.borrow_mut() = Some(root.clone());
            }
            app.active = 2;
            cx.notify();
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, _| {
            assert!(
                (0..3).all(|i| drawn(app, i)),
                "every row is on screen before anything is folded"
            );
        });

        let alpha_key = GroupKey::Auto(alpha.clone());
        app.update(&mut vcx, |app, cx| {
            app.toggle_sidebar_group(Some(&alpha_key), cx)
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, _| {
            assert!(
                !drawn(app, 0) && !drawn(app, 1),
                "the folded group's rows left no rectangle behind"
            );
            assert!(drawn(app, 2), "the group next to it is untouched");
            assert_eq!(
                app.sidebar_groups.auto_collapsed,
                vec![alpha.clone()],
                "the fold is kept with the workspace, where the next launch reads it"
            );
        });

        app.update(&mut vcx, |app, cx| {
            app.toggle_sidebar_group(Some(&alpha_key), cx)
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, _| {
            assert!((0..3).all(|i| drawn(app, i)), "unfolding brings them back");
            assert!(
                app.sidebar_groups.auto_collapsed.is_empty(),
                "and takes the entry back out rather than piling up"
            );
        });
    }

    #[gpui::test]
    fn a_search_outranks_a_fold(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 2);
        let alpha = AutoKey::Repo(PathBuf::from("/w/alpha"));

        app.update(&mut vcx, |app, cx| {
            for i in 0..2 {
                *app.tabs[i].auto_group.borrow_mut() = Some(alpha.clone());
            }
            app.toggle_sidebar_group(Some(&GroupKey::Auto(alpha.clone())), cx);
        });
        vcx.run_until_parked();
        app.update(&mut vcx, |app, _| {
            assert!(!drawn(app, 1), "folded, so the row is not drawn");
        });

        // Whatever the row is actually showing — the label is derived from the
        // test process's cwd, and this has to be a query that matches it.
        app.update_in(&mut vcx, |app, window, cx| {
            let label = app.tab_label(&app.tabs[1], 1, Some(window), cx).to_string();
            app.sidebar_search.update(cx, |state, cx| {
                state.set_value(&label, window, cx);
            });
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, _| {
            assert!(
                drawn(app, 1),
                "a row a query matches has to show, fold or no fold"
            );
        });
    }

    /// A fold hides every row the group has, the active one included. The
    /// alternative — leaving the active row on screen under a shut chevron,
    /// with the header counting rows that are not drawn — looks like a list
    /// that failed to load, which is what folding a group you are working in
    /// used to produce.
    #[gpui::test]
    fn a_fold_hides_the_active_row_too(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 2);
        let alpha = GroupKey::Auto(AutoKey::Repo(PathBuf::from("/w/alpha")));

        app.update(&mut vcx, |app, cx| {
            for i in 0..2 {
                *app.tabs[i].auto_group.borrow_mut() = alpha.auto().cloned();
            }
            app.active = 0;
            app.toggle_sidebar_group(Some(&alpha), cx);
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, _| {
            assert!(!drawn(app, 0), "the active row folds away with the rest");
            assert!(!drawn(app, 1), "and so does everything else in the group");
        });

        app.update(&mut vcx, |app, cx| {
            app.toggle_sidebar_group(Some(&alpha), cx)
        });
        vcx.run_until_parked();

        app.update(&mut vcx, |app, _| {
            assert!((0..2).all(|i| drawn(app, i)), "unfolding brings both back");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// The auto group keyed on repo root `s`.
    fn g(s: &str) -> GroupKey {
        GroupKey::Auto(AutoKey::Repo(p(s)))
    }

    fn host(s: &str) -> GroupKey {
        GroupKey::Auto(AutoKey::SshHost(s.into()))
    }

    fn none() -> WorkspaceGroups {
        WorkspaceGroups::default()
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

    /// Two machines' `/home/ubuntu` are two directories: each host gets a
    /// group of its own, named for the host.
    #[test]
    fn ssh_tabs_on_two_hosts_land_in_two_groups() {
        let keys = [
            Some(host("ubuntu@alpha")),
            Some(host("ubuntu@beta")),
            Some(host("ubuntu@alpha")),
        ];
        let sections = sidebar_sections(&keys, &none());
        let shape: Vec<(Option<String>, Vec<usize>)> =
            sections.into_iter().map(|s| (s.name, s.tabs)).collect();
        assert_eq!(
            shape,
            vec![
                (Some("ubuntu@alpha".into()), vec![0, 2]),
                (Some("ubuntu@beta".into()), vec![1]),
            ]
        );
    }

    #[test]
    fn a_shell_that_sshd_onward_groups_by_host_and_wsl_does_not() {
        use crate::daemon::protocol::{RemoteContext, RemoteKind};
        let ctx = |kind| RemoteContext {
            kind,
            argv: vec![],
            target: "u@h".into(),
        };
        assert_eq!(
            ssh_host(Some(&ctx(RemoteKind::Ssh))).as_deref(),
            Some("u@h")
        );
        assert_eq!(
            ssh_host(Some(&ctx(RemoteKind::NativeSsh))).as_deref(),
            Some("u@h")
        );
        assert_eq!(ssh_host(Some(&ctx(RemoteKind::Wsl))), None);
        assert_eq!(ssh_host(None), None);
    }

    #[test]
    fn sections_order_groups_by_first_appearance_ungrouped_last() {
        let keys = vec![
            Some(g("/w/beta")),
            None,
            Some(g("/w/alpha")),
            Some(g("/w/beta")),
        ];
        let sections = sidebar_sections(&keys, &none());
        let shape: Vec<(Option<GroupKey>, Option<String>, Vec<usize>)> = sections
            .into_iter()
            .map(|s| (s.key, s.name, s.tabs))
            .collect();
        assert_eq!(
            shape,
            vec![
                (Some(g("/w/beta")), Some("beta".into()), vec![0, 3]),
                (Some(g("/w/alpha")), Some("alpha".into()), vec![2]),
                (None, Some("Ungrouped".into()), vec![1]),
            ]
        );

        let flat = sidebar_sections(&[None, None], &none());
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].name, None);
        assert_eq!(flat[0].tabs, vec![0, 1]);
    }

    /// Pinned groups come first, in the order the user gave them, whatever
    /// order their tabs are in — and an empty one keeps its place.
    #[test]
    fn pinned_groups_lead_in_their_own_order_empty_or_not() {
        let mut groups = none();
        let (a, b, c) = (
            PinnedGroup::label("a"),
            PinnedGroup::folder(Path::new("/w/b")),
            PinnedGroup::label("c"),
        );
        groups.pinned = vec![a.clone(), b.clone(), c.clone()];
        let keys = vec![
            Some(g("/w/r")),
            Some(GroupKey::Pinned(c.id)),
            Some(GroupKey::Pinned(a.id)),
            None,
        ];
        let sections = sidebar_sections(&keys, &groups);
        let shape: Vec<(Option<String>, Vec<usize>)> =
            sections.into_iter().map(|s| (s.name, s.tabs)).collect();
        assert_eq!(
            shape,
            vec![
                (Some("a".into()), vec![2]),
                (Some("b".into()), vec![]),
                (Some("c".into()), vec![1]),
                (Some("r".into()), vec![0]),
                (Some("Ungrouped".into()), vec![3]),
            ]
        );
    }

    /// With pinned groups and nothing auto-grouped below them, the rest is
    /// just the list — a header reading "Ungrouped" over all of it would be a
    /// label on nothing.
    #[test]
    fn below_pinned_groups_the_rest_needs_no_header_on_its_own() {
        let mut groups = none();
        let work = PinnedGroup::label("work");
        groups.pinned = vec![work.clone()];
        let keys = vec![Some(GroupKey::Pinned(work.id)), None];
        let sections = sidebar_sections(&keys, &groups);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[1].name, None);
        assert_eq!(sections[1].tabs, vec![1]);
    }

    /// The badge on a row and the tab ⌘N opens are two readings of one order,
    /// taken in two places. Grouping makes them diverge from `self.tabs`
    /// order — tab 3 sits in the second row here — so if they are ever read
    /// off different things, the badge names a chord that opens another tab.
    #[test]
    fn a_row_badge_names_the_chord_that_opens_that_row() {
        let keys = vec![
            Some(g("/w/beta")),
            None,
            Some(g("/w/alpha")),
            Some(g("/w/beta")),
        ];
        // What `visual_tab_order` returns for a left tab bar.
        let order: Vec<usize> = sidebar_sections(&keys, &none())
            .into_iter()
            .flat_map(|s| s.tabs)
            .collect();
        assert_eq!(order, vec![0, 3, 2, 1]);

        let mut badge_pos = vec![0usize; keys.len()];
        for (n, i) in order.iter().copied().enumerate() {
            badge_pos[i] = n;
        }
        for (row, tab) in order.iter().copied().enumerate() {
            // ActivateTabN → activate_visual(N - 1) → order[N - 1].
            let chord = tab_badge_label(badge_pos[tab]);
            let opens = order[badge_pos[tab]];
            assert_eq!(
                opens, tab,
                "row {row} badges ⌘{chord}, which opens tab {opens}"
            );
        }
    }

    #[test]
    fn reordered_rows_moves_within_the_group_only() {
        let keys = vec![
            Some(g("/w/alpha")),
            Some(g("/w/beta")),
            Some(g("/w/alpha")),
            None,
        ];
        let alpha = Some(g("/w/alpha"));
        assert_eq!(
            reordered_rows(&keys, &none(), &alpha, &[0, 2], 0, 1),
            Some(vec![2, 0, 1, 3])
        );
        assert_eq!(
            reordered_rows(&keys, &none(), &alpha, &[0, 2], 1, 0),
            Some(vec![2, 0, 1, 3])
        );
        assert_eq!(reordered_rows(&keys, &none(), &alpha, &[0, 2], 1, 1), None);
    }

    #[test]
    fn reordered_rows_leaves_filtered_out_rows_alone() {
        let keys = vec![Some(g("/w/a")), Some(g("/w/a")), Some(g("/w/a"))];
        let a = Some(g("/w/a"));
        assert_eq!(
            reordered_rows(&keys, &none(), &a, &[0, 2], 0, 1),
            Some(vec![1, 2, 0])
        );
    }

    /// A pinned group is where a tab was put, and rows move within it the
    /// same way they move within a repo group.
    #[test]
    fn rows_reorder_inside_a_pinned_group_too() {
        let mut groups = none();
        let work = PinnedGroup::label("work");
        groups.pinned = vec![work.clone()];
        let w = Some(GroupKey::Pinned(work.id));
        let keys = vec![w.clone(), Some(g("/w/beta")), w.clone()];
        assert_eq!(
            reordered_rows(&keys, &groups, &w, &[0, 2], 0, 1),
            Some(vec![2, 0, 1])
        );
    }

    #[test]
    fn regrouped_order_moves_the_group_into_the_target_slot() {
        let keys = vec![
            Some(g("/w/alpha")),
            None,
            Some(g("/w/beta")),
            Some(g("/w/alpha")),
            Some(g("/w/gamma")),
        ];
        let r = |s: &str| AutoKey::Repo(p(s));
        assert_eq!(
            regrouped_order(&keys, &r("/w/gamma"), &r("/w/alpha")),
            Some(vec![4, 0, 3, 2, 1])
        );
        assert_eq!(
            regrouped_order(&keys, &r("/w/alpha"), &r("/w/gamma")),
            Some(vec![2, 4, 0, 3, 1])
        );
    }

    #[test]
    fn regrouped_order_ignores_self_and_unknown_roots() {
        let keys = vec![Some(g("/w/alpha")), Some(g("/w/beta"))];
        let r = |s: &str| AutoKey::Repo(p(s));
        assert_eq!(regrouped_order(&keys, &r("/w/alpha"), &r("/w/alpha")), None);
        assert_eq!(regrouped_order(&keys, &r("/w/gone"), &r("/w/beta")), None);
        assert_eq!(regrouped_order(&keys, &r("/w/alpha"), &r("/w/gone")), None);
    }

    #[test]
    fn reordered_pinned_moves_a_header_among_the_ones_shown() {
        let ids: Vec<GroupId> = (0..4).map(|_| GroupId::new()).collect();
        // All four shown: dragging the first onto the third.
        assert_eq!(reordered_pinned(&ids, &ids, 0, 2), Some(vec![1, 2, 0, 3]));
        assert_eq!(reordered_pinned(&ids, &ids, 3, 0), Some(vec![3, 0, 1, 2]));
        assert_eq!(reordered_pinned(&ids, &ids, 1, 1), None);
        // A search hides the second; it keeps its place.
        let shown = [ids[0], ids[2], ids[3]];
        assert_eq!(reordered_pinned(&ids, &shown, 2, 0), Some(vec![3, 0, 1, 2]));
    }

    /// A label is the name the user typed. Running it through the path
    /// splitter would chop one containing a `/` into components and then
    /// "shorten" it to the tail, so `work/urgent` would print as `urgent`.
    #[test]
    fn a_label_is_never_shortened_the_way_a_path_is() {
        let pinned = vec![
            PinnedGroup::label("work/urgent"),
            PinnedGroup::folder(Path::new("/home/u/tty7")),
        ];
        assert_eq!(pinned_names(&pinned), vec!["work/urgent", "tty7"]);
    }

    /// Two folders ending in the same component grow a prefix until they
    /// differ, the way two repo roots do; a named one sits that out.
    #[test]
    fn pinned_folders_disambiguate_like_repo_roots() {
        let mut named = PinnedGroup::folder(Path::new("/home/u/other/app"));
        named.name = Some("mine".into());
        let pinned = vec![
            PinnedGroup::folder(Path::new("/home/u/work/app")),
            named,
            PinnedGroup::folder(Path::new("/home/u/fork/app")),
        ];
        assert_eq!(pinned_names(&pinned), vec!["work/app", "mine", "fork/app"]);
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

    /// A header with a long branch on it used to leave the heading as `DEL…`
    /// while the branch kept thirty characters. The name is the group; the
    /// branch is what it happens to be sitting on, and it may take at most
    /// half the line before the heading starts paying for it.
    #[test]
    fn a_long_branch_takes_half_the_header_and_no_more() {
        let avail = 200.;
        assert_eq!(header_name_avail(avail, Some(400.)), 100.);
        assert_eq!(header_name_avail(avail, Some(100.)), 100.);
    }

    #[test]
    fn a_short_branch_leaves_the_heading_the_rest_of_the_header() {
        assert_eq!(header_name_avail(200., Some(40.)), 160.);
        assert_eq!(header_name_avail(200., None), 200.);
    }

    /// Even a header narrow enough that the branch's half swallows the line
    /// keeps a readable stub of the name, rather than eliding it away.
    #[test]
    fn the_heading_keeps_a_floor_on_a_narrow_sidebar() {
        assert_eq!(header_name_avail(60., Some(400.)), HEADER_NAME_FLOOR);
    }
}

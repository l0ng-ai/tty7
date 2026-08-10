//! Dragging one split pane somewhere else in the same tab.
//!
//! The drag itself is gpui's — a pane's handle starts one and the pointer
//! carries it. What lives here is the part gpui cannot answer: where in the
//! layout the pointer is asking the pane to go, and whether that is a place
//! the tree can actually put it.
//!
//! Every landing is read against the pane under the pointer, from the middle
//! outwards:
//!
//! * **a pane's middle** — trade places with it, sizes included.
//! * **a pane's side** — the ring around that middle. Facing a neighbour in the
//!   same row or column, it means "go in beside them", and the newcomer takes
//!   an equal share of that row; facing across it, there is no row to join and
//!   it means "split this pane and take that side of it".
//! * **the far part of a side that is also the tab's own edge** — meaning
//!   "beside everything else", which is how a pane in the middle of a grid
//!   becomes a full-height column. Nothing else can express that: a drop read
//!   against a single pane can only ever split *that* pane. The band is sized
//!   to an even share of the columns (or rows) that side already has, so a
//!   third column is a third of the tab and not half of it.
//!
//! The last one is a share of the pane it is measured in, not a fixed strip
//! along the window. A strip wide enough to aim at on a 1400px tab is most of
//! a narrow pane, and one narrow enough to leave a narrow pane alone can only
//! be hit by accident on a wide one.
//!
//! The zone a pointer resolves to is only offered once the tree agrees it
//! changes something, so the highlight the user sees is never a promise the
//! drop will not keep.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Animation, AnimationExt as _, App, AppContext, Bounds, Context, EntityId, InteractiveElement,
    IntoElement, ParentElement, Pixels, Point, Render, StatefulInteractiveElement, Styled, Window,
    div, point, px, size,
};
use gpui_component::ActiveTheme as _;
use gpui_component::tooltip::Tooltip;

use crate::ui::i18n::{L10nKey, t};
use crate::ui::pane::{Dir, Pane};

/// The grip a pane is picked up by, cut to the shape Ghostty gives its own: a
/// target wide enough to aim at without looking, holding three dots small
/// enough to forget.
///
/// The target is there for as long as the pane can be moved — only the dots
/// come and go. Something the pointer can already grab before it can be seen
/// is what makes the mark itself free to stay this quiet.
const GRIP_TARGET: (f32, f32) = (80., 12.);
const GRIP_DOTS: usize = 3;
const GRIP_DOT: f32 = 3.;
const GRIP_DOT_GAP: f32 = 3.;

/// Where the dots sit in the target: up against its top edge, not centred in
/// it. The rest of the target is reach, not mark.
const GRIP_DOT_TOP: f32 = 2.;

/// The dots with the pointer somewhere in the band, and with it on the grip
/// itself. Nothing but the ink changes between the two: a grip that grew or
/// moved as the pointer closed in would be aiming at where it used to be.
const GRIP_IDLE: f32 = 0.3;
const GRIP_LIVE: f32 = 0.8;

/// Long enough to read as the dots arriving rather than as a flicker, short
/// enough to be over before a pointer crossing the band has settled.
const GRIP_FADE_MS: u64 = 150;

/// The name the dots watch for the pointer under. One name for every pane:
/// a group resolves to the nearest ancestor that carries it, which is always
/// the grip the dots are inside.
const GRIP_GROUP: &str = "pane-grip";

/// How much of a pane, down from its top edge, asks for the grip.
///
/// The pointer only has to be *near* the top of a pane, not on the grip, for
/// the dots to show — which is the whole reason a mark this quiet is findable.
/// A share of the pane rather than a fixed strip, so a short pane answers over
/// most of its top and a tall one over a slice, with a floor under it so the
/// band can never be thinner than the grip it is meant to reveal.
const REVEAL_SHARE: f32 = 0.2;
const REVEAL_MIN: f32 = 24.;

/// The strip a rearrangeable pane keeps clear above its grid for the grip.
///
/// Held open for as long as the tab has panes to rearrange, rather than only
/// while the dots show: the grid is measured from this box, so opening the
/// strip on hover would reflow the terminal every time the pointer crossed a
/// pane. Better to spend it once, when the tab gains its second pane and is
/// being reflowed anyway — which is also why it is sized to the *dots* and not
/// to the target around them. The target's last few pixels hang over the top
/// of the grid, where they cost a sliver of one row's clicks and cover
/// nothing, rather than being paid for in blank space above every pane.
pub(crate) const HANDLE_STRIP: f32 = GRIP_DOT_TOP + GRIP_DOT + 2.;

/// How much of a pane, in from a side that is also the tab's own edge, still
/// means "beside everything else" rather than "split this pane".
///
/// A share of the pane and not of the window: a fixed strip is either a hair's
/// breadth on a wide tab or the whole of a narrow pane. The floor keeps it
/// aimable when a pane is small, the ceiling keeps a huge pane's outer third
/// from being nothing but band.
const BAND_SHARE: f32 = 0.15;
const BAND_MIN: f32 = 32.;
const BAND_MAX: f32 = 120.;

/// The share of a pane, centred, that means "swap" rather than "split".
const SWAP_CORE: f32 = 0.34;

/// Where a dragged pane would land.
///
/// The target pane is named by `T`, which is a position in the tab's leaf
/// order as the geometry reads it off and the pane itself thereafter. The
/// change of name matters: the zone is read on one frame and carried out on
/// the next, and a pane that closed in between shifts every index after it.
/// Pinned to the pane, a drop whose target has gone is refused rather than
/// quietly redirected onto its neighbour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DropZone<T = usize> {
    /// Against an outer edge of the tab, beside every other pane.
    Edge(Dir),
    /// On one side of a pane, splitting it.
    Side(T, Dir),
    /// Trading places with a pane.
    Swap(T),
}

impl<T> DropZone<T> {
    /// Renames the zone's target, dropping the zone when `f` cannot find it.
    pub(crate) fn map<U>(self, f: impl FnOnce(T) -> Option<U>) -> Option<DropZone<U>> {
        Some(match self {
            DropZone::Edge(dir) => DropZone::Edge(dir),
            DropZone::Side(target, dir) => DropZone::Side(f(target)?, dir),
            DropZone::Swap(target) => DropZone::Swap(f(target)?),
        })
    }
}

/// gpui's payload for the drag. It renders nothing: the feedback that matters
/// is the landing lit up over the layout, and a shrunken copy of a terminal
/// under the cursor would say less than the empty space it covered.
pub(crate) struct DragPane;

impl Render for DragPane {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// The grip along a pane's top edge. Dragging it picks the pane up.
///
/// Offered for as long as the pane can be moved; `revealed` says only whether
/// the dots are drawn. A pointer that goes straight for the top of a pane can
/// press the grip on the frame it arrives, without waiting to be told the
/// thing it is already on is there.
pub(crate) fn handle(
    pane: EntityId,
    revealed: bool,
    state: &PaneDragState,
    cx: &App,
) -> gpui::AnyElement {
    let state = state.clone();
    let ink = cx.theme().foreground;
    div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .flex()
        .justify_center()
        .child(
            div()
                .id(("pane-drag-handle", pane.as_u64() as usize))
                .group(GRIP_GROUP)
                .w(px(GRIP_TARGET.0))
                .h(px(GRIP_TARGET.1))
                .flex()
                .items_start()
                .justify_center()
                .pt(px(GRIP_DOT_TOP))
                .map(crate::ui::reorder::cursor_grab)
                .tooltip(|window, cx| {
                    Tooltip::new(t(L10nKey::PaneDragHandleTooltip)).build(window, cx)
                })
                // The grip owns the strip the pane keeps clear for it, but the
                // press still has to be kept: without it the pane underneath
                // reads a grab as the start of a text selection.
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_drag(DragPane, move |_, _, _, cx| {
                    cx.stop_propagation();
                    begin(&state, pane);
                    cx.new(|_| DragPane)
                })
                .when(revealed, |d| d.child(dots(pane, ink))),
        )
        .into_any_element()
}

/// The mark itself, fading up as it arrives.
///
/// Only the ink answers the pointer, and it can do that from paint: a group
/// hover that had to settle a *size* would need the element state an id buys,
/// because the record of the group being hovered has to survive from one
/// frame's paint into the next frame's layout.
fn dots(pane: EntityId, ink: gpui::Hsla) -> impl IntoElement {
    div()
        .flex()
        .gap(px(GRIP_DOT_GAP))
        .children((0..GRIP_DOTS).map(move |_| {
            div()
                .size(px(GRIP_DOT))
                .rounded_full()
                .bg(ink.opacity(GRIP_IDLE))
                .group_hover(GRIP_GROUP, move |s| s.bg(ink.opacity(GRIP_LIVE)))
        }))
        .with_animation(
            ("pane-grip-fade", pane.as_u64() as usize),
            Animation::new(Duration::from_millis(GRIP_FADE_MS)).with_easing(gpui::ease_out_quint()),
            |dots, delta| dots.opacity(delta),
        )
}

/// The band across the top of a pane that reveals its grip.
///
/// It draws nothing and takes nothing: an ordinary hitbox does not stand in
/// front of the ones beneath it, so the terminal under the band still reads
/// its own presses, its own selection and its own links.
pub(crate) fn reveal_band(
    pane: EntityId,
    hovered: &Rc<Cell<Option<EntityId>>>,
) -> gpui::AnyElement {
    let hovered = hovered.clone();
    div()
        .id(("pane-grip-band", pane.as_u64() as usize))
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h(gpui::relative(REVEAL_SHARE))
        .min_h(px(REVEAL_MIN))
        .on_hover(move |over, window, _cx| {
            let next = match (*over, hovered.get() == Some(pane)) {
                (true, _) => Some(pane),
                (false, true) => None,
                // Left a band the pointer had already left: the enter of a
                // neighbour's got here first.
                (false, false) => return,
            };
            hovered.set(next);
            window.refresh();
        })
        .into_any_element()
}

/// A pane drag in flight, and the landing the last painted frame offered.
pub(crate) struct PaneDrag {
    from: EntityId,
    landing: Cell<Option<DropZone<EntityId>>>,
}

pub(crate) type PaneDragState = Rc<RefCell<Option<PaneDrag>>>;

/// Picks a pane up. Replaces any drag already in flight — a second one cannot
/// start while the first is held, so an old one still here was dropped on
/// nothing and never cleared.
pub(crate) fn begin(state: &PaneDragState, from: EntityId) {
    *state.borrow_mut() = Some(PaneDrag {
        from,
        landing: Cell::new(None),
    });
}

/// Which pane is being dragged, if one is.
pub(crate) fn lifted(state: &PaneDragState) -> Option<EntityId> {
    state.borrow().as_ref().map(|d| d.from)
}

/// Forgets last frame's landing, so a frame that offers none drops on nothing.
pub(crate) fn clear_landing(state: &PaneDragState) {
    if let Some(drag) = state.borrow().as_ref() {
        drag.landing.set(None);
    }
}

pub(crate) fn set_landing(state: &PaneDragState, zone: DropZone<EntityId>) {
    if let Some(drag) = state.borrow().as_ref() {
        drag.landing.set(Some(zone));
    }
}

/// Ends the drag, answering the landing it was over when it ended.
pub(crate) fn take_landing(state: &PaneDragState) -> Option<(EntityId, DropZone<EntityId>)> {
    let drag = state.borrow_mut().take()?;
    Some((drag.from, drag.landing.get()?))
}

/// Rearranges `pane` the way `zone` says, answering whether anything moved.
pub(crate) fn apply<L: Clone + PartialEq>(pane: &mut Pane<L>, from: &L, zone: DropZone<L>) -> bool {
    match zone {
        DropZone::Edge(dir) => pane.move_leaf_to_edge(from, dir),
        DropZone::Side(dst, dir) => pane.move_leaf_beside(from, &dst, dir),
        DropZone::Swap(dst) => pane.swap_leaves(from, &dst),
    }
}

/// Where the dragged pane would end up, as the patch of screen it would fill.
///
/// Worked out by carrying the drop out on a copy and measuring where the pane
/// landed, rather than by drawing what the rule is meant to do. The two can
/// only disagree if one of them is wrong, and this way the highlight is wrong
/// exactly when the drop is. `None` when the drop would change nothing, which
/// is also how the caller knows not to offer it.
pub(crate) fn landing<L: Clone + PartialEq>(
    pane: &Pane<L>,
    from: &L,
    zone: DropZone<L>,
    area: Bounds<Pixels>,
) -> Option<Bounds<Pixels>> {
    let mut trial = pane.deep_clone();
    if !apply(&mut trial, from, zone) {
        return None;
    }
    let index = trial.leaves().iter().position(|l| l == from)?;
    leaf_bounds(&trial, area).into_iter().nth(index)
}

/// Where the pointer is asking the pane to go, in a tab whose panes tile
/// `area` as `leaves`, in that same leaf order.
pub(crate) fn zone_at(
    area: Bounds<Pixels>,
    leaves: &[Bounds<Pixels>],
    pointer: Point<Pixels>,
) -> Option<DropZone> {
    let a = Quad::of(area);
    if a.w <= 0. || a.h <= 0. {
        return None;
    }
    let p = (pointer.x.as_f32(), pointer.y.as_f32());
    if p.0 < a.x || p.0 > a.x + a.w || p.1 < a.y || p.1 > a.y + a.h {
        return None;
    }

    // Panes tile the area, so a pointer on a shared border belongs to whichever
    // pane claims it first; nudging it inside the area keeps the far edges from
    // belonging to nobody.
    let inside = (
        p.0.min(a.x + a.w - 0.5).max(a.x),
        p.1.min(a.y + a.h - 0.5).max(a.y),
    );
    let (index, leaf) = leaves
        .iter()
        .map(|b| Quad::of(*b))
        .enumerate()
        .find(|(_, q)| q.holds(inside))?;
    if leaf.w <= 0. || leaf.h <= 0. {
        return None;
    }

    let nx = (inside.0 - leaf.x) / leaf.w;
    let ny = (inside.1 - leaf.y) / leaf.h;
    let core = (1. - SWAP_CORE) / 2.;
    if nx > core && nx < 1. - core && ny > core && ny < 1. - core {
        return Some(DropZone::Swap(index));
    }
    let sides = [
        (Dir::Left, nx),
        (Dir::Right, 1. - nx),
        (Dir::Up, ny),
        (Dir::Down, 1. - ny),
    ];
    let (dir, _) = sides
        .iter()
        .min_by(|(_, l), (_, r)| l.total_cmp(r))
        .expect("four sides");
    let dir = *dir;

    // Past the pane and out at the tab's own edge, the drop is about the tab:
    // the outer part of that side reads as "beside everything else". Measured
    // against the pane rather than the window, so it is a real target on a
    // small pane and does not swallow a large one.
    let (reach, gap) = match dir {
        Dir::Left => (leaf.w, inside.0 - leaf.x),
        Dir::Right => (leaf.w, leaf.x + leaf.w - inside.0),
        Dir::Up => (leaf.h, inside.1 - leaf.y),
        Dir::Down => (leaf.h, leaf.y + leaf.h - inside.1),
    };
    if leaf.is_flush(&a, dir) && gap <= (reach * BAND_SHARE).clamp(BAND_MIN, BAND_MAX) {
        return Some(DropZone::Edge(dir));
    }
    Some(DropZone::Side(index, dir))
}

/// Pane rectangles in window pixels, from the unit-square rectangles the tree
/// tiles itself with.
pub(crate) fn leaf_bounds<L: Clone>(pane: &Pane<L>, area: Bounds<Pixels>) -> Vec<Bounds<Pixels>> {
    pane.leaf_rects()
        .into_iter()
        .map(|(_, r)| Bounds {
            origin: point(
                area.origin.x + area.size.width * r.x,
                area.origin.y + area.size.height * r.y,
            ),
            size: size(area.size.width * r.w, area.size.height * r.h),
        })
        .collect()
}

#[derive(Clone, Copy)]
struct Quad {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Quad {
    fn of(b: Bounds<Pixels>) -> Quad {
        Quad {
            x: b.origin.x.as_f32(),
            y: b.origin.y.as_f32(),
            w: b.size.width.as_f32(),
            h: b.size.height.as_f32(),
        }
    }

    fn holds(&self, p: (f32, f32)) -> bool {
        p.0 >= self.x && p.0 < self.x + self.w && p.1 >= self.y && p.1 < self.y + self.h
    }

    /// Whether this rectangle's `dir` side lies on the same side of `outer` —
    /// that is, whether there is any pane beyond it in that direction.
    fn is_flush(&self, outer: &Quad, dir: Dir) -> bool {
        const SLACK: f32 = 1.;
        match dir {
            Dir::Left => self.x <= outer.x + SLACK,
            Dir::Right => self.x + self.w >= outer.x + outer.w - SLACK,
            Dir::Up => self.y <= outer.y + SLACK,
            Dir::Down => self.y + self.h >= outer.y + outer.h - SLACK,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(w), px(h)),
        }
    }

    fn area() -> Bounds<Pixels> {
        rect(0., 0., 1000., 600.)
    }

    /// 0 1
    /// 2 3
    fn grid() -> Vec<Bounds<Pixels>> {
        vec![
            rect(0., 0., 500., 300.),
            rect(500., 0., 500., 300.),
            rect(0., 300., 500., 300.),
            rect(500., 300., 500., 300.),
        ]
    }

    fn at(x: f32, y: f32) -> Option<DropZone> {
        zone_at(area(), &grid(), point(px(x), px(y)))
    }

    #[test]
    fn the_middle_of_a_pane_asks_to_trade_places() {
        assert_eq!(at(250., 150.), Some(DropZone::Swap(0)));
        assert_eq!(at(750., 450.), Some(DropZone::Swap(3)));
    }

    #[test]
    fn the_ring_around_a_pane_names_the_side_to_split_off() {
        // Sides that face another pane are the pane's own all the way out.
        assert_eq!(at(502., 150.), Some(DropZone::Side(1, Dir::Left)));
        assert_eq!(at(750., 302.), Some(DropZone::Side(3, Dir::Up)));
        // Sides that face the window keep the inner part of the ring.
        assert_eq!(at(900., 150.), Some(DropZone::Side(1, Dir::Right)));
        assert_eq!(at(750., 540.), Some(DropZone::Side(3, Dir::Down)));
    }

    #[test]
    fn the_far_part_of_a_side_facing_the_window_asks_for_a_band() {
        // 75px of a 500px-wide pane, 45px of a 300px-tall one.
        assert_eq!(at(4., 150.), Some(DropZone::Edge(Dir::Left)));
        assert_eq!(at(70., 150.), Some(DropZone::Edge(Dir::Left)));
        assert_eq!(at(996., 450.), Some(DropZone::Edge(Dir::Right)));
        assert_eq!(at(250., 3.), Some(DropZone::Edge(Dir::Up)));
        assert_eq!(at(250., 598.), Some(DropZone::Edge(Dir::Down)));
        assert_eq!(
            at(100., 150.),
            Some(DropZone::Side(0, Dir::Left)),
            "past the band the drop is about the pane again"
        );
        assert_eq!(
            at(750., 310.),
            Some(DropZone::Side(3, Dir::Up)),
            "the top of the bottom-right pane faces pane 1, not the window"
        );
    }

    #[test]
    fn a_band_is_a_share_of_its_pane_with_a_floor_under_it() {
        let single = |w: f32, h: f32| {
            let b = rect(0., 0., w, h);
            move |x: f32, y: f32| zone_at(b, &[b], point(px(x), px(y)))
        };

        // 15% of 400px is 60px of band, and the ring runs to 132px.
        let wide = single(400., 400.);
        assert_eq!(wide(40., 200.), Some(DropZone::Edge(Dir::Left)));
        assert_eq!(wide(100., 200.), Some(DropZone::Side(0, Dir::Left)));

        // 15% of 120px would be 18px, which is not a target anyone can hit.
        let narrow = single(120., 400.);
        assert_eq!(narrow(25., 200.), Some(DropZone::Edge(Dir::Left)));

        // 15% of 2000px would be 300px, most of the way to the middle.
        let huge = single(2000., 400.);
        assert_eq!(huge(110., 200.), Some(DropZone::Edge(Dir::Left)));
        assert_eq!(huge(200., 200.), Some(DropZone::Side(0, Dir::Left)));
    }

    #[test]
    fn a_pointer_off_the_tab_asks_for_nothing() {
        assert_eq!(at(-1., 150.), None);
        assert_eq!(at(150., 601.), None);
        assert_eq!(
            zone_at(rect(0., 0., 0., 0.), &[], point(px(0.), px(0.))),
            None
        );
    }

    #[test]
    fn a_pointer_on_a_shared_border_belongs_to_exactly_one_pane() {
        assert_eq!(at(500., 150.), Some(DropZone::Side(1, Dir::Left)));
        assert_eq!(at(250., 300.), Some(DropZone::Side(2, Dir::Up)));
        assert_eq!(
            at(1000., 600.),
            Some(DropZone::Edge(Dir::Right)),
            "the tab's own corner is a band drop, not a pane's"
        );
    }

    /// Three columns over a 900-wide tab: 0 across the left half, then 1 and 2
    /// sharing the right half.
    fn columns() -> Pane<u32> {
        Pane::split_node(
            gpui::Axis::Horizontal,
            0.5,
            Pane::leaf(0),
            Pane::split_node(gpui::Axis::Horizontal, 0.5, Pane::leaf(1), Pane::leaf(2)),
        )
    }

    #[test]
    fn a_landing_is_where_the_pane_actually_ends_up() {
        let tab = rect(0., 0., 900., 600.);
        let at = |zone| landing(&columns(), &2, zone, tab);

        assert_eq!(
            at(DropZone::Side(0, Dir::Right)),
            Some(rect(300., 0., 300., 600.)),
            "joining a row of columns is an equal share of it, not half of one"
        );
        assert_eq!(
            at(DropZone::Edge(Dir::Left)),
            Some(rect(0., 0., 300., 600.)),
            "a band beside two columns is the third of them"
        );
        assert_eq!(
            at(DropZone::Side(0, Dir::Down)),
            Some(rect(0., 300., 450., 300.)),
            "across the row there is no run to join, so the pane is halved"
        );
        assert_eq!(
            at(DropZone::Swap(0)),
            Some(rect(0., 0., 450., 600.)),
            "a swap takes the other pane's place, and its size"
        );
        assert_eq!(
            at(DropZone::Side(1, Dir::Right)),
            None,
            "2 already sits right of 1: nothing to draw and nothing to drop"
        );
        assert_eq!(
            at(DropZone::Swap(9)),
            None,
            "a zone naming a pane that is not in this tab lands nothing"
        );
    }

    #[test]
    fn a_landing_leaves_the_layout_it_was_measured_on_alone() {
        let live = columns();
        let before = live.leaf_rects();
        let _ = landing(
            &live,
            &2,
            DropZone::Side(0, Dir::Right),
            rect(0., 0., 900., 600.),
        );
        assert_eq!(
            live.leaf_rects(),
            before,
            "trying a drop out must not resize the panes on screen"
        );
    }

    #[test]
    fn leaf_bounds_lay_the_unit_square_over_the_pane_area() {
        let pane: Pane<u32> = Pane::split_node(
            gpui::Axis::Horizontal,
            0.25,
            Pane::leaf(0),
            Pane::split_node(gpui::Axis::Vertical, 0.5, Pane::leaf(1), Pane::leaf(2)),
        );
        let got = leaf_bounds(&pane, rect(10., 20., 1000., 600.));
        assert_eq!(got[0], rect(10., 20., 250., 600.));
        assert_eq!(got[1], rect(260., 20., 750., 300.));
        assert_eq!(got[2], rect(260., 320., 750., 300.));
    }
}

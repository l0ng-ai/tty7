//! Dragging one split pane somewhere else in the same tab.
//!
//! The drag itself is gpui's — a pane's handle starts one and the pointer
//! carries it. What lives here is the part gpui cannot answer: where in the
//! layout the pointer is asking the pane to go, and whether that is a place
//! the tree can actually put it.
//!
//! Three kinds of landing, in the order they are tested:
//!
//! * **the tab's own edge** — a band along the outside of the whole pane area,
//!   meaning "beside everything else", which is how a pane in the middle of a
//!   grid becomes a full-height column. Nothing else can express that: a drop
//!   read against a single pane can only ever split *that* pane.
//! * **a pane's side** — the outer ring of one pane, meaning "split this one
//!   and take that side of it".
//! * **a pane's middle** — trade places with it, sizes included.
//!
//! The zone a pointer resolves to is only offered once the tree agrees it
//! changes something, so the highlight the user sees is never a promise the
//! drop will not keep.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{
    App, AppContext, Bounds, Context, EntityId, InteractiveElement, IntoElement, ParentElement,
    Pixels, Point, Render, Size, StatefulInteractiveElement, Styled, Window, div, point, px, size,
};
use gpui_component::ActiveTheme as _;
use gpui_component::tooltip::Tooltip;

use crate::ui::i18n::{L10nKey, t};
use crate::ui::pane::{Dir, Pane, PaneSlot};

/// The width and height of the grip a pane is picked up by.
const HANDLE: (f32, f32) = (44., 9.);

/// How far from the tab's edge a drop still means "beside everything else".
/// Capped as a share of that side so the band never swallows a small window.
const EDGE_BAND: f32 = 26.;
const EDGE_BAND_SHARE: f32 = 0.12;

/// The share of a pane, centred, that means "swap" rather than "split".
const SWAP_CORE: f32 = 0.34;

/// Where a dragged pane would land.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DropZone {
    /// Against an outer edge of the tab, beside every other pane.
    Edge(Dir),
    /// On one side of a pane, splitting it. The pane is named by its index in
    /// the tab's leaf order.
    Side(usize, Dir),
    /// Trading places with a pane.
    Swap(usize),
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

/// The grip along a pane's top edge, offered while the pointer is over that
/// pane. Dragging it picks the pane up.
pub(crate) fn handle(pane: EntityId, state: &PaneDragState, cx: &App) -> gpui::AnyElement {
    let state = state.clone();
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
                .mt(px(2.))
                .w(px(HANDLE.0))
                .h(px(HANDLE.1))
                .rounded_full()
                .bg(cx.theme().border)
                .hover(|s| s.bg(cx.theme().drag_border))
                .cursor_grab()
                .tooltip(|window, cx| {
                    Tooltip::new(t(L10nKey::PaneDragHandleTooltip)).build(window, cx)
                })
                // The grip sits over the terminal's own grid. Keeping the press
                // stops the pane underneath from reading a grab as the start of
                // a text selection.
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_drag(DragPane, move |_, _, _, cx| {
                    cx.stop_propagation();
                    begin(&state, pane);
                    cx.new(|_| DragPane)
                }),
        )
        .into_any_element()
}

/// A pane drag in flight, and the landing the last painted frame offered.
pub(crate) struct PaneDrag {
    from: EntityId,
    landing: Cell<Option<DropZone>>,
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

pub(crate) fn set_landing(state: &PaneDragState, zone: DropZone) {
    if let Some(drag) = state.borrow().as_ref() {
        drag.landing.set(Some(zone));
    }
}

/// Ends the drag, answering the landing it was over when it ended.
pub(crate) fn take_landing(state: &PaneDragState) -> Option<(EntityId, DropZone)> {
    let drag = state.borrow_mut().take()?;
    Some((drag.from, drag.landing.get()?))
}

/// Rearranges `pane` the way `zone` says, answering whether anything moved.
///
/// `leaves` is the tab's leaf order, which is what a zone's index refers to.
pub(crate) fn apply(pane: &mut Pane, from: EntityId, leaves: &[PaneSlot], zone: DropZone) -> bool {
    match zone {
        DropZone::Edge(dir) => pane.move_leaf_to_edge(from, dir),
        DropZone::Side(index, dir) => match leaves.get(index) {
            Some(dst) => pane.move_leaf(from, dst.entity_id(), dir),
            None => false,
        },
        DropZone::Swap(index) => match leaves.get(index) {
            Some(dst) => pane.swap_leaves(from, dst.entity_id()),
            None => false,
        },
    }
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

    let bands = (
        EDGE_BAND.min(a.w * EDGE_BAND_SHARE),
        EDGE_BAND.min(a.h * EDGE_BAND_SHARE),
    );
    let outer = [
        (Dir::Left, p.0 - a.x, bands.0),
        (Dir::Right, a.x + a.w - p.0, bands.0),
        (Dir::Up, p.1 - a.y, bands.1),
        (Dir::Down, a.y + a.h - p.1, bands.1),
    ];
    let nearest_edge = outer
        .iter()
        .filter(|(_, gap, band)| gap <= band)
        .min_by(|(_, l, _), (_, r, _)| l.total_cmp(r));
    if let Some((dir, _, _)) = nearest_edge {
        return Some(DropZone::Edge(*dir));
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
    Some(DropZone::Side(index, *dir))
}

/// The patch of screen a drop would fill: the half of the tab or of the pane
/// the dragged pane is about to take, or the whole pane it is about to trade
/// places with.
pub(crate) fn landing_rect(
    area: Bounds<Pixels>,
    leaves: &[Bounds<Pixels>],
    zone: DropZone,
) -> Option<Bounds<Pixels>> {
    match zone {
        DropZone::Edge(dir) => Some(half(area, dir)),
        DropZone::Side(index, dir) => leaves.get(index).map(|b| half(*b, dir)),
        DropZone::Swap(index) => leaves.get(index).copied(),
    }
}

fn half(b: Bounds<Pixels>, dir: Dir) -> Bounds<Pixels> {
    let (w, h) = (b.size.width, b.size.height);
    let (o, s): (Point<Pixels>, Size<Pixels>) = match dir {
        Dir::Left => (b.origin, size(w / 2., h)),
        Dir::Right => (point(b.origin.x + w / 2., b.origin.y), size(w - w / 2., h)),
        Dir::Up => (b.origin, size(w, h / 2.)),
        Dir::Down => (point(b.origin.x, b.origin.y + h / 2.), size(w, h - h / 2.)),
    };
    Bounds { origin: o, size: s }
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
        assert_eq!(at(510., 150.), Some(DropZone::Side(1, Dir::Left)));
        assert_eq!(at(950., 150.), Some(DropZone::Side(1, Dir::Right)));
        assert_eq!(at(750., 310.), Some(DropZone::Side(3, Dir::Up)));
        assert_eq!(at(750., 570.), Some(DropZone::Side(3, Dir::Down)));
    }

    #[test]
    fn the_band_along_the_tab_asks_for_a_full_side_and_wins_over_the_pane() {
        assert_eq!(at(4., 150.), Some(DropZone::Edge(Dir::Left)));
        assert_eq!(at(996., 450.), Some(DropZone::Edge(Dir::Right)));
        assert_eq!(at(250., 3.), Some(DropZone::Edge(Dir::Up)));
        assert_eq!(at(250., 598.), Some(DropZone::Edge(Dir::Down)));
        assert_eq!(
            at(4., 3.),
            Some(DropZone::Edge(Dir::Up)),
            "a corner goes to whichever edge is nearer"
        );
        assert_eq!(
            at(30., 150.),
            Some(DropZone::Side(0, Dir::Left)),
            "past the band the drop is about the pane again"
        );
    }

    #[test]
    fn the_band_is_a_share_of_a_small_tab_not_a_fixed_reach() {
        let narrow = rect(0., 0., 100., 100.);
        let one = vec![narrow];
        let zone = |x: f32| zone_at(narrow, &one, point(px(x), px(50.)));
        assert_eq!(zone(11.), Some(DropZone::Edge(Dir::Left)));
        assert_eq!(
            zone(20.),
            Some(DropZone::Side(0, Dir::Left)),
            "26px of a 100px tab would be a quarter of it"
        );
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

    #[test]
    fn a_landing_is_the_patch_of_screen_the_pane_would_fill() {
        assert_eq!(
            landing_rect(area(), &grid(), DropZone::Edge(Dir::Right)),
            Some(rect(500., 0., 500., 600.))
        );
        assert_eq!(
            landing_rect(area(), &grid(), DropZone::Side(1, Dir::Down)),
            Some(rect(500., 150., 500., 150.))
        );
        assert_eq!(
            landing_rect(area(), &grid(), DropZone::Swap(2)),
            Some(rect(0., 300., 500., 300.))
        );
        assert_eq!(landing_rect(area(), &grid(), DropZone::Swap(9)), None);
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

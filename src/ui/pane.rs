use std::cell::Cell;
use std::rc::Rc;

use gpui::{App, Bounds, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Window, canvas, div};
use gpui::{Axis, Entity, prelude::*, px};
use gpui_component::ActiveTheme as _;

use crate::terminal::view::TerminalView;
use crate::ui::pending_pane::PendingPane;

const MIN_RATIO: f32 = 0.1;
const MAX_RATIO: f32 = 0.9;
const DIVIDER_THICKNESS: f32 = 5.;

#[derive(Clone)]
pub enum PaneSlot {
    Ready(Entity<TerminalView>),
    Connecting(Entity<PendingPane>),
}

impl PaneSlot {
    pub fn entity_id(&self) -> gpui::EntityId {
        match self {
            PaneSlot::Ready(v) => v.entity_id(),
            PaneSlot::Connecting(v) => v.entity_id(),
        }
    }

    pub fn terminal(&self) -> Option<&Entity<TerminalView>> {
        match self {
            PaneSlot::Ready(v) => Some(v),
            PaneSlot::Connecting(_) => None,
        }
    }

    pub fn contains_focused(&self, window: &Window, cx: &App) -> bool {
        match self {
            PaneSlot::Ready(v) => v.read(cx).focus_handle.contains_focused(window, cx),
            PaneSlot::Connecting(v) => v.read(cx).focus_handle.contains_focused(window, cx),
        }
    }

    pub fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        match self {
            PaneSlot::Ready(v) => v.read(cx).focus_handle.clone(),
            PaneSlot::Connecting(v) => v.read(cx).focus_handle.clone(),
        }
    }
}

pub enum Pane<L = PaneSlot> {
    Leaf(L),
    Split {
        axis: Axis,
        a: Box<Pane<L>>,
        b: Box<Pane<L>>,
        ratio: Rc<Cell<f32>>,
        dragging: Rc<Cell<bool>>,
    },
    Empty,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    fn axis(self) -> Axis {
        match self {
            Dir::Left | Dir::Right => Axis::Horizontal,
            Dir::Up | Dir::Down => Axis::Vertical,
        }
    }

    fn grows(self) -> bool {
        matches!(self, Dir::Right | Dir::Down)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

fn overlap_1d(a0: f32, alen: f32, b0: f32, blen: f32) -> f32 {
    ((a0 + alen).min(b0 + blen) - a0.max(b0)).max(0.0)
}

pub enum CloseOutcome {
    NotFound,
    Collapsed,
    RemoveSelf,
}

impl<L: Clone> Pane<L> {
    pub fn leaf(view: L) -> Self {
        Pane::Leaf(view)
    }

    pub fn split_node(axis: Axis, ratio: f32, a: Pane<L>, b: Pane<L>) -> Self {
        Pane::Split {
            axis,
            a: Box::new(a),
            b: Box::new(b),
            ratio: Rc::new(Cell::new(ratio.clamp(MIN_RATIO, MAX_RATIO))),
            dragging: Rc::new(Cell::new(false)),
        }
    }

    pub fn collect_leaves<'a>(&'a self, out: &mut Vec<L>) {
        match self {
            Pane::Leaf(v) => out.push(v.clone()),
            Pane::Split { a, b, .. } => {
                a.collect_leaves(out);
                b.collect_leaves(out);
            }
            Pane::Empty => {}
        }
    }

    pub fn leaves(&self) -> Vec<L> {
        let mut v = Vec::new();
        self.collect_leaves(&mut v);
        v
    }

    pub fn first_leaf(&self) -> Option<L> {
        match self {
            Pane::Leaf(v) => Some(v.clone()),
            Pane::Split { a, b, .. } => a.first_leaf().or_else(|| b.first_leaf()),
            Pane::Empty => None,
        }
    }

    pub fn leaf_matching_or_first(&self, pred: impl Fn(&L) -> bool) -> Option<L> {
        self.leaves()
            .into_iter()
            .find(|l| pred(l))
            .or_else(|| self.first_leaf())
    }

    fn split_leaf_where(
        &mut self,
        is_target: &impl Fn(&L) -> bool,
        axis: Axis,
        before: bool,
        new: L,
    ) -> bool {
        match self {
            Pane::Leaf(v) => {
                if is_target(v) {
                    let old = Pane::Leaf(v.clone());
                    let new = Pane::Leaf(new);
                    let (a, b) = if before { (new, old) } else { (old, new) };
                    *self = Pane::split_node(axis, 0.5, a, b);
                    true
                } else {
                    false
                }
            }
            Pane::Split { a, b, .. } => {
                a.split_leaf_where(is_target, axis, before, new.clone())
                    || b.split_leaf_where(is_target, axis, before, new)
            }
            Pane::Empty => false,
        }
    }

    fn replace_leaf_where(&mut self, is_target: &impl Fn(&L) -> bool, new: L) -> bool {
        match self {
            Pane::Leaf(v) => {
                if is_target(v) {
                    *v = new;
                    true
                } else {
                    false
                }
            }
            Pane::Split { a, b, .. } => {
                a.replace_leaf_where(is_target, new.clone()) || b.replace_leaf_where(is_target, new)
            }
            Pane::Empty => false,
        }
    }

    fn close_leaf_where(&mut self, is_target: &impl Fn(&L) -> bool) -> CloseOutcome {
        match self {
            Pane::Leaf(v) => {
                if is_target(v) {
                    CloseOutcome::RemoveSelf
                } else {
                    CloseOutcome::NotFound
                }
            }
            Pane::Split { .. } => {
                let a_outcome = if let Pane::Split { a, .. } = self {
                    a.close_leaf_where(is_target)
                } else {
                    unreachable!()
                };
                match a_outcome {
                    CloseOutcome::RemoveSelf => {
                        if let Pane::Split { b, .. } = std::mem::replace(self, Pane::Empty) {
                            *self = *b;
                        }
                        return CloseOutcome::Collapsed;
                    }
                    CloseOutcome::Collapsed => return CloseOutcome::Collapsed,
                    CloseOutcome::NotFound => {}
                }

                let b_outcome = if let Pane::Split { b, .. } = self {
                    b.close_leaf_where(is_target)
                } else {
                    unreachable!()
                };
                match b_outcome {
                    CloseOutcome::RemoveSelf => {
                        if let Pane::Split { a, .. } = std::mem::replace(self, Pane::Empty) {
                            *self = *a;
                        }
                        CloseOutcome::Collapsed
                    }
                    other => other,
                }
            }
            Pane::Empty => CloseOutcome::NotFound,
        }
    }

    fn collect_leaves_mut<'a>(&'a mut self, out: &mut Vec<&'a mut L>) {
        match self {
            Pane::Leaf(v) => out.push(v),
            Pane::Split { a, b, .. } => {
                a.collect_leaves_mut(out);
                b.collect_leaves_mut(out);
            }
            Pane::Empty => {}
        }
    }

    pub fn swap_leaf_indices(&mut self, i: usize, j: usize) -> bool {
        if i == j {
            return false;
        }
        let mut refs: Vec<&mut L> = Vec::new();
        self.collect_leaves_mut(&mut refs);
        let (lo, hi) = (i.min(j), i.max(j));
        if hi >= refs.len() {
            return false;
        }
        let (left, right) = refs.split_at_mut(hi);
        std::mem::swap(&mut *left[lo], &mut *right[0]);
        true
    }

    pub fn leaf_rects(&self) -> Vec<(L, Rect)> {
        let mut out = Vec::new();
        self.collect_rects(
            Rect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
            },
            &mut out,
        );
        out
    }

    fn collect_rects(&self, area: Rect, out: &mut Vec<(L, Rect)>) {
        match self {
            Pane::Leaf(v) => out.push((v.clone(), area)),
            Pane::Split {
                axis, a, b, ratio, ..
            } => {
                let r = ratio.get().clamp(MIN_RATIO, MAX_RATIO);
                match axis {
                    Axis::Horizontal => {
                        let aw = area.w * r;
                        a.collect_rects(Rect { w: aw, ..area }, out);
                        b.collect_rects(
                            Rect {
                                x: area.x + aw,
                                w: area.w - aw,
                                ..area
                            },
                            out,
                        );
                    }
                    Axis::Vertical => {
                        let ah = area.h * r;
                        a.collect_rects(Rect { h: ah, ..area }, out);
                        b.collect_rects(
                            Rect {
                                y: area.y + ah,
                                h: area.h - ah,
                                ..area
                            },
                            out,
                        );
                    }
                }
            }
            Pane::Empty => {}
        }
    }

    pub fn neighbor_in_direction(&self, from: usize, dir: Dir) -> Option<usize> {
        let rects = self.leaf_rects();
        let f = rects.get(from)?.1;
        const EPS: f32 = 1e-4;
        let mut best: Option<(usize, f32, f32)> = None;
        for (i, (_, c)) in rects.iter().enumerate() {
            if i == from {
                continue;
            }
            let (dist, overlap) = match dir {
                Dir::Left => (f.x - (c.x + c.w), overlap_1d(f.y, f.h, c.y, c.h)),
                Dir::Right => (c.x - (f.x + f.w), overlap_1d(f.y, f.h, c.y, c.h)),
                Dir::Up => (f.y - (c.y + c.h), overlap_1d(f.x, f.w, c.x, c.w)),
                Dir::Down => (c.y - (f.y + f.h), overlap_1d(f.x, f.w, c.x, c.w)),
            };
            if dist < -EPS || overlap <= EPS {
                continue;
            }
            let better = match best {
                None => true,
                Some((_, bd, bo)) => dist < bd - EPS || (dist <= bd + EPS && overlap > bo + EPS),
            };
            if better {
                best = Some((i, dist, overlap));
            }
        }
        best.map(|(i, _, _)| i)
    }

    pub fn resize_focused(&self, is_focused: &impl Fn(&L) -> bool, dir: Dir, step: f32) -> bool {
        let mut path: Vec<(&Pane<L>, bool)> = Vec::new();
        if !self.focus_path(is_focused, &mut path) {
            return false;
        }
        let target_axis = dir.axis();
        for (node, went_a) in path.iter().rev() {
            if let Pane::Split { axis, ratio, .. } = node {
                if *axis == target_axis {
                    let delta = if *went_a == dir.grows() { step } else { -step };
                    let r = (ratio.get() + delta).clamp(MIN_RATIO, MAX_RATIO);
                    ratio.set(r);
                    return true;
                }
            }
        }
        false
    }

    fn focus_path<'a>(
        &'a self,
        is_focused: &impl Fn(&L) -> bool,
        path: &mut Vec<(&'a Pane<L>, bool)>,
    ) -> bool {
        match self {
            Pane::Leaf(v) => is_focused(v),
            Pane::Split { a, b, .. } => {
                path.push((self, true));
                if a.focus_path(is_focused, path) {
                    return true;
                }
                path.pop();
                path.push((self, false));
                if b.focus_path(is_focused, path) {
                    return true;
                }
                path.pop();
                false
            }
            Pane::Empty => false,
        }
    }
}

impl Pane<PaneSlot> {
    pub fn focused_leaf(&self, window: &Window, cx: &App) -> Option<PaneSlot> {
        match self {
            Pane::Leaf(v) => v.contains_focused(window, cx).then(|| v.clone()),
            Pane::Split { a, b, .. } => a
                .focused_leaf(window, cx)
                .or_else(|| b.focused_leaf(window, cx)),
            Pane::Empty => None,
        }
    }

    pub fn focused_or_first_slot(&self, window: &Window, cx: &App) -> Option<PaneSlot> {
        self.focused_leaf(window, cx).or_else(|| self.first_leaf())
    }

    pub fn focused_or_first(&self, window: &Window, cx: &App) -> Option<Entity<TerminalView>> {
        self.focused_or_first_slot(window, cx)
            .and_then(|slot| slot.terminal().cloned())
    }

    pub fn terminals(&self) -> Vec<Entity<TerminalView>> {
        self.leaves()
            .iter()
            .filter_map(|slot| slot.terminal().cloned())
            .collect()
    }

    pub fn neighbor_in_dir(&self, dir: Dir, window: &Window, cx: &App) -> Option<PaneSlot> {
        let focused = self.focused_leaf(window, cx)?;
        let leaves = self.leaves();
        let from = leaves
            .iter()
            .position(|l| l.entity_id() == focused.entity_id())?;
        let target = self.neighbor_in_direction(from, dir)?;
        leaves.get(target).cloned()
    }

    pub fn resize_focused_pane(&self, dir: Dir, step: f32, window: &Window, cx: &App) -> bool {
        let Some(focused) = self.focused_leaf(window, cx) else {
            return false;
        };
        self.resize_focused(&|v| v.entity_id() == focused.entity_id(), dir, step)
    }

    pub fn focused_index(&self, window: &Window, cx: &App) -> Option<usize> {
        let focused = self.focused_leaf(window, cx)?;
        self.leaves()
            .iter()
            .position(|l| l.entity_id() == focused.entity_id())
    }

    pub fn split_leaf(
        &mut self,
        target: gpui::EntityId,
        axis: Axis,
        before: bool,
        new: PaneSlot,
    ) -> bool {
        self.split_leaf_where(&|v| v.entity_id() == target, axis, before, new)
    }

    pub fn replace_leaf(&mut self, target: gpui::EntityId, new: PaneSlot) -> bool {
        self.replace_leaf_where(&|v| v.entity_id() == target, new)
    }

    pub fn close_focused(&mut self, window: &Window, cx: &App) -> CloseOutcome {
        self.close_leaf_where(&|v| v.contains_focused(window, cx))
    }

    pub fn close_leaf(&mut self, target: gpui::EntityId) -> CloseOutcome {
        self.close_leaf_where(&|v| v.entity_id() == target)
    }

    pub fn render(
        &self,
        dim_inactive: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> gpui::AnyElement {
        match self {
            Pane::Empty => div().into_any_element(),
            Pane::Leaf(v) => {
                let focused = v.contains_focused(window, cx);
                div()
                    .size_full()
                    .relative()
                    .overflow_hidden()
                    .when(dim_inactive && !focused, |d| d.opacity(0.55))
                    .map(|d| match v {
                        PaneSlot::Ready(t) => d.child(t.clone()),
                        PaneSlot::Connecting(p) => d.child(p.clone()),
                    })
                    .into_any_element()
            }
            Pane::Split {
                axis,
                a,
                b,
                ratio,
                dragging,
            } => {
                let row = *axis == Axis::Horizontal;
                let r = ratio.get().clamp(MIN_RATIO, MAX_RATIO);

                let idle = cx.theme().border;
                let active = cx.theme().drag_border;

                let container: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));

                let backing = canvas(
                    {
                        let container = container.clone();
                        move |bounds, _window, _cx| container.set(Some(bounds))
                    },
                    {
                        let container = container.clone();
                        let ratio = ratio.clone();
                        let dragging = dragging.clone();
                        move |_bounds, _state, window, _cx| {
                            window.on_mouse_event({
                                let container = container.clone();
                                let ratio = ratio.clone();
                                let dragging = dragging.clone();
                                move |ev: &MouseMoveEvent, _phase, window, _cx| {
                                    if !dragging.get() {
                                        return;
                                    }
                                    let Some(b) = container.get() else {
                                        return;
                                    };
                                    let span = if row { b.size.width } else { b.size.height };
                                    if span.as_f32() <= 0.0 {
                                        return;
                                    }
                                    let offset = if row {
                                        ev.position.x - b.origin.x
                                    } else {
                                        ev.position.y - b.origin.y
                                    };
                                    let new_ratio = offset / span;
                                    ratio.set(new_ratio.clamp(MIN_RATIO, MAX_RATIO));
                                    window.refresh();
                                }
                            });
                            window.on_mouse_event({
                                let dragging = dragging.clone();
                                move |_ev: &MouseUpEvent, _phase, window, cx| {
                                    if dragging.get() {
                                        dragging.set(false);
                                        if let Some(app) =
                                            crate::ui::windows::WindowRegistry::app_in(cx, window)
                                        {
                                            app.update(cx, |app, cx| app.save_session(cx));
                                        }
                                        window.refresh();
                                    }
                                }
                            });
                        }
                    },
                )
                .absolute()
                .size_full();

                let line_color = if dragging.get() { active } else { idle };
                let divider = div()
                    .group("split-divider")
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(row, |d| {
                        d.w(px(DIVIDER_THICKNESS)).h_full().cursor_col_resize()
                    })
                    .when(!row, |d| {
                        d.h(px(DIVIDER_THICKNESS)).w_full().cursor_row_resize()
                    })
                    .child(
                        div()
                            .when(row, |d| d.w(px(1.)).h_full())
                            .when(!row, |d| d.h(px(1.)).w_full())
                            .bg(line_color)
                            .group_hover("split-divider", |s| s.bg(active)),
                    )
                    .on_mouse_down(MouseButton::Left, {
                        let dragging = dragging.clone();
                        move |_ev, window, _cx| {
                            dragging.set(true);
                            window.refresh();
                        }
                    });

                div()
                    .size_full()
                    .relative()
                    .flex()
                    .when(row, |d| d.flex_row())
                    .when(!row, |d| d.flex_col())
                    .child(backing)
                    .child(
                        div()
                            .flex_grow(r)
                            .flex_shrink(1.)
                            .flex_basis(px(0.))
                            .min_w_0()
                            .min_h_0()
                            .child(a.render(dim_inactive, window, cx)),
                    )
                    .child(divider)
                    .child(
                        div()
                            .flex_grow(1. - r)
                            .flex_shrink(1.)
                            .flex_basis(px(0.))
                            .min_w_0()
                            .min_h_0()
                            .child(b.render(dim_inactive, window, cx)),
                    )
                    .into_any_element()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestPane = Pane<u32>;

    fn is(id: u32) -> impl Fn(&u32) -> bool {
        move |v| *v == id
    }

    fn assert_well_formed(pane: &TestPane) {
        match pane {
            Pane::Leaf(_) => {}
            Pane::Split { a, b, ratio, .. } => {
                let r = ratio.get();
                assert!(
                    (MIN_RATIO..=MAX_RATIO).contains(&r),
                    "split ratio {r} escaped the legal band"
                );
                assert!(!matches!(**a, Pane::Empty), "split kept an Empty `a` child");
                assert!(!matches!(**b, Pane::Empty), "split kept an Empty `b` child");
                assert_well_formed(a);
                assert_well_formed(b);
            }
            Pane::Empty => panic!("Empty node left in a live tree"),
        }
    }

    fn split(pane: &mut TestPane, target: u32, axis: Axis, new: u32) {
        assert!(
            pane.split_leaf_where(&is(target), axis, false, new),
            "split target {target} not found"
        );
    }

    #[test]
    fn split_leaf_replaces_target_with_split_keeping_original_first() {
        let mut pane = TestPane::leaf(0);
        assert!(pane.split_leaf_where(&is(0), Axis::Horizontal, false, 1));
        match &pane {
            Pane::Split {
                axis, a, b, ratio, ..
            } => {
                assert!(matches!(axis, Axis::Horizontal));
                assert_eq!(ratio.get(), 0.5);
                assert!(matches!(**a, Pane::Leaf(0)));
                assert!(matches!(**b, Pane::Leaf(1)));
            }
            _ => panic!("split_leaf should replace the leaf with a Split node"),
        }
        assert_well_formed(&pane);
    }

    #[test]
    fn split_leaf_before_puts_the_new_pane_first() {
        let mut pane = TestPane::leaf(0);
        split(&mut pane, 0, Axis::Horizontal, 1);
        assert!(pane.split_leaf_where(&is(1), Axis::Horizontal, true, 2));
        assert_eq!(pane.leaves(), vec![0, 2, 1]);
        match &pane {
            Pane::Split { a, b, .. } => {
                assert!(matches!(**a, Pane::Leaf(0)), "sibling must not move");
                match &**b {
                    Pane::Split { a, b, .. } => {
                        assert!(matches!(**a, Pane::Leaf(2)));
                        assert!(matches!(**b, Pane::Leaf(1)));
                    }
                    _ => panic!("targeted leaf should have become a nested split"),
                }
            }
            _ => panic!("root should still be the original horizontal split"),
        }
        assert_well_formed(&pane);
    }

    #[test]
    fn split_leaf_splits_only_the_matching_leaf() {
        let mut pane = TestPane::leaf(0);
        split(&mut pane, 0, Axis::Horizontal, 1);
        split(&mut pane, 1, Axis::Vertical, 2);

        match &pane {
            Pane::Split { axis, a, b, .. } => {
                assert!(matches!(axis, Axis::Horizontal));
                assert!(
                    matches!(**a, Pane::Leaf(0)),
                    "untargeted leaf must stay a leaf"
                );
                match &**b {
                    Pane::Split { axis, a, b, .. } => {
                        assert!(matches!(axis, Axis::Vertical));
                        assert!(matches!(**a, Pane::Leaf(1)));
                        assert!(matches!(**b, Pane::Leaf(2)));
                    }
                    _ => panic!("targeted leaf should have become a nested split"),
                }
            }
            _ => panic!("root should still be the original horizontal split"),
        }
        assert_well_formed(&pane);
    }

    #[test]
    fn split_leaf_reports_missing_target_without_changing_tree() {
        let mut pane = TestPane::leaf(0);
        split(&mut pane, 0, Axis::Horizontal, 1);
        assert!(!pane.split_leaf_where(&is(99), Axis::Vertical, false, 2));
        assert_eq!(pane.leaves(), vec![0, 1]);
        assert_well_formed(&pane);
    }

    #[test]
    fn split_node_clamps_restored_ratio_into_legal_band() {
        for (given, expected) in [
            (0.0, MIN_RATIO),
            (-1.0, MIN_RATIO),
            (1.0, MAX_RATIO),
            (7.5, MAX_RATIO),
            (0.3, 0.3),
        ] {
            let node = TestPane::split_node(Axis::Vertical, given, Pane::Leaf(1), Pane::Leaf(2));
            match &node {
                Pane::Split { ratio, .. } => assert_eq!(ratio.get(), expected),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn leaves_and_first_leaf_follow_depth_first_a_before_b_order() {
        let mut pane = TestPane::leaf(0);
        split(&mut pane, 0, Axis::Horizontal, 1);
        split(&mut pane, 1, Axis::Vertical, 2);
        split(&mut pane, 0, Axis::Vertical, 3);
        assert_eq!(pane.leaves(), vec![0, 3, 1, 2]);
        assert_eq!(pane.first_leaf(), Some(0));
    }

    #[test]
    fn leaf_matching_or_first_prefers_the_match_then_falls_back_to_first() {
        let mut pane = TestPane::leaf(0);
        split(&mut pane, 0, Axis::Horizontal, 1);
        split(&mut pane, 1, Axis::Vertical, 2);
        assert_eq!(pane.leaves(), vec![0, 1, 2]);

        assert_eq!(pane.leaf_matching_or_first(is(2)), Some(2));
        assert_eq!(pane.leaf_matching_or_first(is(1)), Some(1));
        assert_eq!(pane.leaf_matching_or_first(is(99)), Some(0));
        assert_eq!(TestPane::Empty.leaf_matching_or_first(is(0)), None);
    }

    #[test]
    fn closing_the_root_leaf_defers_removal_to_the_caller() {
        let mut pane = TestPane::leaf(7);
        assert!(matches!(
            pane.close_leaf_where(&is(7)),
            CloseOutcome::RemoveSelf
        ));
        assert!(matches!(pane, Pane::Leaf(7)));
    }

    #[test]
    fn closing_first_child_promotes_second_child_to_root() {
        let mut pane = TestPane::leaf(0);
        split(&mut pane, 0, Axis::Horizontal, 1);
        assert!(matches!(
            pane.close_leaf_where(&is(0)),
            CloseOutcome::Collapsed
        ));
        assert!(matches!(pane, Pane::Leaf(1)));
    }

    #[test]
    fn closing_second_child_promotes_first_child_to_root() {
        let mut pane = TestPane::leaf(0);
        split(&mut pane, 0, Axis::Horizontal, 1);
        assert!(matches!(
            pane.close_leaf_where(&is(1)),
            CloseOutcome::Collapsed
        ));
        assert!(matches!(pane, Pane::Leaf(0)));
    }

    #[test]
    fn closing_nested_leaf_collapses_only_its_parent_split() {
        let mut pane = TestPane::split_node(
            Axis::Horizontal,
            0.3,
            Pane::Leaf(1),
            Pane::split_node(Axis::Vertical, 0.7, Pane::Leaf(2), Pane::Leaf(3)),
        );
        assert!(matches!(
            pane.close_leaf_where(&is(2)),
            CloseOutcome::Collapsed
        ));
        match &pane {
            Pane::Split {
                axis, a, b, ratio, ..
            } => {
                assert!(matches!(axis, Axis::Horizontal));
                assert_eq!(
                    ratio.get(),
                    0.3,
                    "outer split ratio must survive the collapse"
                );
                assert!(matches!(**a, Pane::Leaf(1)));
                assert!(matches!(**b, Pane::Leaf(3)));
            }
            _ => panic!("outer split must survive an inner collapse"),
        }
        assert_well_formed(&pane);
    }

    #[test]
    fn closing_a_leaf_promotes_entire_sibling_subtree() {
        let mut pane = TestPane::split_node(
            Axis::Horizontal,
            0.5,
            Pane::split_node(Axis::Vertical, 0.7, Pane::Leaf(1), Pane::Leaf(2)),
            Pane::Leaf(3),
        );
        assert!(matches!(
            pane.close_leaf_where(&is(3)),
            CloseOutcome::Collapsed
        ));
        match &pane {
            Pane::Split {
                axis, a, b, ratio, ..
            } => {
                assert!(matches!(axis, Axis::Vertical));
                assert_eq!(ratio.get(), 0.7, "promoted subtree must keep its own ratio");
                assert!(matches!(**a, Pane::Leaf(1)));
                assert!(matches!(**b, Pane::Leaf(2)));
            }
            _ => panic!("sibling subtree should have been promoted to the root"),
        }
        assert_well_formed(&pane);
    }

    #[test]
    fn close_reports_not_found_and_leaves_tree_untouched() {
        let mut pane = TestPane::leaf(0);
        split(&mut pane, 0, Axis::Horizontal, 1);
        assert!(matches!(
            pane.close_leaf_where(&is(99)),
            CloseOutcome::NotFound
        ));
        assert_eq!(pane.leaves(), vec![0, 1]);
        assert_well_formed(&pane);
    }

    #[test]
    fn close_removes_only_first_match_in_traversal_order() {
        let mut pane = TestPane::leaf(0);
        split(&mut pane, 0, Axis::Horizontal, 1);
        split(&mut pane, 1, Axis::Vertical, 2);
        assert!(matches!(
            pane.close_leaf_where(&|_| true),
            CloseOutcome::Collapsed
        ));
        assert_eq!(pane.leaves(), vec![1, 2]);
        assert_well_formed(&pane);
    }

    #[test]
    fn deep_split_close_sequence_preserves_invariants_and_leaf_order() {
        enum Op {
            Split(u32, Axis, u32),
            Close(u32),
        }
        use Op::*;
        let script = [
            Split(0, Axis::Horizontal, 1),
            Split(1, Axis::Vertical, 2),
            Split(0, Axis::Vertical, 3),
            Split(2, Axis::Horizontal, 4),
            Split(3, Axis::Horizontal, 5),
            Close(1),
            Close(0),
            Close(4),
            Split(2, Axis::Vertical, 6),
            Close(5),
            Close(3),
            Close(6),
        ];

        let mut pane = TestPane::leaf(0);
        let mut model = vec![0u32];
        for op in script {
            match op {
                Split(target, axis, new) => {
                    split(&mut pane, target, axis, new);
                    let at = model.iter().position(|&v| v == target).unwrap();
                    model.insert(at + 1, new);
                }
                Close(target) => {
                    assert!(
                        matches!(pane.close_leaf_where(&is(target)), CloseOutcome::Collapsed),
                        "closing {target} should collapse a split"
                    );
                    model.retain(|&v| v != target);
                }
            }
            assert_well_formed(&pane);
            assert_eq!(pane.leaves(), model, "tree leaves diverged from the model");
        }
    }

    #[test]
    fn closing_down_to_the_last_pane_hits_remove_self_boundary() {
        let mut pane = TestPane::leaf(0);
        split(&mut pane, 0, Axis::Horizontal, 1);
        split(&mut pane, 1, Axis::Vertical, 2);
        split(&mut pane, 0, Axis::Vertical, 3);

        while pane.leaves().len() > 1 {
            let target = pane.first_leaf().unwrap();
            assert!(matches!(
                pane.close_leaf_where(&is(target)),
                CloseOutcome::Collapsed
            ));
            assert_well_formed(&pane);
        }

        let last = pane.first_leaf().unwrap();
        assert!(matches!(
            pane.close_leaf_where(&is(last)),
            CloseOutcome::RemoveSelf
        ));
        assert!(
            matches!(pane, Pane::Leaf(_)),
            "last pane is dropped by the caller, not the tree"
        );
    }

    #[test]
    fn empty_placeholder_ignores_all_operations() {
        let mut pane: TestPane = Pane::Empty;
        assert!(pane.leaves().is_empty());
        assert_eq!(pane.first_leaf(), None);
        assert!(!pane.split_leaf_where(&is(0), Axis::Horizontal, false, 1));
        assert!(matches!(
            pane.close_leaf_where(&is(0)),
            CloseOutcome::NotFound
        ));
        assert!(matches!(pane, Pane::Empty));
    }

    fn rect_of(pane: &TestPane, id: u32) -> Rect {
        pane.leaf_rects()
            .into_iter()
            .find(|(v, _)| *v == id)
            .map(|(_, r)| r)
            .unwrap()
    }

    fn assert_rect(got: Rect, want: Rect) {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-5;
        assert!(
            close(got.x, want.x)
                && close(got.y, want.y)
                && close(got.w, want.w)
                && close(got.h, want.h),
            "rect {got:?} != {want:?}"
        );
    }

    #[test]
    fn leaf_rects_tile_the_unit_square_with_nested_ratios() {
        let pane = TestPane::split_node(
            Axis::Horizontal,
            0.25,
            Pane::Leaf(0),
            TestPane::split_node(Axis::Vertical, 0.6, Pane::Leaf(1), Pane::Leaf(2)),
        );
        assert_rect(
            rect_of(&pane, 0),
            Rect {
                x: 0.0,
                y: 0.0,
                w: 0.25,
                h: 1.0,
            },
        );
        assert_rect(
            rect_of(&pane, 1),
            Rect {
                x: 0.25,
                y: 0.0,
                w: 0.75,
                h: 0.6,
            },
        );
        assert_rect(
            rect_of(&pane, 2),
            Rect {
                x: 0.25,
                y: 0.6,
                w: 0.75,
                h: 0.4,
            },
        );
        assert_eq!(
            pane.leaf_rects()
                .iter()
                .map(|(v, _)| *v)
                .collect::<Vec<_>>(),
            pane.leaves()
        );
    }

    #[test]
    fn neighbor_in_direction_finds_the_adjacent_pane() {
        let mut pane = TestPane::leaf(0);
        split(&mut pane, 0, Axis::Horizontal, 1);
        let idx = |id: u32| pane.leaves().iter().position(|v| *v == id).unwrap();
        assert_eq!(pane.neighbor_in_direction(idx(0), Dir::Right), Some(idx(1)));
        assert_eq!(pane.neighbor_in_direction(idx(1), Dir::Left), Some(idx(0)));
        assert_eq!(pane.neighbor_in_direction(idx(0), Dir::Up), None);
        assert_eq!(pane.neighbor_in_direction(idx(1), Dir::Right), None);
    }

    #[test]
    fn neighbor_in_direction_prefers_the_largest_overlap() {
        let pane = TestPane::split_node(
            Axis::Horizontal,
            0.5,
            Pane::Leaf(0),
            TestPane::split_node(Axis::Vertical, 0.7, Pane::Leaf(1), Pane::Leaf(2)),
        );
        let idx = |id: u32| pane.leaves().iter().position(|v| *v == id).unwrap();
        assert_eq!(pane.neighbor_in_direction(idx(0), Dir::Right), Some(idx(1)));
    }

    #[test]
    fn resize_grows_the_focused_pane_from_either_side() {
        let build = || TestPane::split_node(Axis::Horizontal, 0.5, Pane::Leaf(0), Pane::Leaf(1));
        let ratio = |p: &TestPane| match p {
            Pane::Split { ratio, .. } => ratio.get(),
            _ => unreachable!(),
        };
        let p = build();
        assert!(p.resize_focused(&is(0), Dir::Right, 0.05));
        assert!((ratio(&p) - 0.55).abs() < 1e-6);
        let p = build();
        assert!(p.resize_focused(&is(1), Dir::Right, 0.05));
        assert!((ratio(&p) - 0.45).abs() < 1e-6);
        let p = build();
        assert!(p.resize_focused(&is(0), Dir::Left, 0.05));
        assert!((ratio(&p) - 0.45).abs() < 1e-6);
    }

    #[test]
    fn resize_without_a_matching_axis_is_a_noop() {
        let pane = TestPane::split_node(Axis::Horizontal, 0.5, Pane::Leaf(0), Pane::Leaf(1));
        assert!(!pane.resize_focused(&is(0), Dir::Up, 0.05));
        assert!(!pane.resize_focused(&is(0), Dir::Down, 0.05));
        assert!(!pane.resize_focused(&is(99), Dir::Right, 0.05));
    }

    #[test]
    fn resize_targets_the_nearest_matching_axis_ancestor() {
        let pane = TestPane::split_node(
            Axis::Horizontal,
            0.5,
            Pane::Leaf(0),
            TestPane::split_node(Axis::Horizontal, 0.5, Pane::Leaf(1), Pane::Leaf(2)),
        );
        assert!(pane.resize_focused(&is(1), Dir::Right, 0.05));
        match &pane {
            Pane::Split { ratio, b, .. } => {
                assert!(
                    (ratio.get() - 0.5).abs() < 1e-6,
                    "outer split must not move"
                );
                match &**b {
                    Pane::Split { ratio, .. } => {
                        assert!(
                            (ratio.get() - 0.55).abs() < 1e-6,
                            "inner split should grow 1"
                        );
                    }
                    _ => unreachable!(),
                }
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn swap_leaf_indices_trades_payloads_in_place() {
        let mut pane = TestPane::leaf(0);
        split(&mut pane, 0, Axis::Horizontal, 1);
        split(&mut pane, 1, Axis::Vertical, 2);
        split(&mut pane, 0, Axis::Vertical, 3);
        assert_eq!(pane.leaves(), vec![0, 3, 1, 2]);
        assert!(pane.swap_leaf_indices(0, 2));
        assert_eq!(pane.leaves(), vec![1, 3, 0, 2]);
        assert_well_formed(&pane);
        assert!(!pane.swap_leaf_indices(1, 1));
        assert!(!pane.swap_leaf_indices(0, 99));
        assert_eq!(pane.leaves(), vec![1, 3, 0, 2]);
    }
}

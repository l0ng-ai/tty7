//! A binary split-pane tree for a single tab. Each leaf is a terminal; splits
//! divide the available space along an axis at an adjustable ratio (default
//! 50/50, draggable via the divider between the two children). The tree is small
//! and mutated in place (split / close-and-collapse), and rendered recursively
//! with flex.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{App, Bounds, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Window, canvas, div};
use gpui::{Axis, Entity, prelude::*, px};
use gpui_component::ActiveTheme as _;

use crate::terminal::view::TerminalView;

/// Legal band for a split's `a`-child ratio; keeps both panes usable.
const MIN_RATIO: f32 = 0.1;
const MAX_RATIO: f32 = 0.9;
/// Thickness (px) of the draggable divider between two split children.
const DIVIDER_THICKNESS: f32 = 5.;

/// The leaf payload is generic (defaulting to the real terminal view) so the
/// pure tree logic can be exercised in tests with plain values; at runtime
/// `Pane` is always `Pane<Entity<TerminalView>>`.
pub enum Pane<L = Entity<TerminalView>> {
    Leaf(L),
    Split {
        axis: Axis,
        a: Box<Pane<L>>,
        b: Box<Pane<L>>,
        /// Fraction of the split occupied by `a` (clamped to `MIN..=MAX_RATIO`).
        /// Stored in a shared cell so the divider's drag closure can update it
        /// without having to locate this node by path in the tree.
        ratio: Rc<Cell<f32>>,
        /// Whether the divider is currently being dragged. Lives in the node so
        /// the in-progress drag survives the re-renders it triggers.
        dragging: Rc<Cell<bool>>,
    },
    /// Transient placeholder used only while collapsing a split; never rendered.
    Empty,
}

/// A direction for pane focus / resize, mapped from the arrow-key actions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    /// The split axis this direction operates along: Left/Right divide width
    /// (a horizontal split), Up/Down divide height (a vertical split).
    fn axis(self) -> Axis {
        match self {
            Dir::Left | Dir::Right => Axis::Horizontal,
            Dir::Up | Dir::Down => Axis::Vertical,
        }
    }

    /// Whether this direction *grows* the focused pane (Right/Down) as opposed
    /// to shrinking it (Left/Up).
    fn grows(self) -> bool {
        matches!(self, Dir::Right | Dir::Down)
    }
}

/// A leaf's normalized rectangle within the tab (the whole tab is the unit
/// square `0,0 → 1,1`). Derived purely from split axes and ratios, so directional
/// focus is a geometry query independent of the actual pixel layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Overlap length of two 1-D intervals `[a0, a0+alen)` and `[b0, b0+blen)`
/// (0 when they don't overlap). Used to score how well two panes line up on the
/// axis perpendicular to a move.
fn overlap_1d(a0: f32, alen: f32, b0: f32, blen: f32) -> f32 {
    ((a0 + alen).min(b0 + blen) - a0.max(b0)).max(0.0)
}

/// Result of attempting to close the focused leaf.
pub enum CloseOutcome {
    /// No focused leaf in this subtree.
    NotFound,
    /// A leaf was removed and the tree collapsed around it.
    Collapsed,
    /// This node *is* the focused leaf; the caller should drop it (e.g. close
    /// the whole tab when it was the tab's only pane).
    RemoveSelf,
}

/// Structural tree operations, independent of what a leaf holds. Matching a
/// specific leaf is expressed as a predicate so the focus- and identity-based
/// public API (below) can share one implementation with the tests.
impl<L: Clone> Pane<L> {
    pub fn leaf(view: L) -> Self {
        Pane::Leaf(view)
    }

    /// Construct a split node from two already-built children. Used when
    /// rebuilding a saved session tree from disk: `ratio` is clamped to the
    /// legal band and the divider starts un-dragged.
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

    /// Split the first leaf matching `is_target` along `axis`, inserting `new`
    /// as the second child. Returns whether a matching leaf was found.
    fn split_leaf_where(&mut self, is_target: &impl Fn(&L) -> bool, axis: Axis, new: L) -> bool {
        match self {
            Pane::Leaf(v) => {
                if is_target(v) {
                    let old = v.clone();
                    *self = Pane::split_node(axis, 0.5, Pane::Leaf(old), Pane::Leaf(new));
                    true
                } else {
                    false
                }
            }
            Pane::Split { a, b, .. } => {
                a.split_leaf_where(is_target, axis, new.clone())
                    || b.split_leaf_where(is_target, axis, new)
            }
            Pane::Empty => false,
        }
    }

    /// Remove the first leaf matching `is_target` (depth-first, `a` before
    /// `b`), collapsing its parent split into the sibling.
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
                // Recurse into `a` first (borrow scoped to this block).
                let a_outcome = if let Pane::Split { a, .. } = self {
                    a.close_leaf_where(is_target)
                } else {
                    unreachable!()
                };
                match a_outcome {
                    CloseOutcome::RemoveSelf => {
                        // Collapse: replace self with its `b` child.
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

    /// Push a mutable reference to every leaf payload, depth-first (`a` before
    /// `b`), matching `leaves()` order. Used by `swap_leaf_indices`.
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

    /// Swap the payloads of the leaves at ordered indices `i` and `j` (indices
    /// into `leaves()`), leaving the tree *structure* untouched — only the two
    /// terminals trade places. Returns whether the swap happened (false for
    /// `i == j` or an out-of-range index).
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
        // Split so the two `&mut L` come from disjoint slices — the borrow
        // checker won't let us index the same slice mutably twice.
        let (left, right) = refs.split_at_mut(hi);
        std::mem::swap(&mut *left[lo], &mut *right[0]);
        true
    }

    /// The normalized rectangle of every leaf within the unit-square tab, in
    /// `leaves()` order. A horizontal split divides width at its ratio (`a` left,
    /// `b` right); a vertical split divides height (`a` top, `b` bottom) — the
    /// same geometry `render` lays out with flex.
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
            Pane::Split { axis, a, b, ratio, .. } => {
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

    /// The ordered index of the pane adjacent to leaf `from` in direction `dir`,
    /// or `None` at the edge. tmux semantics: among panes whose edge sits on the
    /// far side of `from` in that direction and which overlap it on the
    /// perpendicular axis, pick the nearest edge, breaking ties by the largest
    /// overlap.
    pub fn neighbor_in_direction(&self, from: usize, dir: Dir) -> Option<usize> {
        let rects = self.leaf_rects();
        let f = rects.get(from)?.1;
        const EPS: f32 = 1e-4;
        let mut best: Option<(usize, f32, f32)> = None; // (index, edge distance, overlap)
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
            // Must lie in the requested direction (distance ≥ 0) and share some
            // perpendicular extent, or it isn't a real neighbor.
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

    /// Grow or shrink the focused pane along `dir` by `step`, by nudging the
    /// ratio of its nearest enclosing split whose axis matches `dir`. `step`
    /// grows the focused pane when `dir` is Right/Down and shrinks it when
    /// Left/Up, regardless of which side of the split it sits on. Ratios stay
    /// clamped to the legal band. Returns whether a matching split was found.
    /// Takes `&self`: split ratios live in shared `Cell`s, so no `&mut` needed.
    pub fn resize_focused(&self, is_focused: &impl Fn(&L) -> bool, dir: Dir, step: f32) -> bool {
        let mut path: Vec<(&Pane<L>, bool)> = Vec::new();
        if !self.focus_path(is_focused, &mut path) {
            return false;
        }
        let target_axis = dir.axis();
        // Nearest enclosing matching-axis split = deepest entry in the path.
        for (node, went_a) in path.iter().rev() {
            if let Pane::Split { axis, ratio, .. } = node {
                if *axis == target_axis {
                    // ratio is `a`'s share; +step enlarges `a`. Growing the
                    // focused pane means +step when it's in `a` and we grow, or
                    // in `b` and we shrink (== moves the divider toward `b`).
                    let delta = if *went_a == dir.grows() { step } else { -step };
                    let r = (ratio.get() + delta).clamp(MIN_RATIO, MAX_RATIO);
                    ratio.set(r);
                    return true;
                }
            }
        }
        false
    }

    /// Record the path of splits from the root down to the focused leaf, each
    /// tagged with whether the leaf lies in the split's `a` (true) or `b`
    /// (false) child. Returns whether the focused leaf was found.
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

/// Focus- and render-aware operations on the concrete terminal-view tree.
impl Pane<Entity<TerminalView>> {
    /// The currently focused leaf, if any.
    pub fn focused_leaf(&self, window: &Window, cx: &App) -> Option<Entity<TerminalView>> {
        match self {
            // `contains_focused`, not `is_focused`: a leaf is "active" when its
            // terminal surface *or any descendant* holds focus. The inline
            // input editor is a child with its own focus handle, so while the
            // shell idles at its prompt focus lives there, not on the terminal's
            // own handle — an exact `is_focused` check would miss the active pane.
            Pane::Leaf(v) => v
                .read(cx)
                .focus_handle
                .contains_focused(window, cx)
                .then(|| v.clone()),
            Pane::Split { a, b, .. } => a
                .focused_leaf(window, cx)
                .or_else(|| b.focused_leaf(window, cx)),
            Pane::Empty => None,
        }
    }

    /// The operation target: the focused leaf, or the first leaf if none is
    /// focused. This is the standard "act on the current pane" selection rule.
    pub fn focused_or_first(&self, window: &Window, cx: &App) -> Option<Entity<TerminalView>> {
        self.focused_leaf(window, cx).or_else(|| self.first_leaf())
    }

    /// The pane adjacent to the focused one in direction `dir`, matched by
    /// normalized geometry (tmux directional focus). `None` when nothing is
    /// focused or the focused pane is already at that edge.
    pub fn neighbor_in_dir(
        &self,
        dir: Dir,
        window: &Window,
        cx: &App,
    ) -> Option<Entity<TerminalView>> {
        let focused = self.focused_leaf(window, cx)?;
        let leaves = self.leaves();
        let from = leaves
            .iter()
            .position(|l| l.entity_id() == focused.entity_id())?;
        let target = self.neighbor_in_direction(from, dir)?;
        leaves.get(target).cloned()
    }

    /// Resize the focused pane along `dir` by `step` (see the generic
    /// `resize_focused`). Returns whether a matching split was adjusted.
    pub fn resize_focused_pane(&self, dir: Dir, step: f32, window: &Window, cx: &App) -> bool {
        let Some(focused) = self.focused_leaf(window, cx) else {
            return false;
        };
        self.resize_focused(&|v| v.entity_id() == focused.entity_id(), dir, step)
    }

    /// The ordered index of the focused leaf within `leaves()`, if any. Lets the
    /// shell pick the swap partner (`index ± 1`) without re-walking the tree.
    pub fn focused_index(&self, window: &Window, cx: &App) -> Option<usize> {
        let focused = self.focused_leaf(window, cx)?;
        self.leaves()
            .iter()
            .position(|l| l.entity_id() == focused.entity_id())
    }

    /// Split a specific leaf (matched by entity identity) along `axis`, inserting
    /// `new` as the second child. The target must be captured *before* creating
    /// `new`, since constructing a terminal steals window focus.
    pub fn split_leaf(
        &mut self,
        target: &Entity<TerminalView>,
        axis: Axis,
        new: Entity<TerminalView>,
    ) -> bool {
        self.split_leaf_where(&|v| v.entity_id() == target.entity_id(), axis, new)
    }

    /// Remove the focused leaf, collapsing its parent split into the sibling.
    pub fn close_focused(&mut self, window: &Window, cx: &App) -> CloseOutcome {
        self.close_leaf_where(&|v| v.read(cx).focus_handle.contains_focused(window, cx))
    }

    /// Remove a specific leaf (matched by entity identity), collapsing its
    /// parent split into the sibling. Used when a pane closes for a reason
    /// other than user focus — its child exited on its own — so the leaf to
    /// remove is the exited one, wherever focus happens to be.
    pub fn close_leaf(&mut self, target: &Entity<TerminalView>) -> CloseOutcome {
        self.close_leaf_where(&|v| v.entity_id() == target.entity_id())
    }

    /// Render the subtree. `show_focus` draws a focus ring on the active leaf
    /// (suppressed when the tab has a single pane).
    pub fn render(&self, show_focus: bool, window: &mut Window, cx: &mut App) -> gpui::AnyElement {
        match self {
            Pane::Empty => div().into_any_element(),
            Pane::Leaf(v) => {
                let focused = show_focus && v.read(cx).focus_handle.contains_focused(window, cx);
                // No full border (it reads as a hard rectangle).
                div()
                    .size_full()
                    .relative()
                    .overflow_hidden()
                    // Inactive panes (only when the tab is actually split) fade back
                    // so the focused terminal reads as foreground without a hard
                    // border. Element opacity multiplies through the whole subtree
                    // (terminal glyphs + cell fills), unlike a background-tinted
                    // scrim which is near-invisible on a light theme (white on
                    // white). Applied to the container, so a click still lands on
                    // the terminal and focuses it.
                    .when(show_focus && !focused, |d| d.opacity(0.55))
                    .child(v.clone())
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
                // Current ratio for `a`, always within the legal band.
                let r = ratio.get().clamp(MIN_RATIO, MAX_RATIO);

                let idle = cx.theme().border;
                let active = cx.theme().drag_border;

                // Per-frame cell carrying the split container's pixel bounds. It
                // is filled by the backing canvas during prepaint and read by
                // the drag listener to convert a pointer position into a ratio.
                // Recreated each frame; only `dragging`/`ratio` persist.
                let container: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::new(Cell::new(None));

                // Backing canvas: measures the container and installs
                // window-level mouse listeners so a drag keeps tracking even
                // when the pointer outruns the thin divider.
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
                            // Track the pointer while the divider is held.
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
                                    // Map the pointer onto a 0..1 ratio along
                                    // the split axis (Pixels / Pixels -> f32).
                                    let span = if row { b.size.width } else { b.size.height };
                                    // A transiently zero-measured container would make
                                    // the division `NaN`; `f32::clamp` passes `NaN`
                                    // through (NaN comparisons are false), poisoning the
                                    // stored ratio and `flex_grow(NaN)`. Skip instead.
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
                            // End the drag on release.
                            window.on_mouse_event({
                                let dragging = dragging.clone();
                                move |_ev: &MouseUpEvent, _phase, window, _cx| {
                                    if dragging.get() {
                                        dragging.set(false);
                                        window.refresh();
                                    }
                                }
                            });
                        }
                    },
                )
                .absolute()
                .size_full();

                // The draggable divider: a comfortable invisible hit-area holding
                // a centered 1px hairline so the rule reads thin, not as a thick
                // band. The line brightens on hover or while dragging.
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
                    // Backing measurer/listener sits behind the children.
                    .child(backing)
                    .child(
                        div()
                            .flex_grow(r)
                            .flex_shrink(1.)
                            .flex_basis(px(0.))
                            .min_w_0()
                            .min_h_0()
                            .child(a.render(show_focus, window, cx)),
                    )
                    .child(divider)
                    .child(
                        div()
                            .flex_grow(1. - r)
                            .flex_shrink(1.)
                            .flex_basis(px(0.))
                            .min_w_0()
                            .min_h_0()
                            .child(b.render(show_focus, window, cx)),
                    )
                    .into_any_element()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In tests a leaf is just an id: the tree logic only ever clones leaves
    /// and asks a predicate whether one is the operation target.
    type TestPane = Pane<u32>;

    /// Predicate matching the leaf with the given id (the test stand-in for
    /// "is this the focused terminal" / "is this the split target").
    fn is(id: u32) -> impl Fn(&u32) -> bool {
        move |v| *v == id
    }

    /// Walk the tree asserting the structural invariants the live UI relies
    /// on: no transient `Empty` placeholder survives an operation, every
    /// split has two real children, and every stored ratio stays inside the
    /// legal band.
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

    /// Split leaf `target`, inserting `new` as its second sibling, asserting
    /// the target was found.
    fn split(pane: &mut TestPane, target: u32, axis: Axis, new: u32) {
        assert!(
            pane.split_leaf_where(&is(target), axis, new),
            "split target {target} not found"
        );
    }

    // Splitting a lone leaf must turn it into a split on the requested axis,
    // with the original terminal kept first and an even 50/50 ratio.
    #[test]
    fn split_leaf_replaces_target_with_split_keeping_original_first() {
        let mut pane = TestPane::leaf(0);
        assert!(pane.split_leaf_where(&is(0), Axis::Horizontal, 1));
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

    // A split must land on exactly the targeted leaf, leaving every other
    // subtree untouched (guards against splitting the first leaf found).
    #[test]
    fn split_leaf_splits_only_the_matching_leaf() {
        // [0 | 1] -> split 1 vertically with 2 -> [0 | [1 / 2]]
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

    // A split aimed at a leaf that is not in the tree must report failure and
    // leave the tree exactly as it was.
    #[test]
    fn split_leaf_reports_missing_target_without_changing_tree() {
        let mut pane = TestPane::leaf(0);
        split(&mut pane, 0, Axis::Horizontal, 1);
        assert!(!pane.split_leaf_where(&is(99), Axis::Vertical, 2));
        assert_eq!(pane.leaves(), vec![0, 1]);
        assert_well_formed(&pane);
    }

    // Ratios restored from a saved session may be out of range; split_node
    // must clamp them into the legal band so both panes stay usable.
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

    // Leaf traversal drives pane cycling and session persistence: it must be
    // depth-first with `a` before `b`, and first_leaf must agree with it.
    #[test]
    fn leaves_and_first_leaf_follow_depth_first_a_before_b_order() {
        // [[0 / 3] | [1 / 2]]
        let mut pane = TestPane::leaf(0);
        split(&mut pane, 0, Axis::Horizontal, 1);
        split(&mut pane, 1, Axis::Vertical, 2);
        split(&mut pane, 0, Axis::Vertical, 3);
        assert_eq!(pane.leaves(), vec![0, 3, 1, 2]);
        assert_eq!(pane.first_leaf(), Some(0));
    }

    // Closing the tab's only pane must not mutate the tree; the caller reacts
    // to RemoveSelf by closing the whole tab.
    #[test]
    fn closing_the_root_leaf_defers_removal_to_the_caller() {
        let mut pane = TestPane::leaf(7);
        assert!(matches!(
            pane.close_leaf_where(&is(7)),
            CloseOutcome::RemoveSelf
        ));
        assert!(matches!(pane, Pane::Leaf(7)));
    }

    // Closing the first child of a split must promote the second child to
    // take the split's place, leaving no Empty placeholder behind.
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

    // Same as above, mirrored: closing the second child promotes the first.
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

    // Closing a nested leaf must collapse only its own parent split; the
    // grandparent keeps its axis and (dragged) ratio.
    #[test]
    fn closing_nested_leaf_collapses_only_its_parent_split() {
        // [1 |(0.3) [2 / 3]] -> close 2 -> [1 |(0.3) 3]
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

    // When the surviving sibling is itself a split, the whole subtree must be
    // promoted intact, keeping its axis and ratio.
    #[test]
    fn closing_a_leaf_promotes_entire_sibling_subtree() {
        // [[1 /(0.7) 2] | 3] -> close 3 -> [1 /(0.7) 2]
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

    // With no focused/matching leaf anywhere, close must be a no-op reporting
    // NotFound (e.g. focus is in another tab).
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

    // Even if the predicate matches several leaves, exactly one close happens:
    // the first match in `a`-before-`b` order (guards the short-circuit).
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

    // Drive a deep nested split/close sequence against a flat model of the
    // expected leaf order; after every step the tree must stay well-formed
    // and agree with the model. (A split inserts the new leaf right after its
    // target; a close removes exactly its target.)
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

    // Closing panes one by one must collapse down to a single leaf, and only
    // the very last close switches to RemoveSelf (close-the-tab boundary).
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

    // The transient Empty placeholder (also used for the settings tab) must
    // ignore every operation instead of panicking.
    #[test]
    fn empty_placeholder_ignores_all_operations() {
        let mut pane: TestPane = Pane::Empty;
        assert!(pane.leaves().is_empty());
        assert_eq!(pane.first_leaf(), None);
        assert!(!pane.split_leaf_where(&is(0), Axis::Horizontal, 1));
        assert!(matches!(
            pane.close_leaf_where(&is(0)),
            CloseOutcome::NotFound
        ));
        assert!(matches!(pane, Pane::Empty));
    }

    /// The rect for leaf `id` in a pane, by value.
    fn rect_of(pane: &TestPane, id: u32) -> Rect {
        pane.leaf_rects()
            .into_iter()
            .find(|(v, _)| *v == id)
            .map(|(_, r)| r)
            .unwrap()
    }

    /// Assert two rects match within floating-point tolerance (ratios multiply
    /// out to values like 0.39999998, so exact equality is too strict).
    fn assert_rect(got: Rect, want: Rect) {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-5;
        assert!(
            close(got.x, want.x) && close(got.y, want.y) && close(got.w, want.w) && close(got.h, want.h),
            "rect {got:?} != {want:?}"
        );
    }

    // Nested splits with non-even ratios must tile the unit square exactly:
    // a horizontal split divides width, a nested vertical split divides its
    // child's height, and the pieces stay gap-free and non-overlapping.
    #[test]
    fn leaf_rects_tile_the_unit_square_with_nested_ratios() {
        // [0 |(0.25) [1 /(0.6) 2]]
        let pane = TestPane::split_node(
            Axis::Horizontal,
            0.25,
            Pane::Leaf(0),
            TestPane::split_node(Axis::Vertical, 0.6, Pane::Leaf(1), Pane::Leaf(2)),
        );
        assert_rect(rect_of(&pane, 0), Rect { x: 0.0, y: 0.0, w: 0.25, h: 1.0 });
        assert_rect(rect_of(&pane, 1), Rect { x: 0.25, y: 0.0, w: 0.75, h: 0.6 });
        assert_rect(rect_of(&pane, 2), Rect { x: 0.25, y: 0.6, w: 0.75, h: 0.4 });
        // Rects come back in leaves() order.
        assert_eq!(
            pane.leaf_rects().iter().map(|(v, _)| *v).collect::<Vec<_>>(),
            pane.leaves()
        );
    }

    // Directional focus is edge-adjacency: right of 0 is 1, and from 1 the pane
    // to the left is 0. A pane with no neighbor in a direction returns None.
    #[test]
    fn neighbor_in_direction_finds_the_adjacent_pane() {
        // [0 | 1]
        let mut pane = TestPane::leaf(0);
        split(&mut pane, 0, Axis::Horizontal, 1);
        let idx = |id: u32| pane.leaves().iter().position(|v| *v == id).unwrap();
        assert_eq!(pane.neighbor_in_direction(idx(0), Dir::Right), Some(idx(1)));
        assert_eq!(pane.neighbor_in_direction(idx(1), Dir::Left), Some(idx(0)));
        // Nothing above/below in a purely horizontal split.
        assert_eq!(pane.neighbor_in_direction(idx(0), Dir::Up), None);
        assert_eq!(pane.neighbor_in_direction(idx(1), Dir::Right), None);
    }

    // When several panes sit in the requested direction, the one with the
    // largest perpendicular overlap wins (tmux's "line up with the cursor").
    #[test]
    fn neighbor_in_direction_prefers_the_largest_overlap() {
        // Left column is 0 (full height); right column is stacked [1 /(0.7) 2].
        // Moving right from 0 should land on 1 — it covers 70% of the shared
        // edge versus 2's 30%.
        let pane = TestPane::split_node(
            Axis::Horizontal,
            0.5,
            Pane::Leaf(0),
            TestPane::split_node(Axis::Vertical, 0.7, Pane::Leaf(1), Pane::Leaf(2)),
        );
        let idx = |id: u32| pane.leaves().iter().position(|v| *v == id).unwrap();
        assert_eq!(pane.neighbor_in_direction(idx(0), Dir::Right), Some(idx(1)));
    }

    // Resize nudges the nearest matching-axis ancestor's ratio and always grows
    // the focused pane on Right/Down, whichever side it's on.
    #[test]
    fn resize_grows_the_focused_pane_from_either_side() {
        let build = || TestPane::split_node(Axis::Horizontal, 0.5, Pane::Leaf(0), Pane::Leaf(1));
        let ratio = |p: &TestPane| match p {
            Pane::Split { ratio, .. } => ratio.get(),
            _ => unreachable!(),
        };
        // Focus in `a` (left): Right grows a → ratio up.
        let p = build();
        assert!(p.resize_focused(&is(0), Dir::Right, 0.05));
        assert!((ratio(&p) - 0.55).abs() < 1e-6);
        // Focus in `b` (right): Right grows b → ratio down.
        let p = build();
        assert!(p.resize_focused(&is(1), Dir::Right, 0.05));
        assert!((ratio(&p) - 0.45).abs() < 1e-6);
        // Left shrinks the focused pane (focus in a → ratio down).
        let p = build();
        assert!(p.resize_focused(&is(0), Dir::Left, 0.05));
        assert!((ratio(&p) - 0.45).abs() < 1e-6);
    }

    // A resize whose axis matches no ancestor split is a no-op: a purely
    // horizontal split has no vertical divider to move.
    #[test]
    fn resize_without_a_matching_axis_is_a_noop() {
        let pane = TestPane::split_node(Axis::Horizontal, 0.5, Pane::Leaf(0), Pane::Leaf(1));
        assert!(!pane.resize_focused(&is(0), Dir::Up, 0.05));
        assert!(!pane.resize_focused(&is(0), Dir::Down, 0.05));
        // An unfocused/absent target also reports no-op.
        assert!(!pane.resize_focused(&is(99), Dir::Right, 0.05));
    }

    // Resize with a nested tree targets the *nearest* enclosing matching-axis
    // split, not an outer one of the same axis.
    #[test]
    fn resize_targets_the_nearest_matching_axis_ancestor() {
        // [0 |(0.5) [1 |(0.5) 2]] — two nested horizontal splits.
        let pane = TestPane::split_node(
            Axis::Horizontal,
            0.5,
            Pane::Leaf(0),
            TestPane::split_node(Axis::Horizontal, 0.5, Pane::Leaf(1), Pane::Leaf(2)),
        );
        assert!(pane.resize_focused(&is(1), Dir::Right, 0.05));
        // Inner split moved; outer untouched.
        match &pane {
            Pane::Split { ratio, b, .. } => {
                assert!((ratio.get() - 0.5).abs() < 1e-6, "outer split must not move");
                match &**b {
                    Pane::Split { ratio, .. } => {
                        assert!((ratio.get() - 0.55).abs() < 1e-6, "inner split should grow 1");
                    }
                    _ => unreachable!(),
                }
            }
            _ => unreachable!(),
        }
    }

    // Swapping two leaves trades their payloads but keeps the tree shape and
    // leaf *positions* — only the values at those positions change.
    #[test]
    fn swap_leaf_indices_trades_payloads_in_place() {
        // [[0 / 3] | [1 / 2]] → leaves = [0, 3, 1, 2]
        let mut pane = TestPane::leaf(0);
        split(&mut pane, 0, Axis::Horizontal, 1);
        split(&mut pane, 1, Axis::Vertical, 2);
        split(&mut pane, 0, Axis::Vertical, 3);
        assert_eq!(pane.leaves(), vec![0, 3, 1, 2]);
        // Swap positions 0 and 2 (values 0 and 1).
        assert!(pane.swap_leaf_indices(0, 2));
        assert_eq!(pane.leaves(), vec![1, 3, 0, 2]);
        assert_well_formed(&pane);
        // No-op cases.
        assert!(!pane.swap_leaf_indices(1, 1));
        assert!(!pane.swap_leaf_indices(0, 99));
        assert_eq!(pane.leaves(), vec![1, 3, 0, 2]);
    }
}

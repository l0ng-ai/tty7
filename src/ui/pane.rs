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

    /// Render the subtree. `show_focus` draws a focus ring on the active leaf
    /// (suppressed when the tab has a single pane).
    pub fn render(&self, show_focus: bool, window: &mut Window, cx: &mut App) -> gpui::AnyElement {
        match self {
            Pane::Empty => div().into_any_element(),
            Pane::Leaf(v) => {
                let focused = show_focus && v.read(cx).focus_handle.contains_focused(window, cx);
                // No full border (it reads as a hard rectangle).
                // The active pane is marked by a small neutral dot in the corner.
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
                    .when(focused, |d| {
                        d.child(
                            div()
                                .absolute()
                                .top(px(5.))
                                .left(px(5.))
                                .size(px(7.))
                                .rounded_full()
                                .bg(cx.theme().blue),
                        )
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
}

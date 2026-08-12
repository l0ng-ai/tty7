use gpui::{Axis, Bounds, Pixels, Point, Styled, px};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

pub(crate) type ReorderState = Rc<RefCell<Option<Reorder>>>;

/// The cursor for a grip you pick up and drag. Win32 ships no open-hand
/// cursor, and gpui's Windows backend quietly answers `OpenHand` with the
/// plain arrow — so a grip there would look like ordinary background. The
/// pointing hand is the closest thing Windows has that still reads as
/// "this responds to the mouse".
pub(crate) fn cursor_grab<E: Styled>(el: E) -> E {
    if cfg!(target_os = "windows") {
        el.cursor_pointer()
    } else {
        el.cursor_grab()
    }
}

pub(crate) struct Preview {
    pub(crate) order: Vec<usize>,
    pub(crate) target: usize,
    pub(crate) from: usize,
    pub(crate) generation: usize,
    pub(crate) offsets: Vec<Pixels>,
    pub(crate) held: Pixels,
}

pub(crate) fn preview(
    state: &ReorderState,
    surface: &Surface,
    len: usize,
    pointer: Point<Pixels>,
) -> Option<Preview> {
    let state = state.borrow();
    let r = state.as_ref().filter(|r| r.covers(surface, len))?;
    let target = r.target(pointer);
    let (generation, prev) = r.begin_frame(target);
    Some(Preview {
        order: r.order(target),
        target,
        from: r.from,
        generation,
        offsets: (0..len)
            .map(|slot| r.flip_offset(slot, prev, target))
            .collect(),
        held: r.held_offset(pointer, target),
    })
}

pub(crate) fn set_pending(state: &ReorderState, surface: &Surface, order: Vec<usize>) {
    if let Some(r) = state.borrow().as_ref().filter(|r| r.surface == *surface) {
        *r.pending.borrow_mut() = Some(order);
    }
}

pub(crate) fn clear_pending(state: &ReorderState) {
    if let Some(r) = state.borrow().as_ref() {
        r.pending.borrow_mut().take();
    }
}

pub(crate) fn take_pending(state: &ReorderState) -> Option<Vec<usize>> {
    state.borrow_mut().take()?.pending.into_inner()
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Surface {
    Strip,
    SidebarRows(Option<PathBuf>),
    SidebarGroups,
}

pub(crate) struct Reorder {
    pub(crate) surface: Surface,
    pub(crate) from: usize,
    rects: Vec<Bounds<Pixels>>,
    axis: Axis,
    gap: Pixels,
    grab: Point<Pixels>,
    prev: Cell<usize>,
    generation: Cell<usize>,
    pending: RefCell<Option<Vec<usize>>>,
}

impl Reorder {
    pub(crate) fn new(
        surface: Surface,
        from: usize,
        rects: Vec<Bounds<Pixels>>,
        axis: Axis,
        gap: Pixels,
        grab: Point<Pixels>,
    ) -> Self {
        Self {
            surface,
            from,
            rects,
            axis,
            gap,
            grab,
            prev: Cell::new(from),
            generation: Cell::new(0),
            pending: RefCell::new(None),
        }
    }

    pub(crate) fn covers(&self, surface: &Surface, len: usize) -> bool {
        self.surface == *surface && self.rects.len() == len && self.from < len
    }

    fn along(&self, p: Point<Pixels>) -> Pixels {
        match self.axis {
            Axis::Vertical => p.y,
            Axis::Horizontal => p.x,
        }
    }

    fn extent(&self, b: &Bounds<Pixels>) -> Pixels {
        match self.axis {
            Axis::Vertical => b.size.height,
            Axis::Horizontal => b.size.width,
        }
    }

    fn shift(&self) -> Pixels {
        self.extent(&self.rects[self.from]) + self.gap
    }

    /// Whether a slot was actually laid out. The tab strip renders only the
    /// window of chips that fits and leaves the rest holding the
    /// `Bounds::default()` they were seeded with, so a zero extent means "this
    /// one is off screen" rather than "this one is empty". Every geometric
    /// question below has to skip those: they have no origin to compare
    /// against, and treating their origin as a real 0 is what put `min` past
    /// `max` in `held_origin`'s clamp and crashed the first drag on an
    /// overflowing strip.
    fn measured(&self, r: &Bounds<Pixels>) -> bool {
        self.extent(r) > px(0.)
    }

    fn measured_slots(&self) -> impl Iterator<Item = &Bounds<Pixels>> {
        self.rects.iter().filter(|r| self.measured(r))
    }

    pub(crate) fn target(&self, pointer: Point<Pixels>) -> usize {
        let leading = self.free_origin(pointer);
        let trailing = leading + self.extent(&self.rects[self.from]);
        self.rects
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != self.from)
            .filter(|(i, r)| {
                // A slot with no bounds keeps whichever side of the dragged
                // chip it started on, so a windowed strip reorders only among
                // the chips it is actually showing and the ones scrolled off
                // either end stay put.
                if !self.measured(r) {
                    return *i < self.from;
                }
                let centre = self.along(r.origin) + self.extent(r) / 2.;
                if *i < self.from {
                    leading >= centre
                } else {
                    trailing > centre
                }
            })
            .count()
    }

    fn free_origin(&self, pointer: Point<Pixels>) -> Pixels {
        self.along(pointer) - self.along(self.grab)
    }

    fn held_origin(&self, pointer: Point<Pixels>) -> Pixels {
        let free = self.free_origin(pointer);
        // Nothing drawn yet: there is no span to hold the chip inside, so let
        // it follow the pointer rather than clamping against a made-up one.
        let (Some(head), Some(tail)) = (self.measured_slots().next(), self.measured_slots().last())
        else {
            return free;
        };
        let first = self.along(head.origin);
        let end = self.along(tail.origin) + self.extent(tail);
        // The dragged chip can be wider than the span left for it once the
        // window is down to a single chip, which would invert the clamp.
        let last_start = (end - self.extent(&self.rects[self.from])).max(first);
        free.clamp(first, last_start)
    }

    pub(crate) fn held_offset(&self, pointer: Point<Pixels>, target: usize) -> Pixels {
        let home = self.along(self.rects[self.from].origin);
        self.held_origin(pointer) - (home + self.displacement(self.from, target))
    }

    pub(crate) fn order(&self, target: usize) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.rects.len()).collect();
        let dragged = order.remove(self.from);
        order.insert(target.min(order.len()), dragged);
        order
    }

    pub(crate) fn begin_frame(&self, target: usize) -> (usize, usize) {
        let prev = self.prev.get();
        if prev != target {
            self.generation.set(self.generation.get() + 1);
            self.prev.set(target);
        }
        (self.generation.get(), prev)
    }

    fn displacement(&self, slot: usize, target: usize) -> Pixels {
        if slot == self.from {
            let crossed = if target > self.from {
                self.from + 1..=target
            } else {
                target..=self.from.saturating_sub(1)
            };
            let span: Pixels = crossed
                .filter(|&i| i != self.from && i < self.rects.len())
                .filter(|&i| self.measured(&self.rects[i]))
                .map(|i| self.extent(&self.rects[i]) + self.gap)
                .fold(px(0.), |a, b| a + b);
            if target > self.from { span } else { -span }
        } else if self.from < slot && slot <= target {
            -self.shift()
        } else if target <= slot && slot < self.from {
            self.shift()
        } else {
            px(0.)
        }
    }

    pub(crate) fn flip_offset(&self, slot: usize, prev: usize, target: usize) -> Pixels {
        self.displacement(slot, prev) - self.displacement(slot, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    fn column(n: usize, h: f32, gap: f32, from: usize) -> Reorder {
        let rects = (0..n)
            .map(|i| Bounds {
                origin: point(px(0.), px(i as f32 * (h + gap))),
                size: size(px(200.), px(h)),
            })
            .collect();
        Reorder::new(
            Surface::Strip,
            from,
            rects,
            Axis::Vertical,
            px(gap),
            point(px(100.), px(h / 2.)),
        )
    }

    #[test]
    fn target_follows_the_pointer_across_neighbours() {
        let r = column(4, 30., 2., 0);
        assert_eq!(r.target(point(px(100.), px(32.))), 0);
        assert_eq!(r.target(point(px(100.), px(34.))), 1);
        assert_eq!(r.target(point(px(100.), px(66.))), 2);
        assert_eq!(r.target(point(px(100.), px(200.))), 3);
    }

    #[test]
    fn order_lifts_the_dragged_slot_into_the_target() {
        let r = column(4, 30., 2., 3);
        assert_eq!(r.target(point(px(100.), px(2.))), 0);
        assert_eq!(r.order(0), vec![3, 0, 1, 2]);
        assert_eq!(r.order(1), vec![0, 3, 1, 2]);
        assert_eq!(r.order(3), vec![0, 1, 2, 3]);
    }

    #[test]
    fn flip_offset_animates_only_the_slot_just_crossed() {
        let r = column(4, 30., 2., 0);
        assert_eq!(r.flip_offset(1, 0, 1), px(32.));
        assert_eq!(r.flip_offset(2, 0, 1), px(0.));
        assert_eq!(r.flip_offset(3, 0, 1), px(0.));
        assert_eq!(r.flip_offset(0, 0, 3), px(-96.));
        assert_eq!(r.flip_offset(0, 3, 0), px(96.));
        assert_eq!(r.flip_offset(1, 1, 0), px(-32.));
    }

    #[test]
    fn unequal_sizes_swap_in_both_directions() {
        let rects = vec![
            Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(200.), px(60.)),
            },
            Bounds {
                origin: point(px(0.), px(62.)),
                size: size(px(200.), px(140.)),
            },
        ];
        let grab = point(px(100.), px(10.));
        let tall = Reorder::new(
            Surface::SidebarGroups,
            1,
            rects.clone(),
            Axis::Vertical,
            px(2.),
            grab,
        );
        assert_eq!(tall.target(point(px(100.), px(41.))), 1);
        assert_eq!(tall.target(point(px(100.), px(39.))), 0);
        assert_eq!(tall.target(point(px(100.), px(72.))), 1);

        let short = Reorder::new(
            Surface::SidebarGroups,
            0,
            rects,
            Axis::Vertical,
            px(2.),
            grab,
        );
        assert_eq!(short.target(point(px(100.), px(80.))), 0);
        assert_eq!(short.target(point(px(100.), px(84.))), 1);
    }

    #[test]
    fn held_offset_tracks_the_pointer_across_a_crossing() {
        let r = column(4, 30., 2., 0);
        assert_eq!(r.held_offset(point(px(100.), px(25.)), 0), px(10.));
        assert_eq!(r.held_offset(point(px(100.), px(48.)), 1), px(1.));
        assert_eq!(r.held_offset(point(px(100.), px(900.)), 3), px(0.));
    }

    #[test]
    fn pending_only_survives_the_frame_that_recorded_it() {
        let state: ReorderState = Rc::new(RefCell::new(Some(column(3, 30., 2., 0))));
        let mine = Surface::Strip;

        clear_pending(&state);
        set_pending(&state, &mine, vec![1, 0, 2]);
        set_pending(&state, &Surface::SidebarGroups, vec![2, 1, 0]);

        clear_pending(&state);
        assert_eq!(take_pending(&state), None);
        assert!(state.borrow().is_none());

        *state.borrow_mut() = Some(column(3, 30., 2., 0));
        clear_pending(&state);
        set_pending(&state, &mine, vec![1, 0, 2]);
        assert_eq!(take_pending(&state), Some(vec![1, 0, 2]));
    }

    #[test]
    fn begin_frame_bumps_the_generation_only_on_change() {
        let r = column(3, 30., 2., 0);
        assert_eq!(r.begin_frame(0), (0, 0));
        assert_eq!(r.begin_frame(1), (1, 0));
        assert_eq!(r.begin_frame(1), (1, 1));
        assert_eq!(r.begin_frame(2), (2, 1));
    }

    /// A strip that overflows renders a window of chips and leaves the rest of
    /// the slots at the `Bounds::default()` they were seeded with. Those have
    /// no origin and no extent, so taking the span from slot 0 and the last
    /// slot put `min` past `max` and `clamp` panicked — a crash on the first
    /// drag of any tab, as soon as there were more tabs than fit.
    fn windowed_strip(n: usize, visible: std::ops::Range<usize>, from: usize) -> Reorder {
        let w = 120.;
        let gap = 6.;
        let rects = (0..n)
            .map(|i| {
                if !visible.contains(&i) {
                    return Bounds::default();
                }
                let slot = i - visible.start;
                Bounds {
                    origin: point(px(slot as f32 * (w + gap)), px(0.)),
                    size: size(px(w), px(30.)),
                }
            })
            .collect();
        Reorder::new(
            Surface::Strip,
            from,
            rects,
            Axis::Horizontal,
            px(gap),
            point(px(w / 2.), px(15.)),
        )
    }

    #[test]
    fn an_unmeasured_slot_does_not_invert_the_held_clamp() {
        // Six tabs, only 2..5 on screen, dragging the middle visible one.
        let r = windowed_strip(6, 2..5, 3);
        for x in [0., 60., 130., 400., 5000.] {
            // Would have panicked with "assertion failed: min <= max".
            let _ = r.held_offset(point(px(x), px(15.)), r.target(point(px(x), px(15.))));
        }
        // The dragged chip stays inside the span the strip actually drew,
        // which is the three visible slots, not the whole six-tab list.
        let held = |x: f32| r.held_origin(point(px(x), px(15.)));
        assert_eq!(held(-500.), px(0.), "clamped to the first visible slot");
        assert_eq!(held(5000.), px(252.), "clamped to the last visible slot");
    }

    #[test]
    fn a_hidden_slot_keeps_the_side_of_the_dragged_chip_it_started_on() {
        // Six tabs, 2..5 visible, dragging tab 3 (the middle of the window).
        let r = windowed_strip(6, 2..5, 3);
        let at = |x: f32| r.target(point(px(x), px(15.)));
        // Tabs 0 and 1 are hidden ahead of the drag and always count; tab 5 is
        // hidden behind it and never does. So the reachable targets are the
        // three the window shows, and dragging left cannot push the tab past
        // the tabs scrolled off the front.
        assert_eq!(at(-500.), 2, "cannot pass the hidden tabs ahead of it");
        assert_eq!(at(5000.), 4, "cannot pass the hidden tab behind it");
        // A full-length permutation still comes out, with the hidden tabs
        // left where they were.
        assert_eq!(r.order(at(-500.)), vec![0, 1, 3, 2, 4, 5]);
        assert_eq!(r.order(at(5000.)), vec![0, 1, 2, 4, 3, 5]);
    }

    #[test]
    fn a_strip_with_nothing_measured_yet_does_not_panic() {
        let r = windowed_strip(4, 0..0, 0);
        let _ = r.held_offset(point(px(50.), px(15.)), r.target(point(px(50.), px(15.))));
    }

    #[test]
    fn horizontal_lists_measure_along_x() {
        let widths = [100., 160., 120.];
        let mut x = 0.;
        let rects = widths
            .iter()
            .map(|w| {
                let b = Bounds {
                    origin: point(px(x), px(0.)),
                    size: size(px(*w), px(30.)),
                };
                x += w + 6.;
                b
            })
            .collect();
        let r = Reorder::new(
            Surface::Strip,
            0,
            rects,
            Axis::Horizontal,
            px(6.),
            point(px(50.), px(15.)),
        );
        assert_eq!(r.target(point(px(135.), px(15.))), 0);
        assert_eq!(r.target(point(px(137.), px(15.))), 1);
        assert_eq!(r.order(2), vec![1, 2, 0]);
    }
}

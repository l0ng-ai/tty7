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

    pub(crate) fn target(&self, pointer: Point<Pixels>) -> usize {
        let leading = self.free_origin(pointer);
        let trailing = leading + self.extent(&self.rects[self.from]);
        self.rects
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != self.from)
            .filter(|(i, r)| {
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
        let first = self.along(self.rects[0].origin);
        let last = self.rects.last().expect("non-empty");
        let end = self.along(last.origin) + self.extent(last);
        self.free_origin(pointer)
            .clamp(first, end - self.extent(&self.rects[self.from]))
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

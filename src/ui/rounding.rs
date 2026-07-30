use gpui::{Corners, Pixels, Styled, px};

pub(crate) trait RoundedCorners: Styled + Sized {
    fn rounded_corners(self, corners: Corners<Pixels>) -> Self {
        self.rounded_tl(corners.top_left)
            .rounded_tr(corners.top_right)
            .rounded_bl(corners.bottom_left)
            .rounded_br(corners.bottom_right)
    }
}

impl<T: Styled + Sized> RoundedCorners for T {}

pub(crate) const TRACK_RADIUS: Pixels = px(8.);

pub(crate) const CARD_RADIUS: Pixels = px(6.);

pub(crate) const HAIRLINE: Pixels = px(1.);

pub(crate) fn inner_radius(outer: Pixels, border: Pixels) -> Pixels {
    let inset = outer - border;
    if inset > px(0.) { inset } else { px(0.) }
}

pub(crate) fn segment_corners(
    i: usize,
    count: usize,
    outer: Pixels,
    border: Pixels,
) -> Corners<Pixels> {
    let r = inner_radius(outer, border);
    let zero = px(0.);
    let first = i < count && i == 0;
    let last = i < count && i + 1 == count;
    Corners {
        top_left: if first { r } else { zero },
        bottom_left: if first { r } else { zero },
        top_right: if last { r } else { zero },
        bottom_right: if last { r } else { zero },
    }
}

pub(crate) fn stack_corners(
    i: usize,
    count: usize,
    outer: Pixels,
    border: Pixels,
) -> Corners<Pixels> {
    let r = inner_radius(outer, border);
    let zero = px(0.);
    let first = i < count && i == 0;
    let last = i < count && i + 1 == count;
    Corners {
        top_left: if first { r } else { zero },
        top_right: if first { r } else { zero },
        bottom_left: if last { r } else { zero },
        bottom_right: if last { r } else { zero },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inner_radius_insets_by_the_border() {
        assert_eq!(inner_radius(px(8.), px(1.)), px(7.));
        assert_eq!(inner_radius(px(6.), px(1.)), px(5.));
        assert!(inner_radius(TRACK_RADIUS, HAIRLINE) < TRACK_RADIUS);
        assert!(inner_radius(CARD_RADIUS, HAIRLINE) < CARD_RADIUS);
    }

    #[test]
    fn inner_radius_never_goes_negative() {
        assert_eq!(inner_radius(px(1.), px(1.)), px(0.));
        assert_eq!(inner_radius(px(2.), px(6.)), px(0.));
    }

    #[test]
    fn end_segments_cap_the_track_and_the_middle_stays_square() {
        let r = inner_radius(TRACK_RADIUS, HAIRLINE);
        let zero = px(0.);

        let first = segment_corners(0, 3, TRACK_RADIUS, HAIRLINE);
        assert_eq!((first.top_left, first.bottom_left), (r, r));
        assert_eq!((first.top_right, first.bottom_right), (zero, zero));

        let middle = segment_corners(1, 3, TRACK_RADIUS, HAIRLINE);
        assert_eq!(middle, Corners::all(zero));

        let last = segment_corners(2, 3, TRACK_RADIUS, HAIRLINE);
        assert_eq!((last.top_right, last.bottom_right), (r, r));
        assert_eq!((last.top_left, last.bottom_left), (zero, zero));
    }

    #[test]
    fn a_lone_segment_takes_every_corner() {
        let r = inner_radius(TRACK_RADIUS, HAIRLINE);
        assert_eq!(
            segment_corners(0, 1, TRACK_RADIUS, HAIRLINE),
            Corners::all(r)
        );
    }

    #[test]
    fn an_empty_track_has_no_end_caps() {
        assert_eq!(
            segment_corners(0, 0, TRACK_RADIUS, HAIRLINE),
            Corners::all(px(0.))
        );
    }

    #[test]
    fn a_stack_caps_its_first_and_last_band() {
        let r = inner_radius(CARD_RADIUS, HAIRLINE);
        let zero = px(0.);

        let top = stack_corners(0, 2, CARD_RADIUS, HAIRLINE);
        assert_eq!((top.top_left, top.top_right), (r, r));
        assert_eq!((top.bottom_left, top.bottom_right), (zero, zero));

        let bottom = stack_corners(1, 2, CARD_RADIUS, HAIRLINE);
        assert_eq!((bottom.bottom_left, bottom.bottom_right), (r, r));
        assert_eq!((bottom.top_left, bottom.top_right), (zero, zero));

        assert_eq!(stack_corners(0, 1, CARD_RADIUS, HAIRLINE), Corners::all(r));
    }
}

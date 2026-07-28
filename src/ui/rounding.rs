//! Corner radii for children that paint a fill at the end of a rounded track.
//!
//! # The bug this exists to prevent (issue #236)
//!
//! gpui's `overflow_hidden` does **not** round-clip. [`gpui::ContentMask`] is a
//! plain axis-aligned `Bounds` with no corner radii (`Style::overflow_mask`
//! builds it from the element's bounds, shrunk by the border widths, and throws
//! `corner_radii` away), and every shader tests it as a hard per-fragment
//! `clip_distances < 0` discard. So the mask can only ever cut a square, and it
//! cuts it without a hint of anti-aliasing.
//!
//! A container's *own* rounded corners come from somewhere else entirely: the
//! quad shader's signed-distance field, whose `saturate(0.5 - distance)` gives
//! a clean one-device-pixel edge. That is why a plain rounded card looks
//! smooth. The moment a **child** paints a background into that same corner,
//! the two paths diverge:
//!
//! | | corner comes from | anti-aliased |
//! |---|---|---|
//! | the track's border/fill | quad SDF | yes |
//! | a child's fill in that corner | rectangular content mask | **no** |
//!
//! The child fills the whole square corner, the track's border arc floats
//! *inside* that square, and the child's outer edge is a hard vertical cut. It
//! reads exactly like the report: "anti-aliasing is insufficient".
//!
//! # The rule
//!
//! A child that lands in a corner has to carry its own radius, so its fill is
//! drawn by the SDF path too. It sits one border-width inside the track, so the
//! radius that nests concentrically is `outer - border` — see [`inner_radius`].
//!
//! The clip is still worth keeping as a backstop for content overflow; it just
//! can't be the thing that shapes a corner.

use gpui::{Corners, Pixels, Styled, px};

/// `rounded_*` one corner at a time is what gpui offers; this takes the whole
/// [`Corners`] the helpers below return, so a caller never has to spell out four
/// setters and risk transposing two of them.
pub(crate) trait RoundedCorners: Styled + Sized {
    fn rounded_corners(self, corners: Corners<Pixels>) -> Self {
        self.rounded_tl(corners.top_left)
            .rounded_tr(corners.top_right)
            .rounded_bl(corners.bottom_left)
            .rounded_br(corners.bottom_right)
    }
}

impl<T: Styled + Sized> RoundedCorners for T {}

/// gpui's `rounded_lg`, resolved. The Tailwind scale is in `rems`, and every
/// radius here has to do arithmetic against a `px` border width, so the tracks
/// this module serves state their radius in pixels. The rem size is whatever
/// `gpui_component::Root::render` sets each frame from `Theme::font_size`, and
/// tty7 never overrides that from gpui-component's default `px(16.)` — so this
/// is the same 8px `rounded_lg()` paints.
pub(crate) const TRACK_RADIUS: Pixels = px(8.);

/// gpui's `rounded_md`, resolved — see [`TRACK_RADIUS`].
pub(crate) const CARD_RADIUS: Pixels = px(6.);

/// The hairline every outlined track in the app draws (`border_1()`).
pub(crate) const HAIRLINE: Pixels = px(1.);

/// The radius a child needs so its fill follows the *inside* of a rounded,
/// bordered track instead of squaring off the corner it sits in.
///
/// The child's box starts one border-width in from the track's outer edge (that
/// is also where `Style::overflow_mask` puts the clip), so the concentric arc —
/// same centre, tighter by the border — has radius `outer - border`. Any larger
/// and the child bulges past the border and gets square-clipped again; any
/// smaller and a sliver of track shows between the border and the fill.
///
/// Clamped at zero: a border wider than the radius leaves a square corner,
/// which is what a concentric inset actually gives you there.
pub(crate) fn inner_radius(outer: Pixels, border: Pixels) -> Pixels {
    let inset = outer - border;
    if inset > px(0.) { inset } else { px(0.) }
}

/// Which corners segment `i` of `count` owns in a horizontal track.
///
/// The two end segments cap the track and take its rounding; everything between
/// them is square, because its corners are interior seams. A one-option track
/// is both ends at once, so it takes all four.
///
/// `count == 0` never renders a segment, but returning square corners keeps the
/// function total rather than making callers guard it.
pub(crate) fn segment_corners(
    i: usize,
    count: usize,
    outer: Pixels,
    border: Pixels,
) -> Corners<Pixels> {
    let r = inner_radius(outer, border);
    let zero = px(0.);
    // An index past the end is not an end cap — `i < count` keeps a degenerate
    // or out-of-range call square rather than rounding a corner that has no
    // segment to draw it.
    let first = i < count && i == 0;
    let last = i < count && i + 1 == count;
    Corners {
        top_left: if first { r } else { zero },
        bottom_left: if first { r } else { zero },
        top_right: if last { r } else { zero },
        bottom_right: if last { r } else { zero },
    }
}

/// [`segment_corners`] for a vertical stack — a card whose children are bands
/// stacked top to bottom. The first band caps the top of the card, the last caps
/// the bottom; a lone band caps both.
pub(crate) fn stack_corners(
    i: usize,
    count: usize,
    outer: Pixels,
    border: Pixels,
) -> Corners<Pixels> {
    let r = inner_radius(outer, border);
    let zero = px(0.);
    // An index past the end is not an end cap — `i < count` keeps a degenerate
    // or out-of-range call square rather than rounding a corner that has no
    // segment to draw it.
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

    /// The whole point: the child's arc is concentric with the border's inner
    /// edge, so it is *strictly* tighter than the track's outer radius. A child
    /// that reused the track's own radius would bulge past the border and hit
    /// the square content mask again — the shape of issue #236.
    #[test]
    fn inner_radius_insets_by_the_border() {
        assert_eq!(inner_radius(px(8.), px(1.)), px(7.));
        assert_eq!(inner_radius(px(6.), px(1.)), px(5.));
        assert!(inner_radius(TRACK_RADIUS, HAIRLINE) < TRACK_RADIUS);
        assert!(inner_radius(CARD_RADIUS, HAIRLINE) < CARD_RADIUS);
    }

    /// A border at least as wide as the radius eats the curve; the inset must
    /// bottom out at a square corner rather than going negative, which gpui
    /// would carry straight into the shader.
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

    /// One option is both ends of the track, so it has to round all four —
    /// otherwise a single-segment control squares off both sides.
    #[test]
    fn a_lone_segment_takes_every_corner() {
        let r = inner_radius(TRACK_RADIUS, HAIRLINE);
        assert_eq!(
            segment_corners(0, 1, TRACK_RADIUS, HAIRLINE),
            Corners::all(r)
        );
    }

    /// Total for the degenerate count rather than a panic: no segment renders,
    /// so no corner is an end.
    #[test]
    fn an_empty_track_has_no_end_caps() {
        assert_eq!(
            segment_corners(0, 0, TRACK_RADIUS, HAIRLINE),
            Corners::all(px(0.))
        );
    }

    /// A stack caps top and bottom where a segmented track caps left and right —
    /// same rule, rotated.
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

        // A collapsed card is one band, so its header owns every corner.
        assert_eq!(stack_corners(0, 1, CARD_RADIUS, HAIRLINE), Corners::all(r));
    }
}

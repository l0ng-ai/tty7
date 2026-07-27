//! The overlay scrollbar the app's own scroll areas wear (issue #185).
//!
//! gpui's `overflow_y_scroll()` scrolls but paints nothing, so a long file tree
//! or tab list gave no hint that there was more content — or where in it you
//! were. gpui-component ships the [`Scrollbar`] element for exactly this; the
//! only thing missing was a house shape for hanging it off our containers.
//!
//! Why not gpui-component's own `overflow_y_scrollbar()` wrapper: it mints its
//! own `ScrollHandle` internally via `use_keyed_state`, which leaves nothing for
//! `scroll_to_item` to aim at. Our lists need programmatic scrolling (activating
//! a tab pulls its row into view), so the handle stays app-owned and the
//! scrollbar is layered on top of it.
//!
//! Appearance (thumb colour, whether it auto-hides) comes from the theme — see
//! the scrollbar block in [`crate::ui::theme::apply_theme`].
//!
//! One behaviour worth knowing: while the bar is live it claims mouse-downs in
//! the 16px strip along the container's right edge, so the trailing few pixels
//! of a row stop being clickable there. That is inherent to an overlay
//! scrollbar (macOS's own behave the same way) and the reason the bar is only
//! permanently live on the platforms whose scrollbars are permanently visible.

use gpui::{AnyElement, ElementId, ScrollHandle, div, prelude::*};
use gpui_component::scroll::Scrollbar;
use gpui_component::v_flex;

/// Wrap a scrolling column so a vertical scrollbar floats over its right edge.
///
/// `scroll_area` must be the element that carries `.overflow_y_scroll()` and
/// `.track_scroll(handle)`; this only adds the positioned parent the scrollbar
/// measures against and the absolute layer it paints into. The scrollbar draws
/// *over* the content rather than reserving a gutter, so adopting it never
/// reflows the wrapped list.
///
/// `id` names the scrollbar's element state (hover/drag/fade), so it must be
/// unique per scroll area — the helper can't fall back to `Location::caller`
/// the way [`Scrollbar::vertical`] does, since every call site would then share
/// this function's line.
pub(crate) fn with_vertical_scrollbar(
    id: impl Into<ElementId>,
    scroll_area: impl IntoElement,
    handle: &ScrollHandle,
) -> AnyElement {
    v_flex()
        .relative()
        .flex_1()
        .min_h_0()
        .child(scroll_area)
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .child(Scrollbar::vertical(handle).id(id)),
        )
        .into_any_element()
}

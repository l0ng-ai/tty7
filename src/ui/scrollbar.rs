use gpui::{AnyElement, ElementId, ScrollHandle, div, prelude::*};
use gpui_component::scroll::Scrollbar;
use gpui_component::v_flex;

/// Overlays the shared vertical scrollbar on a scroll area.
///
/// The returned element takes its height from `flex_1`, so it wants a flex
/// column with a height of its own to grow inside — give it one rather than
/// dropping it straight into whatever is around it. Without that the wrapper
/// sizes to its content, the `size_full` scroll area inside grows with it, and
/// the pane stops scrolling because nothing overflows any more.
pub(crate) fn with_vertical_scrollbar(
    id: impl Into<ElementId>,
    scroll_area: impl IntoElement,
    handle: &ScrollHandle,
) -> AnyElement {
    v_flex()
        .relative()
        .flex_1()
        .min_h_0()
        // Stretching would size this the same, but not *definitely*: a `w_full`
        // inside the scroll area would then have no width to be a percentage
        // of, and would fall back to its content. That is how the settings
        // reading column lost its 640px cap on the Chinese page — one wide row
        // measured wider, and every row followed it.
        .w_full()
        .child(scroll_area)
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                // No `scrollbar_show` override: it falls back to
                // `cx.theme().scrollbar_show`, which `apply_theme` pins to
                // `Scrolling` for every list in the app. Overriding it here
                // would be one list disagreeing with the rest.
                .child(Scrollbar::vertical(handle).id(id)),
        )
        .into_any_element()
}

/// The same bar, for a scroll area that already carries its own height —
/// a `max_h` box, say.
///
/// `with_vertical_scrollbar` grows into a flex parent, which is wrong for a box
/// that is already the size it wants to be: taking `flex_1` there would either
/// stretch it past its cap or collapse it. This wrapper only lays the bar over
/// whatever the area measured, so the layout is exactly what it was without it.
pub(crate) fn over_vertical_scroll(
    id: impl Into<ElementId>,
    scroll_area: impl IntoElement,
    handle: &ScrollHandle,
) -> AnyElement {
    div()
        .relative()
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

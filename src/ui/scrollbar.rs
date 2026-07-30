use gpui::{AnyElement, ElementId, ScrollHandle, div, prelude::*};
use gpui_component::scroll::Scrollbar;
use gpui_component::v_flex;

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

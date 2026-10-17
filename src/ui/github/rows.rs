//! The list rows' stack connectors.

use gpui::{AnyElement, Div, Hsla, div, prelude::*, px, rems};
use gpui_component::{ActiveTheme as _, h_flex};

use tty7_core::core::github::stack::Cell;

use crate::ui::right_panel::{META, ROW_INSET};

/// A list row: one line, the Source Control tab's row pitch.
pub(crate) const ROW_H: f32 = 26.;
/// The state glyph, and the column it sits in.
pub(crate) const GLYPH: f32 = 14.;

/// A stack's line down lane `lane` of the glyph column.
fn connector(lane: usize, h: f32, color: Hsla) -> Div {
    div()
        .absolute()
        .left(px(lane as f32 * GLYPH + GLYPH / 2. - 0.5))
        .w(px(1.))
        .h(px(h))
        .bg(color)
}

/// `glyph` in its row's node lane, and each lane's connector piece.
pub(crate) fn stack_glyph(glyph: AnyElement, cells: &[Cell], cx: &gpui::App) -> AnyElement {
    let color = cx.theme().muted_foreground.opacity(0.5);
    let gap = (ROW_H - GLYPH) / 2.;
    let x = |lane: usize| lane as f32 * GLYPH + GLYPH / 2.;
    let node = cells
        .iter()
        .position(|c| matches!(c, Cell::Node { .. }))
        .unwrap_or(0);
    let mut column = div()
        .flex_none()
        .relative()
        .w(px(cells.len().max(1) as f32 * GLYPH))
        .h(px(ROW_H))
        .child(
            div()
                .absolute()
                .top(px(gap))
                .left(px(node as f32 * GLYPH))
                .child(glyph),
        );
    for (lane, &cell) in cells.iter().enumerate() {
        column = match cell {
            Cell::Empty => column,
            Cell::Pipe => column.child(connector(lane, ROW_H, color).top_0()),
            Cell::Node { up, down } => column
                .when(up, |d| d.child(connector(lane, gap, color).top_0()))
                .when(down, |d| d.child(connector(lane, gap, color).bottom_0())),
            // Down half the gap, then across to the glyph's lane.
            Cell::Join => column
                .child(connector(lane, gap / 2., color).top_0())
                .child(
                    div()
                        .absolute()
                        .top(px(gap / 2.))
                        .left(px(x(node) - 0.5))
                        .w(px(x(lane) - x(node) + 1.))
                        .h(px(1.))
                        .bg(color),
                ),
        };
    }
    column.into_any_element()
}

/// The branch a stack is based on, under its bottom row: the connector ends
/// in a dot beside the branch's name.
pub(crate) fn stack_trunk(base: &str, cx: &gpui::App) -> AnyElement {
    let theme = cx.theme();
    let color = theme.muted_foreground.opacity(0.5);
    const H: f32 = 18.;
    const DOT: f32 = 5.;
    h_flex()
        .h(px(H))
        .px(px(ROW_INSET))
        .gap(px(8.))
        .items_center()
        .child(
            div()
                .flex_none()
                .relative()
                .w(px(GLYPH))
                .h(px(H))
                .child(connector(0, H / 2., color).top_0())
                .child(
                    div()
                        .absolute()
                        .top(px((H - DOT) / 2.))
                        .left(px((GLYPH - DOT) / 2.))
                        .size(px(DOT))
                        .rounded_full()
                        .bg(color),
                ),
        )
        .child(
            div()
                .text_size(rems(META))
                .font_family(theme.mono_font_family.clone())
                .text_color(theme.muted_foreground)
                .child(base.to_string()),
        )
        .into_any_element()
}

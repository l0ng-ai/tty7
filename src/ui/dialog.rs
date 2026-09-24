//! Chrome for the small modal cards the app draws itself — the SSH sheet, the
//! worktree prompt — and the buttons and key hints the home page shares with
//! them.
//!
//! The workspace switcher is the reference: the same 12px card, 48px header
//! row with an `esc` cap, hairline-divided footer and faint keycaps. Each of
//! these cards used to spell its own padding, title weight and button variants
//! out a builder call at a time, and they had drifted — a 20px inset here, a
//! semibold title there, the accent-filled primary everywhere. One place now,
//! so two sheets on screen at once cannot disagree about what a dialog is.

use gpui::{
    App, ClickEvent, Div, ElementId, Hsla, SharedString, Stateful, Window, div, prelude::*, px,
    rems,
};
use gpui_component::input::Input;
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};

use crate::ui::presets::Surface;
use crate::ui::right_panel::{HEADING, TAB_TEXT, TEXT};

/// The card's corner — the switcher's, a notch rounder than a menu: a dialog
/// is the one thing on screen while it is up.
pub(crate) const CARD_RADIUS: f32 = 12.;

/// The title row, the height of the switcher's search row, so a prompt that
/// opens where the switcher just closed puts its first line in the same place.
pub(crate) const HEADER_H: f32 = 48.;

/// The button row. A 28px button in it leaves 6px above and below, the same
/// fit as the switcher's footer and its New workspace button.
pub(crate) const FOOTER_H: f32 = 40.;

/// How far the title and the fields sit in from the card's edge — the
/// switcher's search-row inset, so the text columns of the two line up.
pub(crate) const INSET: f32 = 18.;

/// A text field: a borderless well on the faint fill, the sidebar search's
/// shape. The field's own `.small()` padding (8px) is what sets the text in.
pub(crate) const FIELD_H: f32 = 28.;

/// Space between a field's label and its well, and between one field and the
/// next. Four-pixel grid: tight inside a pair, a step wider between pairs.
const LABEL_GAP: f32 = 6.;
const FIELD_GAP: f32 = 14.;

/// A button: the field's height, a pixel less rounded so a button beside a
/// field reads as a different kind of thing.
pub(crate) const BUTTON_H: f32 = 28.;
const BUTTON_RADIUS: f32 = 6.;

/// A key named in a hint: an 18px cap on the faint fill.
pub(crate) const KEYCAP: f32 = 18.;

/// The ladder a dialog's hovers are taken from. Cards sit on the popover
/// surface; the home page passes the window's own.
pub(crate) fn popover_rungs(cx: &App) -> Surface {
    cx.global::<crate::ui::presets::Surfaces>().popover
}

/// The card itself, opaque and occluding. Callers add the header, body and
/// footer as children, in that order.
pub(crate) fn card(width: f32, cx: &App) -> Div {
    v_flex()
        .occlude()
        .w(px(width))
        .map(|panel| crate::ui::theme::floating_surface(panel, cx))
        .rounded(px(CARD_RADIUS))
        .overflow_hidden()
}

/// The title row: the title at body size in medium, and the `esc` cap on the
/// right that says how to back out. Every card here answers Escape.
pub(crate) fn header(title: impl IntoElement, cx: &App) -> Div {
    h_flex()
        .flex_none()
        .items_center()
        .gap(px(10.))
        .h(px(HEADER_H))
        .pl(px(INSET))
        .pr(px(14.))
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(rems(TEXT))
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(title),
        )
        .child(keycap("esc", cx))
}

/// The card's content column.
pub(crate) fn body() -> Div {
    v_flex()
        .gap(px(FIELD_GAP))
        .px(px(INSET))
        .pt(px(16.))
        .pb(px(18.))
}

/// The button row: answers on the right, the action last, over a hairline.
pub(crate) fn footer(cx: &App) -> Div {
    h_flex()
        .flex_none()
        .items_center()
        .justify_end()
        .gap(px(6.))
        .h(px(FOOTER_H))
        .px(px(6.))
        .border_t_1()
        .border_color(cx.theme().border)
}

/// A field's name: the compact section-heading step, medium, secondary ink.
pub(crate) fn label(text: impl IntoElement, cx: &App) -> Div {
    div()
        .text_size(rems(HEADING))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

/// A text field in its well. The input loses its own border and fill — the
/// well is the shape — and keeps its padding, which is the text's inset.
pub(crate) fn field(input: Input, cx: &App) -> Div {
    h_flex()
        .items_center()
        .h(px(FIELD_H))
        .rounded(crate::ui::rounding::ROW_RADIUS)
        .bg(cx.theme().muted)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(input.appearance(false).small()),
        )
}

/// A label over its field.
pub(crate) fn labelled(name: impl IntoElement, input: Input, cx: &App) -> Div {
    v_flex()
        .gap(px(LABEL_GAP))
        .child(label(name, cx))
        .child(field(input, cx))
}

/// A block of read-only detail — a host and its key — set on the field's
/// fill, so it reads as a value to check rather than a line of prose.
pub(crate) fn well(cx: &App) -> Div {
    v_flex()
        .gap(px(4.))
        .px(px(10.))
        .py(px(8.))
        .rounded(crate::ui::rounding::ROW_RADIUS)
        .bg(cx.theme().muted)
}

/// A key named in a hint: a small faint cap, never a button outline. The same
/// cap the switcher's header and footer draw.
pub(crate) fn keycap(label: impl Into<SharedString>, cx: &App) -> gpui::AnyElement {
    let theme = cx.theme();
    div()
        .flex_shrink_0()
        .min_w(px(KEYCAP))
        .h(px(KEYCAP))
        .px(px(4.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .bg(theme.muted)
        .text_size(rems(11. / 16.))
        .text_color(theme.muted_foreground)
        .child(label.into())
        .into_any_element()
}

/// What a button is asking for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    /// The one answer a card is for: an inverted neutral, the surface's ink as
    /// the fill — the Commit button's paint, not the accent. The accent is
    /// already spent on focus and on the rail; a dialog of blue buttons made
    /// every sheet look like a call to action.
    Primary,
    /// Everything else: no fill at rest, the surface's hover rung under the
    /// pointer.
    Secondary,
    /// The action a card warns about. Red, and only on the one site that has
    /// earned it — overriding a changed host key.
    Danger,
}

/// A dialog button. The click handler is attached only while `enabled`: a
/// disabled button has to be inert, not merely grey.
///
/// Disabled, a filled tone sinks to the faint fill with secondary ink — the
/// same fall the Commit button takes — so it never advertises an action that
/// cannot run, and a secondary one just loses its ink.
pub(crate) fn button(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    tone: Tone,
    enabled: bool,
    rungs: Surface,
    cx: &App,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let theme = cx.theme();
    let surface: Hsla = gpui::rgb(rungs.base).into();
    let (fill, hover, ink): (Option<Hsla>, Option<Hsla>, Hsla) = match (tone, enabled) {
        (Tone::Secondary, false) => (None, None, theme.muted_foreground),
        (_, false) => (Some(theme.muted), None, theme.muted_foreground),
        (Tone::Primary, true) => {
            let fg = theme.foreground;
            (Some(fg), Some(fg.blend(surface.opacity(0.14))), surface)
        }
        (Tone::Danger, true) => {
            let red = theme.danger;
            (
                Some(red),
                Some(red.blend(surface.opacity(0.14))),
                theme.danger_foreground,
            )
        }
        (Tone::Secondary, true) => (None, Some(gpui::rgb(rungs.hover).into()), theme.foreground),
    };
    h_flex()
        .id(id)
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(BUTTON_H))
        .px(px(12.))
        .rounded(px(BUTTON_RADIUS))
        .text_size(rems(TAB_TEXT))
        .when(tone != Tone::Secondary, |b| {
            b.font_weight(gpui::FontWeight::MEDIUM)
        })
        .text_color(ink)
        .when_some(fill, |b, fill| b.bg(fill))
        .when_some(hover, |b, hover| b.hover(move |s| s.bg(hover)))
        .when(enabled, |b| b.cursor_pointer().on_click(on_click))
        .child(text.into())
}

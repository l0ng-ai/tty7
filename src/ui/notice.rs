//! The pill the app floats over the terminal when something about the
//! connection needs saying.
//!
//! There were two of these — `render_remote_input_notice` in `app.rs` and
//! `render_ssh_status_strip` in `forwards.rs` — written out a builder call at
//! a time in two files, identical down to the padding and differing only in
//! the border colour. Each also placed itself: both were
//! `absolute().left_0().right_0().bottom_4()`, centred, and both were children
//! of the same container in `body_area`, with nothing arbitrating between
//! them. A remote workspace whose ssh link had also dropped drew them on top
//! of each other.
//!
//! So the shell lives here and the notices no longer place themselves. The
//! anchor is a column: a second notice stacks above the first instead of
//! landing on it.

use gpui::{AnyElement, App, Div, Hsla, div, prelude::*, px};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

/// Chrome for one floating notice. `accent` is the leading dot, and is the
/// only thing that says how bad this one is; the rest of the pill is the same
/// whatever went wrong.
///
/// The severity used to tint the pill's whole edge. A coloured outline is
/// chrome wearing a status, and at 40% it was too faint to read as one — a
/// dot is the shape the rail and the switcher already use for "this state,
/// in this colour", and it leaves the edge the same hairline every other
/// floating surface has.
pub(crate) fn pill(accent: Hsla, cx: &App) -> Div {
    let theme = cx.theme();
    h_flex()
        .occlude()
        .items_center()
        .gap(px(8.))
        .min_h(px(PILL_H))
        .pl(px(12.))
        .pr(px(14.))
        .py(px(6.))
        .map(|panel| crate::ui::theme::floating_surface(panel, cx))
        // Off the right panel's ramp on purpose: these float over the
        // terminal, not inside a panel, and are sized against the terminal's
        // own text.
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(div().flex_none().size(px(DOT)).rounded_full().bg(accent))
}

/// A one-line pill's floor. A longer reason still wraps it taller; this only
/// stops a short one from shrinking to a sliver around 12px text.
const PILL_H: f32 = 32.;

/// The severity dot, the rail's status-dot size.
const DOT: f32 = 6.;

/// Anchors whatever notices are up as one bottom-centred column, so two of
/// them stack rather than collide. `None` when there is nothing to show, which
/// is what lets the caller keep using `when_some`.
pub(crate) fn anchor(items: Vec<AnyElement>) -> Option<AnyElement> {
    if items.is_empty() {
        return None;
    }
    Some(
        div()
            .absolute()
            .left_0()
            .right_0()
            .bottom_4()
            .child(v_flex().w_full().items_center().gap_2().children(items))
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_up_means_no_anchor() {
        assert!(anchor(Vec::new()).is_none());
    }

    #[test]
    fn one_notice_still_gets_the_anchor() {
        assert!(anchor(vec![div().into_any_element()]).is_some());
    }

    #[test]
    fn two_notices_share_one_anchor() {
        // The bug this module exists for: two live notices must come back as a
        // single stacked element, not as two things each claiming `bottom_4`.
        let stacked = anchor(vec![div().into_any_element(), div().into_any_element()]);
        assert!(stacked.is_some());
    }
}

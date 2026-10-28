//! DEC mode 2031, colour palette update notifications: a program that switches
//! it on (`CSI ? 2031 h`) is sent `CSI ? 997 ; 1 n` (dark) or `; 2 n` (light)
//! whenever the theme's background changes, and `CSI ? 996 n` asks for the
//! same report. Claude Code's `theme: auto` depends on it: it reads OSC 11 once
//! at startup and again only when a 997 arrives.
//! <https://contour-terminal.org/vt-extensions/color-palette-update-notifications/>

use std::sync::atomic::{AtomicBool, Ordering};

use tty7_vt::model::Rgb;
use gpui::{App, Context};
use gpui_component::ActiveTheme;
use tty7_core::core::term_modes::{COLOR_SCHEME_UPDATES, TerminalModes};

use super::view::TerminalView;
use crate::ui::presets::is_dark;

/// The report for a background, dark or light by the same test the theme
/// presets use.
pub(super) fn report(bg: Rgb) -> String {
    let rgb = u32::from(bg.r) << 16 | u32::from(bg.g) << 8 | u32::from(bg.b);
    format!("\x1b[?997;{}n", if is_dark(rgb) { 1 } else { 2 })
}

/// Fold pane output into `modes` and publish whether 2031 is on.
pub(super) fn fold(modes: &mut TerminalModes, bytes: &[u8], on: &AtomicBool) {
    modes.feed(bytes);
    on.store(modes.is_on(COLOR_SCHEME_UPDATES), Ordering::Relaxed);
}

fn background(cx: &App) -> Rgb {
    super::palette::hsla_to_rgb(cx.theme().background)
}

/// Push a report to the pane each time the theme's background changes while
/// its program has 2031 on. Every path to a new theme (a preset pick, a
/// follow-system flip, a config reload) ends in the `Theme` global.
pub(super) fn watch(cx: &mut Context<TerminalView>) {
    let mut last = background(cx);
    cx.observe_global::<gpui_component::Theme>(move |view, cx| {
        let now = background(cx);
        if std::mem::replace(&mut last, now) != now && view.terminal.color_scheme_updates() {
            view.terminal.write(report(now).into_bytes());
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dark_background_reports_dark_and_a_light_one_light() {
        assert_eq!(report(Rgb { r: 0, g: 43, b: 54 }), "\x1b[?997;1n");
        assert_eq!(
            report(Rgb {
                r: 253,
                g: 246,
                b: 227
            }),
            "\x1b[?997;2n"
        );
    }
}

//! tty7 terminal color scheme.
//!
//! A self-contained, hand-tuned palette (not derived from any other terminal
//! theme) covering the ANSI 16 colors for both dark and light backgrounds, the
//! 256-color xterm fallback cube, and the text-selection colors. The goal is a
//! calm, slightly cool-neutral look where every accent stays legible on its
//! intended background and the bright variants are clearly lifted from the
//! normal ones without becoming neon.

use alacritty_terminal::vte::ansi::Rgb;
use gpui::Global;

/// The terminal-facing slice of the active color scheme: the ANSI-16 set and
/// the selection surface for the current (preset, mode) — the base the search
/// match washes derive from (the selection itself paints as a translucent
/// foreground wash; see `element::PaintColors`). Published as a GPUI global by
/// the UI layer's `apply_theme` so the renderer always paints the active
/// scheme without the terminal layer depending on `ui`.
#[derive(Debug, Clone)]
pub struct ActivePalette {
    pub ansi16: [Rgb; 16],
    pub sel_bg: Rgb,
}

impl Global for ActivePalette {}

/// Convert a GPUI `Hsla` to an alacritty `Rgb` (8-bit per channel, rounded and
/// clamped). Shared by the renderer and the OSC color-query replies.
pub fn hsla_to_rgb(c: gpui::Hsla) -> Rgb {
    let rgba = gpui::Rgba::from(c);
    Rgb {
        r: (rgba.r * 255.0).round().clamp(0.0, 255.0) as u8,
        g: (rgba.g * 255.0).round().clamp(0.0, 255.0) as u8,
        b: (rgba.b * 255.0).round().clamp(0.0, 255.0) as u8,
    }
}

/// Dark-theme ANSI 16 set, tuned for the warm "soft charcoal" background
/// (~#232220 — see ui/theme.rs). The neutral slots (0/7/8/15) carry the same
/// faint warm cast as the shell so grays don't read cool-and-dirty against the
/// warm base; the colored accents stay slightly desaturated for long sessions.
const DARK_ANSI16: [(u8, u8, u8); 16] = [
    (0x2c, 0x2a, 0x26), // 0  black (warm, lifted off the bg so it's not invisible)
    (0xec, 0x6a, 0x78), // 1  red
    (0x8f, 0xbf, 0x6e), // 2  green
    (0xe0, 0xb0, 0x72), // 3  yellow
    (0x6f, 0xa8, 0xe6), // 4  blue
    (0xc0, 0x8a, 0xdf), // 5  magenta
    (0x5f, 0xc2, 0xc9), // 6  cyan
    (0xd2, 0xcf, 0xc8), // 7  white (warm light gray — matches default foreground)
    (0x6b, 0x66, 0x5d), // 8  bright black (warm comment gray)
    (0xf5, 0x86, 0x8f), // 9  bright red
    (0xa8, 0xd9, 0x8a), // 10 bright green
    (0xef, 0xc7, 0x8a), // 11 bright yellow
    (0x8f, 0xc0, 0xf5), // 12 bright blue
    (0xd2, 0xa6, 0xec), // 13 bright magenta
    (0x84, 0xd6, 0xdc), // 14 bright cyan
    (0xf6, 0xf3, 0xec), // 15 bright white (warm)
];

/// Build the full 256-entry xterm palette (dark-theme ANSI 16 in slots 0-15).
///
/// Slots 0-15 are a sensible default only: the renderer overwrites them every
/// paint with the active preset's ANSI set (see `ui::presets::ActivePalette`).
pub fn build() -> [Rgb; 256] {
    let mut p = [Rgb { r: 0, g: 0, b: 0 }; 256];

    // 0-15: ANSI 16.
    for (i, (r, g, b)) in DARK_ANSI16.iter().enumerate() {
        p[i] = Rgb {
            r: *r,
            g: *g,
            b: *b,
        };
    }

    // 16-231: 6×6×6 color cube.
    let steps = [0u8, 95, 135, 175, 215, 255];
    let mut idx = 16;
    for r in 0..6 {
        for g in 0..6 {
            for b in 0..6 {
                p[idx] = Rgb {
                    r: steps[r],
                    g: steps[g],
                    b: steps[b],
                };
                idx += 1;
            }
        }
    }

    // 232-255: grayscale ramp.
    for i in 0..24 {
        let v = 8 + i as u8 * 10;
        p[232 + i] = Rgb { r: v, g: v, b: v };
    }

    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsla_to_rgb_round_trips_a_known_color() {
        // A `#rrggbb` literal → Hsla → Rgb should recover the byte channels.
        let rgb = hsla_to_rgb(gpui::rgb(0x123456).into());
        assert_eq!((rgb.r, rgb.g, rgb.b), (0x12, 0x34, 0x56));
        // Pure black and white clamp cleanly.
        let black = hsla_to_rgb(gpui::rgb(0x000000).into());
        assert_eq!((black.r, black.g, black.b), (0, 0, 0));
        let white = hsla_to_rgb(gpui::rgb(0xffffff).into());
        assert_eq!((white.r, white.g, white.b), (255, 255, 255));
    }

    #[test]
    fn build_lays_out_the_256_color_cube_and_ramp() {
        let p = build();
        // Slots 0-15 are the dark ANSI set.
        for (i, (r, g, b)) in DARK_ANSI16.iter().enumerate() {
            assert_eq!((p[i].r, p[i].g, p[i].b), (*r, *g, *b));
        }
        // The 6×6×6 cube runs 16..=231: first is black, last is white.
        assert_eq!((p[16].r, p[16].g, p[16].b), (0, 0, 0));
        assert_eq!((p[231].r, p[231].g, p[231].b), (255, 255, 255));
        // The grayscale ramp is 232..=255, starting at 8 and stepping by 10.
        assert_eq!(p[232].r, 8);
        assert_eq!(p[255].r, 8 + 23 * 10);
        // Ramp entries are true grays.
        assert_eq!(p[240].r, p[240].g);
        assert_eq!(p[240].g, p[240].b);
    }
}

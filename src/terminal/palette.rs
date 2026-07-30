use alacritty_terminal::vte::ansi::Rgb;
use gpui::Global;

#[derive(Debug, Clone)]
pub struct ActivePalette {
    pub ansi16: [Rgb; 16],
    pub sel_bg: Rgb,
}

impl Global for ActivePalette {}

pub fn hsla_to_rgb(c: gpui::Hsla) -> Rgb {
    let rgba = gpui::Rgba::from(c);
    Rgb {
        r: (rgba.r * 255.0).round().clamp(0.0, 255.0) as u8,
        g: (rgba.g * 255.0).round().clamp(0.0, 255.0) as u8,
        b: (rgba.b * 255.0).round().clamp(0.0, 255.0) as u8,
    }
}

const DARK_ANSI16: [(u8, u8, u8); 16] = [
    (0x2c, 0x2a, 0x26),
    (0xec, 0x6a, 0x78),
    (0x8f, 0xbf, 0x6e),
    (0xe0, 0xb0, 0x72),
    (0x6f, 0xa8, 0xe6),
    (0xc0, 0x8a, 0xdf),
    (0x5f, 0xc2, 0xc9),
    (0xd2, 0xcf, 0xc8),
    (0x6b, 0x66, 0x5d),
    (0xf5, 0x86, 0x8f),
    (0xa8, 0xd9, 0x8a),
    (0xef, 0xc7, 0x8a),
    (0x8f, 0xc0, 0xf5),
    (0xd2, 0xa6, 0xec),
    (0x84, 0xd6, 0xdc),
    (0xf6, 0xf3, 0xec),
];

pub fn build() -> [Rgb; 256] {
    let mut p = [Rgb { r: 0, g: 0, b: 0 }; 256];

    for (i, (r, g, b)) in DARK_ANSI16.iter().enumerate() {
        p[i] = Rgb {
            r: *r,
            g: *g,
            b: *b,
        };
    }

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
        let rgb = hsla_to_rgb(gpui::rgb(0x123456).into());
        assert_eq!((rgb.r, rgb.g, rgb.b), (0x12, 0x34, 0x56));
        let black = hsla_to_rgb(gpui::rgb(0x000000).into());
        assert_eq!((black.r, black.g, black.b), (0, 0, 0));
        let white = hsla_to_rgb(gpui::rgb(0xffffff).into());
        assert_eq!((white.r, white.g, white.b), (255, 255, 255));
    }

    #[test]
    fn build_lays_out_the_256_color_cube_and_ramp() {
        let p = build();
        for (i, (r, g, b)) in DARK_ANSI16.iter().enumerate() {
            assert_eq!((p[i].r, p[i].g, p[i].b), (*r, *g, *b));
        }
        assert_eq!((p[16].r, p[16].g, p[16].b), (0, 0, 0));
        assert_eq!((p[231].r, p[231].g, p[231].b), (255, 255, 255));
        assert_eq!(p[232].r, 8);
        assert_eq!(p[255].r, 8 + 23 * 10);
        assert_eq!(p[240].r, p[240].g);
        assert_eq!(p[240].g, p[240].b);
    }
}

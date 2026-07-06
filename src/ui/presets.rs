//! Built-in color themes. Each theme is a
//! single, self-contained palette — there is no separate "dark" and "light"
//! variant to toggle between; a theme simply *is* dark or light, and that flag
//! drives gpui-component's mode plus how the shell-chrome neutrals are derived.
//!
//! A theme specifies only its essentials — background, foreground, one accent,
//! and the ANSI-16 terminal set. Every other shell surface (borders, hover
//! chips, sidebar, command-palette list, selections) is *derived* from those by
//! blending toward the foreground, so all six themes stay internally consistent
//! without hand-tuning a dozen greys apiece.

use alacritty_terminal::vte::ansi::Rgb;

use crate::terminal::palette::ActivePalette;

/// A single color theme. `dark` is the theme's inherent brightness (not a
/// user choice) — it selects gpui-component's `ThemeMode` and flips the
/// direction the derived neutrals blend.
#[derive(Debug, Clone)]
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub dark: bool,
    pub background: u32,
    pub foreground: u32,
    pub accent: u32,
    /// Optional caret color. `None` derives it from `accent` (the default); a
    /// theme sets this only when it wants a cursor color distinct from its accent.
    pub caret: Option<u32>,
    pub ansi16: [(u8, u8, u8); 16],
}

/// The shell-chrome palette derived from a theme's essentials. Consumed by
/// `apply_theme` to paint gpui-component's `Theme`.
#[derive(Debug, Clone)]
pub struct Neutrals {
    pub background: u32,
    pub foreground: u32,
    pub border: u32,
    pub secondary: u32,
    pub muted: u32,
    pub muted_foreground: u32,
    pub popover: u32,
    pub caret: u32,
    pub selection: u32,
    pub sidebar: u32,
    pub sidebar_sel: u32,
    pub sidebar_fg: u32,
    pub list_active: u32,
    pub list_hover: u32,
}

impl Preset {
    /// Derive the full shell palette by blending `background` toward
    /// `foreground` (chips, borders, surfaces) and `foreground` toward
    /// `background` (dimmed text). One ruleset gives every theme a coherent set
    /// of greys regardless of its base colors.
    pub fn neutrals(&self) -> Neutrals {
        let bg = self.background;
        let fg = self.foreground;
        Neutrals {
            background: bg,
            foreground: fg,
            border: mix(bg, fg, 0.16),
            secondary: mix(bg, fg, 0.09),
            muted: mix(bg, fg, 0.06),
            muted_foreground: mix(fg, bg, 0.42),
            popover: mix(bg, fg, 0.05),
            caret: self.caret.unwrap_or(self.accent),
            selection: mix(bg, fg, 0.20),
            sidebar: mix(bg, fg, 0.03),
            sidebar_sel: mix(bg, fg, 0.12),
            sidebar_fg: mix(fg, bg, 0.28),
            list_active: mix(bg, fg, 0.17),
            list_hover: mix(bg, fg, 0.09),
        }
    }

    /// The terminal-facing slice of the palette: ANSI-16 plus the selection
    /// surface (`mix(bg, fg, 0.24)`), which the renderer's search-match washes
    /// derive from. The selection itself paints as a translucent foreground
    /// wash tuned to composite to this same surface on default-background
    /// cells (see `element::PaintColors::resolve`), so cells keep their own
    /// colors while selected.
    pub fn active_palette(&self) -> ActivePalette {
        let mut ansi16 = [Rgb { r: 0, g: 0, b: 0 }; 16];
        for (i, (r, g, b)) in self.ansi16.iter().enumerate() {
            ansi16[i] = Rgb {
                r: *r,
                g: *g,
                b: *b,
            };
        }
        ActivePalette {
            ansi16,
            sel_bg: rgb_bytes(mix(self.background, self.foreground, 0.24)),
        }
    }
}

/// Blend `a` toward `b` by `t` (0.0 = all `a`, 1.0 = all `b`), per channel.
fn mix(a: u32, b: u32, t: f32) -> u32 {
    let (ar, ag, ab) = (a >> 16 & 0xff, a >> 8 & 0xff, a & 0xff);
    let (br, bg, bb) = (b >> 16 & 0xff, b >> 8 & 0xff, b & 0xff);
    let ch = |x: u32, y: u32| (x as f32 + (y as f32 - x as f32) * t).round() as u32;
    (ch(ar, br) << 16) | (ch(ag, bg) << 8) | ch(ab, bb)
}

/// Split a `0xRRGGBB` literal into an alacritty `Rgb`.
fn rgb_bytes(n: u32) -> Rgb {
    Rgb {
        r: (n >> 16) as u8,
        g: (n >> 8) as u8,
        b: n as u8,
    }
}

/// All built-in themes, in display order (light themes first). The behavioral
/// default is [`DEFAULT_ID`], not the first entry.
pub fn all() -> &'static [Preset] {
    &PRESETS
}

/// The id of the app's default theme. Mirrors `Config`'s default `theme_preset`
/// (which can't reference this module — `core` doesn't depend on `ui`). This is
/// the *behavioral* default; it is independent of `PRESETS`' display order, which
/// lists the light themes first.
pub const DEFAULT_ID: &str = "light";

/// Look a theme up by id, falling back to the default theme ([`DEFAULT_ID`]) for
/// an unknown id so a stale/typo'd config entry never breaks startup.
pub fn by_id(id: &str) -> &'static Preset {
    PRESETS
        .iter()
        .find(|p| p.id == id)
        .or_else(|| PRESETS.iter().find(|p| p.id == DEFAULT_ID))
        .unwrap_or(&PRESETS[0])
}

/// A hand-picked set of familiar terminal palettes.
static PRESETS: [Preset; 8] = [
    Preset {
        id: "light",
        name: "Light",
        dark: false,
        background: 0xffffff,
        foreground: 0x111111,
        accent: 0x00c2ff,
        // A warm orange caret, distinct from the cyan accent (which also tints the
        // active-line highlight and links).
        caret: Some(0xf5a15c),
        // True-hue, high-contrast set tuned for a white ground (GitHub Light-ish):
        // red reads red (not the old magenta-pink), green is a forest green, and
        // "yellow" is a dark gold so it stays legible instead of washing out.
        ansi16: [
            (0x24, 0x29, 0x2e), // black
            (0xd1, 0x24, 0x2f), // red
            (0x1a, 0x7f, 0x37), // green
            (0x9a, 0x67, 0x00), // yellow (dark gold — readable on white)
            (0x09, 0x69, 0xda), // blue
            (0x82, 0x50, 0xdf), // magenta
            (0x1b, 0x7c, 0x83), // cyan (teal)
            (0x6e, 0x77, 0x81), // white (grey)
            (0x57, 0x60, 0x6a), // bright black
            (0xcf, 0x22, 0x2e), // bright red
            (0x1f, 0x88, 0x3d), // bright green
            (0xbf, 0x87, 0x00), // bright yellow (amber)
            (0x21, 0x8b, 0xff), // bright blue
            (0xa4, 0x75, 0xf9), // bright magenta
            (0x31, 0x92, 0xaa), // bright cyan
            (0x8c, 0x95, 0x9f), // bright white
        ],
    },
    // Atom's "One Light" — the light counterpart to the ubiquitous One Dark;
    // a soft off-white (#fafafa) ground with the signature One blue accent.
    // Clean and widely loved as an editor/terminal light scheme.
    Preset {
        id: "one_light",
        name: "One Light",
        dark: false,
        background: 0xfafafa,
        foreground: 0x383a42,
        accent: 0x4078f2,
        caret: None,
        ansi16: [
            (0x38, 0x3a, 0x42),
            (0xe4, 0x56, 0x49),
            (0x50, 0xa1, 0x4f),
            (0xc1, 0x84, 0x01),
            (0x40, 0x78, 0xf2),
            (0xa6, 0x26, 0xa4),
            (0x01, 0x84, 0xbc),
            (0xa0, 0xa1, 0xa7),
            (0x69, 0x6c, 0x77),
            (0xe4, 0x56, 0x49),
            (0x50, 0xa1, 0x4f),
            (0xc1, 0x84, 0x01),
            (0x40, 0x78, 0xf2),
            (0xa6, 0x26, 0xa4),
            (0x01, 0x84, 0xbc),
            (0xfa, 0xfa, 0xfa),
        ],
    },
    // Catppuccin "Latte" — the light flavor of the immensely popular pastel
    // Catppuccin family; a developer favorite across editors and terminals.
    Preset {
        id: "catppuccin_latte",
        name: "Catppuccin Latte",
        dark: false,
        background: 0xeff1f5,
        foreground: 0x4c4f69,
        accent: 0x1e66f5,
        caret: None,
        ansi16: [
            (0xbc, 0xc0, 0xcc),
            (0xd2, 0x0f, 0x39),
            (0x40, 0xa0, 0x2b),
            (0xdf, 0x8e, 0x1d),
            (0x1e, 0x66, 0xf5),
            (0xea, 0x76, 0xcb),
            (0x17, 0x92, 0x99),
            (0x5c, 0x5f, 0x77),
            (0xac, 0xb0, 0xbe),
            (0xd2, 0x0f, 0x39),
            (0x40, 0xa0, 0x2b),
            (0xdf, 0x8e, 0x1d),
            (0x1e, 0x66, 0xf5),
            (0xea, 0x76, 0xcb),
            (0x17, 0x92, 0x99),
            (0x6c, 0x6f, 0x85),
        ],
    },
    // Rosé Pine "Dawn" — the light variant of the beloved Rosé Pine family
    // (soho vibes, muted rose/gold/iris on a warm off-white). Distinctive and
    // widely adored for its soft, tasteful palette. Official terminal mapping:
    // pine→green, foam→blue, rose→cyan, love→red, gold→yellow, iris→magenta.
    Preset {
        id: "rose_pine_dawn",
        name: "Rosé Pine Dawn",
        dark: false,
        background: 0xfaf4ed, // base
        foreground: 0x575279, // text
        accent: 0x907aa9,     // iris
        caret: None,
        ansi16: [
            (0xf2, 0xe9, 0xe1), // black   (overlay)
            (0xb4, 0x63, 0x7a), // red     (love)
            (0x28, 0x69, 0x83), // green   (pine)
            (0xea, 0x9d, 0x34), // yellow  (gold)
            (0x56, 0x94, 0x9f), // blue    (foam)
            (0x90, 0x7a, 0xa9), // magenta (iris)
            (0xd7, 0x82, 0x7e), // cyan    (rose)
            (0x57, 0x52, 0x79), // white   (text)
            (0x98, 0x93, 0xa5), // bright black   (muted)
            (0xb4, 0x63, 0x7a), // bright red
            (0x28, 0x69, 0x83), // bright green
            (0xea, 0x9d, 0x34), // bright yellow
            (0x56, 0x94, 0x9f), // bright blue
            (0x90, 0x7a, 0xa9), // bright magenta
            (0xd7, 0x82, 0x7e), // bright cyan
            (0x57, 0x52, 0x79), // bright white
        ],
    },
    Preset {
        id: "dark",
        name: "Dark",
        dark: true,
        background: 0x000000,
        foreground: 0xffffff,
        accent: 0x19aad8,
        caret: None,
        ansi16: [
            (0x61, 0x61, 0x61),
            (0xff, 0x82, 0x72),
            (0xb4, 0xfa, 0x72),
            (0xfe, 0xfd, 0xc2),
            (0xa5, 0xd5, 0xfe),
            (0xff, 0x8f, 0xfd),
            (0xd0, 0xd1, 0xfe),
            (0xf1, 0xf1, 0xf1),
            (0x8e, 0x8e, 0x8e),
            (0xff, 0xc4, 0xbd),
            (0xd6, 0xfc, 0xb9),
            (0xfe, 0xfd, 0xd5),
            (0xc1, 0xe3, 0xfe),
            (0xff, 0xb1, 0xfe),
            (0xe5, 0xe6, 0xfe),
            (0xfe, 0xff, 0xff),
        ],
    },
    Preset {
        id: "dracula",
        name: "Dracula",
        dark: true,
        background: 0x282a36,
        foreground: 0xf8f8f2,
        accent: 0xff79c6,
        caret: None,
        ansi16: [
            (0x00, 0x00, 0x00),
            (0xff, 0x55, 0x55),
            (0x50, 0xfa, 0x7b),
            (0xf1, 0xfa, 0x8c),
            (0xbd, 0x93, 0xf9),
            (0xff, 0x79, 0xc6),
            (0x8b, 0xe9, 0xfd),
            (0xbb, 0xbb, 0xbb),
            (0x55, 0x55, 0x55),
            (0xff, 0x55, 0x55),
            (0x50, 0xfa, 0x7b),
            (0xf1, 0xfa, 0x8c),
            (0xca, 0xa9, 0xfa),
            (0xff, 0x79, 0xc6),
            (0x8b, 0xe9, 0xfd),
            (0xff, 0xff, 0xff),
        ],
    },
    Preset {
        id: "harbor",
        name: "Harbor",
        dark: true,
        background: 0x1d2022,
        foreground: 0xe4eef5,
        accent: 0x6c96b4,
        caret: None,
        ansi16: [
            (0x12, 0x12, 0x12),
            (0xc7, 0x61, 0x56),
            (0x57, 0xc7, 0x8a),
            (0xc8, 0xa3, 0x5a),
            (0x57, 0x85, 0xc7),
            (0xc7, 0x56, 0xa9),
            (0x57, 0xc7, 0xc3),
            (0xee, 0xed, 0xeb),
            (0x29, 0x29, 0x29),
            (0xd2, 0x2d, 0x1e),
            (0x1c, 0xa0, 0x5a),
            (0xe5, 0xa0, 0x1a),
            (0x14, 0x58, 0xb8),
            (0xa4, 0x37, 0x87),
            (0x4d, 0x99, 0x89),
            (0xff, 0xff, 0xff),
        ],
    },
    // Rosé Pine (main) — the dark counterpart to Dawn: a deep muted-purple base
    // (#191724) with the signature rose/gold/foam/iris accents. One of the most
    // starred and adored schemes across editors and terminals.
    Preset {
        id: "rose_pine",
        name: "Rosé Pine",
        dark: true,
        background: 0x191724, // base
        foreground: 0xe0def4, // text
        accent: 0xc4a7e7,     // iris
        caret: None,
        ansi16: [
            (0x26, 0x23, 0x3a), // black   (overlay)
            (0xeb, 0x6f, 0x92), // red     (love)
            (0x31, 0x74, 0x8f), // green   (pine)
            (0xf6, 0xc1, 0x77), // yellow  (gold)
            (0x9c, 0xcf, 0xd8), // blue    (foam)
            (0xc4, 0xa7, 0xe7), // magenta (iris)
            (0xeb, 0xbc, 0xba), // cyan    (rose)
            (0xe0, 0xde, 0xf4), // white   (text)
            (0x6e, 0x6a, 0x86), // bright black   (muted)
            (0xeb, 0x6f, 0x92), // bright red
            (0x31, 0x74, 0x8f), // bright green
            (0xf6, 0xc1, 0x77), // bright yellow
            (0x9c, 0xcf, 0xd8), // bright blue
            (0xc4, 0xa7, 0xe7), // bright magenta
            (0xeb, 0xbc, 0xba), // bright cyan
            (0xe0, 0xde, 0xf4), // bright white
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(c: Rgb) -> f32 {
        fn chan(v: u8) -> f32 {
            let s = v as f32 / 255.0;
            if s <= 0.03928 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * chan(c.r) + 0.7152 * chan(c.g) + 0.0722 * chan(c.b)
    }

    fn contrast(a: Rgb, b: Rgb) -> f32 {
        let (l1, l2) = (luminance(a), luminance(b));
        let (hi, lo) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Default foreground must stay readable on the background in every theme.
    #[test]
    fn foreground_is_legible_on_background() {
        for p in all() {
            let ratio = contrast(rgb_bytes(p.background), rgb_bytes(p.foreground));
            assert!(
                ratio >= 4.0,
                "{}: fg/bg contrast too low ({ratio:.2})",
                p.id
            );
        }
    }

    /// The selection surface must stay a *tint* — decisively on the
    /// background's side of the fg↔bg axis. The renderer keeps each selected
    /// cell's own foreground and lays this tone over the cell (nothing
    /// re-colors the glyphs for contrast), so a surface that drifted toward
    /// the foreground would wash out the very text it highlights.
    #[test]
    fn selection_surface_stays_on_the_background_side() {
        for p in all() {
            let ap = p.active_palette();
            let to_bg = contrast(ap.sel_bg, rgb_bytes(p.background));
            let to_fg = contrast(ap.sel_bg, rgb_bytes(p.foreground));
            assert!(
                to_fg > to_bg,
                "{}: selection surface sits closer to the foreground \
                 (fg {to_fg:.2} vs bg {to_bg:.2}) — selected text would wash out",
                p.id
            );
        }
    }

    #[test]
    fn by_id_falls_back_to_default() {
        assert_eq!(by_id("nope").id, "light");
        assert_eq!(by_id("dracula").id, "dracula");
    }

    /// `mix` endpoints and midpoint behave.
    #[test]
    fn mix_blends_channels() {
        assert_eq!(mix(0x000000, 0xffffff, 0.0), 0x000000);
        assert_eq!(mix(0x000000, 0xffffff, 1.0), 0xffffff);
        assert_eq!(mix(0x000000, 0xffffff, 0.5), 0x808080);
    }
}

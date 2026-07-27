//! The theme system: the serializable [`Theme`] seed model, the derived
//! shell-chrome [`Neutrals`], the interaction-state [`Surface`] ladders, the
//! [`Themes`] registry, and the loaders that turn built-in tables, user YAML
//! files, and imported iTerm2 schemes into concrete themes.
//!
//! A theme is a **minimal seed** — a background (solid or gradient), a
//! foreground, one accent, an optional cursor/selection, and the ANSI-16
//! terminal set. Every other shell surface (borders, hover chips, sidebar,
//! command-palette list, selections) is *derived* from those by blending toward
//! the foreground (see [`Theme::neutrals`]), so any valid seed — built-in,
//! hand-written, or imported — yields a complete, internally consistent theme.
//!
//! Themes are **files, not constants**: the built-ins are embedded, but users
//! author their own as `~/.config/tty7/themes/*.yaml` (tty7's own schema) or drop
//! in an iTerm2 `*.itermcolors` scheme, which the loader imports on the fly. A
//! theme's light/dark brightness is *inferred* from its background luminance —
//! there is no `dark` field to set.
//!
//! # Interaction state
//!
//! Resting / hover / selected / pressed are a **first-class part of the theme**,
//! not something each widget re-derives. [`Theme::surface`] returns the state
//! ladder for whatever surface a widget paints on, and every rung is derived to
//! hit a *contrast ratio* against that surface rather than a fixed blend ratio —
//! see [`state`] for why that distinction is the whole point.

use std::path::PathBuf;

use alacritty_terminal::vte::ansi::Rgb;
use gpui::{App, Global};
use serde::Deserialize;

use crate::terminal::palette::ActivePalette;

/// A background (or accent) paint: a flat color or a two-stop gradient. The
/// window background renders gradients for real (see `theme::window_background`);
/// every other consumer works from the representative [`Fill::color`].
#[derive(Debug, Clone, PartialEq)]
pub enum Fill {
    Solid(u32),
    Vertical { top: u32, bottom: u32 },
    Horizontal { left: u32, right: u32 },
}

impl Fill {
    /// The single representative color used wherever one flat color is needed
    /// (chrome derivation, the terminal's default cell background): the solid
    /// itself, or a gradient's first stop.
    pub fn color(&self) -> u32 {
        match *self {
            Fill::Solid(c) => c,
            Fill::Vertical { top, .. } => top,
            Fill::Horizontal { left, .. } => left,
        }
    }
}

/// An optional background image layered under the background fill.
#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub path: PathBuf,
    /// 0.0 (invisible) … 1.0 (opaque).
    pub opacity: f32,
}

/// A single color theme — the seed the whole palette derives from. Colors are
/// `0xRRGGBB` literals. `dark` is *inferred* from `background` luminance (it
/// selects gpui-component's `ThemeMode` and flips how neutrals blend), never
/// authored.
#[derive(Debug, Clone)]
pub struct Theme {
    pub id: String,
    pub name: String,
    pub dark: bool,
    pub background: Fill,
    pub foreground: u32,
    pub accent: u32,
    /// Cursor color. `None` derives it from `accent`.
    pub caret: Option<u32>,
    /// Text/selection surface. `None` derives it from background/foreground.
    pub selection: Option<u32>,
    /// Window opacity 0.0…1.0. `None` = fully opaque. Carried for the renderer.
    pub opacity: Option<f32>,
    /// Blur the window background behind a translucent theme. Carried.
    pub blur: bool,
    /// Optional background image, composited over the background fill at its own
    /// opacity (the terminal and chrome paint on top).
    pub image: Option<Image>,
    pub ansi16: [(u8, u8, u8); 16],
    /// The file this theme was loaded from, or `None` for a compiled-in built-in.
    /// A theme with a path is user-owned and editable (see `fork_to_file` and the
    /// in-app color editor); a built-in is read-only until duplicated.
    pub path: Option<PathBuf>,
}

/// The shell-chrome palette derived from a theme's seed. Consumed by
/// `apply_theme` to paint gpui-component's `Theme`.
///
/// These are the theme's *static* colors — the ones that mean the same thing
/// wherever they appear. Anything that varies with interaction state lives in a
/// [`Surface`] instead.
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
    pub sidebar_fg: u32,
    /// The seed accent, nudged until it can carry ink — see [`legible_accent`].
    pub accent: u32,
}

/// One semantic colour in the three shapes the UI actually needs it in.
///
/// Splitting them is not ceremony: a red that is legible as *text* on the
/// background is a different red from one that works as a filled button, and the
/// text on that button is a third. Collapsing them is how a danger button ends up
/// with unreadable text, or a warning label ends up below AA.
#[derive(Debug, Clone, Copy)]
pub struct Semantic {
    /// Text (or a small solid mark) on the window background. WCAG AA, 4.5:1.
    pub ink: u32,
    /// A filled chip or button. The non-text floor, 3:1.
    pub fill: u32,
    /// Text on top of `fill`.
    pub on_fill: u32,
}

/// The status palette, derived from the theme's **own ANSI-16** rather than from
/// a fixed set of brand colours.
///
/// Every theme already ships a red, green, yellow and cyan — they are what the
/// terminal in the same window paints with. Until this existed, gpui-component's
/// stock Tailwind values (`red-400`, `yellow-400`, `green-400`) were used
/// instead, which meant two unrelated reds on screen at once — `#ff5555` in the
/// terminal and `#f87171` on the delete button, on Dracula — and, on the light
/// themes, a danger colour at 2.45:1 that cleared no contrast floor at all.
///
/// Each is conditioned by [`legible_ink`], so a seed too pale or too dark for its
/// role is deepened along its own hue rather than swapped for something foreign.
#[derive(Debug, Clone)]
pub struct Semantics {
    pub danger: Semantic,
    pub warning: Semantic,
    pub success: Semantic,
    pub info: Semantic,
    /// Links. Distinct from `info` only in intent, but it is the field
    /// gpui-component's markdown renderer reads, and left unset it resolves to
    /// the body text colour — a link that looks exactly like prose.
    pub link: Semantic,
}

/// The contrast targets that define how loud each interaction state reads.
///
/// **These four numbers are the app's only knobs for state prominence.** They
/// exist because the alternative — a fixed `mix(bg, fg, t)` per state, which is
/// what this file used to do — makes the *perceived* step depend on the theme.
/// The old ladder (`hover` 0.09, `sidebar_sel` 0.12, `list_active` 0.17) put
/// selected-vs-resting anywhere from 1.20:1 (Catppuccin Latte) to 1.47:1
/// (Dracula), and the segmented control — which read gpui-component's stock
/// `input` grey instead of the ladder at all — landed at **1.03:1 on Dracula**,
/// i.e. invisible. See issue #197.
///
/// A ratio target removes the theme from the equation: every theme lands on the
/// same perceived step, so tuning taste here retunes the whole app at once and
/// no theme can be an outlier.
///
/// `SELECTED` is 1.70 because that is where the already-signed-off Dracula
/// highlight sits (`mix(bg, fg, 0.17)` ≈ `#4b4d56`, 1.72:1) — the value the
/// palette and menu look was tuned against. Anchoring *to* it keeps that look
/// and pulls the light themes, which were as low as 1.20:1, up to match.
pub mod state {
    /// Pointer feedback. Deliberately a whisper: it answers the mouse without
    /// competing with the selection it may be sitting next to.
    pub const HOVER: f32 = 1.18;
    /// The resting selection. Never the *only* signal — see [`super::Surface`].
    pub const SELECTED: f32 = 1.70;
    /// Held down. One step past selected so pressing a selected item still reads.
    pub const PRESSED: f32 = 2.10;
    /// Resting label text. 4.6:1 keeps a de-emphasised label at WCAG AA on every
    /// theme; the fixed `mix(fg, bg, 0.42)` it replaces drifted with the seed.
    pub const TEXT_RESTING: f32 = 4.6;
}

/// The interaction-state ladder for one painting surface: the fills for each
/// state plus the paired label colors.
///
/// # Both channels, always
///
/// A fill alone does not communicate selection. The app learned this the hard
/// way in three separate places — `tab_strip`'s active chip, `tab_strip`'s
/// chrome tiles and the settings sidebar each grew a hand-written
/// fill-plus-text-color pair, while every site that *hadn't* been hand-fixed
/// (segmented controls, the SSH profile list) shipped a fill and nothing else
/// and could not be read. So the text colors ride along in this struct: take a
/// `Surface`, take both channels.
///
/// * **Fill** answers *which one* — it locates the selection in the row.
/// * **Text** (`text_selected` + `FontWeight::MEDIUM` vs `text_resting`)
///   answers *that this is it* — it survives a low-contrast fill, an oddly
///   seeded imported theme, and a color-blind reader.
///
/// A keyboard-driven single cursor (the command palette, a context menu) can get
/// away with the fill alone because the eye tracks the one thing that moves.
/// A *static* choice among visible siblings cannot.
#[derive(Debug, Clone, Copy)]
pub struct Surface {
    /// The surface itself — what a resting item paints on (i.e. no fill).
    pub base: u32,
    pub hover: u32,
    pub selected: u32,
    pub pressed: u32,
    /// Label color for a resting/unselected item on this surface.
    pub text_resting: u32,
    /// Label color for the selected item. Pair it with `FontWeight::MEDIUM`.
    pub text_selected: u32,
}

/// Every surface the shell actually paints interactive rows on, published as a
/// GPUI global by `apply_theme` so a render pass can read the ladder without
/// re-resolving (and cloning) the theme registry every frame.
///
/// Which surface a widget picks matters: a menu row sits on `popover`, not on
/// the window background, and a ladder anchored to the wrong surface is exactly
/// how the context-menu highlight ended up at 1.20:1 while claiming to be the
/// same 0.17 mix that reads fine on the window.
#[derive(Debug, Clone)]
pub struct Surfaces {
    /// The window background — settings sheets, panels, the terminal ground.
    pub window: Surface,
    /// The sunk sidebar rail.
    pub sidebar: Surface,
    /// Elevated surfaces: menus, dropdowns, the command palette.
    pub popover: Surface,
}

impl Global for Surfaces {}

/// The active theme's contrast-conditioned accent (see [`legible_accent`]),
/// published so a render pass can reach it without cloning the theme registry.
///
/// Deliberately its own global rather than a field on [`Surfaces`]: the accent is
/// not a surface, and it has exactly one job — ink that must be *noticed* (the
/// caret, the focus ring, a switch's checked track). Every neutral fill in the
/// app comes from a `Surface`; this is the one thing that doesn't.
pub struct ActiveAccent(pub u32);

impl Global for ActiveAccent {}

impl Theme {
    /// The representative solid background color.
    pub fn background_color(&self) -> u32 {
        self.background.color()
    }

    /// Derive the full shell palette by blending `background` toward a
    /// legibility-guaranteed `foreground` (chips, borders, surfaces) and that
    /// foreground back toward the background (dimmed text). One ruleset gives
    /// every theme — built-in, hand-authored, or imported — a coherent set of
    /// greys regardless of its base colors.
    pub fn neutrals(&self) -> Neutrals {
        let bg = self.background_color();
        let fg = legible_foreground(bg, self.foreground);
        Neutrals {
            background: bg,
            foreground: fg,
            border: mix(bg, fg, 0.16),
            secondary: mix(bg, fg, 0.09),
            muted: mix(bg, fg, 0.06),
            muted_foreground: dim(fg, bg, state::TEXT_RESTING),
            popover: mix(bg, fg, 0.05),
            caret: self.caret.unwrap_or(self.accent),
            selection: self.selection.unwrap_or_else(|| mix(bg, fg, 0.20)),
            sidebar: mix(bg, fg, 0.03),
            sidebar_fg: mix(fg, bg, 0.28),
            accent: legible_accent(bg, self.accent),
        }
    }

    /// The interaction-state ladder for content painted on `base`.
    ///
    /// Every rung blends `base` toward the (legibility-guaranteed) foreground
    /// until it clears its [`state`] contrast target *against `base`* — so the
    /// direction is "toward the text" by construction on light and dark themes
    /// alike. The old fixed-mix ladder had no such guarantee: because the
    /// segmented control's fill came from a stock grey rather than the theme,
    /// selecting a segment made it *darker* than its siblings on light themes
    /// and *lighter* on dark ones, by accident of where `#2f2f2f` happened to
    /// fall.
    pub fn surface(&self, base: u32) -> Surface {
        let bg = self.background_color();
        let fg = legible_foreground(bg, self.foreground);
        let selected = raise(base, fg, state::SELECTED);
        Surface {
            base,
            hover: raise(base, fg, state::HOVER),
            selected,
            pressed: raise(base, fg, state::PRESSED),
            // Dimmed from the foreground until it is merely AA-readable on this
            // surface, rather than a fixed blend — a resting label must stay
            // legible on an imported theme nobody vetted, too.
            text_resting: dim(fg, base, state::TEXT_RESTING),
            // Measured against the *selected fill*, not the surface: that fill is
            // the ground this particular label actually sits on. See `ink_on`.
            text_selected: ink_on(selected, fg, state::TEXT_RESTING),
        }
    }

    /// The status palette, built from this theme's own ANSI red/green/yellow/cyan.
    ///
    /// The normal (not bright) ANSI slots are the seeds: they are what the
    /// terminal in the same window paints, so a danger marker and an error line
    /// of shell output finally wear the same red. Where a slot is too pale or too
    /// dark for a role, [`legible_ink`] deepens it along its own hue rather than
    /// reaching for a colour the theme never declared.
    pub fn semantics(&self) -> Semantics {
        let bg = self.background_color();
        let fg = legible_foreground(bg, self.foreground);
        let ansi = |i: usize| -> u32 {
            let (r, g, b) = self.ansi16[i];
            (r as u32) << 16 | (g as u32) << 8 | b as u32
        };
        let build = |seed: u32| {
            let fill = legible_ink(bg, seed, ACCENT_FLOOR);
            Semantic {
                ink: legible_ink(bg, seed, TEXT_FLOOR),
                fill,
                on_fill: ink_on(fill, fg, TEXT_FLOOR),
            }
        };
        Semantics {
            danger: build(ansi(1)),
            success: build(ansi(2)),
            warning: build(ansi(3)),
            info: build(ansi(6)),
            link: build(ansi(6)),
        }
    }

    /// The ladders for all three surfaces the shell paints rows on.
    pub fn surfaces(&self) -> Surfaces {
        let m = self.neutrals();
        let mut sidebar = self.surface(m.sidebar);
        // The rail's resting label is a tuned value (a lighter 0.28 dim, so rows
        // in a sunk column don't read as disabled), not the generic AA floor.
        sidebar.text_resting = m.sidebar_fg;
        Surfaces {
            window: self.surface(m.background),
            sidebar,
            popover: self.surface(m.popover),
        }
    }

    /// The terminal-facing slice of the palette: ANSI-16 plus the selection
    /// surface (`mix(bg, fg, 0.24)`), which the renderer's search-match washes
    /// derive from. The selection itself paints as a translucent foreground wash
    /// (see `element::PaintColors::resolve`), so cells keep their own colors
    /// while selected.
    pub fn active_palette(&self) -> ActivePalette {
        let mut ansi16 = [Rgb { r: 0, g: 0, b: 0 }; 16];
        for (i, (r, g, b)) in self.ansi16.iter().enumerate() {
            ansi16[i] = Rgb {
                r: *r,
                g: *g,
                b: *b,
            };
        }
        let bg = self.background_color();
        let fg = legible_foreground(bg, self.foreground);
        ActivePalette {
            ansi16,
            sel_bg: rgb_bytes(mix(bg, fg, 0.24)),
        }
    }

    fn from_builtin(b: &BuiltinSpec) -> Theme {
        let bg = b.background;
        Theme {
            id: b.id.to_string(),
            name: b.name.to_string(),
            dark: is_dark(bg),
            background: Fill::Solid(bg),
            foreground: b.foreground,
            accent: b.accent,
            caret: b.caret,
            selection: None,
            opacity: None,
            blur: false,
            image: None,
            ansi16: b.ansi16,
            path: None,
        }
    }
}

/// The shared bisection behind [`raise`] and [`dim`]: find the blend of `from`
/// toward `toward` whose contrast against `from`-or-`toward` (whichever is the
/// surface, passed as `against`) sits at `target`.
///
/// `against` must **not** sit strictly between the endpoints in luminance, or
/// the ratio along the blend is V-shaped rather than monotone and a bisection
/// would return an arbitrary one of the two answers. Every caller satisfies
/// that: [`raise`] and [`dim`] pass one of the endpoints itself, and
/// [`legible_accent`] passes the background, which an accent only reaches this
/// code by being *close* to — while `fg` is guaranteed 4.5:1 away from it.
fn bisect_contrast(from: u32, toward: u32, against: u32, target: f32) -> u32 {
    // 12 halvings resolve t to ~0.0002 — far finer than an 8-bit channel step,
    // so the result is exact in the only units that reach the screen.
    const STEPS: u32 = 12;
    let rising = contrast(toward, against) > contrast(from, against);
    // Unreachable target (e.g. a 4.6:1 label floor on a surface whose own
    // foreground only manages 4.5): clamp to the most extreme blend rather than
    // returning something arbitrary from the middle of the range.
    if rising && contrast(toward, against) <= target {
        return toward;
    }
    if !rising && contrast(toward, against) >= target {
        return toward;
    }
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..STEPS {
        let m = 0.5 * (lo + hi);
        let reached = if rising {
            contrast(mix(from, toward, m), against) >= target
        } else {
            contrast(mix(from, toward, m), against) <= target
        };
        if reached { hi = m } else { lo = m }
    }
    mix(from, toward, hi)
}

/// Lift a fill off `base` toward `toward` (always the foreground) until it
/// clears `target` contrast against `base`.
///
/// This is the primitive that replaced fixed `mix(bg, fg, t)` state colors: the
/// caller names the perceived step it wants and gets it on every theme, instead
/// of naming a blend and getting whatever step that theme's seed implies.
fn raise(base: u32, toward: u32, target: f32) -> u32 {
    bisect_contrast(base, toward, base, target)
}

/// Dim `ink` toward `surface` until it sits *at* `target` contrast against
/// `surface` — a de-emphasised label that is still guaranteed readable, rather
/// than a fixed blend whose ratio drifts with the seed.
fn dim(ink: u32, surface: u32, target: f32) -> u32 {
    bisect_contrast(ink, surface, surface, target)
}

/// The label color for text sitting on `fill`: the theme's foreground when it
/// still clears `target` there, otherwise that foreground pushed *past* itself
/// (toward white on a dark fill, black on a light one) until it does.
///
/// This exists because the fill ladder and the text channel pull against each
/// other. Raising a fill toward the foreground necessarily moves the ground
/// closer to the label it carries, and on a theme whose foreground isn't an
/// extreme — Catppuccin Latte's `#4c4f69` is only 7.4:1 on its own background —
/// a 1.70:1 selected fill drags the selected label down to 4.14:1, *below* the
/// resting labels around it. A selection whose text is harder to read than its
/// neighbours' is not a selection.
///
/// Pushing along the fg→extreme axis rather than snapping to pure black/white
/// keeps the theme's ink hue; Latte's selected label becomes a deeper version of
/// the same blue-grey, not a foreign pure black.
///
/// Three tiers, in order of how much of the theme they preserve: the foreground
/// itself, then the foreground deepened along its own side, then — only when that
/// side simply cannot reach the target — the opposite extreme. That last tier is
/// not hypothetical: the Light theme's danger fill is `#d1242f`, a mid-dark red
/// against which even *pure black* tops out at 3.96:1. White text on a dark red
/// button is the right answer there, and it is only reachable by looking the
/// other way.
fn ink_on(fill: u32, fg: u32, target: f32) -> u32 {
    if contrast(fg, fill) >= target {
        return fg;
    }
    // Push *away* from the fill along the axis the foreground already sits on —
    // darker ink gets darker, lighter ink lighter. Choosing the extreme by the
    // fill's own brightness instead is wrong at the midpoint: Latte's `#b8bac6`
    // fill reads as "dark" to a `< 0.5` luminance test, which sends its already
    // dark ink toward white and *lowers* the contrast it was called to raise.
    let near = if relative_luminance(fg) < relative_luminance(fill) {
        0x000000
    } else {
        0xffffff
    };
    let deepened = bisect_contrast(fg, near, fill, target);
    if contrast(deepened, fill) >= target {
        return deepened;
    }
    // The foreground's own side is exhausted. Take whichever extreme reads best.
    if contrast(fill, 0xffffff) >= contrast(fill, 0x000000) {
        0xffffff
    } else {
        0x000000
    }
}

/// Blend `a` toward `b` by `t` (0.0 = all `a`, 1.0 = all `b`), per channel.
pub(crate) fn mix(a: u32, b: u32, t: f32) -> u32 {
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

// ── Contrast / brightness ───────────────────────────────────────────────────

/// WCAG relative luminance of a `0xRRGGBB` color (0.0 = black, 1.0 = white).
fn relative_luminance(c: u32) -> f32 {
    fn chan(v: u32) -> f32 {
        let s = v as f32 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * chan(c >> 16 & 0xff) + 0.7152 * chan(c >> 8 & 0xff) + 0.0722 * chan(c & 0xff)
}

/// The largest per-channel difference between two colors (0…255). A crude but
/// hue-aware "are these the same colour" check — contrast alone can't tell red
/// from green, since they can share a luminance.
#[cfg(test)]
fn channel_distance(a: u32, b: u32) -> u32 {
    let d = |sh: u32| (a >> sh & 0xff).abs_diff(b >> sh & 0xff);
    d(16).max(d(8)).max(d(0))
}

/// WCAG contrast ratio between two colors (1.0 … 21.0).
fn contrast(a: u32, b: u32) -> f32 {
    let (l1, l2) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (hi + 0.05) / (lo + 0.05)
}

/// A theme is dark when its background is closer to black than white.
fn is_dark(bg: u32) -> bool {
    relative_luminance(bg) < 0.5
}

/// Whether `a` is the lighter of two colors. Lets callers pick "the light end of
/// this theme's axis" without caring which of background/foreground that is —
/// e.g. a switch knob, which is near-white in both macOS appearances.
pub(crate) fn is_lighter(a: u32, b: u32) -> bool {
    relative_luminance(a) > relative_luminance(b)
}

/// The minimum contrast an accent must clear against the background before it is
/// allowed to carry ink (a caret, a link, a focus ring). 3:1 is the WCAG
/// large-text / non-text floor.
const ACCENT_FLOOR: f32 = 3.0;

/// The WCAG AA text floor. What a coloured *label* must clear on its ground.
const TEXT_FLOOR: f32 = 4.5;

/// Make a hued seed usable at `floor` against `bg`: keep it when it already
/// clears, otherwise drive it *away from the background* — toward white on a dark
/// theme, black on a light one — until it does.
///
/// This is why a seed colour can never be used raw. The bundled Light theme's
/// accent `#00c2ff` manages 2.07:1 on white and the built-ins' accents span
/// 2.07:1 to 8.43:1; the ANSI reds behind [`Semantics`] are just as uneven. Any
/// unconditional use of one is a coin flip on some theme.
///
/// It drives toward black/white rather than toward the theme's foreground because
/// the foreground is usually *tinted*, and blending into a tint destroys hue at
/// exactly the moment hue matters most — when a seed is far from the floor and so
/// has to travel far. On Rosé Pine Dawn, whose foreground is the purple-grey
/// `#575279`, routing through it turned the ANSI red into `#9a5e7a` and the ANSI
/// yellow into `#876a62`: two muddy mauves a user could not tell apart, which is
/// no use at all for "did that fail or is it just a warning". Black and white are
/// neutral, so the hue survives the trip.
fn legible_ink(bg: u32, seed: u32, floor: f32) -> u32 {
    if contrast(seed, bg) >= floor {
        return seed;
    }
    // Whichever extreme the background is *further* from, exactly as
    // [`legible_foreground`] picks it — not `is_dark`, whose 0.5 luminance
    // threshold is the wrong question here. The two answers only diverge on a
    // midtone background (luminance 0.18…0.5), where `is_dark` still says "dark"
    // but black outreaches white: an imported scheme on a mid-grey ground would
    // have been driven to pure white and clamped there *below* the floor, losing
    // the hue and failing the job in one go. Every built-in is far enough from
    // the midpoint that this picks what `is_dark` did.
    let away = if contrast(0xffffff, bg) >= contrast(0x000000, bg) {
        0xffffff
    } else {
        0x000000
    };
    bisect_contrast(seed, away, bg, floor)
}

/// The accent conditioned for ink (caret, focus ring, a switch's checked track).
fn legible_accent(bg: u32, accent: u32) -> u32 {
    legible_ink(bg, accent, ACCENT_FLOOR)
}

/// Guarantee a legible default text color: keep the authored `fg` if it clears
/// the WCAG AA text threshold (4.5) against `bg`, otherwise fall back to pure
/// black or white — whichever contrasts more. Protects hand-authored and
/// imported themes from an unreadable foreground without touching the many
/// built-ins that already pass.
fn legible_foreground(bg: u32, fg: u32) -> u32 {
    if contrast(bg, fg) >= 4.5 {
        return fg;
    }
    if contrast(bg, 0xffffff) >= contrast(bg, 0x000000) {
        0xffffff
    } else {
        0x000000
    }
}

/// The render-facing slice of the active theme's window background — fill,
/// window opacity, and optional image — published as a GPUI global by
/// `apply_theme` so the root view can paint gradients/images every frame
/// without re-resolving (and cloning) the whole theme registry.
pub struct ActiveBackground {
    pub fill: Fill,
    /// Window opacity, already filtered to `Some` only when < 1.0.
    pub opacity: Option<f32>,
    pub image: Option<Image>,
}

impl Global for ActiveBackground {}

// ── Registry ────────────────────────────────────────────────────────────────

/// The id of the app's default theme. Mirrors `Config`'s default `theme_preset`
/// (core can't reference this module). Unknown ids fall back to it.
pub const DEFAULT_ID: &str = "light";

/// The loaded set of themes (built-ins first, then user files), stored as a GPUI
/// global so any view can list/resolve them. Rebuilt from disk at startup and on
/// hot-reload.
pub struct Themes(pub Vec<Theme>);

impl Global for Themes {}

/// (Re)load built-ins + user theme files from disk into the [`Themes`] global.
/// Called at startup (before the first `apply_theme`) and on config hot-reload.
pub fn load_registry(cx: &mut App) {
    cx.set_global(Themes(load_all()));
}

/// All themes, in display order (built-ins first, then user files). Falls back to
/// just the built-ins if the registry hasn't been loaded yet (e.g. very early
/// startup).
pub fn all(cx: &App) -> Vec<Theme> {
    cx.try_global::<Themes>()
        .map(|t| t.0.clone())
        .unwrap_or_else(builtins)
}

/// Look a theme up by id, falling back to [`DEFAULT_ID`] (then the first theme)
/// for an unknown id so a stale/typo'd config never breaks startup.
pub fn by_id(cx: &App, id: &str) -> Theme {
    let themes = all(cx);
    themes
        .iter()
        .find(|t| t.id == id)
        .or_else(|| themes.iter().find(|t| t.id == DEFAULT_ID))
        .cloned()
        .unwrap_or_else(|| themes.into_iter().next().expect("at least the built-ins"))
}

impl Theme {
    /// Whether this theme is a user-owned, editable YAML file (as opposed to a
    /// read-only built-in or an imported `.itermcolors`, both of which must be
    /// duplicated first). Drives the in-app color editor and the duplicate action.
    pub fn editable(&self) -> bool {
        self.path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
    }
}

/// Serialize a theme into tty7's YAML schema — the inverse of [`load_yaml_theme`],
/// used by the duplicate action and the in-app editor to write themes to disk.
pub fn to_yaml(t: &Theme) -> String {
    fn hex(c: u32) -> String {
        format!("\"#{:06x}\"", c & 0xff_ffff)
    }
    fn rgb_hex((r, g, b): (u8, u8, u8)) -> String {
        format!("\"#{r:02x}{g:02x}{b:02x}\"")
    }
    let mut s = String::new();
    // `{:?}` on a String yields a double-quoted, escaped literal — valid YAML.
    s.push_str(&format!("name: {:?}\n", t.name));
    match &t.background {
        Fill::Solid(c) => s.push_str(&format!("background: {}\n", hex(*c))),
        Fill::Vertical { top, bottom } => s.push_str(&format!(
            "background: {{ top: {}, bottom: {} }}\n",
            hex(*top),
            hex(*bottom)
        )),
        Fill::Horizontal { left, right } => s.push_str(&format!(
            "background: {{ left: {}, right: {} }}\n",
            hex(*left),
            hex(*right)
        )),
    }
    s.push_str(&format!("foreground: {}\n", hex(t.foreground)));
    s.push_str(&format!("accent: {}\n", hex(t.accent)));
    if let Some(c) = t.caret {
        s.push_str(&format!("cursor: {}\n", hex(c)));
    }
    if let Some(c) = t.selection {
        s.push_str(&format!("selection: {}\n", hex(c)));
    }
    if let Some(o) = t.opacity {
        s.push_str(&format!("opacity: {o}\n"));
    }
    if t.blur {
        s.push_str("blur: true\n");
    }
    // Written back as the (expanded) absolute path: `expand_path` already
    // resolved `~`/relative forms on load, and dropping the field here would
    // silently delete a theme's image on the first in-app color edit.
    if let Some(img) = &t.image {
        s.push_str(&format!(
            "background_image:\n  path: {:?}\n  opacity: {}\n",
            img.path.display().to_string(),
            img.opacity
        ));
    }
    let row = |range: std::ops::Range<usize>| {
        range
            .map(|i| rgb_hex(t.ansi16[i]))
            .collect::<Vec<_>>()
            .join(", ")
    };
    s.push_str("ansi:\n");
    s.push_str(&format!("  normal: [{}]\n", row(0..8)));
    s.push_str(&format!("  bright: [{}]\n", row(8..16)));
    s
}

/// Duplicate `t` into a new editable YAML file in the themes folder, returning the
/// new theme's id (its file stem). The id is `<base>-custom` (deduplicated with a
/// numeric suffix), so duplicating "Dracula" yields "dracula-custom".
pub fn fork_to_file(t: &Theme) -> std::io::Result<String> {
    let dir = themes_dir()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no themes directory"))?;
    std::fs::create_dir_all(&dir)?;
    let base = format!("{}-custom", t.id.trim_end_matches("-custom"));
    let mut stem = base.clone();
    let mut n = 2;
    while dir.join(format!("{stem}.yaml")).exists() {
        stem = format!("{base}-{n}");
        n += 1;
    }
    let mut copy = t.clone();
    copy.name = format!("{} (custom)", t.name.trim_end_matches(" (custom)"));
    crate::core::config::write_atomic(
        &dir.join(format!("{stem}.yaml")),
        to_yaml(&copy).as_bytes(),
    )?;
    Ok(stem)
}

/// Write an edited theme back to its own file (the in-app color editor). Errors if
/// the theme isn't file-backed.
pub fn write_theme_file(t: &Theme) -> std::io::Result<()> {
    let path = t.path.clone().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "theme is not file-backed")
    })?;
    crate::core::config::write_atomic(&path, to_yaml(t).as_bytes())
}

/// Build the full theme list from disk: the built-ins, then every parseable user
/// file under the themes directory. A user file whose id collides with a built-in
/// is appended (both remain listed); `by_id` resolves to the first match, so
/// built-ins win a straight id clash.
fn load_all() -> Vec<Theme> {
    let mut themes = builtins();
    themes.extend(load_user_themes());
    dedupe_ids(&mut themes);
    themes
}

/// Guarantee every theme carries a unique `id` so `by_id` (and thus selection)
/// can address each one. Built-ins are added first and keep their canonical ids;
/// a later theme — typically a user file whose stem matches a built-in, e.g.
/// `dracula.itermcolors` vs the built-in `dracula` — gets the first free
/// `<id>-2`, `-3`, … and its display name is suffixed to match, so the gallery
/// doesn't show two identical labels and both entries stay selectable. Order is
/// stable (user paths are pre-sorted), so a given file keeps its id across
/// launches and a persisted `theme_preset` stays valid.
fn dedupe_ids(themes: &mut [Theme]) {
    let mut seen = std::collections::HashSet::new();
    for t in themes.iter_mut() {
        if seen.insert(t.id.clone()) {
            continue;
        }
        let base = t.id.clone();
        let mut n = 2;
        let mut candidate = format!("{base}-{n}");
        while !seen.insert(candidate.clone()) {
            n += 1;
            candidate = format!("{base}-{n}");
        }
        t.name = format!("{} ({n})", t.name);
        t.id = candidate;
    }
}

/// The themes directory, `~/.config/tty7/themes` (honoring `--config-dir`).
pub fn themes_dir() -> Option<PathBuf> {
    crate::core::config::config_path("themes")
}

/// Parse every `*.yaml` / `*.yml` / `*.itermcolors` file in the themes directory
/// into a [`Theme`]. Missing directory → empty. A file that fails to parse is
/// skipped with a warning; it never blocks the others or startup.
fn load_user_themes() -> Vec<Theme> {
    let Some(dir) = themes_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    // Stable, case-insensitive order so the gallery doesn't reshuffle per launch.
    paths.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    for path in paths {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        let parsed = match ext.as_deref() {
            Some("yaml") | Some("yml") => load_yaml_theme(&path),
            Some("itermcolors") => load_iterm_theme(&path),
            _ => continue,
        };
        match parsed {
            Ok(theme) => out.push(theme),
            Err(e) => log::warn!("skipping theme {}: {e}", path.display()),
        }
    }
    out
}

/// Derive a theme id/name from a file stem: the id is the raw stem, the name is
/// a title-cased version (`solarized_dark` → "Solarized Dark").
fn id_and_name(path: &std::path::Path) -> (String, String) {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("theme")
        .to_string();
    let name = stem
        .split(|c| c == '_' || c == '-' || c == ' ')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut cs = w.chars();
            match cs.next() {
                Some(f) => f.to_uppercase().collect::<String>() + cs.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    (
        stem,
        if name.is_empty() {
            "Theme".into()
        } else {
            name
        },
    )
}

// ── YAML theme files (tty7's own schema) ─────────────────────────────────────

/// A theme as authored in a `*.yaml` file. This is the on-disk schema; it
/// converts into a runtime [`Theme`]. Unknown fields are ignored by serde, so a
/// file may carry extra keys without failing.
#[derive(Deserialize)]
struct ThemeFile {
    name: Option<String>,
    background: FillFile,
    foreground: String,
    accent: String,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    selection: Option<String>,
    #[serde(default)]
    opacity: Option<f32>,
    #[serde(default)]
    blur: bool,
    #[serde(default)]
    background_image: Option<ImageFile>,
    ansi: AnsiFile,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FillFile {
    Solid(String),
    Vertical { top: String, bottom: String },
    Horizontal { left: String, right: String },
}

#[derive(Deserialize)]
struct AnsiFile {
    normal: [String; 8],
    bright: [String; 8],
}

#[derive(Deserialize)]
struct ImageFile {
    path: String,
    #[serde(default = "default_image_opacity")]
    opacity: f32,
}

fn default_image_opacity() -> f32 {
    0.3
}

fn load_yaml_theme(path: &std::path::Path) -> Result<Theme, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let file: ThemeFile = serde_yaml::from_str(&text).map_err(|e| e.to_string())?;
    let (id, derived_name) = id_and_name(path);

    let background = file.background.into_fill()?;
    let bg = background.color();
    let mut ansi16 = [(0u8, 0u8, 0u8); 16];
    for i in 0..8 {
        ansi16[i] = parse_rgb(&file.ansi.normal[i])?;
        ansi16[i + 8] = parse_rgb(&file.ansi.bright[i])?;
    }

    Ok(Theme {
        id,
        name: file.name.unwrap_or(derived_name),
        dark: is_dark(bg),
        background,
        foreground: parse_hex(&file.foreground)?,
        accent: parse_hex(&file.accent)?,
        caret: file.cursor.as_deref().map(parse_hex).transpose()?,
        selection: file.selection.as_deref().map(parse_hex).transpose()?,
        opacity: file.opacity.map(|o| o.clamp(0.0, 1.0)),
        blur: file.blur,
        image: file.background_image.map(|i| Image {
            path: expand_path(&i.path),
            opacity: i.opacity.clamp(0.0, 1.0),
        }),
        ansi16,
        path: Some(path.to_path_buf()),
    })
}

impl FillFile {
    fn into_fill(self) -> Result<Fill, String> {
        Ok(match self {
            FillFile::Solid(s) => Fill::Solid(parse_hex(&s)?),
            FillFile::Vertical { top, bottom } => Fill::Vertical {
                top: parse_hex(&top)?,
                bottom: parse_hex(&bottom)?,
            },
            FillFile::Horizontal { left, right } => Fill::Horizontal {
                left: parse_hex(&left)?,
                right: parse_hex(&right)?,
            },
        })
    }
}

/// Expand a leading `~` to `$HOME`; resolve a relative path against the themes
/// directory (so a theme can ship an image beside it).
fn expand_path(p: &str) -> PathBuf {
    let p = p.trim();
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    let path = PathBuf::from(p);
    if path.is_absolute() {
        return path;
    }
    themes_dir().map(|d| d.join(&path)).unwrap_or(path)
}

// ── iTerm2 `.itermcolors` import ─────────────────────────────────────────────

/// Import an iTerm2 color scheme (an XML plist). Maps `Ansi 0..15 Color` to the
/// ANSI-16 set, `Background/Foreground/Cursor Color` to the seed, and derives the
/// accent from the cursor (falling back to bright blue). iTerm's explicit
/// selection color is intentionally dropped — tty7 derives selection from
/// background/foreground for consistency.
fn load_iterm_theme(path: &std::path::Path) -> Result<Theme, String> {
    let value = plist::Value::from_file(path).map_err(|e| e.to_string())?;
    let dict = value
        .as_dictionary()
        .ok_or("not an iTerm color plist (expected a dictionary)")?;

    let color = |key: &str| -> Option<u32> {
        let c = dict.get(key)?.as_dictionary()?;
        let comp = |k: &str| -> Option<u32> {
            let f = c.get(k)?.as_real()?;
            Some((f.clamp(0.0, 1.0) * 255.0).round() as u32)
        };
        Some(
            (comp("Red Component")? << 16)
                | (comp("Green Component")? << 8)
                | comp("Blue Component")?,
        )
    };

    let mut ansi16 = [(0u8, 0u8, 0u8); 16];
    for i in 0..16 {
        let c = color(&format!("Ansi {i} Color"))
            .ok_or_else(|| format!("missing or malformed 'Ansi {i} Color'"))?;
        ansi16[i] = ((c >> 16) as u8, (c >> 8) as u8, c as u8);
    }

    let background = color("Background Color").ok_or("missing 'Background Color'")?;
    let foreground = color("Foreground Color").ok_or("missing 'Foreground Color'")?;
    let cursor = color("Cursor Color");
    // Accent: the cursor color when it's distinct enough from the background,
    // else bright blue (slot 12) — a sensible, always-present pick.
    let bright_blue = {
        let (r, g, b) = ansi16[12];
        (r as u32) << 16 | (g as u32) << 8 | b as u32
    };
    let accent = match cursor {
        Some(c) if contrast(background, c) >= 1.5 => c,
        _ => bright_blue,
    };

    let (id, name) = id_and_name(path);
    Ok(Theme {
        id,
        name,
        dark: is_dark(background),
        background: Fill::Solid(background),
        foreground,
        accent,
        caret: cursor,
        selection: None,
        opacity: None,
        blur: false,
        image: None,
        ansi16,
        path: Some(path.to_path_buf()),
    })
}

// ── Hex parsing ──────────────────────────────────────────────────────────────

/// Parse a `#rrggbb` (or bare `rrggbb`) string into a `0xRRGGBB` value.
fn parse_hex(s: &str) -> Result<u32, String> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return Err(format!("'{s}' is not a 6-digit hex color"));
    }
    u32::from_str_radix(hex, 16).map_err(|_| format!("'{s}' is not a hex color"))
}

/// Parse a `#rrggbb` string into an `(r, g, b)` byte triple.
fn parse_rgb(s: &str) -> Result<(u8, u8, u8), String> {
    let n = parse_hex(s)?;
    Ok(((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

// ── Built-in themes ──────────────────────────────────────────────────────────

/// The built-in themes as concrete [`Theme`] values (built-ins first in display
/// order: light themes, then dark).
pub fn builtins() -> Vec<Theme> {
    BUILTINS.iter().map(Theme::from_builtin).collect()
}

/// A built-in theme's seed data, kept as a static table (with `&'static str`
/// ids) and converted to an owned [`Theme`] by [`Theme::from_builtin`].
struct BuiltinSpec {
    id: &'static str,
    name: &'static str,
    background: u32,
    foreground: u32,
    accent: u32,
    caret: Option<u32>,
    ansi16: [(u8, u8, u8); 16],
}

/// A hand-picked set of familiar terminal palettes.
static BUILTINS: [BuiltinSpec; 8] = [
    BuiltinSpec {
        id: "light",
        name: "Light",
        background: 0xffffff,
        foreground: 0x111111,
        accent: 0x00c2ff,
        // A warm orange caret, distinct from the cyan accent (which also tints the
        // active-line highlight and links).
        caret: Some(0xf5a15c),
        // True-hue, high-contrast set tuned for a white ground (GitHub Light-ish).
        ansi16: [
            (0x24, 0x29, 0x2e),
            (0xd1, 0x24, 0x2f),
            (0x1a, 0x7f, 0x37),
            (0x9a, 0x67, 0x00),
            (0x09, 0x69, 0xda),
            (0x82, 0x50, 0xdf),
            (0x1b, 0x7c, 0x83),
            (0x6e, 0x77, 0x81),
            (0x57, 0x60, 0x6a),
            (0xcf, 0x22, 0x2e),
            (0x1f, 0x88, 0x3d),
            (0xbf, 0x87, 0x00),
            (0x21, 0x8b, 0xff),
            (0xa4, 0x75, 0xf9),
            (0x31, 0x92, 0xaa),
            (0x8c, 0x95, 0x9f),
        ],
    },
    BuiltinSpec {
        id: "one_light",
        name: "One Light",
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
    BuiltinSpec {
        id: "catppuccin_latte",
        name: "Catppuccin Latte",
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
    BuiltinSpec {
        id: "rose_pine_dawn",
        name: "Rosé Pine Dawn",
        background: 0xfaf4ed,
        foreground: 0x575279,
        accent: 0x907aa9,
        caret: None,
        ansi16: [
            (0xf2, 0xe9, 0xe1),
            (0xb4, 0x63, 0x7a),
            (0x28, 0x69, 0x83),
            (0xea, 0x9d, 0x34),
            (0x56, 0x94, 0x9f),
            (0x90, 0x7a, 0xa9),
            (0xd7, 0x82, 0x7e),
            (0x57, 0x52, 0x79),
            (0x98, 0x93, 0xa5),
            (0xb4, 0x63, 0x7a),
            (0x28, 0x69, 0x83),
            (0xea, 0x9d, 0x34),
            (0x56, 0x94, 0x9f),
            (0x90, 0x7a, 0xa9),
            (0xd7, 0x82, 0x7e),
            (0x57, 0x52, 0x79),
        ],
    },
    BuiltinSpec {
        id: "dark",
        name: "Dark",
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
    BuiltinSpec {
        id: "dracula",
        name: "Dracula",
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
    BuiltinSpec {
        id: "harbor",
        name: "Harbor",
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
    BuiltinSpec {
        id: "rose_pine",
        name: "Rosé Pine",
        background: 0x191724,
        foreground: 0xe0def4,
        accent: 0xc4a7e7,
        caret: None,
        ansi16: [
            (0x26, 0x23, 0x3a),
            (0xeb, 0x6f, 0x92),
            (0x31, 0x74, 0x8f),
            (0xf6, 0xc1, 0x77),
            (0x9c, 0xcf, 0xd8),
            (0xc4, 0xa7, 0xe7),
            (0xeb, 0xbc, 0xba),
            (0xe0, 0xde, 0xf4),
            (0x6e, 0x6a, 0x86),
            (0xeb, 0x6f, 0x92),
            (0x31, 0x74, 0x8f),
            (0xf6, 0xc1, 0x77),
            (0x9c, 0xcf, 0xd8),
            (0xc4, 0xa7, 0xe7),
            (0xeb, 0xbc, 0xba),
            (0xe0, 0xde, 0xf4),
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Default foreground must stay readable on the background in every built-in.
    #[test]
    fn foreground_is_legible_on_background() {
        for t in builtins() {
            let ratio = contrast(t.background_color(), t.foreground);
            assert!(
                ratio >= 4.0,
                "{}: fg/bg contrast too low ({ratio:.2})",
                t.id
            );
        }
    }

    /// Brightness is inferred correctly: the four light built-ins classify light,
    /// the four dark ones dark.
    #[test]
    fn dark_is_inferred_from_background() {
        let dark: Vec<_> = builtins()
            .into_iter()
            .filter(|t| t.dark)
            .map(|t| t.id)
            .collect();
        assert_eq!(dark, ["dark", "dracula", "harbor", "rose_pine"]);
    }

    /// The selection surface must stay a *tint* — decisively on the background's
    /// side of the fg↔bg axis — or selected text (whose glyphs keep their own
    /// color) would wash out.
    #[test]
    fn selection_surface_stays_on_the_background_side() {
        for t in builtins() {
            let ap = t.active_palette();
            let sel = (ap.sel_bg.r as u32) << 16 | (ap.sel_bg.g as u32) << 8 | ap.sel_bg.b as u32;
            let to_bg = contrast(sel, t.background_color());
            let to_fg = contrast(sel, t.foreground);
            assert!(
                to_fg > to_bg,
                "{}: selection surface sits closer to the foreground",
                t.id
            );
        }
    }

    /// Every surface's state ladder must be *strictly ordered and separable* on
    /// every built-in — this is the regression guard for issue #197, where the
    /// segmented control's selected fill sat 1.03:1 from its unselected siblings
    /// on Dracula (and no better than 1.20:1 on any other bundled theme).
    ///
    /// The assertions are deliberately below the [`state`] targets: they pin the
    /// *property* (a selection is distinguishable from resting and from hover on
    /// every surface of every theme), not the current taste, so retuning the
    /// constants doesn't force a test edit but abandoning the ladder does.
    #[test]
    fn state_ladder_is_separable_on_every_surface() {
        for t in builtins() {
            let s = t.surfaces();
            for (name, sf) in [
                ("window", s.window),
                ("sidebar", s.sidebar),
                ("popover", s.popover),
            ] {
                let sel_base = contrast(sf.selected, sf.base);
                let sel_hover = contrast(sf.selected, sf.hover);
                let hover_base = contrast(sf.hover, sf.base);
                assert!(
                    sel_base >= 1.6,
                    "{}/{name}: selected is only {sel_base:.2}:1 from the surface",
                    t.id
                );
                assert!(
                    sel_hover >= 1.3,
                    "{}/{name}: selected is only {sel_hover:.2}:1 from hover",
                    t.id
                );
                assert!(
                    hover_base >= 1.1,
                    "{}/{name}: hover is only {hover_base:.2}:1 from the surface",
                    t.id
                );
                assert!(
                    contrast(sf.pressed, sf.base) > sel_base,
                    "{}/{name}: pressed must read past selected",
                    t.id
                );
            }
        }
    }

    /// The whole point of a ratio target over a blend ratio: the *perceived* step
    /// is the same on every theme. A fixed `mix` put selected-vs-resting between
    /// 1.20:1 and 1.47:1 depending on the seed; these must all agree.
    #[test]
    fn state_ladder_is_theme_independent() {
        let ratios: Vec<f32> = builtins()
            .iter()
            .map(|t| {
                let w = t.surfaces().window;
                contrast(w.selected, w.base)
            })
            .collect();
        let (lo, hi) = ratios
            .iter()
            .fold((f32::MAX, 0.0f32), |(l, h), r| (l.min(*r), h.max(*r)));
        assert!(
            hi - lo < 0.05,
            "selected step drifts across themes: {lo:.2}:1 … {hi:.2}:1"
        );
        assert!(
            (lo - state::SELECTED).abs() < 0.05,
            "selected step {lo:.2}:1 missed its {:.2}:1 target",
            state::SELECTED
        );
    }

    /// Anchoring `SELECTED` to 1.70 must leave the signed-off Dracula highlight
    /// where it was — the value the palette/menu look was tuned against. This is
    /// what makes the fix a no-op on the theme it was designed on and a lift for
    /// everything else; if a retune moves Dracula, that was a taste decision and
    /// wants to be a deliberate one.
    #[test]
    fn dracula_selection_matches_the_signed_off_grey() {
        let dracula = builtins().into_iter().find(|t| t.id == "dracula").unwrap();
        let bg = dracula.background_color();
        let legacy = mix(bg, dracula.foreground, 0.17); // the old `list_active`
        let now = dracula.surfaces().window.selected;
        assert!(
            contrast(now, legacy) < 1.05,
            "Dracula's selection moved: {now:#08x} vs the tuned {legacy:#08x}"
        );
    }

    /// A resting label must clear WCAG AA on the surface it sits on, for every
    /// theme *and* every surface — a menu row's label sits on `popover`, not on
    /// the window background, and the fixed dim it replaced was anchored to the
    /// latter wherever it was used.
    #[test]
    fn resting_labels_stay_readable() {
        for t in builtins() {
            let s = t.surfaces();
            for (name, sf) in [("window", s.window), ("popover", s.popover)] {
                let ratio = contrast(sf.text_resting, sf.base);
                assert!(
                    ratio >= 4.5,
                    "{}/{name}: resting label only {ratio:.2}:1",
                    t.id
                );
            }
        }
    }

    /// The text channel's two invariants, on every surface of every theme.
    ///
    /// 1. A selected label is readable **on its own fill** — never merely on the
    ///    surface it would have sat on unselected. Getting this wrong is subtle:
    ///    raising the fill toward the foreground eats the label's contrast, and
    ///    Catppuccin Latte's selected label landed at 4.14:1 (below the 4.57:1 of
    ///    the *resting* labels beside it) before `ink_on` existed.
    /// 2. The two label colors differ enough to read as a step, so the channel
    ///    still says something when the fill is washed out — a translucent
    ///    window, a blurred background, an imported seed nobody vetted.
    #[test]
    fn label_channel_is_readable_and_stepped() {
        for t in builtins() {
            let s = t.surfaces();
            for (name, sf) in [
                ("window", s.window),
                ("sidebar", s.sidebar),
                ("popover", s.popover),
            ] {
                let on_fill = contrast(sf.text_selected, sf.selected);
                assert!(
                    on_fill >= 4.5,
                    "{}/{name}: selected label only {on_fill:.2}:1 on its own fill",
                    t.id
                );
                let step = contrast(sf.text_selected, sf.text_resting);
                assert!(
                    step >= 1.35,
                    "{}/{name}: label step is only {step:.2}:1 — the channel says nothing",
                    t.id
                );
            }
        }
    }

    /// A switch's two tracks must both be distinguishable *from each other* and
    /// from the surface, and the knob — one colour serving both states — has to
    /// stay visible on each.
    ///
    /// The toggles shipped inverted on every dark theme (stock near-black knob on
    /// a near-white checked track, and invisible on the unchecked one) because
    /// `switch`, `switch_thumb` and `tokens.background` were all unset. This pins
    /// the arrangement that replaced it: knob at the light end of the theme's
    /// axis, unchecked track on the ladder, checked track on the accent.
    ///
    /// The knob-on-checked-track floor is 1.25, not 3 — a white knob on a
    /// coloured track is separated by the component's `shadow_md`, exactly as it
    /// is in macOS, and demanding raw contrast there would force every accent to
    /// go dark.
    #[test]
    fn switch_tracks_and_knob_stay_legible() {
        for t in builtins() {
            let m = t.neutrals();
            let unchecked = t.surfaces().window.selected;
            let checked = m.accent;
            let knob = if is_lighter(m.background, m.foreground) {
                m.background
            } else {
                m.foreground
            };
            assert!(
                contrast(knob, unchecked) >= 1.25,
                "{}: knob {knob:#08x} lost on the unchecked track {unchecked:#08x}",
                t.id
            );
            assert!(
                contrast(knob, checked) >= 1.25,
                "{}: knob {knob:#08x} lost on the checked track {checked:#08x}",
                t.id
            );
            // The two states must not be near-identical greys, or the switch says
            // nothing but the knob's position.
            assert!(
                contrast(checked, unchecked) >= 1.3,
                "{}: checked and unchecked tracks are {:.2}:1 apart",
                t.id,
                contrast(checked, unchecked)
            );
        }
    }

    /// Status colours must clear their floors on every theme, and — the point of
    /// deriving them from the theme's own ANSI-16 — must stay *recognisable* as
    /// red / green / yellow rather than converging on the foreground.
    ///
    /// Before this, `danger` was gpui-component's stock `#f87171` on every theme:
    /// 2.45:1 on Catppuccin Latte (under even the 3:1 non-text floor) and a
    /// different red from the `#ff5555` the terminal beside it paints.
    #[test]
    fn semantic_colors_clear_their_floors() {
        for t in builtins() {
            let bg = t.background_color();
            let s = t.semantics();
            for (name, c) in [
                ("danger", s.danger),
                ("warning", s.warning),
                ("success", s.success),
                ("info", s.info),
                ("link", s.link),
            ] {
                assert!(
                    contrast(c.ink, bg) >= TEXT_FLOOR - 0.01,
                    "{}/{name}: ink {:#08x} only {:.2}:1 on the background",
                    t.id,
                    c.ink,
                    contrast(c.ink, bg)
                );
                assert!(
                    contrast(c.fill, bg) >= ACCENT_FLOOR - 0.01,
                    "{}/{name}: fill {:#08x} only {:.2}:1 on the background",
                    t.id,
                    c.fill,
                    contrast(c.fill, bg)
                );
                assert!(
                    contrast(c.on_fill, c.fill) >= TEXT_FLOOR - 0.01,
                    "{}/{name}: text on its own fill is only {:.2}:1",
                    t.id,
                    contrast(c.on_fill, c.fill)
                );
            }
            // Conditioning must not wash the hues into each other: a user has to
            // be able to tell an error from a success without reading the label.
            for (a, b, pair) in [
                (s.danger.ink, s.success.ink, "danger/success"),
                (s.danger.ink, s.warning.ink, "danger/warning"),
                (s.success.ink, s.warning.ink, "success/warning"),
            ] {
                assert!(
                    channel_distance(a, b) >= 40,
                    "{}: {pair} collapsed to nearly the same colour ({a:#08x} vs {b:#08x})",
                    t.id
                );
            }
        }
    }

    /// Each status colour must stay recognisably its own theme's hue — that is
    /// the whole reason for sourcing them from ANSI-16 rather than a brand set.
    /// Where a seed already clears its floor it must pass through untouched.
    #[test]
    fn semantic_colors_keep_the_theme_hue() {
        let dracula = builtins().into_iter().find(|t| t.id == "dracula").unwrap();
        let ansi_red = {
            let (r, g, b) = dracula.ansi16[1];
            (r as u32) << 16 | (g as u32) << 8 | b as u32
        };
        assert_eq!(ansi_red, 0xff5555, "Dracula's ANSI red moved");
        // 4.53:1 on Dracula's background — already over AA, so it is used as-is
        // and the danger dot matches the terminal's own error output exactly.
        assert_eq!(dracula.semantics().danger.ink, ansi_red);
    }

    /// Every theme's accent must be able to carry ink (caret, link, focus ring).
    /// The bundled Light theme's raw `#00c2ff` manages 2.07:1 on white, which is
    /// why this conditioning exists rather than using the seed directly.
    #[test]
    fn accents_are_conditioned_to_carry_ink() {
        for t in builtins() {
            let bg = t.background_color();
            let a = t.neutrals().accent;
            let ratio = contrast(a, bg);
            assert!(
                ratio >= ACCENT_FLOOR - 0.01,
                "{}: accent {a:#08x} only {ratio:.2}:1 on the background",
                t.id
            );
        }
        // ...and a seed that already clears the floor is passed through untouched,
        // so conditioning never dulls a theme that didn't need it.
        let rose = builtins()
            .into_iter()
            .find(|t| t.id == "rose_pine")
            .unwrap();
        assert_eq!(rose.neutrals().accent, rose.accent);
    }

    /// `raise`/`dim` must land *just* past their targets from either direction,
    /// and clamp rather than return a mid-range guess when one is unreachable.
    ///
    /// "Just past" is one 8-bit channel step, not zero: the tightest grey clearing
    /// 2.0:1 on black is `#404040` at 2.025:1, because a channel step near there
    /// moves the ratio by ~0.03. Anything tighter would be asserting sub-pixel
    /// precision the framebuffer can't hold.
    #[test]
    fn contrast_bisection_hits_its_target() {
        const SLACK: f32 = 0.05;
        // Reachable, rising: a fill lifted off black.
        let f = raise(0x000000, 0xffffff, 2.0);
        assert!((2.0..2.0 + SLACK).contains(&contrast(f, 0x000000)));
        // Reachable, rising: lifted off white — the direction flips, the API
        // doesn't (this is what the old fixed-mix ladder got wrong per theme).
        let f = raise(0xffffff, 0x000000, 2.0);
        assert!((2.0..2.0 + SLACK).contains(&contrast(f, 0xffffff)));
        // Reachable, falling: white ink dimmed to just above AA on black.
        let d = dim(0xffffff, 0x000000, 4.5);
        assert!((contrast(d, 0x000000) - 4.5).abs() < SLACK);
        // Unreachable: nothing between these two clears 21:1, so clamp to the
        // far endpoint instead of bisecting to something arbitrary.
        assert_eq!(raise(0x000000, 0x808080, 21.0), 0x808080);
    }

    /// Conditioning has to take the extreme it can actually *reach*, which on a
    /// midtone ground is not the one `is_dark`'s 0.5 luminance threshold names.
    /// A mid-grey background is "dark" by that test, yet white tops out at
    /// 3.95:1 on it while black manages 5.32:1 — so driving toward white would
    /// clamp at pure white, below the floor and with the hue thrown away, in the
    /// one case where a status colour most needs both. Reachable only for an
    /// imported scheme; every built-in sits far enough from the midpoint that
    /// this picks the same extreme `is_dark` did.
    #[test]
    fn semantic_conditioning_survives_a_midtone_background() {
        let bg = 0x808080;
        for seed in [0xff5555u32, 0x50fa7b, 0xf1fa8c, 0x8be9fd] {
            let ink = legible_ink(bg, seed, TEXT_FLOOR);
            assert!(
                contrast(ink, bg) >= TEXT_FLOOR - 0.01,
                "{seed:#08x} conditioned to {ink:#08x}, only {:.2}:1 on a midtone ground",
                contrast(ink, bg)
            );
        }
    }

    /// A bad foreground is swapped for a legible black/white; a good one is kept.
    #[test]
    fn legible_foreground_rescues_unreadable_text() {
        // Light-grey text on white is unreadable → forced to black.
        assert_eq!(legible_foreground(0xffffff, 0xeeeeee), 0x000000);
        // A genuine dark foreground on white is kept.
        assert_eq!(legible_foreground(0xffffff, 0x111111), 0x111111);
        // Dark-grey on black is unreadable → forced to white.
        assert_eq!(legible_foreground(0x000000, 0x222222), 0xffffff);
    }

    #[test]
    fn parse_hex_accepts_optional_hash_and_rejects_junk() {
        assert_eq!(parse_hex("#123456").unwrap(), 0x123456);
        assert_eq!(parse_hex("abcdef").unwrap(), 0xabcdef);
        assert!(parse_hex("#fff").is_err());
        assert!(parse_hex("nope!!").is_err());
    }

    /// A minimal YAML theme parses, derives its name from the caller-supplied id,
    /// and lays its ANSI set out normal-then-bright.
    #[test]
    fn yaml_theme_parses_normal_then_bright() {
        let yaml = r##"
background: "#101010"
foreground: "#e0e0e0"
accent: "#ff8800"
ansi:
  normal: ["#000000","#111111","#222222","#333333","#444444","#555555","#666666","#777777"]
  bright: ["#888888","#999999","#aaaaaa","#bbbbbb","#cccccc","#dddddd","#eeeeee","#ffffff"]
"##;
        let file: ThemeFile = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(file.background, FillFile::Solid(_)));
        let bg = file.background.into_fill().unwrap().color();
        assert_eq!(bg, 0x101010);
        assert_eq!(parse_rgb(&file.ansi.normal[0]).unwrap(), (0, 0, 0));
        assert_eq!(parse_rgb(&file.ansi.bright[7]).unwrap(), (0xff, 0xff, 0xff));
    }

    /// A gradient background deserializes and reports its top stop as the
    /// representative color.
    #[test]
    fn yaml_gradient_background_parses() {
        let file: ThemeFile = serde_yaml::from_str(
            r##"
background: { top: "#001122", bottom: "#334455" }
foreground: "#ffffff"
accent: "#ff0000"
ansi:
  normal: ["#000000","#000000","#000000","#000000","#000000","#000000","#000000","#000000"]
  bright: ["#ffffff","#ffffff","#ffffff","#ffffff","#ffffff","#ffffff","#ffffff","#ffffff"]
"##,
        )
        .unwrap();
        let fill = file.background.into_fill().unwrap();
        assert_eq!(
            fill,
            Fill::Vertical {
                top: 0x001122,
                bottom: 0x334455
            }
        );
        assert_eq!(fill.color(), 0x001122);
    }

    /// Window fields and the background image must survive a serialize → parse
    /// round trip, or an in-app color edit would silently strip them from the
    /// user's file.
    #[test]
    fn to_yaml_round_trips_window_and_image_fields() {
        let mut theme = builtins().into_iter().next().unwrap();
        theme.background = Fill::Vertical {
            top: 0x001122,
            bottom: 0x334455,
        };
        theme.opacity = Some(0.85);
        theme.blur = true;
        theme.image = Some(Image {
            path: PathBuf::from("/pictures/koi.jpg"),
            opacity: 0.4,
        });
        let file: ThemeFile = serde_yaml::from_str(&to_yaml(&theme)).unwrap();
        assert_eq!(
            file.background.into_fill().unwrap(),
            theme.background,
            "gradient background lost"
        );
        assert_eq!(file.opacity, Some(0.85));
        assert!(file.blur);
        let img = file.background_image.expect("image field lost");
        assert_eq!(img.path, "/pictures/koi.jpg");
        assert_eq!(img.opacity, 0.4);
    }

    #[test]
    fn id_and_name_titlecases_the_stem() {
        let (id, name) = id_and_name(std::path::Path::new("/x/solarized_dark.yaml"));
        assert_eq!(id, "solarized_dark");
        assert_eq!(name, "Solarized Dark");
    }

    /// `mix` endpoints and midpoint behave.
    #[test]
    fn mix_blends_channels() {
        assert_eq!(mix(0x000000, 0xffffff, 0.0), 0x000000);
        assert_eq!(mix(0x000000, 0xffffff, 1.0), 0xffffff);
        assert_eq!(mix(0x000000, 0xffffff, 0.5), 0x808080);
    }
}

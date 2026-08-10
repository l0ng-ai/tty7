use std::path::PathBuf;

use alacritty_terminal::vte::ansi::Rgb;
use gpui::{App, Global, Hsla};
use serde::Deserialize;

use crate::terminal::palette::ActivePalette;

#[derive(Debug, Clone, PartialEq)]
pub enum Fill {
    Solid(u32),
    Vertical { top: u32, bottom: u32 },
    Horizontal { left: u32, right: u32 },
}

impl Fill {
    pub fn color(&self) -> u32 {
        match *self {
            Fill::Solid(c) => c,
            Fill::Vertical { top, .. } => top,
            Fill::Horizontal { left, .. } => left,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub path: PathBuf,
    pub opacity: f32,
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub id: String,
    pub name: String,
    pub dark: bool,
    pub background: Fill,
    pub foreground: u32,
    pub accent: u32,
    pub caret: Option<u32>,
    pub selection: Option<u32>,
    pub opacity: Option<f32>,
    pub blur: bool,
    pub image: Option<Image>,
    pub ansi16: [(u8, u8, u8); 16],
    pub path: Option<PathBuf>,
}

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
    pub accent: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Semantic {
    pub ink: u32,
    pub fill: u32,
    pub on_fill: u32,
}

#[derive(Debug, Clone)]
pub struct Semantics {
    pub danger: Semantic,
    pub warning: Semantic,
    pub success: Semantic,
    pub info: Semantic,
    pub link: Semantic,
}

pub mod state {
    pub const HOVER: f32 = 1.18;
    pub const SELECTED: f32 = 1.30;
    pub const PRESSED: f32 = 1.55;
    pub const CURSOR: f32 = 1.70;
    pub const TEXT_RESTING: f32 = 4.6;
    pub const TEXT_STEP: f32 = 1.4;
}

#[derive(Debug, Clone, Copy)]
pub struct Surface {
    pub base: u32,
    pub hover: u32,
    pub selected: u32,
    pub pressed: u32,
    pub cursor: u32,
    pub text_resting: u32,
    pub text_selected: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Scrim {
    pub ink: u32,
    pub alpha: f32,
}

#[derive(Debug, Clone)]
pub struct Surfaces {
    pub window: Surface,
    pub sidebar: Surface,
    pub popover: Surface,
    pub scrim: Scrim,
}

impl Global for Surfaces {}

pub fn scrim_fill(cx: &App) -> Hsla {
    let s = cx.global::<Surfaces>().scrim;
    Hsla::from(gpui::rgb(s.ink)).opacity(s.alpha)
}

pub struct ActiveAccent(pub u32);

impl Global for ActiveAccent {}

/// How many lanes of the commit graph get a colour of their own.
///
/// Six because that is how many hues of the ANSI set survive being pulled to a
/// contrast floor while staying apart from each other — and because the graph
/// caps its visible lanes at the same number, which is what guarantees no two
/// columns on screen are ever the same colour.
pub const LANE_SLOTS: usize = 6;

#[derive(Debug, Clone, Copy)]
pub struct Lanes {
    pub ink: [u32; LANE_SLOTS],
    /// Everything past the last slot shares one column, so it gets a neutral:
    /// a hue there would claim a branch identity the column does not have.
    pub overflow: u32,
}

pub struct ActiveLanes(pub Lanes);

impl Global for ActiveLanes {}

impl Theme {
    pub fn background_color(&self) -> u32 {
        self.background.color()
    }

    pub fn neutrals(&self) -> Neutrals {
        let bg = self.background_color();
        let fg = legible_foreground(bg, self.foreground);
        let sidebar = mix(bg, fg, 0.03);
        let popover = mix(bg, fg, 0.05);
        // One hairline value divides all three neutral fills — it is handed to
        // `sidebar_border` too, and popover chrome draws with it. Floor it on
        // each of them, not only on the window.
        let border = [bg, sidebar, popover]
            .into_iter()
            .fold(mix(bg, fg, 0.16), |hairline, surface| {
                at_least(hairline, fg, surface, BORDER_FLOOR)
            });
        Neutrals {
            background: bg,
            foreground: fg,
            border,
            secondary: mix(bg, fg, 0.09),
            muted: mix(bg, fg, 0.06),
            muted_foreground: dim(fg, bg, state::TEXT_RESTING),
            popover,
            caret: legible_ink(bg, self.caret.unwrap_or(self.accent), ACCENT_FLOOR),
            selection: self.selection.unwrap_or_else(|| mix(bg, fg, 0.20)),
            sidebar,
            // Blended, not bisected, so a palette's own softness carries into
            // the sidebar — but floored on the fill it is actually painted on
            // (`sidebar`, not `background`), because four of the nine builtins
            // land this under 4.5:1 and it is the tab title, not a caption.
            sidebar_fg: at_least(mix(fg, bg, 0.28), fg, sidebar, TEXT_FLOOR),
            accent: legible_accent(bg, self.accent),
        }
    }

    pub fn surface(&self, base: u32) -> Surface {
        let bg = self.background_color();
        let fg = legible_foreground(bg, self.foreground);
        let selected = raise(base, fg, state::SELECTED);
        let text_resting = dim(fg, base, state::TEXT_RESTING);
        Surface {
            base,
            hover: raise(base, fg, state::HOVER),
            selected,
            pressed: raise(base, fg, state::PRESSED),
            cursor: raise(base, fg, state::CURSOR),
            text_resting,
            text_selected: stepped_ink(selected, base, fg, text_resting),
        }
    }

    fn ansi_seed(&self, index: usize) -> u32 {
        let (r, g, b) = self.ansi16[index];
        (r as u32) << 16 | (g as u32) << 8 | b as u32
    }

    /// Clears a seed colour for use as ink on any of tty7's neutral fills.
    ///
    /// An error line lands on a popover or a sidebar row as often as on the
    /// window, and both of those fills sit a step toward the foreground. Clear
    /// the floor on every surface the ink can be painted on, not just on the
    /// darkest one.
    fn clear_ink(&self, seed: u32, floor: f32) -> u32 {
        let bg = self.background_color();
        let fg = legible_foreground(bg, self.foreground);
        [bg, mix(bg, fg, 0.03), mix(bg, fg, 0.05)]
            .into_iter()
            .fold(seed, |ink, surface| legible_ink(surface, ink, floor))
    }

    /// One entry of the theme's own ANSI ramp, legible as chrome text.
    ///
    /// The ramp is authored for terminal cells, where a colour only has to
    /// stand on the background. Chrome draws the same hues on the sidebar and
    /// popover fills too, so they go through the same floor the semantic inks
    /// do.
    pub fn ansi_ink(&self, index: usize) -> u32 {
        self.clear_ink(self.ansi_seed(index), TEXT_FLOOR)
    }

    pub fn semantics(&self) -> Semantics {
        let bg = self.background_color();
        let fg = legible_foreground(bg, self.foreground);
        let build = |seed: u32| {
            let fill = self.clear_ink(seed, ACCENT_FLOOR);
            Semantic {
                ink: self.clear_ink(seed, TEXT_FLOOR),
                fill,
                on_fill: ink_on(fill, fg, TEXT_FLOOR),
            }
        };
        Semantics {
            danger: build(self.ansi_seed(1)),
            success: build(self.ansi_seed(2)),
            warning: build(self.ansi_seed(3)),
            info: build(self.ansi_seed(6)),
            link: build(self.ansi_seed(6)),
        }
    }

    /// Lane colours for the commit graph, derived the same way every other
    /// colour in this file is: seeded from the theme's own palette, then walked
    /// to a contrast floor on each surface it can be painted on.
    ///
    /// Not a fixed table of hexes. A hard-coded palette would be the one thing
    /// here that does not follow the theme, and — worse — the contrast tests
    /// below cannot see it, so the four light builtins would ship a graph whose
    /// lanes sit at 2:1 against their own background.
    ///
    /// The seed order is blue, yellow, magenta, green, cyan, red. Three
    /// constraints picked it: no two adjacent slots share a hue family; red and
    /// green are never neighbours, for the readers who cannot tell them apart;
    /// and red is last because a panel three or four lanes wide never reaches
    /// it, so the one colour that also means "danger" everywhere else in the UI
    /// stays out of the common case.
    pub fn lanes(&self) -> Lanes {
        const SEEDS: [usize; LANE_SLOTS] = [4, 3, 5, 2, 6, 1];
        let bg = self.background_color();
        let fg = legible_foreground(bg, self.foreground);
        // Through `clear_ink`, the same three surfaces `semantics` clears on:
        // the graph draws on the window in a floating panel, on the sidebar
        // when the panel is docked, and on a popover in the detail view.
        let mut ink = [0u32; LANE_SLOTS];
        for (slot, seed) in SEEDS.iter().enumerate() {
            ink[slot] = self.clear_ink(self.ansi_seed(*seed), ACCENT_FLOOR);
        }
        Lanes {
            ink,
            overflow: dim(fg, bg, state::TEXT_RESTING),
        }
    }

    pub fn surfaces(&self) -> Surfaces {
        let m = self.neutrals();
        let mut sidebar = self.surface(m.sidebar);
        sidebar.text_resting = m.sidebar_fg;
        sidebar.text_selected = stepped_ink(
            sidebar.selected,
            sidebar.base,
            legible_foreground(self.background_color(), self.foreground),
            sidebar.text_resting,
        );
        Surfaces {
            window: self.surface(m.background),
            sidebar,
            popover: self.surface(m.popover),
            scrim: Scrim {
                ink: mix(m.background, 0x000000, 0.82),
                alpha: match self.dark {
                    true => 0.55,
                    false => 0.30,
                },
            },
        }
    }

    pub fn active_palette(&self, legible: bool) -> ActivePalette {
        let bg = self.background_color();
        let fg = legible_foreground(bg, self.foreground);
        let mut ansi16 = [Rgb { r: 0, g: 0, b: 0 }; 16];
        for (i, (r, g, b)) in self.ansi16.iter().enumerate() {
            let ink = (*r as u32) << 16 | (*g as u32) << 8 | *b as u32;
            // The bright half of the palette (SGR 90-97) is the shell's
            // readable-variant family: PSReadLine paints parameters, operators
            // and members with it, so a bright slot that cannot clear the text
            // floor on the theme background is walked toward the foreground
            // until it can — the same rescue `sidebar_fg` uses. Dark themes
            // tend to author it as a muted grey (invisible), light themes as a
            // pale grey (invisible the other way); both are lifted here, while
            // already-legible slots render byte-for-byte as authored. The
            // `legible` flag mirrors Settings → Appearance: off renders the
            // palette exactly as authored.
            ansi16[i] = rgb_bytes(if legible && i >= 8 {
                at_least(ink, fg, bg, TEXT_FLOOR)
            } else {
                ink
            });
        }
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

fn bisect_contrast(from: u32, toward: u32, against: u32, target: f32) -> u32 {
    const STEPS: u32 = 12;
    let rising = contrast(toward, against) > contrast(from, against);
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

fn raise(base: u32, toward: u32, target: f32) -> u32 {
    bisect_contrast(base, toward, base, target)
}

fn dim(ink: u32, surface: u32, target: f32) -> u32 {
    bisect_contrast(ink, surface, surface, target)
}

fn stepped_ink(fill: u32, base: u32, fg: u32, resting: u32) -> u32 {
    let ink = ink_on(fill, fg, state::TEXT_RESTING);
    if contrast(ink, resting) >= state::TEXT_STEP {
        return ink;
    }
    let away = match relative_luminance(base) < relative_luminance(resting) {
        true => 0xffffff,
        false => 0x000000,
    };
    bisect_contrast(ink, away, resting, state::TEXT_STEP)
}

fn ink_on(fill: u32, fg: u32, target: f32) -> u32 {
    if contrast(fg, fill) >= target {
        return fg;
    }
    let near = if relative_luminance(fg) < relative_luminance(fill) {
        0x000000
    } else {
        0xffffff
    };
    let deepened = bisect_contrast(fg, near, fill, target);
    if contrast(deepened, fill) >= target {
        return deepened;
    }
    if contrast(fill, 0xffffff) >= contrast(fill, 0x000000) {
        0xffffff
    } else {
        0x000000
    }
}

/// The colour a glyph is redrawn in once an opaque block caret sits under it.
/// The terminal's own background is the conventional choice and wins in every
/// shipped theme; the foreground is the fallback for a caret close enough to
/// the background to swallow it.
pub(crate) fn caret_ink(caret: Hsla, background: Hsla, foreground: Hsla) -> Hsla {
    let pack = |c: Hsla| {
        let rgb = crate::terminal::palette::hsla_to_rgb(c);
        (rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32
    };
    let (caret, bg, fg) = (pack(caret), pack(background), pack(foreground));
    if contrast(caret, bg) >= contrast(caret, fg) {
        background
    } else {
        foreground
    }
}

/// Whether a filled shape needs a hairline to stay a shape. A brand colour is a
/// fixed value; a theme background is not, and pure black on a dark window is
/// no shape at all.
pub(crate) fn needs_edge(fill: u32, surface: Hsla) -> bool {
    let rgb = crate::terminal::palette::hsla_to_rgb(surface);
    let packed = (rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32;
    contrast(fill, packed) < 1.25
}

pub(crate) fn mix(a: u32, b: u32, t: f32) -> u32 {
    let (ar, ag, ab) = (a >> 16 & 0xff, a >> 8 & 0xff, a & 0xff);
    let (br, bg, bb) = (b >> 16 & 0xff, b >> 8 & 0xff, b & 0xff);
    let ch = |x: u32, y: u32| (x as f32 + (y as f32 - x as f32) * t).round() as u32;
    (ch(ar, br) << 16) | (ch(ag, bg) << 8) | ch(ab, bb)
}

fn rgb_bytes(n: u32) -> Rgb {
    Rgb {
        r: (n >> 16) as u8,
        g: (n >> 8) as u8,
        b: n as u8,
    }
}

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

#[cfg(test)]
fn channel_distance(a: u32, b: u32) -> u32 {
    let d = |sh: u32| (a >> sh & 0xff).abs_diff(b >> sh & 0xff);
    d(16).max(d(8)).max(d(0))
}

fn contrast(a: u32, b: u32) -> f32 {
    let (l1, l2) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (hi + 0.05) / (lo + 0.05)
}

fn is_dark(bg: u32) -> bool {
    relative_luminance(bg) < 0.5
}

pub(crate) fn is_lighter(a: u32, b: u32) -> bool {
    relative_luminance(a) > relative_luminance(b)
}

const ACCENT_FLOOR: f32 = 3.0;

/// What the glyph painted on top of a match wash keeps for itself.
///
/// WCAG's non-text ratio, not its text one. A match is a state you read
/// *through* for a moment, not a surface you set body copy on — and holding the
/// glyph at 4.5:1 is not a choice this can make anyway: three of the builtins
/// only give their own text 6.6:1, which caps the wash at 1.46:1. That is a
/// hairline's worth of shift (a hairline is 1.5:1) spread over a whole cell,
/// and it is what made a match impossible to find by scanning.
const GLYPH_FLOOR: f32 = 3.0;

/// The two ends of what a wash is allowed to be, whatever the theme.
///
/// The floor buys the weakest palettes a match you can actually see, at the
/// price of a glyph that dips under `GLYPH_FLOOR` on one. The ceiling stops the
/// strongest from painting what reads as a solid block rather than a highlight.
const WASH_FLOOR: f32 = 1.9;
const WASH_CEILING: f32 = 3.2;

/// How far a search match's wash stands off the background it sits on, and how
/// much further the one you are looking at stands off the rest. Returned as
/// `(hit, current)` contrast targets.
///
/// Derived from the theme rather than fixed, because the budget being spent is
/// the theme's: a palette with 21:1 between its text and its background can
/// afford a wash you cannot miss, and one with 6.6:1 cannot. A single constant
/// has to be safe for the second, which leaves the first with nothing.
pub(crate) fn match_wash_targets(bg: u32, fg: u32) -> (f32, f32) {
    let current = (contrast(fg, bg) / GLYPH_FLOOR).clamp(WASH_FLOOR, WASH_CEILING);
    // The plain hits are the crowd the current one has to read out of, so they
    // get a little over half the distance it travels.
    (1.0 + (current - 1.0) * 0.55, current)
}

/// The opacity at which `tint` over `surface` first reaches `target` contrast,
/// as a blended colour.
///
/// A fixed alpha does not survive a theme swap: the terminal's selection tint
/// is `mix(bg, fg, 0.24)`, so painting it at 0.32 is a 7% shift away from the
/// background in *every* theme — invisible on a white one, and barely there on
/// a dark one. Solving for the ratio instead keeps the same wash weight
/// whatever the two colours happen to be.
pub(crate) fn wash(surface: u32, tint: u32, target: f32) -> u32 {
    if contrast(mix(surface, tint, 1.0), surface) < target {
        // The tint cannot get there on its own — a theme whose selection colour
        // is nearly its background. Fall back to the ink that reads on it.
        return legible_ink(surface, tint, target);
    }
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..16 {
        let mid = (lo + hi) / 2.0;
        if contrast(mix(surface, tint, mid), surface) >= target {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    mix(surface, tint, hi)
}

const TEXT_FLOOR: f32 = 4.5;

/// Hairlines are separators, not control outlines — the surfaces they divide
/// carry their own fills, so WCAG 1.4.11's 3:1 does not apply and painting them
/// that hard would read as a wireframe. This floor only rescues the palettes
/// where the flat blend disappears entirely, so a divider is worth the same
/// amount in every theme.
const BORDER_FLOOR: f32 = 1.5;

/// Keep an authored blend when it already clears `target` on the surface it is
/// painted on, and walk it back toward `toward` only when it does not.
fn at_least(ink: u32, toward: u32, surface: u32, target: f32) -> u32 {
    if contrast(ink, surface) >= target {
        return ink;
    }
    let lifted = bisect_contrast(ink, toward, surface, target);
    if contrast(lifted, surface) >= target {
        return lifted;
    }
    legible_ink(surface, lifted, target)
}

fn legible_ink(bg: u32, seed: u32, floor: f32) -> u32 {
    if contrast(seed, bg) >= floor {
        return seed;
    }
    let away = if contrast(0xffffff, bg) >= contrast(0x000000, bg) {
        0xffffff
    } else {
        0x000000
    };
    bisect_contrast(seed, away, bg, floor)
}

fn legible_accent(bg: u32, accent: u32) -> u32 {
    legible_ink(bg, accent, ACCENT_FLOOR)
}

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

pub struct ActiveBackground {
    pub fill: Fill,
    pub opacity: Option<f32>,
    pub image: Option<Image>,
}

impl Global for ActiveBackground {}

pub const DEFAULT_ID: &str = "light";

pub struct Themes(pub Vec<Theme>);

impl Global for Themes {}

/// Files in the themes folder that could not be read, and why. A malformed
/// theme used to log a warning and then simply not be in the list — the folder
/// is one the user opens and drops files into, so "it isn't there" needs a
/// reason attached to it.
#[derive(Default)]
pub struct RejectedThemes(pub Vec<(String, String)>);

impl Global for RejectedThemes {}

pub fn load_registry(cx: &mut App) {
    let (themes, rejected) = load_all();
    cx.set_global(Themes(themes));
    cx.set_global(RejectedThemes(rejected));
}

pub fn all(cx: &App) -> Vec<Theme> {
    cx.try_global::<Themes>()
        .map(|t| t.0.clone())
        .unwrap_or_else(builtins)
}

pub fn rejected(cx: &App) -> Vec<(String, String)> {
    cx.try_global::<RejectedThemes>()
        .map(|r| r.0.clone())
        .unwrap_or_default()
}

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
    pub fn editable(&self) -> bool {
        self.path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
    }
}

pub fn to_yaml(t: &Theme) -> String {
    fn hex(c: u32) -> String {
        format!("\"#{:06x}\"", c & 0xff_ffff)
    }
    fn rgb_hex((r, g, b): (u8, u8, u8)) -> String {
        format!("\"#{r:02x}{g:02x}{b:02x}\"")
    }
    let mut s = String::new();
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
    // The name lands in the YAML on disk and is matched back with
    // `trim_end_matches(" (custom)")`, so it stays English in every locale —
    // a translated suffix would survive a language switch and stack up.
    copy.name = format!("{} (custom)", t.name.trim_end_matches(" (custom)"));
    crate::core::config::write_atomic(
        &dir.join(format!("{stem}.yaml")),
        to_yaml(&copy).as_bytes(),
    )?;
    Ok(stem)
}

pub fn write_theme_file(t: &Theme) -> std::io::Result<()> {
    let path = t.path.clone().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "theme is not file-backed")
    })?;
    crate::core::config::write_atomic(&path, to_yaml(t).as_bytes())
}

fn load_all() -> (Vec<Theme>, Vec<(String, String)>) {
    let mut themes = builtins();
    let (user, rejected) = load_user_themes();
    themes.extend(user);
    dedupe_ids(&mut themes);
    (themes, rejected)
}

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

pub fn themes_dir() -> Option<PathBuf> {
    crate::core::config::config_path("themes")
}

fn load_user_themes() -> (Vec<Theme>, Vec<(String, String)>) {
    let Some(dir) = themes_dir() else {
        return (Vec::new(), Vec::new());
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (Vec::new(), Vec::new());
    };
    let mut out = Vec::new();
    let mut rejected = Vec::new();
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
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
            Err(e) => {
                log::warn!("skipping theme {}: {e}", path.display());
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string());
                rejected.push((name, e));
            }
        }
    }
    (out, rejected)
}

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
            // Theme names are data — they get written back to the YAML file,
            // so the fallback stays English rather than following the GUI.
            "Theme".into()
        } else {
            name
        },
    )
}

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
    let file: ThemeFile =
        serde_yaml::from_str(crate::core::config::strip_bom(&text)).map_err(|e| e.to_string())?;
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

fn parse_hex(s: &str) -> Result<u32, String> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return Err(format!("'{s}' is not a 6-digit hex color"));
    }
    u32::from_str_radix(hex, 16).map_err(|_| format!("'{s}' is not a hex color"))
}

fn parse_rgb(s: &str) -> Result<(u8, u8, u8), String> {
    let n = parse_hex(s)?;
    Ok(((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

pub fn builtins() -> Vec<Theme> {
    BUILTINS.iter().map(Theme::from_builtin).collect()
}

struct BuiltinSpec {
    id: &'static str,
    name: &'static str,
    background: u32,
    foreground: u32,
    accent: u32,
    caret: Option<u32>,
    ansi16: [(u8, u8, u8); 16],
}

static BUILTINS: [BuiltinSpec; 9] = [
    BuiltinSpec {
        id: "light",
        name: "Light",
        background: 0xffffff,
        foreground: 0x111111,
        accent: 0x00c2ff,
        caret: Some(0xf5a15c),
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
        id: "one_dark_pro",
        name: "One Dark Pro",
        background: 0x282c34,
        foreground: 0xabb2bf,
        accent: 0x528bff,
        caret: None,
        ansi16: [
            (0x3f, 0x44, 0x51),
            (0xe0, 0x6c, 0x75),
            (0x98, 0xc3, 0x79),
            (0xe5, 0xc0, 0x7b),
            (0x61, 0xaf, 0xef),
            (0xc6, 0x78, 0xdd),
            (0x56, 0xb6, 0xc2),
            (0xab, 0xb2, 0xbf),
            (0x5c, 0x63, 0x70),
            (0xff, 0x61, 0x6e),
            (0xa5, 0xe0, 0x75),
            (0xf0, 0xa4, 0x5d),
            (0x4d, 0xc4, 0xff),
            (0xde, 0x73, 0xff),
            (0x4c, 0xd1, 0xe0),
            (0xe6, 0xe6, 0xe6),
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

    #[test]
    fn dark_is_inferred_from_background() {
        let dark: Vec<_> = builtins()
            .into_iter()
            .filter(|t| t.dark)
            .map(|t| t.id)
            .collect();
        assert_eq!(
            dark,
            ["dark", "dracula", "harbor", "one_dark_pro", "rose_pine"]
        );
    }

    #[test]
    fn bright_slots_clear_the_text_floor_on_their_background() {
        // The renderer reads this palette for every pane, so every bright slot
        // must be legible on the theme background — SGR 90-97 is the family
        // PSReadLine paints parameters/operators/members with.
        for t in builtins() {
            let bg = t.background_color();
            let ap = t.active_palette(true);
            for (i, c) in ap.ansi16.iter().enumerate().skip(8) {
                let ink = (c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32;
                assert!(
                    contrast(ink, bg) >= TEXT_FLOOR - 0.01,
                    "{}/ansi16[{i}]: bright slot {ink:#08x} only {:.2}:1 on the background",
                    t.id,
                    contrast(ink, bg)
                );
            }
        }
    }

    #[test]
    fn legible_bright_slots_keep_their_authored_values() {
        // The lift is a rescue, not a restyle: a bright slot that already
        // clears the floor must come out of `active_palette` untouched.
        for t in builtins() {
            let bg = t.background_color();
            let ap = t.active_palette(true);
            for (i, (r, g, b)) in t.ansi16.iter().enumerate().skip(8) {
                let authored = (*r as u32) << 16 | (*g as u32) << 8 | *b as u32;
                let rendered = (ap.ansi16[i].r as u32) << 16
                    | (ap.ansi16[i].g as u32) << 8
                    | ap.ansi16[i].b as u32;
                if contrast(authored, bg) >= TEXT_FLOOR {
                    assert_eq!(
                        rendered, authored,
                        "{}/ansi16[{i}]: a legible bright slot was touched",
                        t.id
                    );
                }
            }
        }
    }

    #[test]
    fn palette_renders_as_authored_when_legibility_is_off() {
        // The Settings → Appearance switch turns the rescue off entirely: the
        // renderer then sees every slot byte-for-byte as authored.
        for t in builtins() {
            let ap = t.active_palette(false);
            for (i, (r, g, b)) in t.ansi16.iter().enumerate() {
                let rendered = (ap.ansi16[i].r as u32) << 16
                    | (ap.ansi16[i].g as u32) << 8
                    | ap.ansi16[i].b as u32;
                assert_eq!(
                    rendered,
                    (*r as u32) << 16 | (*g as u32) << 8 | *b as u32,
                    "{}/ansi16[{i}]: palette changed with legibility off",
                    t.id
                );
            }
        }
    }

    #[test]
    fn dark_bright_black_is_lifted_past_the_floor() {
        // The reported bug: PSReadLine paints parameters/operators/members
        // with DarkGray (SGR 90 → bright black), and the dark builtins author
        // that slot far below the floor, so the tokens vanished on the dark
        // background. The palette the renderer sees must come out lifted.
        for (id, authored) in [
            ("dracula", 0x555555),
            ("harbor", 0x292929),
            ("one_dark_pro", 0x5c6370),
            ("rose_pine", 0x6e6a86),
        ] {
            let t = builtins().into_iter().find(|t| t.id == id).unwrap();
            let c = t.active_palette(true).ansi16[8];
            let rendered = (c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32;
            assert_ne!(rendered, authored, "{id}: brightBlack was left as authored");
            assert!(
                contrast(rendered, t.background_color()) >= TEXT_FLOOR - 0.01,
                "{id}: brightBlack {rendered:#08x} still only {:.2}:1 on the background",
                contrast(rendered, t.background_color())
            );
        }
        // Light themes hit the same wall from the other side: a pale
        // bright-black vanishes on a near-white background.
        let latte = builtins()
            .into_iter()
            .find(|t| t.id == "catppuccin_latte")
            .unwrap();
        let c = latte.active_palette(true).ansi16[8];
        let rendered = (c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32;
        assert_ne!(
            rendered, 0xacb0be,
            "catppuccin_latte: brightBlack was left as authored"
        );
        assert!(
            contrast(rendered, latte.background_color()) >= TEXT_FLOOR - 0.01,
            "catppuccin_latte: brightBlack {rendered:#08x} still only {:.2}:1",
            contrast(rendered, latte.background_color())
        );
    }

    #[test]
    fn selection_surface_stays_on_the_background_side() {
        for t in builtins() {
            let ap = t.active_palette(true);
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
                let cursor_sel = contrast(sf.cursor, sf.selected);
                assert!(
                    sel_base >= 1.25,
                    "{}/{name}: selected is only {sel_base:.2}:1 from the surface",
                    t.id
                );
                assert!(
                    sel_hover >= 1.08,
                    "{}/{name}: selected is only {sel_hover:.2}:1 from hover",
                    t.id
                );
                assert!(
                    cursor_sel >= 1.2,
                    "{}/{name}: cursor is only {cursor_sel:.2}:1 from the resting selection",
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

    #[test]
    fn dracula_selection_matches_the_signed_off_greys() {
        let dracula = builtins().into_iter().find(|t| t.id == "dracula").unwrap();
        let bg = dracula.background_color();
        let s = dracula.surfaces();
        for (what, now, legacy) in [
            (
                "resting",
                s.sidebar.selected,
                mix(bg, dracula.foreground, 0.12),
            ),
            ("cursor", s.window.cursor, mix(bg, dracula.foreground, 0.17)),
        ] {
            assert!(
                contrast(now, legacy) < 1.05,
                "Dracula's {what} selection moved: {now:#08x} vs the tuned {legacy:#08x}"
            );
        }
    }

    /// CIE L*a*b* for a packed sRGB colour, D65.
    ///
    /// Contrast is a luminance ratio and says nothing about hue: two lanes can
    /// both clear 3:1 against the background and still be the same colour to
    /// look at. ΔE is the measure that catches that, and it needs Lab.
    fn lab(c: u32) -> (f32, f32, f32) {
        fn linear(v: u32) -> f32 {
            let s = v as f32 / 255.0;
            if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        }
        let (r, g, b) = (
            linear(c >> 16 & 0xff),
            linear(c >> 8 & 0xff),
            linear(c & 0xff),
        );
        // sRGB → XYZ, then normalised by the D65 white point.
        let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.95047;
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.08883;
        let f = |t: f32| {
            if t > 0.008856 {
                t.cbrt()
            } else {
                7.787 * t + 16.0 / 116.0
            }
        };
        let (fx, fy, fz) = (f(x), f(y), f(z));
        (116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz))
    }

    fn delta_e76(a: u32, b: u32) -> f32 {
        let (l1, a1, b1) = lab(a);
        let (l2, a2, b2) = lab(b);
        ((l1 - l2).powi(2) + (a1 - a2).powi(2) + (b1 - b2).powi(2)).sqrt()
    }

    #[test]
    fn lane_colours_clear_the_floor_on_every_surface() {
        for t in builtins() {
            let bg = t.background_color();
            let fg = legible_foreground(bg, t.foreground);
            let lanes = t.lanes();
            for (name, surface) in [
                ("background", bg),
                ("sidebar", mix(bg, fg, 0.03)),
                ("popover", mix(bg, fg, 0.05)),
            ] {
                for (slot, ink) in lanes.ink.iter().enumerate() {
                    let ratio = contrast(*ink, surface);
                    assert!(
                        ratio >= ACCENT_FLOOR,
                        "{}/{name}: lane {slot} is only {ratio:.2}:1",
                        t.id
                    );
                }
                let ratio = contrast(lanes.overflow, surface);
                assert!(
                    ratio >= ACCENT_FLOOR,
                    "{}/{name}: the overflow lane is only {ratio:.2}:1",
                    t.id
                );
            }
        }
    }

    #[test]
    fn adjacent_lanes_are_never_the_same_colour() {
        // A just-noticeable difference is around 2.3. The floor is set far
        // above it because these are 1.5px lines a few pixels apart, not
        // patches side by side, and the eye is much worse at hairlines.
        const FLOOR: f32 = 12.0;
        for t in builtins() {
            let lanes = t.lanes();
            for slot in 0..LANE_SLOTS - 1 {
                let d = delta_e76(lanes.ink[slot], lanes.ink[slot + 1]);
                assert!(
                    d >= FLOOR,
                    "{}: lanes {slot} and {} are ΔE {d:.1} apart",
                    t.id,
                    slot + 1
                );
            }
        }
    }

    #[test]
    fn lane_colours_are_deterministic() {
        for t in builtins() {
            assert_eq!(
                t.lanes().ink,
                t.lanes().ink,
                "{}: lane derivation is not a pure function",
                t.id
            );
        }
    }

    /// The seeds were chosen so that no two neighbours share a hue family and
    /// red never sits beside green. Both are properties of the *order*, so a
    /// reshuffle has to fail here rather than only looking slightly worse.
    #[test]
    fn the_lane_seed_order_keeps_red_and_green_apart() {
        let seeds = [4usize, 3, 5, 2, 6, 1];
        let red = seeds.iter().position(|s| *s == 1).expect("red is a seed");
        let green = seeds.iter().position(|s| *s == 2).expect("green is a seed");
        assert!(
            red.abs_diff(green) > 1,
            "red and green ended up adjacent at slots {red} and {green}"
        );
        assert_eq!(red, LANE_SLOTS - 1, "red should be the last slot reached");
    }

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

    #[test]
    fn selected_label_is_stepped_off_the_resting_one() {
        let (base, fill, fg) = (0x2f333b, 0x40454d, 0xabb2bf);
        let resting = dim(fg, base, state::TEXT_RESTING);

        let plain = ink_on(fill, fg, state::TEXT_RESTING);
        assert!(
            contrast(plain, resting) < state::TEXT_STEP,
            "the case this floor exists for no longer collapses — pick another"
        );

        let ink = stepped_ink(fill, base, fg, resting);
        assert!(
            contrast(ink, resting) >= state::TEXT_STEP - 0.01,
            "stepped to {ink:#08x}, still only {:.2}:1 off the resting label",
            contrast(ink, resting)
        );
        assert!(
            contrast(ink, fill) >= state::TEXT_RESTING - 0.01,
            "stepping cost the label its fill: {:.2}:1 on {fill:#08x}",
            contrast(ink, fill)
        );
    }

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
            assert!(
                contrast(checked, unchecked) >= 1.3,
                "{}: checked and unchecked tracks are {:.2}:1 apart",
                t.id,
                contrast(checked, unchecked)
            );
        }
    }

    #[test]
    fn semantic_colors_clear_their_floors() {
        for t in builtins() {
            let bg = t.background_color();
            let m = t.neutrals();
            let s = t.semantics();
            for (name, c) in [
                ("danger", s.danger),
                ("warning", s.warning),
                ("success", s.success),
                ("info", s.info),
                ("link", s.link),
            ] {
                // Every neutral fill the ink can land on, not just the window:
                // a failure notice is usually read inside a popover.
                for (surface, fill) in [
                    ("background", bg),
                    ("sidebar", m.sidebar),
                    ("popover", m.popover),
                ] {
                    assert!(
                        contrast(c.ink, fill) >= TEXT_FLOOR - 0.01,
                        "{}/{name}: ink {:#08x} only {:.2}:1 on the {surface}",
                        t.id,
                        c.ink,
                        contrast(c.ink, fill)
                    );
                    assert!(
                        contrast(c.fill, fill) >= ACCENT_FLOOR - 0.01,
                        "{}/{name}: fill {:#08x} only {:.2}:1 on the {surface}",
                        t.id,
                        c.fill,
                        contrast(c.fill, fill)
                    );
                }
                assert!(
                    contrast(c.on_fill, c.fill) >= TEXT_FLOOR - 0.01,
                    "{}/{name}: text on its own fill is only {:.2}:1",
                    t.id,
                    contrast(c.on_fill, c.fill)
                );
            }
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

    #[test]
    fn semantic_colors_keep_the_theme_hue() {
        let dracula = builtins().into_iter().find(|t| t.id == "dracula").unwrap();
        let ansi_red = {
            let (r, g, b) = dracula.ansi16[1];
            (r as u32) << 16 | (g as u32) << 8 | b as u32
        };
        assert_eq!(ansi_red, 0xff5555, "Dracula's ANSI red moved");
        // Conditioning may lift the seed to clear its floor on a popover, but
        // the result has to stay recognisably the palette's own red rather than
        // some house error colour.
        let ink = dracula.semantics().danger.ink;
        assert!(
            channel_distance(ink, ansi_red) <= 32,
            "danger ink {ink:#08x} drifted off Dracula's ANSI red {ansi_red:#08x}"
        );
        let (r, g, b) = (ink >> 16 & 0xff, ink >> 8 & 0xff, ink & 0xff);
        assert!(
            r > g && r > b,
            "danger ink {ink:#08x} is no longer red-dominant"
        );
    }

    #[test]
    fn sidebar_text_reads_on_the_fill_it_is_painted_on() {
        for t in builtins() {
            let m = t.neutrals();
            let ratio = contrast(m.sidebar_fg, m.sidebar);
            assert!(
                ratio >= TEXT_FLOOR - 0.01,
                "{}: sidebar text {:#08x} is only {ratio:.2}:1 on the sidebar fill {:#08x}",
                t.id,
                m.sidebar_fg,
                m.sidebar
            );
        }
    }

    #[test]
    fn carets_stay_visible_on_the_background() {
        for t in builtins() {
            let m = t.neutrals();
            let ratio = contrast(m.caret, m.background);
            assert!(
                ratio >= ACCENT_FLOOR - 0.01,
                "{}: caret {:#08x} is only {ratio:.2}:1 on the background",
                t.id,
                m.caret
            );
        }
        // The default theme is the one that used to fail: an orange caret on
        // pure white read at 2.07:1.
        let light = builtins().into_iter().find(|t| t.id == DEFAULT_ID).unwrap();
        assert_ne!(light.neutrals().caret, light.caret.unwrap());
    }

    #[test]
    fn hairlines_are_worth_the_same_in_every_theme() {
        for t in builtins() {
            let m = t.neutrals();
            for (name, surface) in [
                ("background", m.background),
                ("sidebar", m.sidebar),
                ("popover", m.popover),
            ] {
                let ratio = contrast(m.border, surface);
                assert!(
                    ratio >= BORDER_FLOOR - 0.01,
                    "{}: border {:#08x} is only {ratio:.2}:1 on the {name}",
                    t.id,
                    m.border
                );
                // A floor, not a target — a hairline that shouts is worse than
                // one that whispers.
                assert!(
                    ratio <= 2.2,
                    "{}: border {:#08x} is {ratio:.2}:1 on the {name} and reads as a frame",
                    t.id,
                    m.border
                );
            }
        }
    }

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
        let rose = builtins()
            .into_iter()
            .find(|t| t.id == "rose_pine")
            .unwrap();
        assert_eq!(rose.neutrals().accent, rose.accent);
    }

    #[test]
    fn a_match_wash_is_visible_on_a_white_theme_and_a_black_one() {
        // The old fixed alpha: 0.32 of a tint 24% off the background.
        let faint = |bg: u32, fg: u32| {
            let tint = mix(bg, fg, 0.24);
            contrast(mix(bg, tint, 0.32), bg)
        };
        for (bg, fg) in [(0xffffff, 0x111111), (0x111111, 0xffffff)] {
            assert!(
                faint(bg, fg) < 1.25,
                "the old wash was {:.2}:1 on {bg:06x} — that is what made it vanish",
                faint(bg, fg)
            );
            let tint = mix(bg, fg, 0.24);
            let (hit, current) = match_wash_targets(bg, fg);
            let w = wash(bg, tint, hit);
            assert!(
                contrast(w, bg) >= hit - 0.02,
                "{:.2}:1 on {bg:06x}",
                contrast(w, bg)
            );
            let cur = wash(bg, tint, current);
            assert!(
                contrast(cur, bg) > contrast(w, bg),
                "the current match has to stand out from the rest"
            );
        }
    }

    #[test]
    fn a_low_contrast_theme_gets_a_gentler_wash_than_a_high_contrast_one() {
        let (soft_hit, soft_cur) = match_wash_targets(0x282c34, 0xabb2bf);
        let (hard_hit, hard_cur) = match_wash_targets(0x000000, 0xffffff);
        assert!(
            soft_cur < hard_cur && soft_hit < hard_hit,
            "the wash spends the theme's own contrast budget, so it has to \
             scale with it: {soft_cur:.2} vs {hard_cur:.2}"
        );
        assert!(
            soft_hit >= 1.45,
            "even the gentle end has to beat the flat constant it replaced"
        );
        assert!(hard_cur <= WASH_CEILING, "a highlight, not a solid block");
    }

    #[test]
    fn a_match_wash_leaves_the_glyph_painted_on_top_of_it_readable() {
        // The wash is opaque and the text is drawn over it, so every step up in
        // visibility is a step down in the contrast the glyph has left. This is
        // the ceiling both targets are chosen against — on every builtin, and
        // in the accent the terminal actually washes with.
        for t in builtins() {
            let n = t.neutrals();
            let (hit_t, cur_t) = match_wash_targets(n.background, n.foreground);
            let hit = wash(n.background, n.accent, hit_t);
            let cur = wash(n.background, n.accent, cur_t);
            for (label, fill) in [("hit", hit), ("current", cur)] {
                // `wash` bisects to *at least* its target and 8-bit channels do
                // not divide evenly, so a fill can sit a hair past where the
                // target asked for it — and the glyph pays that hair.
                assert!(
                    contrast(n.foreground, fill) >= GLYPH_FLOOR - 0.05,
                    "{}: text on a {label} is {:.2}:1",
                    t.id,
                    contrast(n.foreground, fill)
                );
            }
            assert!(
                contrast(hit, n.background) > 1.5,
                "{}: a hit only shifts {:.2}:1 off the background — a hairline",
                t.id,
                contrast(hit, n.background)
            );
            assert!(
                contrast(cur, hit) >= 1.2,
                "{}: the current match only reads {:.2}:1 apart from the rest",
                t.id,
                contrast(cur, hit)
            );
        }
    }

    #[test]
    fn a_wash_whose_tint_cannot_reach_the_target_still_lands_on_something_legible() {
        // A theme whose selection colour is its background: no alpha gets there.
        let (hit, _) = match_wash_targets(0xffffff, 0x111111);
        let w = wash(0xffffff, 0xfefefe, hit);
        assert!(contrast(w, 0xffffff) >= hit - 0.02, "{w:06x}");
    }

    #[test]
    fn contrast_bisection_hits_its_target() {
        const SLACK: f32 = 0.05;
        let f = raise(0x000000, 0xffffff, 2.0);
        assert!((2.0..2.0 + SLACK).contains(&contrast(f, 0x000000)));
        let f = raise(0xffffff, 0x000000, 2.0);
        assert!((2.0..2.0 + SLACK).contains(&contrast(f, 0xffffff)));
        let d = dim(0xffffff, 0x000000, 4.5);
        assert!((contrast(d, 0x000000) - 4.5).abs() < SLACK);
        assert_eq!(raise(0x000000, 0x808080, 21.0), 0x808080);
    }

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

    #[test]
    fn legible_foreground_rescues_unreadable_text() {
        assert_eq!(legible_foreground(0xffffff, 0xeeeeee), 0x000000);
        assert_eq!(legible_foreground(0xffffff, 0x111111), 0x111111);
        assert_eq!(legible_foreground(0x000000, 0x222222), 0xffffff);
    }

    #[test]
    fn parse_hex_accepts_optional_hash_and_rejects_junk() {
        assert_eq!(parse_hex("#123456").unwrap(), 0x123456);
        assert_eq!(parse_hex("abcdef").unwrap(), 0xabcdef);
        assert!(parse_hex("#fff").is_err());
        assert!(parse_hex("nope!!").is_err());
    }

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

    #[test]
    fn theme_names_stay_english_under_a_translated_gui() {
        crate::ui::i18n::set_locale("zh-CN");
        // A stem of only separators leaves nothing to title-case.
        let (_, name) = id_and_name(std::path::Path::new("/x/_.yaml"));
        assert_eq!(name, "Theme");
        crate::ui::i18n::set_locale("en");
    }

    #[test]
    fn mix_blends_channels() {
        assert_eq!(mix(0x000000, 0xffffff, 0.0), 0x000000);
        assert_eq!(mix(0x000000, 0xffffff, 1.0), 0xffffff);
        assert_eq!(mix(0x000000, 0xffffff, 0.5), 0x808080);
    }
}

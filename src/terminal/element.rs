use std::cell::RefCell;
use std::collections::HashMap;

use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column as AlacColumn, Line as AlacLine, Point as AlacPoint};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor, Rgb};
use gpui::{
    App, BorderStyle, Bounds, ContentMask, Corners, CursorStyle, Element, ElementId, Font,
    FontStyle, FontWeight, GlobalElementId, Hitbox, HitboxBehavior, HitboxId, Hsla, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Rgba,
    SharedString, StrikethroughStyle, Style, TextAlign, TextRun, Window, fill, outline, point, px,
    relative, size,
};
use gpui_component::ActiveTheme as _;

use super::view::{TerminalView, should_show_context_menu};
use crate::core::config::Config;

const DIM_OPACITY: f32 = 0.66;

/// How much of the text's own colour a link's underline keeps before the
/// modifier is down. Low enough to read as a hint rather than as markup, high
/// enough to survive a light theme.
const HELD_BACK_LINK_ALPHA: f32 = 0.45;

#[derive(Clone, Copy, PartialEq, Default, Debug)]
enum UnderlineKind {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[derive(Clone)]
pub(super) struct RenderCell {
    c: char,
    marks: Option<Box<[char]>>,
    fg: Hsla,
    bg: Hsla,
    draw_bg: bool,
    bold: bool,
    italic: bool,
    strikeout: bool,
    underline: UnderlineKind,
    underline_color: Option<Hsla>,
    spacer: bool,
    selected: bool,
    match_hit: bool,
    match_current: bool,
    link_hover: bool,
}

impl Default for RenderCell {
    fn default() -> Self {
        Self {
            c: ' ',
            marks: None,
            fg: Hsla::default(),
            bg: Hsla::default(),
            draw_bg: false,
            bold: false,
            italic: false,
            strikeout: false,
            underline: UnderlineKind::None,
            underline_color: None,
            spacer: false,
            selected: false,
            match_hit: false,
            match_current: false,
            link_hover: false,
        }
    }
}

pub struct TerminalElement {
    view: gpui::Entity<TerminalView>,
}

impl TerminalElement {
    pub fn new(view: gpui::Entity<TerminalView>) -> Self {
        Self { view }
    }
}

pub struct TermLayout {
    cell_width: Pixels,
    line_height: Pixels,
    cols: usize,
    rows: usize,
    hitbox: Hitbox,
}

fn pack_rgb(c: Rgb) -> u32 {
    (c.r as u32) << 16 | (c.g as u32) << 8 | c.b as u32
}

fn unpack_rgb(n: u32) -> Rgb {
    Rgb {
        r: (n >> 16) as u8,
        g: (n >> 8) as u8,
        b: n as u8,
    }
}

fn to_hsla(c: Rgb) -> Hsla {
    Rgba {
        r: c.r as f32 / 255.,
        g: c.g as f32 / 255.,
        b: c.b as f32 / 255.,
        a: 1.,
    }
    .into()
}

fn resolve(
    color: AnsiColor,
    palette: &[Rgb; 256],
    default_fg: Rgb,
    default_bg: Rgb,
) -> (Rgb, bool) {
    match color {
        AnsiColor::Spec(rgb) => (rgb, false),
        AnsiColor::Indexed(i) => (palette[i as usize], false),
        AnsiColor::Named(named) => match named {
            NamedColor::Foreground => (default_fg, true),
            NamedColor::Background => (default_bg, true),
            other => {
                let idx = other as usize;
                if idx < 256 {
                    (palette[idx], false)
                } else {
                    (default_fg, true)
                }
            }
        },
    }
}

fn build_font(base: &Font, bold: bool, italic: bool) -> Font {
    let mut f = base.clone();
    f.weight = if bold {
        FontWeight::BOLD
    } else {
        FontWeight::NORMAL
    };
    f.style = if italic {
        FontStyle::Italic
    } else {
        FontStyle::Normal
    };
    if f.features.tag_value_list().is_empty() {
        f.features = ligatures_off();
    }
    f
}

/// The feature set that actually turns ligatures off in a terminal grid.
///
/// gpui's own `FontFeatures::disable_ligatures()` emits `calt: 0` and nothing
/// else. `calt` is only the contextual-alternate feature a programming face
/// drives `=>` and `!=` from; the `fi`/`ffl`/`ffi` ligatures live in `liga` and
/// `clig`, which that call never mentions — so they were left at the shaper's
/// default, and CoreText, DirectWrite and rustybuzz all default them on. A
/// terminal cannot have them: a ligature is one glyph where the grid budgeted a
/// cell per character, so the row drifts.
///
/// Naming all three is therefore the whole fix, and it is not platform
/// specific. Windows is the sharpest case only because gpui's DirectWrite
/// backend always attaches an `IDWriteTypography` to the run: an empty feature
/// list leaves that object empty, which suppresses DirectWrite's own defaults,
/// while any non-empty list has `liga: 1`/`clig: 1` appended to it
/// (`gpui_windows/src/direct_write.rs`, `apply_font_features`). Asking for
/// `calt: 0` there bought ligatures that asking for nothing would not have.
///
/// `rlig` is deliberately left alone: it carries the required ligatures a
/// script cannot be written without.
fn ligatures_off() -> gpui::FontFeatures {
    gpui::FontFeatures(std::sync::Arc::new(vec![
        ("calt".to_string(), 0),
        ("liga".to_string(), 0),
        ("clig".to_string(), 0),
    ]))
}

fn snapshot_cell(
    cell: &Cell,
    point: AlacPoint,
    palette: &[Rgb; 256],
    colors: &PaintColors,
    selection: Option<&SelectionRange>,
) -> RenderCell {
    let flags = cell.flags;
    if flags.contains(Flags::WIDE_CHAR_SPACER) || flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
        return RenderCell {
            spacer: true,
            ..RenderCell::default()
        };
    }

    let inverse = flags.contains(Flags::INVERSE);
    let (mut fgc, _) = resolve(cell.fg, palette, colors.fg_rgb, colors.bg_rgb);
    let (bgc, bg_default) = resolve(cell.bg, palette, colors.fg_rgb, colors.bg_rgb);
    let (fgc, bgc, draw_bg) = if inverse {
        (bgc, fgc, true)
    } else {
        if flags.contains(Flags::HIDDEN) {
            fgc = bgc;
        }
        (fgc, bgc, !bg_default)
    };

    let mut rc = RenderCell {
        c: cell.c,
        marks: cell
            .zerowidth()
            .filter(|marks| !marks.is_empty())
            .map(Box::from),
        fg: to_hsla(fgc),
        bg: to_hsla(bgc),
        draw_bg,
        bold: flags.contains(Flags::BOLD) || flags.contains(Flags::BOLD_ITALIC),
        italic: flags.contains(Flags::ITALIC) || flags.contains(Flags::BOLD_ITALIC),
        strikeout: flags.contains(Flags::STRIKEOUT),
        underline: if flags.contains(Flags::DOUBLE_UNDERLINE) {
            UnderlineKind::Double
        } else if flags.contains(Flags::UNDERCURL) {
            UnderlineKind::Curly
        } else if flags.contains(Flags::DOTTED_UNDERLINE) {
            UnderlineKind::Dotted
        } else if flags.contains(Flags::DASHED_UNDERLINE) {
            UnderlineKind::Dashed
        } else if flags.contains(Flags::UNDERLINE) {
            UnderlineKind::Single
        } else {
            UnderlineKind::None
        },
        underline_color: cell
            .underline_color()
            .map(|c| to_hsla(resolve(c, palette, colors.fg_rgb, colors.bg_rgb).0)),
        ..RenderCell::default()
    };

    if selection.is_some_and(|s| s.contains(point)) {
        rc.selected = true;
    }
    if flags.contains(Flags::DIM) {
        rc.fg = dim_fg(rc.fg, bgc, colors.legible_dim);
    }
    rc
}

/// SGR 2's colour: the ink at `DIM_OPACITY` over its cell, or — on a light
/// cell where that fade would be illegible — the opaque ink
/// `presets::legible_dim` solved for. `legible` is Settings → Appearance's
/// palette switch; off keeps the plain fade everywhere.
fn dim_fg(fg: Hsla, under: Rgb, legible: bool) -> Hsla {
    let mut faded = fg;
    faded.a *= DIM_OPACITY;
    if !legible {
        return faded;
    }
    // A faint run is one (ink, background) pair repeated across the row, and
    // the rescue is a contrast bisection — remember the last answer so a
    // screen of dim text costs one solve per colour change, not one per cell.
    thread_local! {
        static LAST: std::cell::Cell<Option<(u32, u32, Option<u32>)>> =
            const { std::cell::Cell::new(None) };
    }
    let (ink, bg) = (pack_rgb(super::palette::hsla_to_rgb(fg)), pack_rgb(under));
    let solved = LAST.with(|last| match last.get() {
        Some((i, b, s)) if i == ink && b == bg => s,
        _ => {
            let s = crate::ui::presets::legible_dim(ink, bg, DIM_OPACITY);
            last.set(Some((ink, bg, s)));
            s
        }
    });
    match solved {
        Some(c) => Hsla {
            a: fg.a,
            ..to_hsla(unpack_rgb(c))
        },
        None => faded,
    }
}

fn active_selection_bg(cx: &gpui::App) -> Rgb {
    match cx.try_global::<crate::terminal::palette::ActivePalette>() {
        Some(a) => a.sel_bg,
        None => Rgb {
            r: 0x4a,
            g: 0x43,
            b: 0x39,
        },
    }
}

/// Blends a finished cell's colours toward `under` for a dimmed pane. The
/// blend has to happen on the *cell*, not on the palette: truecolour cells
/// (the direct `38;2;…`/`48;2;…` SGR a prompt like starship emits for its
/// segments) carry their own `Spec` colour that a palette-level dim would
/// never see, leaving a truecolour prompt at full brightness while indexed
/// content around it dims.
fn dim_cell(mut rc: RenderCell, dim: f32, under: Rgba) -> RenderCell {
    rc.fg = blend_toward(rc.fg, dim, under);
    rc.bg = blend_toward(rc.bg, dim, under);
    if let Some(u) = rc.underline_color.as_mut() {
        *u = blend_toward(*u, dim, under);
    }
    rc
}

/// What a search match is washed with.
///
/// The theme's accent, not the terminal palette's selection colour. A hit and a
/// selection are different things, and washing both from the same tint left the
/// only difference between them a few percent of luminance — on a grid that is
/// already grey on grey, that reads as "slightly dirty background", not as "the
/// thing you searched for". The accent is the one colour the terminal surface
/// has nothing else in, and `legible_accent` has already floored it at 3:1
/// against the background, so the wash always has somewhere to travel.
fn match_tint(cx: &gpui::App) -> u32 {
    match cx.try_global::<crate::ui::presets::ActiveAccent>() {
        Some(a) => a.0,
        None => pack_rgb(active_selection_bg(cx)),
    }
}

pub(super) struct PaintColors {
    default_fg: Hsla,
    default_bg: Hsla,
    caret: Hsla,
    selection_bg: Hsla,
    match_bg: Hsla,
    current_match_bg: Hsla,
    fg_rgb: Rgb,
    bg_rgb: Rgb,
    /// Whether faint text on a light cell is held at the text floor (see
    /// `dim_fg`). Mirrors `theme_legible_palette`.
    legible_dim: bool,
}

/// The under-colour a dimmed pane blends its content toward: the window
/// background as it actually sits in the frame, i.e. the active preset fill
/// premultiplied by the window's own opacity. Dimming by blending every colour
/// toward this value (instead of alpha-multiplying each primitive) keeps the
/// composite of stacked layers — a powerline separator path over its segment
/// quad, text over a tint — exactly as dimmed as any single layer, so the
/// decorations of a prompt stay continuous when the pane goes inactive.
///
/// The fill comes from the active preset rather than the flat
/// `theme().background` token because the workspace root paints the preset
/// (see `theme::workspace_background`): with a gradient or wallpaper preset
/// the actual backdrop is not the theme token, and blending toward the wrong
/// colour would leave a tinted slab inside the dimmed pane. Solid fills match
/// exactly; gradients are approximated by their midpoint stop; a wallpaper
/// image rides over the fill at low opacity, so the fill stays the best
/// available target. Cells whose background is not painted (the default
/// terminal background) intentionally keep showing the un-dimmed window
/// background through them, so the backdrop material stays visible.
///
/// Two trade-offs follow from painting opaque, pre-blended colours instead of
/// the old element-opacity style. On a translucent window the desktop no
/// longer shows through a dimmed pane's painted cells — the old style left
/// them at alpha `dim` and let the backdrop contribute; the new style paints
/// them opaque, blended toward `fill × window_opacity`, which ignores the
/// backdrop's own contribution. And only the terminal element's cells are
/// dimmed: the search bar, completion menu and integration notice render
/// outside the grid and stay at full brightness, where the old style faded
/// the whole `TerminalView` — the same "the fading is worn by what it holds"
/// rule the drag grip follows.
fn dim_under(cx: &gpui::App) -> Rgba {
    // Mirror `workspace_background`: the active preset fill when a preset is
    // installed, the theme token otherwise. The alpha is the window opacity
    // in both arms, so the premultiply is uniform.
    match cx.try_global::<crate::ui::presets::ActiveBackground>() {
        Some(bg) => fill_under(&bg.fill, bg.opacity),
        None => theme_under(cx.theme().background),
    }
}

/// The premultiplied under-colour for an active preset's fill at the window's
/// own opacity. Solid fills are exact; gradients are approximated by their
/// midpoint stop (a wallpaper image rides over the fill at low opacity, so
/// the fill stays the best available target).
fn fill_under(fill: &crate::ui::presets::Fill, opacity: Option<f32>) -> Rgba {
    let packed = match fill {
        crate::ui::presets::Fill::Solid(c) => *c,
        crate::ui::presets::Fill::Vertical { top, bottom } => {
            crate::ui::presets::mix(*top, *bottom, 0.5)
        }
        crate::ui::presets::Fill::Horizontal { left, right } => {
            crate::ui::presets::mix(*left, *right, 0.5)
        }
    };
    premultiplied(packed, opacity.unwrap_or(1.))
}

/// The premultiplied under-colour for the theme background token, used when
/// no preset is installed — the `None` arm of `workspace_background`.
fn theme_under(bg: Hsla) -> Rgba {
    let bg = Rgba::from(bg);
    Rgba {
        r: bg.r * bg.a,
        g: bg.g * bg.a,
        b: bg.b * bg.a,
        a: 1.,
    }
}

/// The premultiplied under-colour for a packed fill colour at the window's
/// own opacity — exactly the colour a `Solid` workspace fill paints over the
/// OS backdrop, ignoring the backdrop's own contribution.
fn premultiplied(packed: u32, opacity: f32) -> Rgba {
    Rgba {
        r: ((packed >> 16) & 0xff) as f32 / 255. * opacity,
        g: ((packed >> 8) & 0xff) as f32 / 255. * opacity,
        b: (packed & 0xff) as f32 / 255. * opacity,
        a: 1.,
    }
}

/// Blends `c` toward `under` in RGB space, keeping `c`'s own alpha. `under`
/// is passed pre-converted to `Rgba` because it is a frame constant: every
/// cell blends fg, bg and an optional underline colour against it, and the
/// HSL↔RGB round trip would otherwise run once per colour per cell. Linear
/// in the composited result: painting `blend_toward(a)` over `blend_toward(b)`
/// equals `blend_toward(a over b)`, which is exactly what a uniform opacity
/// of `dim` over the window background would produce.
fn blend_toward(c: Hsla, dim: f32, under: Rgba) -> Hsla {
    let c = Rgba::from(c);
    Rgba {
        r: dim * c.r + (1. - dim) * under.r,
        g: dim * c.g + (1. - dim) * under.g,
        b: dim * c.b + (1. - dim) * under.b,
        a: c.a,
    }
    .into()
}

impl PaintColors {
    pub(super) fn resolve(theme: &gpui_component::Theme, cx: &gpui::App) -> Self {
        let default_fg = theme.foreground;
        let default_bg = theme.background;
        let caret = theme.caret;
        let selection_bg = {
            let mut c = default_fg;
            c.a = 0.24;
            c
        };
        // Opaque, and solved for a contrast against the background rather than
        // a fixed alpha: the selection tint is only 24% away from the
        // background to begin with, so a 0.32 alpha landed 7% off it in every
        // theme — on a white one the matches were all but invisible. How far
        // the wash may travel is the theme's to say (`match_wash_targets`),
        // since the glyph on top of it pays for every step.
        let fg_rgb = super::palette::hsla_to_rgb(default_fg);
        let bg_rgb = super::palette::hsla_to_rgb(default_bg);
        let bg_packed = pack_rgb(bg_rgb);
        let tint = match_tint(cx);
        let (hit_target, current_target) =
            crate::ui::presets::match_wash_targets(bg_packed, pack_rgb(fg_rgb));
        let match_bg = to_hsla(unpack_rgb(crate::ui::presets::wash(
            bg_packed, tint, hit_target,
        )));
        let current_match_bg = to_hsla(unpack_rgb(crate::ui::presets::wash(
            bg_packed,
            tint,
            current_target,
        )));
        Self {
            default_fg,
            default_bg,
            caret,
            selection_bg,
            match_bg,
            current_match_bg,
            fg_rgb,
            bg_rgb,
            legible_dim: cx
                .try_global::<Config>()
                .is_none_or(|c| c.theme_legible_palette),
        }
    }

    /// Blends every colour toward `under` for a dimmed pane. The alpha of each
    /// colour is left alone — the blend happens in RGB, so translucent tints
    /// (selection, matches) composite over the already-blended cell colours
    /// exactly as dimmed as opaque content does.
    ///
    /// `fg_rgb`/`bg_rgb` are deliberately left raw: they only feed `resolve`
    /// for the named foreground/background cells, and those cells get blended
    /// by `dim_cell` like every other cell — blending them here as well would
    /// dim the named colours twice.
    fn dimmed(&self, dim: f32, under: Rgba) -> Self {
        Self {
            default_fg: blend_toward(self.default_fg, dim, under),
            default_bg: blend_toward(self.default_bg, dim, under),
            caret: blend_toward(self.caret, dim, under),
            selection_bg: blend_toward(self.selection_bg, dim, under),
            match_bg: blend_toward(self.match_bg, dim, under),
            current_match_bg: blend_toward(self.current_match_bg, dim, under),
            fg_rgb: self.fg_rgb,
            bg_rgb: self.bg_rgb,
            legible_dim: self.legible_dim,
        }
    }
}

fn paint_backgrounds(window: &mut Window, geom: &CellGeom, buf: &[RenderCell]) {
    for row in 0..geom.rows {
        let mut col = 0;
        while col < geom.cols {
            let cell = &buf[row * geom.cols + col];
            if !cell.draw_bg {
                col += 1;
                continue;
            }
            let bg = cell.bg;
            let start = col;
            while col < geom.cols {
                let c = &buf[row * geom.cols + col];
                if c.spacer || (c.draw_bg && c.bg == bg) {
                    col += 1;
                } else {
                    break;
                }
            }
            window.paint_quad(fill(geom.cell_rect(row, start, col - start), bg));
        }
    }
}

fn paint_cell_runs(
    window: &mut Window,
    geom: &CellGeom,
    buf: &[RenderCell],
    color: Hsla,
    mut covered: impl FnMut(&RenderCell) -> bool,
) {
    for row in 0..geom.rows {
        let mut col = 0;
        while col < geom.cols {
            if !covered(&buf[row * geom.cols + col]) {
                col += 1;
                continue;
            }
            let start = col;
            while col < geom.cols {
                let cell = &buf[row * geom.cols + col];
                if covered(cell) || cell.spacer {
                    col += 1;
                } else {
                    break;
                }
            }
            window.paint_quad(fill(geom.cell_rect(row, start, col - start), color));
        }
    }
}

fn for_each_special_underline(
    bounds: Bounds<Pixels>,
    kind: UnderlineKind,
    scale: f32,
    mut draw: impl FnMut(Bounds<Pixels>),
) {
    let scale = scale.max(0.1);
    let snap = |v: f32| (v * scale).round() / scale;
    let device_px = 1. / scale;
    let x0 = snap(bounds.origin.x.as_f32());
    let x1 = snap((bounds.origin.x + bounds.size.width).as_f32());
    let y1 = snap((bounds.origin.y + bounds.size.height).as_f32());
    let line = |y: f32, start: f32, end: f32| {
        Bounds::new(
            point(px(start), px(y)),
            size(px((end - start).max(0.)), px(device_px)),
        )
    };

    match kind {
        UnderlineKind::Double => {
            draw(line(y1 - 4. * device_px, x0, x1));
            draw(line(y1 - 2. * device_px, x0, x1));
        }
        UnderlineKind::Dotted | UnderlineKind::Dashed => {
            // The ink is one device pixel thick, but its rhythm is measured in
            // logical pixels. Otherwise at 2x a dotted underline alternates a
            // single physical pixel on/off and reads as a grey solid line.
            let (on, off) = if kind == UnderlineKind::Dotted {
                (1., 1.)
            } else {
                (3., 2.)
            };
            let mut x = x0;
            while x < x1 {
                draw(line(y1 - 2. * device_px, x, (x + on).min(x1)));
                x += on + off;
            }
        }
        _ => {}
    }
}

fn paint_special_underlines(window: &mut Window, geom: &CellGeom, buf: &[RenderCell], scale: f32) {
    for row in 0..geom.rows {
        let row_base = row * geom.cols;
        let mut col = 0;
        while col < geom.cols {
            let cell = &buf[row_base + col];
            if !matches!(
                cell.underline,
                UnderlineKind::Double | UnderlineKind::Dotted | UnderlineKind::Dashed
            ) {
                col += 1;
                continue;
            }
            let kind = cell.underline;
            let color = cell.underline_color.unwrap_or(cell.fg);
            let start = col;
            col += 1;
            while col < geom.cols {
                let next = &buf[row_base + col];
                if next.spacer
                    || (next.underline == kind && next.underline_color.unwrap_or(next.fg) == color)
                {
                    col += 1;
                } else {
                    break;
                }
            }
            for_each_special_underline(
                geom.cell_rect(row, start, col - start),
                kind,
                scale,
                |rect| {
                    window.paint_quad(fill(rect, color));
                },
            );
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct GlyphStyle {
    fg: Hsla,
    bold: bool,
    italic: bool,
    strikeout: bool,
    underline: UnderlineKind,
    underline_color: Option<Hsla>,
    link_hover: bool,
}

impl GlyphStyle {
    fn of(cell: &RenderCell) -> Self {
        Self {
            fg: cell.fg,
            bold: cell.bold,
            italic: cell.italic,
            strikeout: cell.strikeout,
            underline: cell.underline,
            underline_color: cell.underline_color,
            link_hover: cell.link_hover,
        }
    }

    fn draws_on_blanks(&self) -> bool {
        self.underline != UnderlineKind::None || self.strikeout || self.link_hover
    }

    fn underline_style(&self) -> Option<gpui::UnderlineStyle> {
        (matches!(self.underline, UnderlineKind::Single | UnderlineKind::Curly)
            || (self.link_hover && self.underline == UnderlineKind::None))
            .then(|| {
                let wavy = self.underline == UnderlineKind::Curly;
                gpui::UnderlineStyle {
                    thickness: px(1.),
                    color: Some(self.underline_color.unwrap_or(self.fg)),
                    wavy,
                }
            })
    }

    fn strikethrough_style(&self) -> Option<StrikethroughStyle> {
        self.strikeout.then_some(StrikethroughStyle {
            thickness: px(1.),
            color: Some(self.fg),
        })
    }
}

fn is_blank(cell: &RenderCell) -> bool {
    (cell.c == '\0' || cell.c == ' ') && cell.marks.is_none()
}

#[derive(Debug, PartialEq)]
enum RowSeg {
    Run {
        start: usize,
        cells: usize,
        text: String,
    },
    Wide {
        start: usize,
        cells: usize,
        text: SharedString,
    },
    Solo {
        col: usize,
    },
    Cluster {
        col: usize,
        cells: usize,
        text: String,
        wide_base: bool,
    },
}

fn push_cell(text: &mut String, cell: &RenderCell) {
    text.push(cell.c);
    text.extend(cell.marks.iter().flat_map(|marks| marks.iter()));
}

fn is_sara_am(c: char) -> bool {
    matches!(c, '\u{0E33}' | '\u{0EB3}')
}

fn sara_am_at(row: &[RenderCell], col: usize) -> Option<&RenderCell> {
    row.get(col)
        .filter(|cell| !cell.spacer && is_sara_am(cell.c))
}

fn is_regional_indicator(c: char) -> bool {
    matches!(c, '\u{1F1E6}'..='\u{1F1FF}')
}

fn regional_indicator_at(row: &[RenderCell], col: usize) -> Option<&RenderCell> {
    row.get(col)
        .filter(|cell| !cell.spacer && is_regional_indicator(cell.c))
}

fn segment_row(row: &[RenderCell]) -> Vec<RowSeg> {
    let mut segs = Vec::new();
    let mut col = 0;
    while col < row.len() {
        let cell = &row[col];
        if cell.spacer {
            col += 1;
            continue;
        }
        if is_blank(cell) {
            let style = GlyphStyle::of(cell);
            if style.underline == UnderlineKind::None && !style.strikeout {
                col += 1;
                continue;
            }
            let start = col;
            let mut text = String::new();
            while col < row.len()
                && !row[col].spacer
                && is_blank(&row[col])
                && GlyphStyle::of(&row[col]) == style
            {
                text.push(' ');
                col += 1;
            }
            segs.push(RowSeg::Run {
                start,
                cells: col - start,
                text,
            });
            continue;
        }
        // Ahead of the marks branch: a stray mark on either half must not
        // split the pair, or the other half paints as a lettered box.
        if is_regional_indicator(cell.c)
            && let Some(next) = regional_indicator_at(row, col + 1)
        {
            let mut text = String::with_capacity(8);
            push_cell(&mut text, cell);
            push_cell(&mut text, next);
            segs.push(RowSeg::Cluster {
                col,
                cells: 2,
                text,
                wide_base: false,
            });
            col += 2;
            continue;
        }
        if let Some(marks) = &cell.marks {
            let wide_base = col + 1 < row.len() && row[col + 1].spacer;
            let mut cells = if wide_base { 2 } else { 1 };
            let mut text = String::with_capacity(1 + marks.len());
            push_cell(&mut text, cell);
            if !wide_base
                && !is_sara_am(cell.c)
                && let Some(am) = sara_am_at(row, col + 1)
            {
                push_cell(&mut text, am);
                cells = 2;
            }
            segs.push(RowSeg::Cluster {
                col,
                cells,
                text,
                wide_base,
            });
            col += cells;
            continue;
        }
        if !cell.c.is_ascii_graphic() {
            if col + 1 < row.len() && row[col + 1].spacer {
                segs.push(RowSeg::Wide {
                    start: col,
                    cells: 2,
                    text: char_string(cell.c),
                });
                col += 2;
            } else if !is_sara_am(cell.c)
                && let Some(am) = sara_am_at(row, col + 1)
            {
                let mut text = String::with_capacity(2);
                push_cell(&mut text, cell);
                push_cell(&mut text, am);
                segs.push(RowSeg::Cluster {
                    col,
                    cells: 2,
                    text,
                    wide_base: false,
                });
                col += 2;
            } else {
                segs.push(RowSeg::Solo { col });
                col += 1;
            }
            continue;
        }

        let style = GlyphStyle::of(cell);
        let start = col;
        let mut text = String::new();
        text.push(cell.c);
        let mut cells = 1;
        col += 1;
        let mut gap = 0;
        while col < row.len() {
            let c = &row[col];
            if is_blank(c) && !c.spacer {
                if style.draws_on_blanks() {
                    break;
                }
                gap += 1;
                col += 1;
                continue;
            }
            if c.spacer
                || c.marks.is_some()
                || !c.c.is_ascii_graphic()
                || GlyphStyle::of(c) != style
            {
                break;
            }
            for _ in 0..gap {
                text.push(' ');
            }
            cells += gap;
            gap = 0;
            text.push(c.c);
            cells += 1;
            col += 1;
        }
        segs.push(RowSeg::Run { start, cells, text });
    }
    segs
}

thread_local! {
                            static CHAR_STRINGS: RefCell<HashMap<char, SharedString>> = RefCell::new(HashMap::new());

                        /// Measured ink bounds and the font size they were measured at.
                        static INK_BOUNDS: RefCell<(Pixels, HashMap<(gpui::FontId, char), Option<Bounds<Pixels>>>)> =
                            RefCell::new((px(0.), HashMap::new()));
}

fn char_string(c: char) -> SharedString {
    CHAR_STRINGS.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() > 32_768 {
            m.clear();
        }
        m.entry(c)
            .or_insert_with(|| SharedString::from(c.to_string()))
            .clone()
    })
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum PowerlineShape {
    TriangleRight,
    TriangleLeft,
    HalfCircleRight,
    HalfCircleLeft,
    SlantLowerLeft,
    SlantLowerRight,
    SlantUpperLeft,
    SlantUpperRight,
}

impl PowerlineShape {
    fn of(c: char) -> Option<Self> {
        Some(match c {
            '\u{e0b0}' => Self::TriangleRight,
            '\u{e0b2}' => Self::TriangleLeft,
            '\u{e0b4}' => Self::HalfCircleRight,
            '\u{e0b6}' => Self::HalfCircleLeft,
            '\u{e0b8}' => Self::SlantLowerLeft,
            '\u{e0ba}' => Self::SlantLowerRight,
            '\u{e0bc}' => Self::SlantUpperLeft,
            '\u{e0be}' => Self::SlantUpperRight,
            _ => return None,
        })
    }

    fn closing_edge_x(self, bounds: Bounds<Pixels>) -> Pixels {
        // Keep this match exhaustive so adding a shape cannot silently assign
        // its solid closing edge to the wrong side of the cell.
        match self {
            Self::TriangleRight
            | Self::HalfCircleRight
            | Self::SlantLowerLeft
            | Self::SlantUpperLeft => bounds.left(),
            Self::TriangleLeft
            | Self::HalfCircleLeft
            | Self::SlantLowerRight
            | Self::SlantUpperRight => bounds.right(),
        }
    }
}

fn powerline_path(bounds: Bounds<Pixels>, shape: PowerlineShape) -> gpui::Path<Pixels> {
    let (x0, y0) = (bounds.origin.x, bounds.origin.y);
    let (x1, y1) = (x0 + bounds.size.width, y0 + bounds.size.height);
    let ymid = y0 + bounds.size.height / 2.;

    let tri = |a: Point<Pixels>, b: Point<Pixels>, c: Point<Pixels>| {
        let mut p = gpui::Path::new(a);
        p.line_to(b);
        p.line_to(c);
        p
    };
    let half_circle = |anchor_x: Pixels, dir: f32| {
        const SEGS: usize = 12;

        let (rx, ry) = (bounds.size.width.as_f32(), bounds.size.height.as_f32() / 2.);

        let at = |scale: f32, theta: f32| {
            let x = anchor_x.as_f32() + dir * rx * scale * theta.cos();
            let y = ymid.as_f32() + ry * scale * theta.sin();
            point(
                px(x.clamp(x0.as_f32(), x1.as_f32())),
                px(y.clamp(y0.as_f32(), y1.as_f32())),
            )
        };

        let step = std::f32::consts::PI / SEGS as f32;
        let mut p = gpui::Path::new(at(1., -std::f32::consts::FRAC_PI_2));
        for i in 0..SEGS {
            let t0 = -std::f32::consts::FRAC_PI_2 + step * i as f32;
            let t1 = t0 + step;
            let ctrl = at(1. / (step / 2.).cos(), (t0 + t1) / 2.);
            p.curve_to(at(1., t1), ctrl);
        }
        p
    };
    match shape {
        PowerlineShape::TriangleRight => tri(point(x0, y0), point(x1, ymid), point(x0, y1)),
        PowerlineShape::TriangleLeft => tri(point(x1, y0), point(x0, ymid), point(x1, y1)),
        PowerlineShape::SlantLowerLeft => tri(point(x0, y0), point(x1, y1), point(x0, y1)),
        PowerlineShape::SlantLowerRight => tri(point(x1, y0), point(x1, y1), point(x0, y1)),
        PowerlineShape::SlantUpperLeft => tri(point(x0, y0), point(x1, y0), point(x0, y1)),
        PowerlineShape::SlantUpperRight => tri(point(x0, y0), point(x1, y0), point(x1, y1)),
        PowerlineShape::HalfCircleRight => half_circle(x0, 1.),
        PowerlineShape::HalfCircleLeft => half_circle(x1, -1.),
    }
}

fn powerline_solid_edge(
    bounds: Bounds<Pixels>,
    shape: PowerlineShape,
    scale_factor: f32,
    fg_alpha: f32,
) -> Option<Bounds<Pixels>> {
    // A filled path anti-aliases a closing edge that falls between device
    // pixels. Cover exactly that partially occupied pixel with opaque
    // foreground color; an already aligned edge needs no extra primitive.
    //
    // Only opaque separators qualify. A translucent one (DIM) would composite
    // the cover quad and the path on top of each other, pushing that single
    // column past the glyph's own alpha and tinting the neighboring cell.
    if !fg_alpha.is_finite() || fg_alpha < 1. {
        return None;
    }
    let scale_factor = if scale_factor.is_finite() && scale_factor > 0. {
        scale_factor
    } else {
        1.
    };
    let physical_edge = shape.closing_edge_x(bounds).as_f32() * scale_factor;
    let physical_left = physical_edge.floor();
    let physical_right = physical_edge.ceil();
    if physical_left == physical_right {
        return None;
    }

    Some(Bounds::from_corners(
        point(px(physical_left / scale_factor), bounds.top()),
        point(px(physical_right / scale_factor), bounds.bottom()),
    ))
}

/// What colour a hovered link's underline takes.
///
/// Until the modifier is down the link is only being pointed out, so it is
/// drawn as the same underline held back. A cell carrying an underline of its
/// own keeps the colour it was given: dimming that would be editing the
/// application's output rather than annotating it.
fn link_underline_color(cell: &RenderCell, armed: bool) -> Option<Hsla> {
    if armed || cell.underline != UnderlineKind::None || cell.underline_color.is_some() {
        return cell.underline_color;
    }
    Some(cell.fg.opacity(HELD_BACK_LINK_ALPHA))
}

fn native_cell_residue(style: &GlyphStyle) -> Option<char> {
    // Special underlines are painted directly from the cell buffer, so a
    // shaped blank is only needed for decorations GPUI owns.
    (style.strikeout
        || matches!(
            style.underline,
            UnderlineKind::Single | UnderlineKind::Curly
        )
        || style.link_hover)
        .then_some(' ')
}

/// How much room a segment's glyph gets before it is made to shrink.
///
/// No monospace face is wide enough for an emoji at the sizes people read at.
/// Apple Color Emoji is a bitmap face whose strikes are quantised, so its ink
/// is widest relative to the em exactly where it hurts: 1.25em from 12 to 16px,
/// tapering to 1.0em by 24px. Two cells only cover 1.25em from 0.63em advance
/// up, and the common faces sit at 0.60em (Menlo, SF Mono) or 0.50em (Ubuntu
/// Mono). Every terminal therefore has to choose between shrinking the glyph
/// and letting it spill. This follows WezTerm, whose answer shows the glyph
/// whole most often: a quarter cell of slack always, and a whole extra cell
/// when the neighbouring cell is blank and has nothing to lose.
fn seg_budget(solo: bool, measured: bool, cells: usize, room: bool, cell_width: Pixels) -> Pixels {
    if solo {
        // Single-cell glyphs are allowed to lean into the next cell — that is
        // what keeps the pile of symbols that look fine today from shrinking —
        // but only while that cell is empty. A Nerd Font icon pulled from a
        // fallback face inks well past a narrow primary's cell (1.6 cells is
        // typical), and where the next cell has a glyph of its own the lean is
        // not a lean, it is an overlap: the neighbour is painted afterwards
        // and lands on top of the overshoot. Hand those their own cell and let
        // `fit_scale` bring them down into it, the way kitty and ghostty do.
        //
        // Only where the ink was measured, though. This number is the clip as
        // well as the threshold to shrink at, so narrowing it for a segment
        // nothing could measure would cut the glyph instead of scaling it —
        // see `ink_covers_segment`.
        if room || !measured {
            cell_width * 2.
        } else {
            cell_width
        }
    } else if room {
        cell_width * (cells as f32 + 1.)
    } else {
        cell_width * (cells as f32 + 0.25)
    }
}

/// How much to shrink a segment so its glyph stops inside its budget.
///
/// Never more than 1. A fallback icon is not held to this at all:
/// [`icon_fit`] sizes it, up or down.
fn fit_scale(ink: Pixels, budget: Pixels) -> f32 {
    if ink <= budget || ink <= px(0.) {
        1.
    } else {
        budget.as_f32() / ink.as_f32()
    }
}

/// Whether the platform's `typographic_bounds` is the glyph's ink.
///
/// CoreText and DirectWrite answer with the ink box. cosmic-text answers with
/// the advance box pinned to the baseline, which says nothing about where an
/// icon's ink sits; sizing and centring on that would misplace it, so the
/// Linux build keeps the shrink-only rule for icons too.
const INK_BOUNDS_ARE_INK: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// The most an icon is ever enlarged by.
///
/// The cap keeps a glyph that is small by design — a dot, a bullet — from
/// being blown up to a cell-sized blob.
const MAX_ICON_GROWTH: f32 = 2.;

/// Whether a character is an icon: a code point in one of Unicode's Private
/// Use Areas, where Nerd Fonts, Font Awesome, Codicons and Material Design
/// icons all live.
///
/// Nothing in these ranges is text, so sizing one to its cells cannot change
/// how prose reads — the reason the rule is keyed on the code point and not
/// on the face: a CJK or emoji fallback keeps its own metrics.
///
/// The Powerline separators (`U+E0B0`–`U+E0D7`) are not icons. They are
/// shapes meant to abut the cells around them, the solid ones are painted
/// natively, and the thin ones already reach the row's full height, so
/// fitting them to an em and centring them would only pull them away from
/// the segments they join.
fn is_icon(c: char) -> bool {
    matches!(
        c as u32,
        0xE000..=0xE0AF | 0xE0D8..=0xF8FF | 0xF_0000..=0xF_FFFD | 0x10_0000..=0x10_FFFD
    )
}

/// Whether the cell after an icon can lend it room, the way kitty lets a
/// Private Use Area glyph followed by a space take two cells.
///
/// Everything [`has_room_after`] lends, and one thing more: a blank that
/// paints the *same* background as the icon's own cell. That is the space
/// after every icon in a coloured prompt segment, and backgrounds are painted
/// before any glyph, so the icon lands on its own colour. Held back to one
/// cell there, a Nerd Font icon is shrunk to about two thirds of the text's
/// height, which is #866.
fn lends_to_icon(row: &[RenderCell], start: usize, cells: usize) -> bool {
    if has_room_after(row, start, cells) {
        return true;
    }
    let (Some(icon), Some(next)) = (row.get(start), row.get(start + cells)) else {
        return false;
    };
    is_blank(next)
        && !next.selected
        && icon.draw_bg
        && next.draw_bg
        && next.bg == icon.bg
        && next.match_hit == icon.match_hit
        && next.match_current == icon.match_current
        && !GlyphStyle::of(next).draws_on_blanks()
}

/// How to size an icon a fallback face supplied, and how many cells it spans.
///
/// A fallback face is drawn at the primary's font size on its own metrics, so
/// what an icon looks like beside the text depends on how the two faces'
/// cells compare. Symbols Nerd Font Mono inks a full em per icon, which is
/// 1.6 cells of a 0.6em primary; a face with a narrower cell than the
/// primary's inks less than one. Neither is the icon at the text's size.
///
/// So fit the ink, aspect ratio kept, to the cells it is given and one em of
/// height — the size the icon was drawn at in its own face, and never taller
/// than the text's line. That may shrink or grow it, growth capped at
/// [`MAX_ICON_GROWTH`] and ignored under 2%, which is not worth a reshape.
/// When `lend` says the next cell is free the icon may take it too, but only
/// if that lets it come out bigger: a narrow glyph that already fits one cell
/// keeps to it and stays beside what follows.
fn icon_fit(
    ink: Bounds<Pixels>,
    cell_width: Pixels,
    cells: usize,
    lend: bool,
    height: Pixels,
) -> (f32, usize) {
    let (w, h) = (ink.size.width.as_f32(), ink.size.height.as_f32());
    if !(w > 0. && h > 0.) {
        return (1., cells);
    }
    let fit = |cells: usize| {
        let scale = (cell_width.as_f32() * cells as f32 / w)
            .min(height.as_f32() / h)
            .min(MAX_ICON_GROWTH);
        if (1. ..=1.02).contains(&scale) {
            1.
        } else {
            scale
        }
    };
    let own = fit(cells);
    if lend && fit(cells + 1) > own {
        (fit(cells + 1), cells + 1)
    } else {
        (own, cells)
    }
}

/// Where to paint a fitted icon, relative to its cell's top-left corner, so
/// its ink sits centred in `room` — vertically always, and horizontally only
/// when `centre_x`. An icon spilling into a borrowed cell stays where its
/// face put it instead, as it would unfitted, so the gap before the text
/// after it is the one its face drew.
///
/// `ink` is the glyph's bounds at its native size (y up from the baseline);
/// `ascent` and `descent` are the line's as shaped at the fitted size, which
/// is what gpui centres the baseline in the row with.
fn icon_offset(
    ink: Bounds<Pixels>,
    scale: f32,
    room: gpui::Size<Pixels>,
    centre_x: bool,
    ascent: Pixels,
    descent: Pixels,
) -> Point<Pixels> {
    let baseline = (room.height - ascent - descent) / 2. + ascent;
    let ink_top = baseline - (ink.origin.y + ink.size.height) * scale;
    let x = if centre_x {
        (room.width - ink.size.width * scale) / 2. - ink.origin.x * scale
    } else {
        // Nudged only as far as it takes to keep the ink inside the room.
        px(0.)
            .min(room.width - (ink.origin.x + ink.size.width) * scale)
            .max(-ink.origin.x * scale)
    };
    point(x, (room.height - ink.size.height * scale) / 2. - ink_top)
}

/// The ink bounds of a segment's first glyph, relative to its pen position,
/// y up from the baseline.
///
/// Advance is the wrong yardstick: Apple Color Emoji advances 1.31em but only
/// inks 1.25em, so going by advance shrinks glyphs that would have fit. Ask
/// for the glyph's own bounds instead, cached per (face, char) because the
/// lookup is a font query a screenful of CJK would otherwise repeat on every
/// cell of every frame.
fn ink_bounds(
    cx: &App,
    shaped: &gpui::ShapedLine,
    text: &str,
    font_size: Pixels,
) -> Option<Bounds<Pixels>> {
    let font_id = shaped.runs.first()?.font_id;
    let c = text.chars().next()?;
    INK_BOUNDS.with(|cache| {
        let hit = {
            let mut cache = cache.borrow_mut();
            if cache.0 != font_size {
                cache.0 = font_size;
                cache.1.clear();
            }
            if cache.1.len() > 32_768 {
                cache.1.clear();
            }
            cache.1.get(&(font_id, c)).copied()
        };
        if let Some(extent) = hit {
            return extent;
        }
        let bounds = cx
            .text_system()
            .typographic_bounds(font_id, font_size, c)
            .ok();
        cache.borrow_mut().1.insert((font_id, c), bounds);
        bounds
    })
}

/// Where a shaped run stops standing over the cells it came from.
///
/// `force_width` puts the *n*th glyph at *n × cell_width*, which is the whole
/// grid only while the shaper hands back one glyph per character. A ligature
/// substitution collapses several characters into one glyph, and from there on
/// every glyph is pulled left by the cells the substitution swallowed: the
/// text after it overlaps the ligature's tail, and the caret — drawn at the
/// grid column, not at the ink — stands past the end of the run with a gap.
///
/// Takes the glyphs' byte indices, which in a run are their columns, and
/// answers with the first column that has drifted. Only a glyph that arrives
/// later than its ordinal counts: a face that *adds* glyphs cannot be helped
/// by cutting the run, and treating it as drift would cut at column zero
/// forever.
fn run_drift(indices: impl IntoIterator<Item = usize>) -> Option<usize> {
    indices
        .into_iter()
        .enumerate()
        .find_map(|(ordinal, index)| (index > ordinal).then_some(index))
}

/// One thing [`paint_run`] asks of the caller, in bytes into the run's text.
///
/// A run is ASCII, one byte per cell, so `at` is also a column offset and
/// `len` is also a width in cells.
#[derive(Debug, PartialEq)]
enum RunStep {
    /// Shape `text[at..at + len]` and answer [`run_drift`] for it.
    Drift { at: usize, len: usize },
    /// Shape and paint `text[at..at + len]` starting at column `col`.
    Paint { col: usize, at: usize, len: usize },
}

/// Walk a run in the pieces that stand over their own cells.
///
/// A run that does not drift is one `Drift` and one `Paint` of the whole
/// thing, which is every run in a monospace face. Where it drifts, the part
/// in front of the drift is painted as a piece of its own — shaped on its
/// own, which is the point: clipping the tail away would not help, because
/// the tail drifted *left*, into the columns this piece is keeping. The
/// remainder then starts over in the column the grid puts it in.
///
/// The loop is separated from the shaping so it can be tested at all: gpui's
/// test text system hands back one glyph per character, so nothing shaped
/// through it ever drifts and the interesting half would never run.
fn paint_run(start: usize, len: usize, mut step: impl FnMut(RunStep) -> Option<usize>) {
    let mut col = start;
    let mut at = 0;
    loop {
        match step(RunStep::Drift { at, len: len - at }) {
            None => {
                step(RunStep::Paint {
                    col,
                    at,
                    len: len - at,
                });
                return;
            }
            Some(drift) => {
                step(RunStep::Paint {
                    col,
                    at,
                    len: drift,
                });
                col += drift;
                at += drift;
            }
        }
    }
}

/// Shape one piece of a segment's text.
///
/// The whole of it keeps the string it arrived in; only a piece cut out of a
/// drifted run has to be copied, and that happens where a ligature forced the
/// cut. Repeat calls for the same piece within a frame are answered from
/// gpui's line-layout cache rather than shaped again.
fn shape_piece(
    window: &mut Window,
    run_buf: &mut [TextRun; 1],
    text: &SharedString,
    at: usize,
    len: usize,
    font_size: Pixels,
    force_width: Option<Pixels>,
) -> gpui::ShapedLine {
    let piece = if at == 0 && len == text.len() {
        text.clone()
    } else {
        SharedString::from(text[at..at + len].to_string())
    };
    run_buf[0].len = piece.len();
    window
        .text_system()
        .shape_line(piece, font_size, run_buf, force_width)
}

/// Whether [`ink_bounds`]'s answer speaks for the whole segment.
///
/// It measures the segment's first character in the run's first face. That is
/// all of a one-character segment and only part of anything longer: a
/// cluster's combining marks can ink well to the right of the base they hang
/// off — Devanagari `\u{915}` + `\u{93E}` — and are not in the number. `None`
/// is a face that answered no bounds at all, which is no measurement either.
///
/// Neither can be scaled to fit, so neither may be held to one cell: the
/// budget doubles as the clip, and a segment that cannot shrink into a
/// narrowed one is simply cut off at it.
fn ink_covers_segment(ink: Option<Pixels>, text: &str) -> bool {
    ink.is_some() && text.chars().nth(1).is_none()
}

/// Whether the cell after a segment is free for its glyph to lean into.
///
/// A blank still owns its cell if it paints anything there: a background, a
/// selection, or a rule of its own. An underlined or hovered blank becomes a
/// `Run` of its own and is painted after the segment beside it, so lending it
/// out would drag a stroke straight across the borrowed glyph.
fn has_room_after(row: &[RenderCell], start: usize, cells: usize) -> bool {
    match row.get(start + cells) {
        None => true,
        Some(next) => {
            is_blank(next)
                && !next.draw_bg
                && !next.selected
                && !GlyphStyle::of(next).draws_on_blanks()
        }
    }
}

fn paint_glyphs(
    window: &mut Window,
    cx: &mut App,
    geom: &CellGeom,
    buf: &[RenderCell],
    font_size: Pixels,
    base_font: &Font,
    bold_font: Option<&Font>,
    italic_font: Option<&Font>,
) {
    let faces = [
        build_font(base_font, false, false),
        build_font(bold_font.unwrap_or(base_font), true, false),
        build_font(italic_font.unwrap_or(base_font), false, true),
        build_font(bold_font.unwrap_or(base_font), true, true),
    ];

    // Resolved only once an icon asks, so a pane without one pays nothing.
    let mut primary_ids: [Option<gpui::FontId>; 4] = [None; 4];

    let run_buf = &mut [TextRun {
        len: 0,
        font: base_font.clone(),
        color: Hsla::default(),
        background_color: None,
        underline: None,
        strikethrough: None,
    }];

    for row in 0..geom.rows {
        let row_base = row * geom.cols;
        let y = geom.origin.y + geom.line_height * (row as f32);

        let row_cells = &buf[row_base..row_base + geom.cols];

        for seg in segment_row(row_cells) {
            // A `Run` is one glyph per cell in the main font, which by
            // definition already fits; the rest can carry a glyph from a
            // fallback face that is wider than the cells it was handed.
            //
            // A run is also the only segment `segment_row` builds out of more
            // than one cell of plain text, and it only batches ASCII: one
            // byte, one character, one column. That is what lets `run_drift`
            // read a glyph's byte index as the column it came from.
            let run = matches!(seg, RowSeg::Run { .. });
            let (start, cells, text, force_width, solo) = match seg {
                RowSeg::Run { start, cells, text } => (
                    start,
                    cells,
                    SharedString::from(text),
                    Some(geom.cell_width),
                    false,
                ),
                RowSeg::Wide { start, cells, text } => {
                    (start, cells, text, Some(geom.cell_width * 2.), false)
                }
                RowSeg::Solo { col } => {
                    let cell = &buf[row_base + col];
                    let cell_bounds = Bounds::new(
                        point(geom.origin.x + geom.cell_width * (col as f32), y),
                        size(geom.cell_width, geom.line_height),
                    );
                    let native = if let Some(shape) = PowerlineShape::of(cell.c) {
                        let fg = GlyphStyle::of(cell).fg;
                        if let Some(edge) =
                            powerline_solid_edge(cell_bounds, shape, window.scale_factor(), fg.a)
                        {
                            window.paint_quad(fill(edge, fg));
                        }
                        let path = powerline_path(cell_bounds, shape);
                        window.paint_path(path, fg);
                        true
                    } else if let Some(ink) =
                        super::boxdraw::glyph(cell.c, cell_bounds, window.scale_factor())
                    {
                        let fg = GlyphStyle::of(cell).fg;
                        for piece in ink {
                            match piece {
                                super::boxdraw::Ink::Rect(r) => window.paint_quad(fill(r, fg)),
                                super::boxdraw::Ink::Shade(r, alpha) => {
                                    let mut c = fg;
                                    c.a *= alpha;
                                    window.paint_quad(fill(r, c));
                                }
                                super::boxdraw::Ink::Path(p) => window.paint_path(p, fg),
                            }
                        }
                        true
                    } else {
                        false
                    };
                    if !native {
                        (col, 1, char_string(cell.c), None, true)
                    } else {
                        match native_cell_residue(&GlyphStyle::of(cell)) {
                            None => continue,
                            Some(c) => (col, 1, char_string(c), None, false),
                        }
                    }
                }
                RowSeg::Cluster {
                    col,
                    cells,
                    text,
                    wide_base,
                } => (
                    col,
                    cells,
                    SharedString::from(text),
                    (cells == 2).then(|| geom.cell_width * if wide_base { 2. } else { 1. }),
                    cells == 1,
                ),
            };

            let style = GlyphStyle::of(&buf[row_base + start]);
            let face_ix = (style.bold as usize) | ((style.italic as usize) << 1);
            run_buf[0] = TextRun {
                len: text.len(),
                font: faces[face_ix].clone(),
                color: style.fg,
                background_color: None,
                underline: style.underline_style(),
                strikethrough: style.strikethrough_style(),
            };

            let x = geom.origin.x + geom.cell_width * (start as f32);

            // A run is walked in the pieces that stand over their own cells;
            // every other segment is one shaping and one paint.
            if run {
                paint_run(start, text.len(), |step| match step {
                    RunStep::Drift { at, len } => {
                        let shaped =
                            shape_piece(window, run_buf, &text, at, len, font_size, force_width);
                        run_drift(
                            shaped
                                .runs
                                .iter()
                                .flat_map(|r| r.glyphs.iter().map(|g| g.index)),
                        )
                    }
                    RunStep::Paint { col, at, len } => {
                        let shaped =
                            shape_piece(window, run_buf, &text, at, len, font_size, force_width);
                        let x = geom.origin.x + geom.cell_width * (col as f32);
                        let clip = Bounds::new(
                            point(x, y),
                            size(geom.cell_width * len as f32, geom.line_height),
                        );
                        window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                            _ = shaped.paint(
                                point(x, y),
                                geom.line_height,
                                TextAlign::Left,
                                None,
                                window,
                                cx,
                            );
                        });
                        None
                    }
                });
                continue;
            }

            let mut shaped =
                window
                    .text_system()
                    .shape_line(text.clone(), font_size, run_buf, force_width);
            // Measured before the budget is set, because how far a solo glyph
            // may reach turns on whether its ink is known at all.
            let bounds = ink_bounds(cx, &shaped, &text, font_size);
            let ink = bounds.map(|b| b.origin.x + b.size.width);
            let measured = ink_covers_segment(ink, &text);
            let budget = seg_budget(
                solo,
                measured,
                cells,
                has_room_after(row_cells, start, cells),
                geom.cell_width,
            );
            let mut origin = point(x, y);
            let mut clip_width = budget;
            if let (Some(ink), Some(bounds)) = (ink, bounds) {
                // Only a lone icon that a fallback face supplied is fitted:
                // the primary's own icons were drawn for its cell already.
                let icon = INK_BOUNDS_ARE_INK
                    && measured
                    && text.chars().next().is_some_and(is_icon)
                    && shaped.runs.first().is_some_and(|r| {
                        r.font_id
                            != *primary_ids[face_ix].get_or_insert_with(|| {
                                window.text_system().resolve_font(&faces[face_ix])
                            })
                    });
                let (scale, room) = if icon {
                    let lend = lends_to_icon(row_cells, start, cells);
                    let height = font_size.min(geom.line_height);
                    let (scale, spans) = icon_fit(bounds, geom.cell_width, cells, lend, height);
                    let room = size(geom.cell_width * spans as f32, geom.line_height);
                    (scale, Some((room, spans == cells)))
                } else {
                    (fit_scale(ink, budget), None)
                };
                if scale != 1. {
                    shaped = window.text_system().shape_line(
                        text.clone(),
                        font_size * scale,
                        run_buf,
                        force_width,
                    );
                }
                if let Some((room, centre_x)) = room {
                    origin +=
                        icon_offset(bounds, scale, room, centre_x, shaped.ascent, shaped.descent);
                    clip_width = room.width;
                }
            }

            let clip = Bounds::new(point(x, y), size(clip_width, geom.line_height));
            window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                _ = shaped.paint(origin, geom.line_height, TextAlign::Left, None, window, cx);
            });
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct GridCursor {
    row: usize,
    col: usize,
    // Where the IME candidate window should anchor: the fake caret drawn by
    // cursor-hiding TUIs when one is identifiable, else `col`.
    ime_col: usize,
    hidden: bool,
    style: crate::core::config::CursorStyle,
}

fn paint_cursor(
    window: &mut Window,
    geom: &CellGeom,
    cursor: Option<(usize, usize, crate::core::config::CursorStyle)>,
    focused: bool,
    cursor_visible: bool,
    caret: Hsla,
) {
    use crate::core::config::CursorStyle;
    let Some((row, col, style)) = cursor else {
        return;
    };
    let rect = geom.cell_rect(row, col, 1);
    if !focused {
        window.paint_quad(outline(rect, caret, BorderStyle::Solid));
        return;
    }
    if !cursor_visible {
        return;
    }
    match style {
        // A focused block is drawn as reverse video by `invert_cursor_cell`
        // before the glyphs go down, so the caret is opaque and the character
        // under it stays readable. Nothing left to paint here.
        CursorStyle::Block => {}
        CursorStyle::Bar => {
            let w = (geom.cell_width * 0.15).max(px(1.)).min(px(3.));
            let bar = Bounds::new(rect.origin, size(w, rect.size.height));
            window.paint_quad(fill(bar, caret));
        }
        CursorStyle::Underline => {
            let h = (geom.line_height * 0.12).max(px(1.)).min(px(3.));
            let y = rect.origin.y + rect.size.height - h;
            let line = Bounds::new(point(rect.origin.x, y), size(rect.size.width, h));
            window.paint_quad(fill(line, caret));
        }
    }
}

/// Turn the cell under a focused block cursor into reverse video: opaque caret
/// fill, glyph redrawn on top of it. Every other terminal paints a block this
/// way, and a translucent tint would give back most of the contrast the theme
/// conditioned the caret to carry.
fn invert_cursor_cell(
    buf: &mut [RenderCell],
    cols: usize,
    row: usize,
    col: usize,
    colors: &PaintColors,
) {
    let ink = crate::ui::presets::caret_ink(colors.caret, colors.default_bg, colors.default_fg);
    // A TUI can park the cursor on the second half of a wide character. The
    // glyph lives on the lead cell, so inverting the spacer would fill half a
    // character with the caret and leave the glyph in its own colour across
    // both halves. Step back to the cell that owns the glyph.
    let col = match buf.get(row * cols + col) {
        Some(c) if c.spacer && col > 0 => col - 1,
        _ => col,
    };
    let Some(cell) = buf.get_mut(row * cols + col) else {
        return;
    };
    cell.bg = colors.caret;
    cell.draw_bg = true;
    cell.fg = ink;
    if let Some(under) = cell.underline_color.as_mut() {
        *under = ink;
    }
    // A wide character's trailing spacer is absorbed into the lead cell's
    // background run by `paint_backgrounds`, so the fill already covers both
    // columns and only the lead cell carries the glyph.
}

fn cursor_style_from_shape(shape: CursorShape) -> crate::core::config::CursorStyle {
    match shape {
        CursorShape::Beam => crate::core::config::CursorStyle::Bar,
        CursorShape::Underline => crate::core::config::CursorStyle::Underline,
        _ => crate::core::config::CursorStyle::Block,
    }
}

fn paint_marked(
    window: &mut Window,
    cx: &mut App,
    geom: &CellGeom,
    cursor: Option<(usize, usize)>,
    marked: &str,
    font_size: Pixels,
    base_font: &Font,
    default_fg: Hsla,
    default_bg: Hsla,
) {
    if marked.is_empty() {
        return;
    }
    let Some((row, col)) = cursor else {
        return;
    };
    let x = geom.origin.x + geom.cell_width * (col as f32);
    let y = geom.origin.y + geom.line_height * (row as f32);
    let run = TextRun {
        len: marked.len(),
        font: base_font.clone(),
        color: default_fg,
        background_color: None,
        underline: Some(gpui::UnderlineStyle {
            thickness: px(1.),
            color: Some(default_fg),
            wavy: false,
        }),
        strikethrough: None,
    };
    let shaped = window.text_system().shape_line(
        SharedString::from(marked.to_owned()),
        font_size,
        &[run],
        None,
    );
    let bg_rect = Bounds::new(point(x, y), size(shaped.width, geom.line_height));
    window.paint_quad(fill(bg_rect, default_bg));
    _ = shaped.paint(
        point(x, y),
        geom.line_height,
        TextAlign::Left,
        None,
        window,
        cx,
    );
}

#[derive(Clone)]
pub(super) struct GridSnapshot {
    cursor: Option<GridCursor>,
    sliver: Option<Vec<RenderCell>>,
    any_selected: bool,
    any_match: bool,
    any_current: bool,
    /// Scrollback state at snapshot time, so the paint pass can map a kitty
    /// image's absolute anchor row back to a screen row: screen_row =
    /// anchor_row - history_size + display_offset.
    display_offset: i32,
    history_size: usize,
}

impl GridSnapshot {
    /// The terminal caret this frame paints, if any. None while the program
    /// has the cursor hidden: alacritty reports a DECTCEM-reset cursor
    /// (`?25l`) as `CursorShape::Hidden`, and a TUI that hides it is usually
    /// drawing a caret of its own that ours must not cover (#844).
    pub(super) fn painted_cursor(
        &self,
    ) -> Option<(usize, usize, crate::core::config::CursorStyle)> {
        self.cursor
            .filter(|c| !c.hidden)
            .map(|c| (c.row, c.col, c.style))
    }
}

impl TerminalElement {
    pub(super) fn build_grid(
        &self,
        colors: &PaintColors,
        buf: &mut Vec<RenderCell>,
        rows: usize,
        cols: usize,
        want_sliver: bool,
        cx: &App,
        dim: f32,
        under: Rgba,
        must_block: bool,
    ) -> Option<GridSnapshot> {
        let mut cursor: Option<GridCursor> = None;
        let mut sliver: Option<Vec<RenderCell>> = None;
        let mut any_selected = false;
        let display_offset;
        let history_size;
        {
            let mut palette = self.view.read(cx).terminal.palette;
            if let Some(active) = cx.try_global::<crate::terminal::palette::ActivePalette>() {
                palette[..16].copy_from_slice(&active.ansi16);
            }
            let term = self.view.read(cx).terminal.term.clone();
            // Rendering does not queue for the grid lock. Holding it is the
            // pane's own reader, part-way through feeding a batch of output
            // into the emulator — and one UI thread draws every pane in every
            // window, so waiting here wires one pane's write speed to the frame
            // rate of the whole window. That is the same illness as #709, which
            // was this thread parked in `write(2)` for a stalled link; this is
            // the read side of it. A frame that cannot have the lock paints the
            // one before it, and nobody can see a frame of lag.
            //
            // `try_lock_unfair` rather than a lease: a painter that queued
            // would make the reader wait for a frame it is not going to get
            // anyway. Skipping the queue is safe precisely because it never
            // waits.
            let term = match term.try_lock_unfair() {
                Some(term) => term,
                // The two frames that have to have it: the first one, with no
                // previous grid to fall back on, and the one after a resize,
                // where the previous grid is the wrong shape. Both are rare and
                // neither is in the steady state.
                None if must_block => term.lock(),
                None => return None,
            };
            // After the lock, not before: an early return must leave the
            // previous frame's cells intact for the caller to paint again.
            buf.clear();
            buf.resize(rows * cols, RenderCell::default());
            let content = term.renderable_content();
            display_offset = content.display_offset as i32;
            history_size = term.grid().history_size();
            let selection = content.selection;

            let cur = content.cursor;
            let cursor_row = cur.point.line.0 + display_offset;
            let cursor_hidden = matches!(cur.shape, CursorShape::Hidden);
            // TUIs that hide the hardware cursor (Kimi CLI, Ink apps) draw
            // their own caret as a reverse-video cell and park the real
            // cursor wherever the frame's last write ended, which strands
            // the IME candidate window there (#275). A lone short inverse
            // run on the cursor's row is that fake caret; collect runs so
            // the IME anchor can snap to it.
            let mut inverse_runs: Vec<(usize, usize)> = Vec::new();

            for cell in content.display_iter {
                let row = cell.point.line.0 + display_offset;
                let col = cell.point.column.0;
                if row < 0 || row as usize >= rows || col >= cols {
                    continue;
                }
                if cursor_hidden
                    && row == cursor_row
                    && cell.cell.flags.contains(Flags::INVERSE)
                    && !cell
                        .cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    match inverse_runs.last_mut() {
                        Some((start, len)) if *start + *len == col => *len += 1,
                        _ => inverse_runs.push((col, 1)),
                    }
                }
                let rc = snapshot_cell(cell.cell, cell.point, &palette, colors, selection.as_ref());
                any_selected |= rc.selected;
                buf[row as usize * cols + col] = if dim < 1. {
                    dim_cell(rc, dim, under)
                } else {
                    rc
                };
            }

            if want_sliver && (display_offset as usize) < term.grid().history_size() {
                let line = AlacLine(-display_offset - 1);
                let mut row_buf = vec![RenderCell::default(); cols];
                for (col, slot) in row_buf
                    .iter_mut()
                    .enumerate()
                    .take(term.columns().min(cols))
                {
                    let point = AlacPoint::new(line, AlacColumn(col));
                    let mut rc = snapshot_cell(
                        &term.grid()[line][AlacColumn(col)],
                        point,
                        &palette,
                        colors,
                        selection.as_ref(),
                    );
                    if dim < 1. {
                        rc = dim_cell(rc, dim, under);
                    }
                    any_selected |= rc.selected;
                    *slot = rc;
                }
                sliver = Some(row_buf);
            }

            let row = cursor_row;
            let col = cur.point.column.0;
            if row >= 0 && (row as usize) < rows && col < cols {
                // Snap the IME anchor to the fake caret when there is
                // exactly one caret-sized inverse run on the row.
                let ime_col = match inverse_runs.as_slice() {
                    [(start, len)] if cursor_hidden && *len <= 2 => *start,
                    _ => col,
                };
                cursor = Some(GridCursor {
                    row: row as usize,
                    col,
                    ime_col,
                    hidden: cursor_hidden,
                    style: cursor_style_from_shape(cur.shape),
                });
            }
        }

        let (any_match, any_current) =
            self.flag_search_matches(buf, rows, cols, display_offset, cx);
        self.flag_hovered_link(buf, rows, cols, display_offset, cx);
        Some(GridSnapshot {
            cursor,
            sliver,
            any_selected,
            any_match,
            any_current,
            display_offset,
            history_size,
        })
    }

    fn flag_hovered_link(
        &self,
        buf: &mut [RenderCell],
        rows: usize,
        cols: usize,
        display_offset: i32,
        cx: &App,
    ) {
        let Some(link) = self.view.read(cx).hovered_link.as_ref() else {
            return;
        };
        let (start, end) = (link.start, link.end);
        let mut line = start.line.0;
        while line <= end.line.0 {
            let grid_row = line + display_offset;
            if grid_row >= 0 && (grid_row as usize) < rows {
                let grid_row = grid_row as usize;
                let col_start = if line == start.line.0 {
                    start.column.0
                } else {
                    0
                };
                let col_end = if line == end.line.0 {
                    end.column.0
                } else {
                    cols.saturating_sub(1)
                };
                let mut col = col_start;
                while col <= col_end && col < cols {
                    let cell = &mut buf[grid_row * cols + col];
                    cell.link_hover = true;
                    cell.underline_color = link_underline_color(cell, link.armed);
                    col += 1;
                }
            }
            line += 1;
        }
    }

    fn flag_search_matches(
        &self,
        buf: &mut [RenderCell],
        rows: usize,
        cols: usize,
        display_offset: i32,
        cx: &App,
    ) -> (bool, bool) {
        let Some(search) = self.view.read(cx).search.as_ref() else {
            return (false, false);
        };
        let (mut any_hit, mut any_current) = (false, false);
        let first = search
            .matches
            .partition_point(|m| m.end().line.0 + display_offset < 0);
        for (i, m) in search.matches.iter().enumerate().skip(first) {
            let is_current = search.current_index == Some(i);
            let start = *m.start();
            let end = *m.end();
            if start.line.0 + display_offset >= rows as i32 {
                break;
            }
            if is_current {
                any_current = true;
            } else {
                any_hit = true;
            }
            let mut line = start.line.0;
            while line <= end.line.0 {
                let row = line + display_offset;
                if row >= 0 && (row as usize) < rows {
                    let col_start = if line == start.line.0 {
                        start.column.0
                    } else {
                        0
                    };
                    let col_end = if line == end.line.0 {
                        end.column.0
                    } else {
                        cols.saturating_sub(1)
                    };
                    let mut col = col_start;
                    while col <= col_end && col < cols {
                        let rc = &mut buf[row as usize * cols + col];
                        if is_current {
                            rc.match_current = true;
                        } else {
                            rc.match_hit = true;
                        }
                        col += 1;
                    }
                }
                line += 1;
            }
        }
        (any_hit, any_current)
    }

    fn register_mouse_handlers(
        &self,
        geom: CellGeom,
        bounds: Bounds<Pixels>,
        hitbox: HitboxId,
        window: &mut Window,
    ) {
        let view = self.view.clone();
        window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
            if !phase.bubble() || !hitbox.is_hovered(window) {
                return;
            }
            let (col, row, left) = geom.pos_to_cell(ev.position);
            let raw_row = geom.pos_to_row_raw(ev.position);
            let mods = ev.modifiers;
            let button = ev.button;
            let clicks = ev.click_count;
            view.update(cx, |v, cx| {
                if button == MouseButton::Right {
                    // Before anything else: gpui-component builds the popup
                    // from a deferred callback, and by then the pointer is
                    // only a memory. Reading the grid now is what lets the
                    // menu name the file that was actually under the click.
                    match should_show_context_menu(v.mouse_mode(), mods.shift) {
                        true => v.record_menu_link(col, row, cx),
                        false => v.forget_menu_link(),
                    }
                }
                let link_modifier = mods.secondary() || v.link_modifier_down();
                if link_modifier
                    && button == MouseButton::Left
                    && v.open_link_at(col, row, window, cx)
                {
                    return;
                }
                // The mirror image of the context-menu gate in
                // `TerminalView::render`: a click the application gets is never
                // also a click tty7 acts on, and vice versa.
                if !should_show_context_menu(v.mouse_mode(), mods.shift) {
                    v.mouse_press(button, col, row, &mods);
                    return;
                }
                if button == MouseButton::Left
                    && v.editor_click(col, raw_row, clicks, mods.shift, cx)
                {
                    return;
                }
                if button == MouseButton::Left {
                    v.on_select_start(col, row, left, clicks, mods.shift, cx);
                }
            });
        });

        let view = self.view.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, _phase, window, cx| {
            let (col, row, left) = geom.pos_to_cell(ev.position);
            let raw_row = geom.pos_to_row_raw(ev.position);
            let mods = ev.modifiers;
            let Some(button) = ev.pressed_button else {
                let inside = hitbox.is_hovered(window);
                if inside && cx.global::<Config>().focus_follows_mouse {
                    let handle = view.read(cx).focus_handle.clone();
                    if !handle.is_focused(window) {
                        window.focus(&handle, cx);
                    }
                }
                view.update(cx, |v, cx| {
                    if inside {
                        if !mods.shift {
                            v.mouse_motion(col, row, &mods);
                        }
                        let armed = mods.secondary() || v.link_modifier_down();
                        v.hover_link_at(col, row, armed, cx);
                    } else {
                        v.clear_hovered_link(cx);
                    }
                });
                return;
            };
            view.update(cx, |v, cx| {
                if v.mouse_mode() && !mods.shift {
                    v.mouse_drag(button, col, row, &mods);
                    return;
                }
                if button == MouseButton::Left && v.editor_drag(col, raw_row, cx) {
                    return;
                }
                if button == MouseButton::Left {
                    v.on_select_update(col, row, left, cx);
                    let overshoot = drag_overshoot(ev.position.y, bounds, geom.line_height);
                    v.select_autoscroll(overshoot, col, left, cx);
                }
            });
        });

        let view = self.view.clone();
        window.on_mouse_event(move |ev: &MouseUpEvent, phase, _window, cx| {
            if !phase.bubble() {
                return;
            }
            let (col, row, _left) = geom.pos_to_cell(ev.position);
            let mods = ev.modifiers;
            let button = ev.button;
            view.update(cx, |v, cx| {
                if v.mouse_mode() && !mods.shift {
                    v.mouse_release(button, col, row, &mods);
                    return;
                }
                v.on_select_end(cx);
            });
        });
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TermLayout;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        style.flex_grow = 1.0;
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let font_size = self.view.read(cx).font_size;
        let base_font = self.view.read(cx).font.clone();

        let sample = window.text_system().shape_line(
            SharedString::new_static("M"),
            font_size,
            &[TextRun {
                len: 1,
                font: base_font.clone(),
                color: Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        let cell_width = sample.width.max(px(1.));
        let line_height_mul = self.view.read(cx).line_height_mul;
        let line_height = px((font_size.as_f32() * line_height_mul).round()).max(px(1.));

        let cols = (bounds.size.width.as_f32() / cell_width.as_f32())
            .floor()
            .max(1.0) as usize;
        let rows = (bounds.size.height.as_f32() / line_height.as_f32())
            .floor()
            .max(1.0) as usize;

        self.view.update(cx, |view, cx| {
            view.set_grid_size(
                cols,
                rows,
                cell_width,
                line_height,
                window.scale_factor(),
                cx,
            );
        });

        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);

        TermLayout {
            cell_width,
            line_height,
            cols,
            rows,
            hitbox,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let fps_start = super::fps::enabled().then(std::time::Instant::now);

        let frac = self.view.read(cx).scroll_frac.clamp(0., 1.);
        let input_shift = self.view.read(cx).input_scroll_rows();
        let geom = CellGeom {
            origin: point(
                bounds.origin.x,
                bounds.origin.y + prepaint.line_height * (frac - input_shift as f32),
            ),
            cell_width: prepaint.cell_width,
            line_height: prepaint.line_height,
            cols: prepaint.cols,
            rows: prepaint.rows,
        };
        let colors = PaintColors::resolve(cx.theme(), cx);

        let font_size = self.view.read(cx).font_size;
        let base_font = self.view.read(cx).font.clone();
        let bold_font = self.view.read(cx).font_bold.clone();
        let italic_font = self.view.read(cx).font_italic.clone();
        let focused = self.view.read(cx).focus_handle.is_focused(window);
        let cursor_visible = self.view.read(cx).cursor_visible;
        let bell_flash = self.view.read(cx).bell_flash;
        let editor_active = self.view.read(cx).input_active();
        // The pane leaf stores its per-frame dim here (see `TerminalView::dim`).
        // Blending the palette and paint colours toward the window background is
        // what actually dims the pane; the pane no longer wraps the terminal in
        // an element-opacity style, whose per-primitive alpha multiplication
        // would leave stacked decorations with a seam against the segments
        // below them.
        let dim = self.view.read(cx).dim.clamp(0., 1.);
        let (colors, dim, under) = if dim < 1. {
            let under = dim_under(cx);
            (colors.dimmed(dim, under), dim, under)
        } else {
            // `under` is only read while `dim < 1.` (per-cell in the grid and
            // over bitmaps), so a default keeps the common rest frame from
            // paying for the preset lookup and colour math.
            (colors, 1., Rgba::default())
        };

        // This pane's previous frame, borrowed for the duration of this one.
        // `build_grid` overwrites it when it gets the terminal lock, and leaves
        // it exactly as it is when it does not.
        let mut buf = self
            .view
            .update(cx, |view, _| std::mem::take(&mut view.grid_buf));
        let previous = self.view.read(cx).grid_snap.clone();
        // The two frames with nothing to fall back on: the first one this pane
        // ever paints, and the one after a resize, whose previous grid is the
        // wrong shape to paint into these bounds. Those wait for the lock.
        let must_block = previous.is_none() || buf.len() != geom.rows * geom.cols;
        let built = self.build_grid(
            &colors,
            &mut buf,
            geom.rows,
            geom.cols,
            frac > 0.,
            cx,
            dim,
            under,
            must_block,
        );
        if let Some(snap) = &built {
            let snap = snap.clone();
            self.view.update(cx, |view, _| view.grid_snap = Some(snap));
        }
        // `must_block` above is exactly the condition under which `build_grid`
        // is not allowed to come back empty, so one of the two is always here.
        let Some(snap) = built.or(previous) else {
            return;
        };
        let cursor = snap.cursor;
        let sliver = snap.sliver.as_ref();

        let cursor_cell = cursor.map(|c| (c.row, c.ime_col));
        let render_cursor = snap.painted_cursor();

        // Reverse-video the block cursor's cell up front, so it rides the
        // normal background-then-glyph path instead of being tinted on top of
        // the finished frame. `paint_cursor` skips the focused block for the
        // same reason.
        if !editor_active
            && focused
            && cursor_visible
            && let Some((row, col, crate::core::config::CursorStyle::Block)) = render_cursor
        {
            invert_cursor_cell(&mut buf, geom.cols, row, col, &colors);
        }

        // With the input bar up, the IME composes into the bar, which starts a
        // row below the prompt when the prompt left it too little room.
        let ime_cell = cursor.map(|c| {
            self.view
                .read(cx)
                .input_ime_cell(c.row, c.col, geom.cols)
                .unwrap_or((c.row, c.ime_col))
        });
        let cursor_bounds = ime_cell.map(|(row, col)| geom.cell_rect(row, col, 1));
        let focus_handle = self.view.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            super::input::TerminalInputHandler::new(self.view.clone(), cursor_bounds),
            cx,
        );
        let marked = self.view.read(cx).marked_text.clone();

        // Kitty-graphics placements for this pane. Each image anchors to an
        // absolute scrollback row (recorded when its command arrived in-stream);
        // map that back to a screen row with the snapshot's scroll state and
        // drop anything scrolled out of the viewport.
        let image_store = self.view.read(cx).terminal.images();
        // Take the retired list *before* the snapshot. The decode worker runs on
        // its own thread and can retire a frame between the two calls; taking
        // retired second would hand us a list containing an image the snapshot
        // still says to paint, and `sprite_atlas.remove` takes effect
        // immediately — so the frame would paint and then vanish. In this order
        // the worst case is evicting one paint late, which is invisible.
        let retired_images = image_store.take_retired();
        let images = image_store.snapshot();

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            paint_backgrounds(window, &geom, &buf);
            if snap.any_selected {
                paint_cell_runs(window, &geom, &buf, colors.selection_bg, |c| c.selected);
            }
            if snap.any_match {
                paint_cell_runs(window, &geom, &buf, colors.match_bg, |c| {
                    c.match_hit && !c.match_current
                });
            }
            if snap.any_current {
                // Fill only. The current match used to carry a caret-coloured
                // outline as well, because a wash it could only be 2.1:1 off the
                // background had no way to say "this one" on its own. Now that
                // it is the accent at the top of the theme's budget, the outline
                // is the same colour saying the same thing a second time.
                paint_cell_runs(window, &geom, &buf, colors.current_match_bg, |c| {
                    c.match_current
                });
            }
            paint_glyphs(
                window,
                cx,
                &geom,
                &buf,
                font_size,
                &base_font,
                bold_font.as_ref(),
                italic_font.as_ref(),
            );
            let scale = window.scale_factor();
            paint_special_underlines(window, &geom, &buf, scale);
            // Kitty-graphics images, painted over the placeholder cells they
            // occupy. `anchor_row` is absolute (measured from the top of
            // scrollback); convert it to a screen row with the same scroll state
            // `build_grid` captured. Rows spanning past the viewport top/bottom
            // are clipped by the surrounding content mask.
            //
            // Sizing: per the kitty spec, an image with no `c=`/`r=` is shown at
            // its natural size — one image pixel per terminal *device* pixel. A
            // sender like terminal-browser renders its frame to exactly fill the
            // pixel area we reported to the child via `ws_xpixel`/`ws_ypixel`,
            // which the daemon sets to `cols × round(cell_w × scale)` /
            // `rows × round(cell_h × scale)` — the cell size in *device* pixels
            // (see `set_grid_size`). So the frame is at device resolution; to map
            // it back to a cell span we divide by that *same* device cell size,
            // `round(cell_logical × scale)`. Painting the resulting cell-span
            // bounds (in logical px) lets gpui blit the device-resolution bitmap
            // ~1:1 on the framebuffer — sharp, and the right size — instead of
            // upscaling a half-resolution one. Deriving here, not at placement,
            // keeps it correct across font-size / zoom / display-scale changes.
            let scale = window.scale_factor();
            let scale = if scale.is_finite() && scale > 0. {
                scale
            } else {
                1.
            };
            for img in &images {
                let round_w = (geom.cell_width.as_f32() * scale).round().max(1.);
                let round_h = (geom.line_height.as_f32() * scale).round().max(1.);
                let span_cols = if img.cols > 0 {
                    img.cols as f32
                } else {
                    (img.width_px as f32 / round_w).round().max(1.)
                };
                let span_rows = if img.rows > 0 {
                    img.rows as f32
                } else {
                    (img.height_px as f32 / round_h).round().max(1.)
                };
                let screen_row =
                    img.anchor_row - snap.history_size as i64 + snap.display_offset as i64;
                // Fully above or below the viewport: nothing visible to paint.
                if screen_row + span_rows as i64 <= 0 || screen_row >= geom.rows as i64 {
                    continue;
                }
                if !image_store.claim_for_paint(img) {
                    continue;
                }
                let top = geom.origin.y + geom.line_height * screen_row as f32;
                let left = geom.origin.x + geom.cell_width * img.anchor_col as f32;
                let bounds = Bounds {
                    origin: point(left, top),
                    size: size(geom.cell_width * span_cols, geom.line_height * span_rows),
                };
                let _ = window.paint_image(bounds, Corners::default(), img.data.clone(), 0, false);
                // A bitmap is a single layer, so its dim cannot come from the
                // pre-blended palette; blend the image itself toward the under
                // the same way the surrounding cells are blended.
                if dim < 1. {
                    let mut c: Hsla = under.into();
                    c.a = 1. - dim;
                    window.paint_quad(fill(bounds, c));
                }
            }
            // Evict superseded / deleted frames from the sprite atlas. Without
            // this a browser re-transmitting at 60fps would leak one GPU tile per
            // frame, growing the atlas without bound until the compositor stalls.
            for retired in &retired_images {
                let _ = window.drop_image(retired.clone());
            }
            if let Some(row) = sliver {
                let sg = CellGeom {
                    origin: point(geom.origin.x, geom.origin.y - geom.line_height),
                    rows: 1,
                    ..geom
                };
                paint_backgrounds(window, &sg, row);
                if snap.any_selected {
                    paint_cell_runs(window, &sg, row, colors.selection_bg, |c| c.selected);
                }
                paint_glyphs(
                    window,
                    cx,
                    &sg,
                    row,
                    font_size,
                    &base_font,
                    bold_font.as_ref(),
                    italic_font.as_ref(),
                );
                paint_special_underlines(window, &sg, row, scale);
            }
            if !editor_active {
                paint_cursor(
                    window,
                    &geom,
                    render_cursor,
                    focused,
                    cursor_visible,
                    colors.caret,
                );
                paint_marked(
                    window,
                    cx,
                    &geom,
                    cursor_cell,
                    &marked,
                    font_size,
                    &base_font,
                    colors.default_fg,
                    colors.default_bg,
                );
            }

            if bell_flash {
                let mut c = colors.default_fg;
                c.a = 0.12;
                window.paint_quad(fill(bounds, c));
            }
        });

        self.view.update(cx, |view, _| view.grid_buf = buf);

        self.register_mouse_handlers(geom, bounds, prepaint.hitbox.id, window);

        let view = self.view.read(cx);
        if view.hovered_link.as_ref().is_some_and(|link| link.armed) {
            window.set_cursor_style(CursorStyle::PointingHand, &prepaint.hitbox);
        } else if !view.mouse_mode() {
            window.set_cursor_style(CursorStyle::IBeam, &prepaint.hitbox);
        }

        if let Some(start) = fps_start {
            super::fps::record(start.elapsed());
        }

        // The dim is a per-frame hand-off from the pane leaf to this paint
        // (see `TerminalView::set_dim`): the pane writes it while rendering,
        // this paint reads it above. Reset it here so the hand-off is
        // structural — any frame that ends without a render site having set
        // the dim paints at full brightness next, instead of carrying a stale
        // value. A render site that forgets to set it (a maximized pane has
        // no chrome to set it) therefore degrades to a single stale frame at
        // worst, never a permanently dimmed terminal.
        self.view.update(cx, |v, _cx| v.set_dim(1.));
    }
}

#[derive(Clone, Copy)]
struct CellGeom {
    origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    cols: usize,
    rows: usize,
}

impl CellGeom {
    fn cell_rect(&self, row: usize, col: usize, span: usize) -> Bounds<Pixels> {
        let x = self.origin.x + self.cell_width * (col as f32);
        let y = self.origin.y + self.line_height * (row as f32);
        Bounds::new(
            point(x, y),
            size(self.cell_width * (span as f32), self.line_height),
        )
    }

    fn pos_to_cell(&self, pos: Point<Pixels>) -> (usize, usize, bool) {
        let lx = (pos.x - self.origin.x).as_f32().max(0.);
        let ly = (pos.y - self.origin.y).as_f32().max(0.);
        let colf = lx / self.cell_width.as_f32();
        let col = (colf.floor() as usize).min(self.cols.saturating_sub(1));
        let row =
            ((ly / self.line_height.as_f32()).floor() as usize).min(self.rows.saturating_sub(1));
        let left = (colf - colf.floor()) <= 0.5;
        (col, row, left)
    }

    fn pos_to_row_raw(&self, pos: Point<Pixels>) -> usize {
        let ly = (pos.y - self.origin.y).as_f32().max(0.);
        (ly / self.line_height.as_f32()).floor() as usize
    }
}

fn drag_overshoot(y: Pixels, bounds: Bounds<Pixels>, line_height: Pixels) -> f32 {
    let lh = line_height.as_f32().max(1.);
    if y < bounds.top() {
        (bounds.top() - y).as_f32() / lh
    } else if y > bounds.bottom() {
        -((y - bounds.bottom()).as_f32() / lh)
    } else {
        0.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caret_colors() -> PaintColors {
        PaintColors {
            default_fg: to_hsla(Rgb {
                r: 17,
                g: 17,
                b: 17,
            }),
            default_bg: to_hsla(Rgb {
                r: 255,
                g: 255,
                b: 255,
            }),
            caret: to_hsla(Rgb {
                r: 0xf5,
                g: 0xa1,
                b: 0x5c,
            }),
            selection_bg: Hsla::default(),
            match_bg: Hsla::default(),
            current_match_bg: Hsla::default(),
            legible_dim: true,
            fg_rgb: Rgb {
                r: 17,
                g: 17,
                b: 17,
            },
            bg_rgb: Rgb {
                r: 255,
                g: 255,
                b: 255,
            },
        }
    }

    #[test]
    fn a_block_cursor_turns_its_cell_into_reverse_video() {
        let colors = caret_colors();
        let mut buf = vec![RenderCell::default(); 6];
        buf[3].c = 'o';
        buf[3].fg = to_hsla(Rgb { r: 1, g: 2, b: 3 });
        invert_cursor_cell(&mut buf, 3, 1, 0, &colors);

        let cell = &buf[3];
        assert!(cell.draw_bg, "the caret fill has to be painted");
        assert_eq!(cell.bg, colors.caret);
        assert_ne!(
            cell.fg,
            to_hsla(Rgb { r: 1, g: 2, b: 3 }),
            "the glyph is redrawn on the caret, not left in its own colour"
        );
        assert_eq!(cell.c, 'o', "the character itself survives");

        for (i, other) in buf.iter().enumerate() {
            if i != 3 {
                assert!(!other.draw_bg, "cell {i} was not the cursor cell");
            }
        }
    }

    #[test]
    fn a_cursor_parked_on_a_wide_char_inverts_the_half_that_holds_the_glyph() {
        let colors = caret_colors();
        let mut buf = vec![RenderCell::default(); 4];
        buf[1].c = '世';
        buf[2].spacer = true;

        // Cursor reported on the trailing spacer: the lead cell is the one
        // that gets the fill, so it can absorb the spacer into its run.
        invert_cursor_cell(&mut buf, 4, 0, 2, &colors);
        assert!(buf[1].draw_bg, "the glyph's own cell carries the caret");
        assert!(!buf[2].draw_bg, "the spacer is absorbed, not painted");
        assert_eq!(buf[1].bg, colors.caret);

        // A spacer in column 0 has no lead to step back to; leave it alone
        // rather than wrapping to the previous row.
        let mut edge = vec![RenderCell::default(); 4];
        edge[0].spacer = true;
        invert_cursor_cell(&mut edge, 4, 0, 0, &colors);
        assert!(edge[0].draw_bg);
    }

    #[test]
    fn caret_ink_takes_whichever_of_the_two_reads_on_the_caret() {
        let colors = caret_colors();
        // A pale caret on a white background: the foreground wins.
        let ink = crate::ui::presets::caret_ink(colors.caret, colors.default_bg, colors.default_fg);
        assert_eq!(ink, colors.default_fg);

        // A caret far from the background: the background wins, which is the
        // conventional reverse-video pairing.
        let deep = to_hsla(Rgb {
            r: 0x20,
            g: 0x20,
            b: 0x80,
        });
        let ink = crate::ui::presets::caret_ink(deep, colors.default_bg, colors.default_fg);
        assert_eq!(ink, colors.default_bg);
    }

    #[test]
    fn to_hsla_normalizes_channels_and_alpha() {
        let black = to_hsla(Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(black.a, 1.0);
        assert!(black.l.abs() < 1e-6, "black has zero lightness");

        let white = to_hsla(Rgb {
            r: 255,
            g: 255,
            b: 255,
        });
        assert!((white.l - 1.0).abs() < 1e-6, "white has full lightness");
        assert!(white.s.abs() < 1e-6, "white is desaturated");

        let back = Rgba::from(to_hsla(Rgb { r: 255, g: 0, b: 0 }));
        assert!((back.r - 1.0).abs() < 1e-3);
        assert!(back.g.abs() < 1e-3 && back.b.abs() < 1e-3);
    }

    #[test]
    fn resolve_covers_every_color_slot() {
        let mut palette = [Rgb { r: 0, g: 0, b: 0 }; 256];
        for (i, slot) in palette.iter_mut().enumerate() {
            slot.r = i as u8;
        }
        let fg = Rgb {
            r: 200,
            g: 201,
            b: 202,
        };
        let bg = Rgb {
            r: 10,
            g: 11,
            b: 12,
        };

        let spec = Rgb { r: 1, g: 2, b: 3 };
        assert_eq!(
            resolve(AnsiColor::Spec(spec), &palette, fg, bg),
            (spec, false)
        );

        let (rgb, is_def) = resolve(AnsiColor::Indexed(5), &palette, fg, bg);
        assert_eq!(rgb.r, 5);
        assert!(!is_def);

        assert_eq!(
            resolve(AnsiColor::Named(NamedColor::Foreground), &palette, fg, bg),
            (fg, true)
        );
        assert_eq!(
            resolve(AnsiColor::Named(NamedColor::Background), &palette, fg, bg),
            (bg, true)
        );

        let (rgb, is_def) = resolve(AnsiColor::Named(NamedColor::Red), &palette, fg, bg);
        assert_eq!(rgb.r, NamedColor::Red as u8);
        assert!(!is_def);
    }

    #[test]
    fn build_font_sets_weight_and_style() {
        let base = gpui::font("Courier");
        let plain = build_font(&base, false, false);
        assert_eq!(plain.weight, FontWeight::NORMAL);
        assert_eq!(plain.style, FontStyle::Normal);

        let bold_italic = build_font(&base, true, true);
        assert_eq!(bold_italic.weight, FontWeight::BOLD);
        assert_eq!(bold_italic.style, FontStyle::Italic);

        assert_eq!(bold_italic.family, base.family);
    }

    #[test]
    fn cell_rect_maps_grid_to_pixels() {
        let geom = CellGeom {
            origin: point(px(10.), px(20.)),
            cell_width: px(8.),
            line_height: px(16.),
            cols: 80,
            rows: 24,
        };
        let r = geom.cell_rect(2, 3, 1);
        assert_eq!(r.origin.x, px(10. + 8. * 3.));
        assert_eq!(r.origin.y, px(20. + 16. * 2.));
        assert_eq!(r.size.width, px(8.));
        assert_eq!(r.size.height, px(16.));
        assert_eq!(geom.cell_rect(0, 0, 4).size.width, px(32.));
    }

    #[test]
    fn pos_to_cell_clamps_and_detects_halves() {
        let geom = CellGeom {
            origin: point(px(0.), px(0.)),
            cell_width: px(10.),
            line_height: px(20.),
            cols: 5,
            rows: 3,
        };
        let (c, r, left) = geom.pos_to_cell(point(px(2.), px(5.)));
        assert_eq!((c, r), (0, 0));
        assert!(left);
        let (_, _, left) = geom.pos_to_cell(point(px(8.), px(5.)));
        assert!(!left);
        let (c, r, _) = geom.pos_to_cell(point(px(-100.), px(-100.)));
        assert_eq!((c, r), (0, 0));
        let (c, r, _) = geom.pos_to_cell(point(px(9999.), px(9999.)));
        assert_eq!((c, r), (4, 2));
    }

    #[test]
    fn drag_overshoot_signed_by_edge_and_zero_inside() {
        let bounds = Bounds::new(point(px(0.), px(100.)), size(px(200.), px(100.)));
        assert_eq!(drag_overshoot(px(150.), bounds, px(10.)), 0.);
        assert_eq!(drag_overshoot(px(100.), bounds, px(10.)), 0.);
        assert_eq!(drag_overshoot(px(200.), bounds, px(10.)), 0.);
        assert_eq!(drag_overshoot(px(80.), bounds, px(10.)), 2.);
        assert_eq!(drag_overshoot(px(230.), bounds, px(10.)), -3.);
    }

    #[test]
    fn fit_scale_shrinks_what_overflows_and_never_enlarges() {
        let budget = px(15.);
        assert_eq!(fit_scale(px(19.2), budget), 15. / 19.2);
        // Whatever already fits keeps its own size, including a glyph that
        // lands exactly on the edge and a run that measured as empty. Making
        // a small glyph bigger is `icon_fit`'s rule, not this one's.
        assert_eq!(fit_scale(px(15.), budget), 1.);
        assert_eq!(fit_scale(px(12.), budget), 1.);
        assert_eq!(fit_scale(px(0.), budget), 1.);
    }

    #[test]
    fn an_emoji_only_shrinks_where_the_face_is_narrow_and_the_next_cell_is_taken() {
        // Apple Color Emoji inks 1.25em, which is 18.75px at a 15px font size.
        let ink = px(18.75);
        let scale = |advance_em: f32, room: bool| {
            let cell = px(15. * advance_em);
            fit_scale(ink, seg_budget(false, true, 2, room, cell))
        };

        // Menlo and friends: a quarter cell of slack is enough on its own.
        assert_eq!(scale(0.6, false), 1.);
        // Ubuntu Mono is narrow enough that a crowded neighbour forces a
        // shrink, but a blank one lends a whole cell and the emoji stays whole.
        assert!(scale(0.5, false) < 1.);
        assert_eq!(scale(0.5, true), 1.);
    }

    #[test]
    fn a_run_that_keeps_one_glyph_per_cell_is_painted_whole() {
        assert_eq!(run_drift([0, 1, 2, 3]), None);
        assert_eq!(run_drift([0]), None);
        assert_eq!(run_drift([0usize; 0]), None);
    }

    #[test]
    fn a_ligature_cuts_the_run_at_the_column_that_drifted() {
        // "office" through a face that ligates "ffi": the shaper answers with
        // o, ffi, c, e, and `force_width` sits them on columns 0..4 — so 'c'
        // lands two columns early, over the ligature's tail, and the row ends
        // two columns short of where the caret is drawn.
        assert_eq!(run_drift([0, 1, 4, 5]), Some(4));
        // The remainder, restarted at column 4, lines up on its own.
        assert_eq!(run_drift([0, 1]), None);
        // A two-character ligature drifts by one: "afib" -> a, fi, b.
        assert_eq!(run_drift([0, 1, 3]), Some(3));
        // And a run can drift on its very first pair: "fib" -> fi, b.
        assert_eq!(run_drift([0, 2]), Some(2));
    }

    /// Drives the loop `paint_glyphs` runs a `RowSeg::Run` through, with the
    /// shaping stubbed out. `drifts` is what the shaper is pretending to
    /// answer for each piece it is handed, in order.
    fn run_steps(start: usize, text: &str, drifts: &[Option<usize>]) -> Vec<RunStep> {
        let mut seen = Vec::new();
        let mut answers = drifts.iter().copied();
        paint_run(start, text.len(), |step| {
            let drift = matches!(step, RunStep::Drift { .. })
                .then(|| {
                    answers
                        .next()
                        .expect("asked to shape more pieces than scripted")
                })
                .flatten();
            seen.push(step);
            drift
        });
        seen
    }

    /// What the caller is told to paint, as `(column, text)`.
    fn painted<'a>(steps: &[RunStep], text: &'a str) -> Vec<(usize, &'a str)> {
        steps
            .iter()
            .filter_map(|step| match *step {
                RunStep::Paint { col, at, len } => Some((col, &text[at..at + len])),
                RunStep::Drift { .. } => None,
            })
            .collect()
    }

    #[test]
    fn a_run_that_does_not_drift_is_shaped_once_and_painted_whole() {
        let steps = run_steps(7, "hello", &[None]);
        assert_eq!(
            steps,
            [
                RunStep::Drift { at: 0, len: 5 },
                RunStep::Paint {
                    col: 7,
                    at: 0,
                    len: 5
                }
            ],
            "no drift is one shaping and one paint, at the column it started in"
        );
    }

    #[test]
    fn a_drifted_run_paints_the_head_alone_and_restarts_at_its_own_column() {
        // "office" through a face that ligates "ffi": the shaper answers with
        // o, ffi, c, e, so 'c' arrives at byte 4 as the third glyph and the
        // run has to be cut there.
        let steps = run_steps(0, "office", &[Some(4), None]);
        assert_eq!(
            painted(&steps, "office"),
            [(0, "offi"), (4, "ce")],
            "the head is a piece of its own, and the rest starts at column 4"
        );
        // The head must be *shaped* as "offi", not painted as the whole run
        // under a four-cell clip: 'c' and 'e' drifted left, into those very
        // cells, so a clip would leave them on top of the ligature.
        assert_eq!(
            steps[1],
            RunStep::Paint {
                col: 0,
                at: 0,
                len: 4
            }
        );
        // And the remainder is re-shaped on its own before it is painted, so
        // its own drift is measured from its own column.
        assert_eq!(steps[2], RunStep::Drift { at: 4, len: 2 });
    }

    #[test]
    fn a_run_that_drifts_twice_keeps_advancing_and_finishes() {
        // "affib" -> a, ffi, b cuts once; the tail "b" then stands on its own.
        assert_eq!(
            painted(&run_steps(3, "affib", &[Some(4), None]), "affib"),
            [(3, "affi"), (7, "b")]
        );
        // Two cuts in a row: every piece moves the column on by its own width
        // and the walk ends on the piece that does not drift.
        assert_eq!(
            painted(
                &run_steps(0, "offifie", &[Some(4), Some(2), None]),
                "offifie"
            ),
            [(0, "offi"), (4, "fi"), (6, "e")]
        );
    }

    #[test]
    fn a_face_that_adds_glyphs_is_left_alone() {
        // Two glyphs for one character walk ahead of their columns, not
        // behind them. Cutting there would restart the run where it already
        // is and never finish.
        assert_eq!(run_drift([0, 0, 1, 2]), None);
        assert_eq!(run_drift([0, 1, 1, 2]), None);
    }

    #[test]
    fn only_a_plain_blank_cell_counts_as_room() {
        let mut row: Vec<_> = "ab".chars().map(cell).collect();
        row.push(cell(' '));
        assert!(!has_room_after(&row, 0, 1), "a letter is not room");
        assert!(has_room_after(&row, 1, 1), "a blank is");
        assert!(has_room_after(&row, 2, 1), "so is the end of the row");

        // A blank that paints something of its own is not free real estate.
        row[2].draw_bg = true;
        assert!(!has_room_after(&row, 1, 1));
        row[2].draw_bg = false;
        row[2].selected = true;
        assert!(!has_room_after(&row, 1, 1));
        row[2].selected = false;

        // Nor is a blank that carries a rule of its own: it is painted after
        // the segment next to it, so the stroke would land on the glyph.
        row[2].underline = UnderlineKind::Single;
        assert!(!has_room_after(&row, 1, 1), "an underlined blank");
        row[2].underline = UnderlineKind::None;
        row[2].strikeout = true;
        assert!(!has_room_after(&row, 1, 1), "a struck-through blank");
        row[2].strikeout = false;
        row[2].link_hover = true;
        assert!(!has_room_after(&row, 1, 1), "a hovered link's blank");
    }

    #[test]
    fn terminal_cursor_shape_maps_to_painted_cursor_style() {
        use crate::core::config::CursorStyle;

        assert_eq!(cursor_style_from_shape(CursorShape::Beam), CursorStyle::Bar);
        assert_eq!(
            cursor_style_from_shape(CursorShape::Underline),
            CursorStyle::Underline
        );
        assert_eq!(
            cursor_style_from_shape(CursorShape::Block),
            CursorStyle::Block
        );
        assert_eq!(
            cursor_style_from_shape(CursorShape::HollowBlock),
            CursorStyle::Block
        );
    }

    fn cell(c: char) -> RenderCell {
        RenderCell {
            c,
            ..RenderCell::default()
        }
    }

    fn run(start: usize, cells: usize, text: &str) -> RowSeg {
        RowSeg::Run {
            start,
            cells,
            text: text.to_string(),
        }
    }

    fn wide(start: usize, cells: usize, text: &str) -> RowSeg {
        RowSeg::Wide {
            start,
            cells,
            text: text.into(),
        }
    }

    fn wide_cells(chars: &str) -> Vec<RenderCell> {
        let mut row = Vec::new();
        for c in chars.chars() {
            row.push(cell(c));
            let mut sp = cell(' ');
            sp.spacer = true;
            row.push(sp);
        }
        row
    }

    #[test]
    fn segment_row_batches_uniform_ascii() {
        let row: Vec<_> = "hello".chars().map(cell).collect();
        assert_eq!(segment_row(&row), [run(0, 5, "hello")]);
    }

    #[test]
    fn segment_row_joins_plain_runs_across_gaps_but_trims_edges() {
        let row: Vec<_> = " ab  cd  ".chars().map(cell).collect();
        assert_eq!(segment_row(&row), [run(1, 6, "ab  cd")]);
        let mut row: Vec<_> = "ab cd".chars().map(cell).collect();
        row[2].c = '\0';
        assert_eq!(segment_row(&row), [run(0, 5, "ab cd")]);
    }

    /// `paint_glyphs` reads a glyph's byte index as the column it came from
    /// and cuts a drifted run there, which only holds while a run is ASCII and
    /// one byte wide per cell. Pin that here rather than in the paint code.
    #[test]
    fn a_run_is_as_long_in_bytes_as_it_is_wide_in_cells() {
        let rows: [Vec<RenderCell>; 4] = [
            " ab  cd  ".chars().map(cell).collect(),
            "https://例/a".chars().map(cell).collect(),
            {
                let mut row: Vec<_> = "ab cd".chars().map(cell).collect();
                for c in &mut row {
                    c.underline = UnderlineKind::Single;
                }
                row
            },
            "x->y a//b".chars().map(cell).collect(),
        ];
        for row in rows {
            for seg in segment_row(&row) {
                if let RowSeg::Run { cells, text, .. } = seg {
                    assert!(text.is_ascii(), "{text:?} is not ASCII");
                    assert_eq!(text.len(), cells, "{text:?} does not span {cells} cells");
                }
            }
        }
    }

    #[test]
    fn segment_row_ends_underlined_runs_at_blanks() {
        let mut row: Vec<_> = "ab cd".chars().map(cell).collect();
        for c in &mut row {
            c.underline = UnderlineKind::Single;
        }
        assert_eq!(
            segment_row(&row),
            [run(0, 2, "ab"), run(2, 1, " "), run(3, 2, "cd")]
        );

        let mut row: Vec<_> = "ab cd".chars().map(cell).collect();
        for c in &mut row {
            c.link_hover = true;
        }
        assert_eq!(segment_row(&row), [run(0, 2, "ab"), run(3, 2, "cd")]);
    }

    #[test]
    fn segment_row_keeps_decorated_blank_runs() {
        let mut row: Vec<_> = "   ".chars().map(cell).collect();
        for c in &mut row {
            c.strikeout = true;
        }
        assert_eq!(segment_row(&row), [run(0, 3, "   ")]);

        for c in &mut row {
            c.strikeout = false;
            c.underline = UnderlineKind::Dotted;
        }
        assert_eq!(segment_row(&row), [run(0, 3, "   ")]);
    }

    #[test]
    fn segment_row_splits_on_style_changes() {
        let mut row: Vec<_> = "abcd".chars().map(cell).collect();
        row[2].fg = gpui::red();
        row[3].fg = gpui::red();
        assert_eq!(segment_row(&row), [run(0, 2, "ab"), run(2, 2, "cd")]);

        let mut row: Vec<_> = "abcd".chars().map(cell).collect();
        row[0].bold = true;
        assert_eq!(segment_row(&row), [run(0, 1, "a"), run(1, 3, "bcd")]);
    }

    #[test]
    fn segment_row_isolates_non_ascii() {
        let mut row: Vec<_> = "ok?字 no".chars().map(cell).collect();
        row.insert(4, {
            let mut sp = cell(' ');
            sp.spacer = true;
            sp
        });
        assert_eq!(
            segment_row(&row),
            [run(0, 3, "ok?"), wide(3, 2, "字"), run(6, 2, "no")]
        );

        let row: Vec<_> = "a─b".chars().map(cell).collect();
        assert_eq!(
            segment_row(&row),
            [run(0, 1, "a"), RowSeg::Solo { col: 1 }, run(2, 1, "b")]
        );
    }

    #[test]
    fn powerline_shape_maps_only_the_solid_separators() {
        for (c, shape) in [
            ('\u{e0b0}', PowerlineShape::TriangleRight),
            ('\u{e0b2}', PowerlineShape::TriangleLeft),
            ('\u{e0b4}', PowerlineShape::HalfCircleRight),
            ('\u{e0b6}', PowerlineShape::HalfCircleLeft),
            ('\u{e0b8}', PowerlineShape::SlantLowerLeft),
            ('\u{e0ba}', PowerlineShape::SlantLowerRight),
            ('\u{e0bc}', PowerlineShape::SlantUpperLeft),
            ('\u{e0be}', PowerlineShape::SlantUpperRight),
        ] {
            assert_eq!(PowerlineShape::of(c), Some(shape), "U+{:04X}", c as u32);
        }
        for c in [
            '\u{e0b1}', '\u{e0b3}', '\u{e0b5}', '\u{e0b7}', '\u{e0b9}', '\u{e0bb}', '\u{e0bd}',
            '\u{e0bf}', '\u{e0a0}', '\u{e0c0}', '\u{2500}', '❯', '➜',
        ] {
            assert_eq!(PowerlineShape::of(c), None, "U+{:04X}", c as u32);
        }
    }

    #[test]
    fn powerline_path_fills_exactly_one_cell() {
        let (x0, y0, w, h) = (px(10.), px(20.), px(9.), px(21.));
        let bounds = Bounds::new(point(x0, y0), size(w, h));
        for shape in [
            PowerlineShape::TriangleRight,
            PowerlineShape::TriangleLeft,
            PowerlineShape::HalfCircleRight,
            PowerlineShape::HalfCircleLeft,
            PowerlineShape::SlantLowerLeft,
            PowerlineShape::SlantLowerRight,
            PowerlineShape::SlantUpperLeft,
            PowerlineShape::SlantUpperRight,
        ] {
            let path = powerline_path(bounds, shape);
            assert!(!path.vertices.is_empty(), "{shape:?} produced no geometry");
            let (mut min_x, mut max_x) = (px(f32::MAX), px(f32::MIN));
            for v in &path.vertices {
                let p = v.xy_position;
                assert!(
                    p.x >= x0 && p.x <= x0 + w && p.y >= y0 && p.y <= y0 + h,
                    "{shape:?} vertex at {p:?} escapes the cell"
                );
                min_x = min_x.min(p.x);
                max_x = max_x.max(p.x);
            }
            assert_eq!(min_x, x0, "{shape:?} does not reach the left cell edge");
            assert_eq!(
                max_x,
                x0 + w,
                "{shape:?} does not reach the right cell edge"
            );
            match shape {
                PowerlineShape::HalfCircleRight => assert!(
                    path.vertices[1].xy_position.x <= x0 + w * 0.4
                        && path.vertices[path.vertices.len() - 2].xy_position.x <= x0 + w * 0.4,
                    "right caps should not collapse into a diagonal wedge"
                ),
                PowerlineShape::HalfCircleLeft => assert!(
                    path.vertices[1].xy_position.x >= x0 + w * 0.6
                        && path.vertices[path.vertices.len() - 2].xy_position.x >= x0 + w * 0.6,
                    "left caps should not collapse into a diagonal wedge"
                ),
                _ => {}
            }
        }
    }

    #[test]
    fn powerline_solid_edge_covers_only_the_fractional_device_pixel() {
        let bounds = Bounds::new(point(px(10.2), px(20.)), size(px(9.2), px(21.)));
        let scale = 1.25;

        for shape in [
            PowerlineShape::TriangleRight,
            PowerlineShape::HalfCircleRight,
            PowerlineShape::SlantLowerLeft,
            PowerlineShape::SlantUpperLeft,
        ] {
            assert_eq!(
                powerline_solid_edge(bounds, shape, scale, 1.),
                Some(Bounds::from_corners(
                    point(px(12. / scale), bounds.top()),
                    point(px(13. / scale), bounds.bottom()),
                )),
                "{shape:?} must cover only the device pixel containing its left edge"
            );
        }

        for shape in [
            PowerlineShape::TriangleLeft,
            PowerlineShape::HalfCircleLeft,
            PowerlineShape::SlantLowerRight,
            PowerlineShape::SlantUpperRight,
        ] {
            assert_eq!(
                powerline_solid_edge(bounds, shape, scale, 1.),
                Some(Bounds::from_corners(
                    point(px(24. / scale), bounds.top()),
                    point(px(25. / scale), bounds.bottom()),
                )),
                "{shape:?} must cover only the device pixel containing its right edge"
            );
        }
    }

    #[test]
    fn powerline_solid_edge_skips_device_aligned_edges() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(9.), px(21.)));

        for shape in [
            PowerlineShape::TriangleRight,
            PowerlineShape::TriangleLeft,
            PowerlineShape::HalfCircleRight,
            PowerlineShape::HalfCircleLeft,
            PowerlineShape::SlantLowerLeft,
            PowerlineShape::SlantLowerRight,
            PowerlineShape::SlantUpperLeft,
            PowerlineShape::SlantUpperRight,
        ] {
            assert_eq!(
                powerline_solid_edge(bounds, shape, 2., 1.),
                None,
                "{shape:?}"
            );
        }
    }

    #[test]
    fn powerline_solid_edge_sanitizes_invalid_scale() {
        let bounds = Bounds::new(point(px(10.25), px(20.)), size(px(9.), px(21.)));
        let expected = Some(Bounds::from_corners(
            point(px(10.), bounds.top()),
            point(px(11.), bounds.bottom()),
        ));

        for scale in [0., -1., f32::NAN] {
            assert_eq!(
                powerline_solid_edge(bounds, PowerlineShape::TriangleRight, scale, 1.),
                expected
            );
        }
    }

    #[test]
    fn powerline_solid_edge_skips_translucent_separators() {
        let bounds = Bounds::new(point(px(10.2), px(20.)), size(px(9.2), px(21.)));

        for shape in [
            PowerlineShape::TriangleRight,
            PowerlineShape::TriangleLeft,
            PowerlineShape::HalfCircleRight,
            PowerlineShape::HalfCircleLeft,
            PowerlineShape::SlantLowerLeft,
            PowerlineShape::SlantLowerRight,
            PowerlineShape::SlantUpperLeft,
            PowerlineShape::SlantUpperRight,
        ] {
            assert!(
                powerline_solid_edge(bounds, shape, 1.25, 1.).is_some(),
                "{shape:?} still needs the cover quad when opaque"
            );
            for alpha in [DIM_OPACITY, 0., 0.99, f32::NAN] {
                assert_eq!(
                    powerline_solid_edge(bounds, shape, 1.25, alpha),
                    None,
                    "{shape:?} must not stack a cover quad under a translucent path"
                );
            }
        }
    }

    #[test]
    fn seg_budget_frees_solo_symbols_and_lends_a_cell_only_when_one_is_free() {
        let cell = px(10.);
        assert_eq!(
            seg_budget(true, true, 1, true, cell),
            px(20.),
            "a solo glyph leans into a free cell"
        );
        assert_eq!(
            seg_budget(true, true, 1, false, cell),
            px(10.),
            "but keeps to its own once the next cell is taken"
        );
        assert_eq!(
            seg_budget(false, true, 2, false, cell),
            px(22.5),
            "a quarter cell"
        );
        assert_eq!(
            seg_budget(false, true, 2, true, cell),
            px(30.),
            "a whole cell"
        );
        assert_eq!(seg_budget(false, true, 1, false, cell), px(12.5));
    }

    #[test]
    fn a_fallback_icon_is_fitted_to_its_cell_only_when_the_next_one_is_taken() {
        // Measured on Windows: Maple Mono NF CN supplying U+F059 to a
        // 15px Cascadia Mono grid inks 13.85px across an 8.79px cell.
        let cell = px(8.789);
        let ink = px(13.845);

        let leaning = fit_scale(ink, seg_budget(true, true, 1, true, cell));
        assert_eq!(leaning, 1., "a blank neighbour still lends its cell");

        let crowded = fit_scale(ink, seg_budget(true, true, 1, false, cell));
        assert!(crowded < 1., "an occupied neighbour does not");
        assert!(
            ink * crowded <= cell,
            "and the icon has to end inside its own cell"
        );
    }

    #[test]
    fn a_solo_segment_is_held_to_its_cell_only_where_its_ink_was_measured() {
        let cell = px(10.);
        let budget = |ink, text| seg_budget(true, ink_covers_segment(ink, text), 1, false, cell);

        // One character the face answered bounds for: the whole of it was
        // measured, so `fit_scale` can bring it into one cell and the clip
        // may be drawn there.
        assert!(ink_covers_segment(Some(px(15.)), "\u{f059}"));
        assert_eq!(budget(Some(px(15.)), "\u{f059}"), px(10.));

        // A base with a combining mark hanging off it reaches paint_glyphs as
        // a one-cell `Cluster`, which counts as solo. `ink_extent` read the
        // base alone: Devanagari ka plus the aa matra inks to the right of the
        // base, and none of that overhang is in the 9px. Nothing would shrink
        // it, so a one-cell clip would cut the matra clean off.
        assert!(!ink_covers_segment(Some(px(9.)), "\u{915}\u{93E}"));
        assert_eq!(budget(Some(px(9.)), "\u{915}\u{93E}"), px(20.));

        // A face that answers no bounds at all is the same story from the
        // other side: `fit_scale` is never reached, so halving the clip only
        // clips.
        assert!(!ink_covers_segment(None, "\u{f059}"));
        assert_eq!(budget(None, "\u{f059}"), px(20.));

        // A free neighbour lends its cell either way \u2014 the measurement only
        // decides whether the lean can be withdrawn.
        for ink in [Some(px(15.)), None] {
            for text in ["\u{f059}", "\u{915}\u{93E}"] {
                assert_eq!(
                    seg_budget(true, ink_covers_segment(ink, text), 1, true, cell),
                    px(20.)
                );
            }
        }
    }

    /// The emitted feature set is the whole contract with the shaper, so pin it
    /// tag for tag rather than asking after one tag at a time.
    ///
    /// `calt: 0` alone is not "ligatures off" on any platform: it never asks
    /// for `liga`/`clig`, which every shaper defaults on. Shaping "office
    /// waffle fluffier" in Calibri through the real DirectWrite shaper gives 15
    /// glyphs for 22 characters with `calt: 0`, and 22 once all three are named
    /// zero.
    #[test]
    fn build_font_emits_the_pinned_feature_set_for_each_ligature_setting() {
        fn tags(font: &Font) -> Vec<(&str, u32)> {
            font.features
                .tag_value_list()
                .iter()
                .map(|(tag, value)| (tag.as_str(), *value))
                .collect()
        }

        // Default config, and the settings toggle switched off: both leave
        // `font_features` unset, so the terminal names its own.
        let font = build_font(&gpui::font("Test"), false, false);
        assert_eq!(
            tags(&font),
            vec![("calt", 0), ("liga", 0), ("clig", 0)],
            "an unconfigured face must name every ligature feature off",
        );
        assert_eq!(font.features.is_calt_enabled(), Some(false));

        // Every face the grid paints with gets the same set, not just the plain one.
        for (bold, italic) in [(true, false), (false, true), (true, true)] {
            assert_eq!(
                tags(&build_font(&gpui::font("Test"), bold, italic)),
                vec![("calt", 0), ("liga", 0), ("clig", 0)],
                "bold={bold} italic={italic}",
            );
        }

        // Explicitly on: what Settings → Appearance → Font ligatures writes.
        let mut configured = gpui::font("Test");
        configured.features = crate::core::config::gpui_font_features(
            &serde_json::from_str(r#"{"calt":true,"liga":1}"#).unwrap(),
        );
        let font = build_font(&configured, false, false);
        assert_eq!(
            tags(&font),
            vec![("calt", 1), ("liga", 1)],
            "an explicit request for ligatures must survive untouched",
        );
        assert_eq!(font.features.is_calt_enabled(), Some(true));

        // Explicitly off in `config.json`: also passed through unchanged.
        let mut configured = gpui::font("Test");
        configured.features = crate::core::config::gpui_font_features(
            &serde_json::from_str(r#"{"calt":0,"liga":0,"clig":0}"#).unwrap(),
        );
        assert_eq!(
            tags(&build_font(&configured, false, false)),
            vec![("calt", 0), ("liga", 0), ("clig", 0)],
        );
    }

    #[test]
    fn natively_drawn_cells_keep_decorations_owned_by_text_shaping() {
        let plain = GlyphStyle::of(&cell('│'));
        assert_eq!(
            native_cell_residue(&plain),
            None,
            "an unstyled box character has nothing left to shape"
        );

        for kind in [UnderlineKind::Single, UnderlineKind::Curly] {
            let mut c = cell('│');
            c.underline = kind;
            assert_eq!(
                native_cell_residue(&GlyphStyle::of(&c)),
                Some(' '),
                "{kind:?} underline dropped on a box-drawing cell"
            );
        }

        for ch in ['│', '─', '╭', '█', '\u{e0b0}'] {
            let mut c = cell(ch);
            c.link_hover = true;
            assert_eq!(
                native_cell_residue(&GlyphStyle::of(&c)),
                Some(' '),
                "hovered-link underline dropped on U+{:04X}",
                ch as u32
            );
        }
    }

    #[test]
    fn segment_row_keeps_powerline_separators_solo() {
        let row = vec![cell('a'), cell('\u{e0b0}'), cell('\u{e0b4}'), cell('b')];
        assert_eq!(
            segment_row(&row),
            [
                run(0, 1, "a"),
                RowSeg::Solo { col: 1 },
                RowSeg::Solo { col: 2 },
                run(3, 1, "b")
            ]
        );
    }

    #[test]
    fn segment_row_gives_each_wide_glyph_its_own_segment() {
        let row = wide_cells("你好世界");
        assert_eq!(
            segment_row(&row),
            [
                wide(0, 2, "你"),
                wide(2, 2, "好"),
                wide(4, 2, "世"),
                wide(6, 2, "界")
            ]
        );
    }

    #[test]
    fn segment_row_isolates_narrow_fullwidth_punctuation() {
        let row = wide_cells("（这样");
        assert_eq!(
            segment_row(&row),
            [wide(0, 2, "（"), wide(2, 2, "这"), wide(4, 2, "样")]
        );
    }

    #[test]
    fn segment_row_tracks_wide_columns_across_styles_and_gaps() {
        let mut row = wide_cells("你好世界");
        row[4].fg = gpui::red();
        row[6].fg = gpui::red();
        assert_eq!(
            segment_row(&row),
            [
                wide(0, 2, "你"),
                wide(2, 2, "好"),
                wide(4, 2, "世"),
                wide(6, 2, "界")
            ]
        );

        let mut row = wide_cells("你好");
        row.insert(2, cell(' '));
        assert_eq!(segment_row(&row), [wide(0, 2, "你"), wide(3, 2, "好")]);
    }

    #[test]
    fn segment_row_leaves_spacerless_wide_char_solo() {
        let row = vec![cell('a'), cell('字')];
        assert_eq!(segment_row(&row), [run(0, 1, "a"), RowSeg::Solo { col: 1 }]);
    }

    #[test]
    fn segment_row_ignores_blank_rows() {
        let row: Vec<_> = "  \0 ".chars().map(cell).collect();
        assert!(segment_row(&row).is_empty());
    }

    fn cluster(col: usize, cells: usize, text: &str) -> RowSeg {
        RowSeg::Cluster {
            col,
            cells,
            text: text.to_string(),
            wide_base: false,
        }
    }

    fn wide_cluster(col: usize, cells: usize, text: &str) -> RowSeg {
        RowSeg::Cluster {
            col,
            cells,
            text: text.to_string(),
            wide_base: true,
        }
    }

    #[test]
    fn segment_row_shapes_combining_marks_with_their_base() {
        let mut row = vec![cell('a'), cell('e'), cell('b')];
        row[1].marks = Some(Box::from(['\u{0301}']));
        assert_eq!(
            segment_row(&row),
            [run(0, 1, "a"), cluster(1, 1, "e\u{0301}"), run(2, 1, "b"),]
        );

        let mut row = wide_cells("\u{2764}");
        row[0].marks = Some(Box::from(['\u{FE0F}']));
        assert_eq!(segment_row(&row), [wide_cluster(0, 2, "\u{2764}\u{FE0F}")]);

        let mut row = vec![cell('\u{0E17}'), cell('a')];
        row[0].marks = Some(Box::from(['\u{0E35}', '\u{0E48}']));
        assert_eq!(
            segment_row(&row),
            [cluster(0, 1, "\u{0E17}\u{0E35}\u{0E48}"), run(1, 1, "a")]
        );
    }

    #[test]
    fn segment_row_absorbs_sara_am_into_its_base() {
        let mut row = vec![cell('\u{0E19}'), cell('\u{0E33}'), cell('a')];
        row[0].marks = Some(Box::from(['\u{0E49}']));
        assert_eq!(
            segment_row(&row),
            [cluster(0, 2, "\u{0E19}\u{0E49}\u{0E33}"), run(2, 1, "a")]
        );

        let row = vec![cell('\u{0E01}'), cell('\u{0E33}')];
        assert_eq!(segment_row(&row), [cluster(0, 2, "\u{0E01}\u{0E33}")]);

        let row = vec![cell('\u{0E81}'), cell('\u{0EB3}')];
        assert_eq!(segment_row(&row), [cluster(0, 2, "\u{0E81}\u{0EB3}")]);

        let mut row = vec![cell('\u{0E01}'), cell('\u{0E33}')];
        row[1].fg = gpui::red();
        assert_eq!(segment_row(&row), [cluster(0, 2, "\u{0E01}\u{0E33}")]);
    }

    #[test]
    fn segment_row_leaves_a_baseless_sara_am_alone() {
        let row = vec![cell('\u{0E33}'), cell('a')];
        assert_eq!(segment_row(&row), [RowSeg::Solo { col: 0 }, run(1, 1, "a")]);

        let row = vec![cell(' '), cell('\u{0E33}')];
        assert_eq!(segment_row(&row), [RowSeg::Solo { col: 1 }]);

        let row = vec![cell('\u{0E33}'), cell('\u{0E33}')];
        assert_eq!(
            segment_row(&row),
            [RowSeg::Solo { col: 0 }, RowSeg::Solo { col: 1 }]
        );
    }

    #[test]
    fn segment_row_joins_a_regional_indicator_pair() {
        let row = vec![cell('\u{1F1E8}'), cell('\u{1F1F3}')];
        assert_eq!(segment_row(&row), [cluster(0, 2, "\u{1F1E8}\u{1F1F3}")]);

        let row = vec![cell('a'), cell('\u{1F1E8}'), cell('\u{1F1F3}'), cell('b')];
        assert_eq!(
            segment_row(&row),
            [
                run(0, 1, "a"),
                cluster(1, 2, "\u{1F1E8}\u{1F1F3}"),
                run(3, 1, "b"),
            ]
        );

        let row = vec![
            cell('\u{1F1E8}'),
            cell('\u{1F1F3}'),
            cell('\u{1F1FA}'),
            cell('\u{1F1F8}'),
        ];
        assert_eq!(
            segment_row(&row),
            [
                cluster(0, 2, "\u{1F1E8}\u{1F1F3}"),
                cluster(2, 2, "\u{1F1FA}\u{1F1F8}"),
            ]
        );

        let mut row = vec![cell('\u{1F1E8}'), cell('\u{1F1F3}')];
        row[1].fg = gpui::red();
        assert_eq!(segment_row(&row), [cluster(0, 2, "\u{1F1E8}\u{1F1F3}")]);

        let mut row = vec![cell('\u{1F1E8}'), cell('\u{1F1F3}')];
        row[0].marks = Some(Box::from(['\u{FE0F}']));
        assert_eq!(
            segment_row(&row),
            [cluster(0, 2, "\u{1F1E8}\u{FE0F}\u{1F1F3}")]
        );

        let mut row = vec![cell('\u{1F1E8}'), cell('\u{1F1F3}')];
        row[1].marks = Some(Box::from(['\u{FE0F}']));
        assert_eq!(
            segment_row(&row),
            [cluster(0, 2, "\u{1F1E8}\u{1F1F3}\u{FE0F}")]
        );
    }

    #[test]
    fn segment_row_leaves_an_unpaired_regional_indicator_alone() {
        let row = vec![cell('\u{1F1E8}')];
        assert_eq!(segment_row(&row), [RowSeg::Solo { col: 0 }]);

        let row = vec![cell('\u{1F1E8}'), cell('\u{1F1F3}'), cell('\u{1F1FA}')];
        assert_eq!(
            segment_row(&row),
            [cluster(0, 2, "\u{1F1E8}\u{1F1F3}"), RowSeg::Solo { col: 2 }]
        );

        let row = vec![cell('\u{1F1E8}'), cell('a')];
        assert_eq!(segment_row(&row), [RowSeg::Solo { col: 0 }, run(1, 1, "a")]);

        let row = vec![cell('\u{1F1E8}'), cell(' '), cell('\u{1F1F3}')];
        assert_eq!(
            segment_row(&row),
            [RowSeg::Solo { col: 0 }, RowSeg::Solo { col: 2 }]
        );
    }

    #[test]
    fn a_regional_indicator_pair_reaches_segment_row_as_two_plain_columns() {
        let mut term = alacritty_terminal::Term::new(
            alacritty_terminal::term::Config::default(),
            &crate::terminal::size::TermSize::new(80, 24),
            alacritty_terminal::event::VoidListener,
        );
        let mut parser: alacritty_terminal::vte::ansi::Processor =
            alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut term, "\u{1F1E8}\u{1F1F3}x".as_bytes());

        let palette = [Rgb { r: 0, g: 0, b: 0 }; 256];
        let colors = test_colors();
        let row: Vec<_> = (0..4)
            .map(|col| {
                let point = AlacPoint::new(AlacLine(0), AlacColumn(col));
                snapshot_cell(&term.grid()[point], point, &palette, &colors, None)
            })
            .collect();
        assert_eq!(
            segment_row(&row),
            [cluster(0, 2, "\u{1F1E8}\u{1F1F3}"), run(2, 1, "x")]
        );
    }

    #[test]
    fn segment_row_never_batches_a_marked_cell() {
        let mut row: Vec<_> = "abc".chars().map(cell).collect();
        row[1].marks = Some(Box::from(['\u{0301}']));
        assert_eq!(
            segment_row(&row),
            [run(0, 1, "a"), cluster(1, 1, "b\u{0301}"), run(2, 1, "c"),]
        );

        let mut row = wide_cells("你好世");
        row[2].marks = Some(Box::from(['\u{FE0F}']));
        assert_eq!(
            segment_row(&row),
            [
                wide(0, 2, "你"),
                wide_cluster(2, 2, "好\u{FE0F}"),
                wide(4, 2, "世"),
            ]
        );
    }

    #[test]
    fn segment_row_keeps_a_blank_that_carries_marks() {
        let mut row = vec![cell(' '), cell(' ')];
        row[0].marks = Some(Box::from(['\u{0301}']));
        assert_eq!(segment_row(&row), [cluster(0, 1, " \u{0301}")]);
    }

    #[test]
    fn char_string_memoizes_per_char() {
        let a = char_string('界');
        let b = char_string('界');
        assert_eq!(a, b);
        assert_eq!(a.as_ref(), "界");
    }

    fn test_colors() -> PaintColors {
        let fg = Rgb {
            r: 10,
            g: 10,
            b: 10,
        };
        let bg = Rgb {
            r: 250,
            g: 250,
            b: 245,
        };
        let wash = |a: f32| {
            let mut c = to_hsla(Rgb {
                r: 0x4a,
                g: 0x43,
                b: 0x39,
            });
            c.a = a;
            c
        };
        PaintColors {
            default_fg: to_hsla(fg),
            default_bg: to_hsla(bg),
            caret: to_hsla(fg),
            selection_bg: wash(0.55),
            match_bg: wash(0.32),
            current_match_bg: wash(0.85),
            fg_rgb: fg,
            bg_rgb: bg,
            legible_dim: true,
        }
    }

    #[test]
    fn selected_cells_keep_their_own_colors_for_the_translucent_wash() {
        let mut palette = [Rgb { r: 0, g: 0, b: 0 }; 256];
        palette[1] = Rgb {
            r: 0xcc,
            g: 0x22,
            b: 0x22,
        };
        palette[2] = Rgb {
            r: 0x22,
            g: 0x88,
            b: 0x22,
        };
        let colors = test_colors();
        let point = AlacPoint::new(AlacLine(0), AlacColumn(0));
        let range = SelectionRange::new(point, point, false);

        let swatch = Cell {
            bg: AnsiColor::Indexed(1),
            ..Cell::default()
        };
        let rc = snapshot_cell(&swatch, point, &palette, &colors, Some(&range));
        assert!(rc.selected);
        assert!(rc.draw_bg, "the swatch background still paints");
        assert_eq!(rc.bg, to_hsla(palette[1]), "background not replaced");

        let text = Cell {
            c: 'x',
            fg: AnsiColor::Indexed(2),
            ..Cell::default()
        };
        let rc = snapshot_cell(&text, point, &palette, &colors, Some(&range));
        assert!(rc.selected);
        assert_eq!(rc.fg, to_hsla(palette[2]), "foreground not forced");

        assert!(colors.selection_bg.a < 1.0);
    }

    #[test]
    fn inverse_swaps_colors_and_always_paints_the_background() {
        let mut palette = [Rgb { r: 0, g: 0, b: 0 }; 256];
        palette[2] = Rgb {
            r: 0x22,
            g: 0x88,
            b: 0x22,
        };
        let colors = test_colors();
        let point = AlacPoint::new(AlacLine(0), AlacColumn(0));

        let cell = Cell {
            c: 'x',
            fg: AnsiColor::Indexed(2),
            flags: Flags::INVERSE,
            ..Cell::default()
        };
        let rc = snapshot_cell(&cell, point, &palette, &colors, None);
        assert_eq!(rc.fg, to_hsla(colors.bg_rgb), "fg takes the old background");
        assert_eq!(rc.bg, to_hsla(palette[2]), "bg takes the old foreground");
        assert!(
            rc.draw_bg,
            "inverse cells paint their background even on the default bg"
        );
    }

    #[test]
    fn hidden_paints_the_foreground_as_the_background() {
        let mut palette = [Rgb { r: 0, g: 0, b: 0 }; 256];
        palette[1] = Rgb {
            r: 0xcc,
            g: 0x22,
            b: 0x22,
        };
        let colors = test_colors();
        let point = AlacPoint::new(AlacLine(0), AlacColumn(0));

        let on_default = Cell {
            c: 's',
            fg: AnsiColor::Indexed(1),
            flags: Flags::HIDDEN,
            ..Cell::default()
        };
        let rc = snapshot_cell(&on_default, point, &palette, &colors, None);
        assert_eq!(rc.fg, rc.bg, "concealed text is invisible");
        assert_eq!(rc.fg, to_hsla(colors.bg_rgb));
        assert!(!rc.draw_bg);

        let on_colored = Cell {
            c: 's',
            bg: AnsiColor::Indexed(1),
            flags: Flags::HIDDEN,
            ..Cell::default()
        };
        let rc = snapshot_cell(&on_colored, point, &palette, &colors, None);
        assert_eq!(rc.fg, to_hsla(palette[1]));
        assert_eq!(rc.fg, rc.bg);
    }

    #[test]
    fn dim_flag_reduces_foreground_intensity() {
        let palette = [Rgb { r: 0, g: 0, b: 0 }; 256];
        let colors = test_colors();
        let point = AlacPoint::new(AlacLine(0), AlacColumn(0));
        let cell = Cell {
            c: 'f',
            flags: Flags::DIM,
            ..Cell::default()
        };

        let rc = snapshot_cell(&cell, point, &palette, &colors, None);

        assert_eq!(rc.fg.h, colors.default_fg.h);
        assert_eq!(rc.fg.s, colors.default_fg.s);
        assert_eq!(rc.fg.l, colors.default_fg.l);
        assert!(
            rc.fg.a < colors.default_fg.a,
            "SGR 2 text must paint with reduced intensity"
        );
    }

    /// What a cell's foreground lands on screen as: its rgb composited over
    /// the cell background by its alpha, the way the glyph is actually drawn.
    fn on_screen(fg: Hsla, under: Rgb) -> u32 {
        let c = Rgba::from(fg);
        let ch = |v: f32, u: u8| ((v * c.a + (u as f32 / 255.) * (1. - c.a)) * 255.).round() as u32;
        ch(c.r, under.r) << 16 | ch(c.g, under.g) << 8 | ch(c.b, under.b)
    }

    /// The colours a pane on builtin `t` resolves cells with.
    fn builtin_colors(t: &crate::ui::presets::Theme) -> (PaintColors, [Rgb; 256]) {
        let (fg, bg) = (unpack_rgb(t.foreground), unpack_rgb(t.background_color()));
        let mut colors = test_colors();
        colors.default_fg = to_hsla(fg);
        colors.default_bg = to_hsla(bg);
        colors.fg_rgb = fg;
        colors.bg_rgb = bg;
        let mut palette = super::super::palette::build();
        palette[..16].copy_from_slice(&t.active_palette(true).ansi16);
        (colors, palette)
    }

    /// Every colour faint text is commonly written in: the default foreground,
    /// the sixteen palette slots, the 256-colour greys apps reach for as
    /// "muted", and a truecolour grey.
    fn faint_samples() -> Vec<(String, AnsiColor)> {
        let mut v = vec![("fg".to_string(), AnsiColor::Named(NamedColor::Foreground))];
        for i in 0..16u8 {
            v.push((format!("ansi{i}"), AnsiColor::Indexed(i)));
        }
        for i in [240u8, 244, 248, 250] {
            v.push((format!("256:{i}"), AnsiColor::Indexed(i)));
        }
        v.push((
            "#999999".to_string(),
            AnsiColor::Spec(Rgb {
                r: 153,
                g: 153,
                b: 153,
            }),
        ));
        v
    }

    /// (plain, faint) on-screen colours of `color` as a pane on `t` paints them.
    fn plain_and_faint(
        colors: &PaintColors,
        palette: &[Rgb; 256],
        color: AnsiColor,
    ) -> (RenderCell, RenderCell) {
        let point = AlacPoint::new(AlacLine(0), AlacColumn(0));
        let mut cell = Cell {
            c: 'x',
            fg: color,
            ..Cell::default()
        };
        let plain = snapshot_cell(&cell, point, palette, colors, None);
        cell.flags = Flags::DIM;
        let faint = snapshot_cell(&cell, point, palette, colors, None);
        (plain, faint)
    }

    #[test]
    fn faint_text_keeps_the_text_floor_on_every_light_builtin() {
        // #858: SGR 2 painted the ink at 66% over the cell, which on a light
        // background took Catppuccin Latte's foreground to 3.2:1 and every
        // bright-black the palette rescue had lifted to 4.5:1 back to ~2.5:1.
        // Faint text on a light cell must now clear 4.5:1 — or, for an ink
        // that never cleared it undimmed, stay at the ink's own ratio.
        use crate::ui::presets::{builtins, contrast};
        let mut failures = Vec::new();
        eprintln!("| theme | colour | plain | faint |");
        for t in builtins().into_iter().filter(|t| !t.dark) {
            let (colors, palette) = builtin_colors(&t);
            let bg = t.background_color();
            for (name, color) in faint_samples() {
                let (plain, faint) = plain_and_faint(&colors, &palette, color);
                let plain = contrast(on_screen(plain.fg, colors.bg_rgb), bg);
                let faint = contrast(on_screen(faint.fg, colors.bg_rgb), bg);
                eprintln!("| {} | {name} | {plain:.2} | {faint:.2} |", t.id);
                if faint < 4.5_f32.min(plain) - 0.05 {
                    failures.push(format!(
                        "{}/{name}: faint {faint:.2}:1 (plain {plain:.2}:1)",
                        t.id
                    ));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "faint text under the floor:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn faint_text_on_dark_builtins_is_the_plain_fade() {
        // The floor is a light-background rescue: every dark builtin must
        // keep painting SGR 2 exactly as it did, as the ink at DIM_OPACITY.
        for t in crate::ui::presets::builtins()
            .into_iter()
            .filter(|t| t.dark)
        {
            let (colors, palette) = builtin_colors(&t);
            for (name, color) in faint_samples() {
                let (plain, faint) = plain_and_faint(&colors, &palette, color);
                let mut fade = plain.fg;
                fade.a *= DIM_OPACITY;
                assert_eq!(
                    faint.fg, fade,
                    "{}/{name}: dark theme faint text changed",
                    t.id
                );
            }
        }
    }

    #[test]
    fn faint_text_stays_fainter_than_plain_text() {
        // The rescue buys legibility, not a restyle: faint text never gains
        // contrast over its own ink, and an ink with room to spare above the
        // floor still reads visibly fainter.
        use crate::ui::presets::{builtins, contrast};
        for t in builtins() {
            let (colors, palette) = builtin_colors(&t);
            let bg = t.background_color();
            for (name, color) in faint_samples() {
                let (plain, faint) = plain_and_faint(&colors, &palette, color);
                let plain = contrast(on_screen(plain.fg, colors.bg_rgb), bg);
                let faint = contrast(on_screen(faint.fg, colors.bg_rgb), bg);
                assert!(
                    faint <= plain + 0.01,
                    "{}/{name}: faint {faint:.2} > plain {plain:.2}",
                    t.id
                );
                if plain >= 6.0 {
                    assert!(
                        faint <= plain * 0.85,
                        "{}/{name}: faint {faint:.2} no longer reads fainter than {plain:.2}",
                        t.id
                    );
                }
            }
        }
    }

    #[test]
    fn faint_text_is_the_plain_fade_with_the_legibility_switch_off() {
        // Settings → Appearance's palette switch off renders colours as
        // authored; that includes faint text.
        for t in crate::ui::presets::builtins() {
            let (mut colors, palette) = builtin_colors(&t);
            colors.legible_dim = false;
            for (name, color) in faint_samples() {
                let (plain, faint) = plain_and_faint(&colors, &palette, color);
                let mut fade = plain.fg;
                fade.a *= DIM_OPACITY;
                assert_eq!(faint.fg, fade, "{}/{name}: switch off still rescued", t.id);
            }
        }
    }

    #[test]
    fn blend_toward_mixes_in_rgb_space_and_keeps_alpha() {
        let under = Rgba {
            r: 16. / 255.,
            g: 18. / 255.,
            b: 19. / 255.,
            a: 1.,
        };
        let red = to_hsla(Rgb {
            r: 218,
            g: 98,
            b: 125,
        });
        let half = Rgba::from(blend_toward(red, 0.5, under));
        assert!((half.r - 0.5 * (218. / 255.) - 0.5 * (16. / 255.)).abs() < 1e-3);
        assert!((half.g - 0.5 * (98. / 255.) - 0.5 * (18. / 255.)).abs() < 1e-3);
        assert!((half.b - 0.5 * (125. / 255.) - 0.5 * (19. / 255.)).abs() < 1e-3);
        assert_eq!(half.a, 1.0, "an opaque colour stays opaque");

        // A translucent tint keeps its own alpha: only the rgb is blended.
        let tint = Hsla { a: 0.24, ..red };
        let dimmed = Rgba::from(blend_toward(tint, 0.55, under));
        assert!((dimmed.a - 0.24).abs() < 1e-3, "alpha is untouched");
        assert!(
            (dimmed.r - 0.55 * (218. / 255.) - 0.45 * (16. / 255.)).abs() < 1e-3,
            "rgb still blends toward the under"
        );
    }

    #[test]
    fn dimming_a_stacked_fill_stays_continuous_with_its_segment() {
        // The reported bug: a powerline separator (fill) drawn over its
        // segment (bg) dimmed unevenly because the old pane element-opacity
        // alpha-multiplied each layer, so the separator kept the segment's own
        // dim visible through its (1 - dim) and landed with a seam against
        // the segment (brighter on this palette). Blending every colour
        // toward the under first keeps the composite exactly as dimmed as any
        // single layer.
        let under = Rgba {
            r: 16. / 255.,
            g: 18. / 255.,
            b: 19. / 255.,
            a: 1.,
        };
        let segment = to_hsla(Rgb {
            r: 218,
            g: 98,
            b: 125,
        });
        let fill = to_hsla(Rgb {
            r: 154,
            g: 52,
            b: 142,
        });
        let dim = 0.55;

        let over = |top: Rgba, bottom: Rgba| Rgba {
            r: top.r * top.a + bottom.r * (1. - top.a),
            g: top.g * top.a + bottom.g * (1. - top.a),
            b: top.b * top.a + bottom.b * (1. - top.a),
            a: top.a + bottom.a * (1. - top.a),
        };

        // New rendering: both layers are pre-blended and stay opaque, so the
        // fill over the segment composites to exactly the dimmed fill — the
        // same value the segment itself renders as, with no seam between them.
        let dimmed_segment = Rgba::from(blend_toward(segment, dim, under));
        let dimmed_fill = Rgba::from(blend_toward(fill, dim, under));
        let new_composite = over(dimmed_fill, dimmed_segment);
        assert!((new_composite.r - dimmed_fill.r).abs() < 1e-6);
        assert!((new_composite.g - dimmed_fill.g).abs() < 1e-6);
        assert!((new_composite.b - dimmed_fill.b).abs() < 1e-6);

        // Old rendering: the pane opacity turned both layers translucent, so
        // the fill showed the already-dimmed segment underneath and painted
        // with a seam against the segment next to it.
        let old_fill = Rgba {
            a: dim,
            ..Rgba::from(fill)
        };
        let old_segment = Rgba {
            a: dim,
            ..Rgba::from(segment)
        };
        let old_composite = over(old_fill, old_segment);
        assert!(
            old_composite.r > new_composite.r + 0.05,
            "on this palette the old alpha-multiplied separator was visibly brighter than the segment"
        );
    }

    #[test]
    fn dim_cell_blends_truecolor_cells_toward_the_under() {
        // Starship's prompt paints its segments with direct 38;2;/48;2; SGR
        // colours that a palette-level dim would never see; the pane dim must
        // reach those cells too, or the whole prompt would stay at full
        // brightness while indexed content around it dims.
        let under = Rgba {
            r: 16. / 255.,
            g: 18. / 255.,
            b: 19. / 255.,
            a: 1.,
        };
        let mut cell = RenderCell::default();
        cell.c = 'x';
        cell.fg = to_hsla(Rgb {
            r: 218,
            g: 98,
            b: 125,
        });
        cell.bg = to_hsla(Rgb {
            r: 154,
            g: 52,
            b: 142,
        });
        cell.draw_bg = true;

        let dimmed = dim_cell(cell, 0.55, under);
        assert_eq!(dimmed.c, 'x', "the character itself survives");
        assert!(dimmed.draw_bg, "the explicit background flag survives");
        let fg = Rgba::from(dimmed.fg);
        assert!((fg.r - (0.55 * 218. + 0.45 * 16.) / 255.).abs() < 1e-3);
        assert!((fg.g - (0.55 * 98. + 0.45 * 18.) / 255.).abs() < 1e-3);
        assert!((fg.b - (0.55 * 125. + 0.45 * 19.) / 255.).abs() < 1e-3);
        let bg = Rgba::from(dimmed.bg);
        assert!((bg.r - (0.55 * 154. + 0.45 * 16.) / 255.).abs() < 1e-3);
        assert_eq!(fg.a, 1.0, "opaque cell colours stay opaque");

        // A DIM-flagged cell keeps its reduced alpha through the blend.
        let mut dim_flag = RenderCell::default();
        dim_flag.fg = to_hsla(Rgb {
            r: 218,
            g: 98,
            b: 125,
        });
        dim_flag.fg.a = DIM_OPACITY;
        let dimmed_flag = dim_cell(dim_flag, 0.55, under);
        assert!(
            (dimmed_flag.fg.a - DIM_OPACITY).abs() < 1e-3,
            "the SGR dim alpha survives the pane dim"
        );
    }

    #[test]
    fn paint_colors_dimmed_keeps_translucent_tint_alphas() {
        let colors = caret_colors();
        let under = Rgba {
            r: 16. / 255.,
            g: 18. / 255.,
            b: 19. / 255.,
            a: 1.,
        };
        let dimmed = colors.dimmed(0.55, under);
        assert!((dimmed.caret.a - colors.caret.a).abs() < 1e-3);
        assert!(
            (dimmed.selection_bg.a - colors.selection_bg.a).abs() < 1e-3,
            "translucent overlays keep their own alpha so they still tint"
        );
        assert_ne!(dimmed.default_fg, colors.default_fg);
        // The named foreground/background rgb feed `resolve` for default cells,
        // which `dim_cell` blends — pre-blending them here would double-dim.
        assert_eq!(dimmed.fg_rgb, colors.fg_rgb);
        assert_eq!(dimmed.bg_rgb, colors.bg_rgb);
    }

    #[test]
    fn premultiplied_under_scales_rgb_by_the_window_opacity_and_stays_opaque() {
        // A dimmed pane blends toward the colour the workspace actually paints
        // behind it: the fill scaled by the window's own opacity (premultiplied
        // alpha), never toward a colour that includes the OS backdrop.
        let u = premultiplied(0xda_62_7d, 0.82);
        assert_eq!(u.a, 1.0, "the under must stay an opaque paint colour");
        assert!((u.r - 0.82 * (218. / 255.)).abs() < 1e-6);
        assert!((u.g - 0.82 * (98. / 255.)).abs() < 1e-6);
        assert!((u.b - 0.82 * (125. / 255.)).abs() < 1e-6);
        // A fully opaque window keeps the fill untouched.
        let opaque = premultiplied(0xda_62_7d, 1.0);
        assert!((opaque.r - 218. / 255.).abs() < 1e-6);
        assert!((opaque.g - 98. / 255.).abs() < 1e-6);
        assert!((opaque.b - 125. / 255.).abs() < 1e-6);
    }

    #[test]
    fn fill_under_maps_preset_fills_to_their_premultiplied_under() {
        use crate::ui::presets::Fill;

        // A solid fill is exactly the colour the workspace paints behind the
        // terminal, scaled by the window's own opacity.
        let solid = fill_under(&Fill::Solid(0xda_62_7d), Some(0.82));
        assert!((solid.r - 0.82 * (218. / 255.)).abs() < 1e-6);
        assert!((solid.g - 0.82 * (98. / 255.)).abs() < 1e-6);
        assert!((solid.b - 0.82 * (125. / 255.)).abs() < 1e-6);
        assert_eq!(solid.a, 1.0);

        // A gradient is approximated by its midpoint stop (`mix` rounds each
        // channel, so 0x00…ff lands on 128/255 rather than exactly 0.5).
        let vertical = fill_under(
            &Fill::Vertical {
                top: 0x00_00_00,
                bottom: 0xff_ff_ff,
            },
            None,
        );
        assert!((vertical.r - 128. / 255.).abs() < 1e-6);
        assert_eq!(vertical.r, vertical.g);
        assert_eq!(vertical.r, vertical.b);
        let horizontal = fill_under(
            &Fill::Horizontal {
                left: 0xff_00_00,
                right: 0x00_00_ff,
            },
            None,
        );
        assert!((horizontal.r - 128. / 255.).abs() < 1e-6);
        assert!((horizontal.b - 128. / 255.).abs() < 1e-6);

        // No explicit opacity means the fill is used at full strength.
        let full = fill_under(&Fill::Solid(0xda_62_7d), None);
        assert!((full.r - 218. / 255.).abs() < 1e-6);
    }

    #[test]
    fn theme_under_premultiplies_by_the_theme_background_alpha() {
        // The no-preset fallback mirrors `theme().background`, whose alpha is
        // the window's own opacity.
        let bg = to_hsla(Rgb {
            r: 218,
            g: 98,
            b: 125,
        });
        let mut bg = bg;
        bg.a = 0.82;
        let under = theme_under(bg);
        assert_eq!(under.a, 1.0);
        assert!((under.r - 0.82 * (218. / 255.)).abs() < 1e-6);
        assert!((under.g - 0.82 * (98. / 255.)).abs() < 1e-6);
        assert!((under.b - 0.82 * (125. / 255.)).abs() < 1e-6);
        // An opaque theme token is used untouched.
        let opaque = theme_under(to_hsla(Rgb {
            r: 218,
            g: 98,
            b: 125,
        }));
        assert!((opaque.r - 218. / 255.).abs() < 1e-6);
    }

    #[test]
    fn underline_flag_bits_map_to_their_variants() {
        let palette = [Rgb { r: 0, g: 0, b: 0 }; 256];
        let colors = test_colors();
        let point = AlacPoint::new(AlacLine(0), AlacColumn(0));
        let kind = |flags: Flags| {
            let cell = Cell {
                c: 'u',
                flags,
                ..Cell::default()
            };
            snapshot_cell(&cell, point, &palette, &colors, None).underline
        };

        assert_eq!(kind(Flags::empty()), UnderlineKind::None);
        assert_eq!(kind(Flags::UNDERLINE), UnderlineKind::Single);
        assert_eq!(kind(Flags::DOUBLE_UNDERLINE), UnderlineKind::Double);
        assert_eq!(kind(Flags::UNDERCURL), UnderlineKind::Curly);
        assert_eq!(kind(Flags::DOTTED_UNDERLINE), UnderlineKind::Dotted);
        assert_eq!(kind(Flags::DASHED_UNDERLINE), UnderlineKind::Dashed);
        assert_eq!(
            kind(Flags::UNDERLINE | Flags::UNDERCURL),
            UnderlineKind::Curly
        );
    }

    #[test]
    fn strikeout_flag_maps_to_a_text_decoration() {
        let palette = [Rgb { r: 0, g: 0, b: 0 }; 256];
        let colors = test_colors();
        let point = AlacPoint::new(AlacLine(0), AlacColumn(0));
        let cell = Cell {
            c: 's',
            flags: Flags::STRIKEOUT,
            ..Cell::default()
        };

        let rc = snapshot_cell(&cell, point, &palette, &colors, None);
        let style = GlyphStyle::of(&rc).strikethrough_style().unwrap();
        assert_eq!(style.thickness, px(1.));
        assert_eq!(style.color, Some(rc.fg));
    }

    #[test]
    fn special_underlines_have_distinct_device_pixel_geometry() {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(12.), px(20.)));
        let ink = |kind| {
            let mut rects = Vec::new();
            for_each_special_underline(bounds, kind, 2., |rect| rects.push(rect));
            rects
        };

        let double = ink(UnderlineKind::Double);
        assert_eq!(
            double.len(),
            2,
            "double underline uses two separate strokes"
        );
        assert_ne!(double[0].origin.y, double[1].origin.y);

        let dotted = ink(UnderlineKind::Dotted);
        let dashed = ink(UnderlineKind::Dashed);
        assert!(
            dotted.len() > dashed.len(),
            "dots repeat more often than dashes"
        );
        assert!(dotted.len() > 1 && dashed.len() > 1);
        assert!(
            dotted.iter().all(|r| r.size.width == px(1.)),
            "each dot is a logical pixel wide at 2x"
        );
        assert!(
            dashed.iter().all(|r| r.size.width <= px(3.)),
            "each dash is at most three logical pixels wide at 2x"
        );
    }

    #[test]
    fn a_link_is_pointed_out_faintly_until_the_modifier_is_down() {
        let mut plain = cell('l');
        plain.fg = gpui::hsla(0., 0., 0.9, 1.);

        assert_eq!(
            link_underline_color(&plain, true),
            None,
            "armed, the underline is the text's own colour"
        );
        let held = link_underline_color(&plain, false).expect("a colour of its own");
        assert!(
            held.a < plain.fg.a,
            "held back, it is the same colour with less of it"
        );
        assert_eq!(
            (held.h, held.s, held.l),
            (plain.fg.h, plain.fg.s, plain.fg.l)
        );

        let mut own = cell('l');
        own.underline = UnderlineKind::Curly;
        own.underline_color = Some(gpui::hsla(0.1, 1., 0.5, 1.));
        assert_eq!(
            link_underline_color(&own, false),
            own.underline_color,
            "a spelling mistake stays the colour the application painted it"
        );
    }

    #[test]
    fn special_underlines_are_not_overpainted_by_link_hover() {
        for kind in [
            UnderlineKind::Double,
            UnderlineKind::Dotted,
            UnderlineKind::Dashed,
        ] {
            let mut c = cell('l');
            c.underline = kind;
            c.link_hover = true;
            assert_eq!(GlyphStyle::of(&c).underline_style(), None, "{kind:?}");
        }
    }

    #[test]
    fn native_cells_keep_only_decorations_that_need_glyph_shaping() {
        for kind in [
            UnderlineKind::Double,
            UnderlineKind::Dotted,
            UnderlineKind::Dashed,
        ] {
            let mut c = cell('│');
            c.underline = kind;
            assert_eq!(native_cell_residue(&GlyphStyle::of(&c)), None);
        }

        let mut c = cell('│');
        c.strikeout = true;
        assert_eq!(native_cell_residue(&GlyphStyle::of(&c)), Some(' '));
    }

    #[test]
    fn sgr58_underline_color_resolves_through_the_palette() {
        let mut palette = [Rgb { r: 0, g: 0, b: 0 }; 256];
        palette[5] = Rgb {
            r: 0xd0,
            g: 0x30,
            b: 0x30,
        };
        let colors = test_colors();
        let point = AlacPoint::new(AlacLine(0), AlacColumn(0));

        let mut cell = Cell {
            c: 'e',
            flags: Flags::UNDERCURL,
            ..Cell::default()
        };
        cell.set_underline_color(Some(AnsiColor::Indexed(5)));
        let rc = snapshot_cell(&cell, point, &palette, &colors, None);
        assert_eq!(rc.underline_color, Some(to_hsla(palette[5])));

        let plain = Cell {
            c: 'e',
            flags: Flags::UNDERCURL,
            ..Cell::default()
        };
        let rc = snapshot_cell(&plain, point, &palette, &colors, None);
        assert_eq!(
            rc.underline_color, None,
            "no SGR 58 → paint falls back to fg"
        );
    }

    #[test]
    fn bold_italic_flag_sets_both_emphases() {
        let palette = [Rgb { r: 0, g: 0, b: 0 }; 256];
        let colors = test_colors();
        let point = AlacPoint::new(AlacLine(0), AlacColumn(0));
        let emphases = |flags: Flags| {
            let cell = Cell {
                c: 'b',
                flags,
                ..Cell::default()
            };
            let rc = snapshot_cell(&cell, point, &palette, &colors, None);
            (rc.bold, rc.italic)
        };

        assert_eq!(emphases(Flags::BOLD), (true, false));
        assert_eq!(emphases(Flags::ITALIC), (false, true));
        assert_eq!(emphases(Flags::BOLD_ITALIC), (true, true));
    }

    #[test]
    fn wide_char_spacers_defer_to_the_leading_cell() {
        let mut palette = [Rgb { r: 0, g: 0, b: 0 }; 256];
        palette[1] = Rgb {
            r: 0xcc,
            g: 0x22,
            b: 0x22,
        };
        let colors = test_colors();
        let point = AlacPoint::new(AlacLine(0), AlacColumn(0));

        for flags in [Flags::WIDE_CHAR_SPACER, Flags::LEADING_WIDE_CHAR_SPACER] {
            let cell = Cell {
                c: ' ',
                bg: AnsiColor::Indexed(1),
                flags: flags | Flags::UNDERLINE,
                ..Cell::default()
            };
            let rc = snapshot_cell(&cell, point, &palette, &colors, None);
            assert!(rc.spacer);
            assert!(!rc.draw_bg, "spacers never paint");
            assert_eq!(rc.underline, UnderlineKind::None);
        }
    }

    #[test]
    fn only_real_combining_marks_set_marks() {
        let palette = [Rgb { r: 0, g: 0, b: 0 }; 256];
        let colors = test_colors();
        let point = AlacPoint::new(AlacLine(0), AlacColumn(0));

        let mut colored = Cell {
            c: 'e',
            flags: Flags::UNDERCURL,
            ..Cell::default()
        };
        colored.set_underline_color(Some(AnsiColor::Indexed(5)));
        assert_eq!(
            snapshot_cell(&colored, point, &palette, &colors, None).marks,
            None,
            "SGR 58 alone is not a combining mark"
        );

        let mut linked = Cell {
            c: 'e',
            ..Cell::default()
        };
        linked.set_hyperlink(Some(alacritty_terminal::term::cell::Hyperlink::new(
            Some("id"),
            "https://example.com".to_string(),
        )));
        assert_eq!(
            snapshot_cell(&linked, point, &palette, &colors, None).marks,
            None,
            "OSC 8 alone is not a combining mark"
        );

        let mut marked = Cell {
            c: '\u{2764}',
            ..Cell::default()
        };
        marked.push_zerowidth('\u{FE0F}');
        assert_eq!(
            snapshot_cell(&marked, point, &palette, &colors, None)
                .marks
                .as_deref(),
            Some(&['\u{FE0F}'][..]),
        );
    }

    fn ink(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    /// Menlo at 22px: a 13.2px cell in a 30.8px row (line height 1.4).
    const CELL: f32 = 13.2;
    const EM: f32 = 22.;

    fn fit(ink: Bounds<Pixels>, cells: usize, lend: bool) -> (f32, usize) {
        icon_fit(ink, px(CELL), cells, lend, px(EM))
    }

    #[test]
    fn only_private_use_code_points_count_as_icons() {
        for c in [
            '\u{e000}',   // first of the BMP Private Use Area
            '\u{e0a0}',   // Powerline branch
            '\u{e0d8}',   // first past the Powerline separators
            '\u{f179}',   // Font Awesome apple
            '\u{f8ff}',   // last of the BMP Private Use Area
            '\u{f0001}',  // Material Design icon, Supplementary PUA-A
            '\u{10fffd}', // end of Supplementary PUA-B
        ] {
            assert!(is_icon(c), "U+{:04X}", c as u32);
        }
        // Text a fallback face supplies keeps its own metrics: CJK, emoji,
        // Latin, and the drawing characters painted natively anyway.
        for c in [
            '中',
            'あ',
            '한',
            '\u{1F600}',
            '\u{2764}',
            'M',
            '─',
            '█',
            '\u{f900}',
        ] {
            assert!(!is_icon(c), "U+{:04X}", c as u32);
        }
        // Nor do the Powerline separators, which are shapes, not icons.
        for c in ['\u{e0b0}', '\u{e0b1}', '\u{e0b3}', '\u{e0d7}'] {
            assert!(!is_icon(c), "U+{:04X}", c as u32);
        }
    }

    #[test]
    fn a_nerd_font_icon_before_a_space_takes_two_cells_at_full_size() {
        // Symbols Nerd Font Mono's apple, as CoreText measures it at 22px: a
        // full em tall and 1.4 cells wide. Held to one cell it is shrunk to
        // 0.71 of the text's size, which is what #866 saw; with the space
        // after it lent, it keeps its whole em.
        let apple = ink(1.76, -4.43, 18.48, 22.03);
        let (held, cells) = fit(apple, 1, false);
        assert_eq!(cells, 1);
        assert!((apple.size.width.as_f32() * held - CELL).abs() < 1e-3);
        let (lent, cells) = fit(apple, 1, true);
        assert_eq!(cells, 2);
        assert!((apple.size.height.as_f32() * lent - EM).abs() < 1e-3);
        assert!(apple.size.width.as_f32() * lent <= 2. * CELL);
    }

    #[test]
    fn a_narrow_icon_keeps_to_its_own_cell_even_when_the_next_is_free() {
        // The branch symbol is an em tall and 0.4em wide: one cell already
        // holds it at full height, so borrowing would only push it off
        // towards the text after it.
        let branch = ink(6.52, -4.4, 8.96, 22.);
        assert_eq!(fit(branch, 1, true), (1., 1));
    }

    #[test]
    fn a_small_icon_from_a_narrow_face_grows_to_fill_its_cell() {
        // A face with a narrower cell than the primary inks well under it.
        let icon = ink(0.5, -1., 8.8, 9.);
        let (scale, cells) = fit(icon, 1, false);
        assert_eq!(cells, 1);
        assert!(scale > 1.);
        assert!((icon.size.width.as_f32() * scale - CELL).abs() < 1e-3);
        // The same icon two cells wide may use both.
        let (two, cells) = fit(icon, 2, false);
        assert_eq!(cells, 2);
        assert!(two > scale);
    }

    #[test]
    fn a_fitted_icon_is_centred_in_its_room() {
        let icon = ink(0.4, -0.6, 5.3, 6.);
        let room = size(px(CELL), px(30.8));
        let (scale, _) = fit(icon, 1, false);
        let (ascent, descent) = (px(20.4 * scale), px(5.2 * scale));
        let at = icon_offset(icon, scale, room, true, ascent, descent);
        // Spilling into a borrowed cell, it stays where its face put it,
        // unless that would take its ink past either edge of the room.
        assert_eq!(
            icon_offset(icon, scale, room, false, ascent, descent).x,
            px(0.)
        );
        let lent = size(px(2. * CELL), px(30.8));
        let overhang = ink(-1., 0., 20., 20.);
        assert_eq!(
            icon_offset(overhang, 1., lent, false, ascent, descent).x,
            px(1.)
        );
        let pushed = ink(8., 0., 20., 20.);
        let x = icon_offset(pushed, 1., lent, false, ascent, descent).x;
        assert!((x.as_f32() - (2. * CELL - 28.)).abs() < 1e-4, "{x:?}");
        let left = at.x.as_f32() + icon.origin.x.as_f32() * scale;
        let right = left + icon.size.width.as_f32() * scale;
        assert!((left - (CELL - right)).abs() < 1e-3, "{left}..{right}");
        let baseline = at.y + (room.height - ascent - descent) / 2. + ascent;
        let top = (baseline - (icon.origin.y + icon.size.height) * scale).as_f32();
        let bottom = (baseline - icon.origin.y * scale).as_f32();
        assert!((top - (30.8 - bottom)).abs() < 1e-3, "{top}..{bottom}");
    }

    #[test]
    fn icon_fit_leaves_what_is_already_the_right_size_alone() {
        // Within 2% of the cell: not worth shaping the glyph again.
        assert_eq!(fit(ink(0., 0., 13., 13.), 1, false), (1., 1));
        // Nothing measured, nothing to scale by.
        assert_eq!(fit(ink(0., 0., 0., 0.), 1, true), (1., 1));
        // A dot is small by design and grows no more than the cap.
        assert_eq!(fit(ink(1., 2., 2., 2.), 1, false).0, MAX_ICON_GROWTH);
    }

    fn icon_row(next: RenderCell) -> Vec<RenderCell> {
        let icon = RenderCell {
            c: '\u{f179}',
            ..RenderCell::default()
        };
        vec![icon, next]
    }

    #[test]
    fn a_blank_in_the_same_colour_lends_an_icon_its_cell() {
        let blank = RenderCell::default();
        assert!(lends_to_icon(&icon_row(blank.clone()), 0, 1));
        // Whatever `has_room_after` lends, the end of the row included.
        assert!(lends_to_icon(&icon_row(blank.clone())[..1], 0, 1));
        // A prompt segment: icon and the space after it on one background.
        let bg = Hsla::blue();
        let mut row = icon_row(RenderCell {
            bg,
            draw_bg: true,
            ..RenderCell::default()
        });
        row[0].bg = bg;
        row[0].draw_bg = true;
        assert!(lends_to_icon(&row, 0, 1));
        // The next segment's colour, a glyph or a selection do not.
        row[1].bg = Hsla::red();
        assert!(!lends_to_icon(&row, 0, 1));
        let letter = RenderCell {
            c: 'x',
            ..RenderCell::default()
        };
        assert!(!lends_to_icon(&icon_row(letter), 0, 1));
        let selected = RenderCell {
            selected: true,
            ..RenderCell::default()
        };
        assert!(!lends_to_icon(&icon_row(selected), 0, 1));
        // Nor does a coloured blank after an icon on the default background.
        let coloured = RenderCell {
            bg,
            draw_bg: true,
            ..blank
        };
        assert!(!lends_to_icon(&icon_row(coloured), 0, 1));
    }
}

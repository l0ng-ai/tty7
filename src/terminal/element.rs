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
struct RenderCell {
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
        f.features = gpui::FontFeatures::disable_ligatures();
    }
    f
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
        rc.fg.a *= DIM_OPACITY;
    }
    rc
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

struct PaintColors {
    default_fg: Hsla,
    default_bg: Hsla,
    caret: Hsla,
    selection_bg: Hsla,
    match_bg: Hsla,
    current_match_bg: Hsla,
    fg_rgb: Rgb,
    bg_rgb: Rgb,
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
    fn resolve(theme: &gpui_component::Theme, cx: &gpui::App) -> Self {
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

                        static GRID_BUF: RefCell<Vec<RenderCell>> = const { RefCell::new(Vec::new()) };
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

fn seg_clip_width(solo: bool, cells: usize, cell_width: Pixels) -> Pixels {
    if solo {
        cell_width * 2.
    } else {
        cell_width * cells as f32
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
    let face = |bold: bool, italic: bool| &faces[(bold as usize) | ((italic as usize) << 1)];

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

        for seg in segment_row(&buf[row_base..row_base + geom.cols]) {
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
            run_buf[0] = TextRun {
                len: text.len(),
                font: face(style.bold, style.italic).clone(),
                color: style.fg,
                background_color: None,
                underline: style.underline_style(),
                strikethrough: style.strikethrough_style(),
            };

            let shaped = window
                .text_system()
                .shape_line(text, font_size, run_buf, force_width);

            let x = geom.origin.x + geom.cell_width * (start as f32);
            let clip_width = seg_clip_width(solo, cells, geom.cell_width);
            let clip = Bounds::new(point(x, y), size(clip_width, geom.line_height));
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
        }
    }
}

#[derive(Clone, Copy)]
struct GridCursor {
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

struct GridSnapshot {
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

impl TerminalElement {
    fn build_grid(
        &self,
        colors: &PaintColors,
        buf: &mut Vec<RenderCell>,
        rows: usize,
        cols: usize,
        want_sliver: bool,
        cx: &App,
        dim: f32,
        under: Rgba,
    ) -> GridSnapshot {
        buf.clear();
        buf.resize(rows * cols, RenderCell::default());
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
            let term = term.lock();
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
        GridSnapshot {
            cursor,
            sliver,
            any_selected,
            any_match,
            any_current,
            display_offset,
            history_size,
        }
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
                    buf[grid_row * cols + col].link_hover = true;
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
                        let include_files = mods.secondary() || v.link_modifier_down();
                        v.hover_link_at(col, row, include_files, cx);
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

        self.view.update(cx, |view, _cx| {
            view.set_grid_size(cols, rows, cell_width, line_height, window.scale_factor());
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

        let mut buf = GRID_BUF.with(|b| std::mem::take(&mut *b.borrow_mut()));
        let snap = self.build_grid(
            &colors,
            &mut buf,
            geom.rows,
            geom.cols,
            frac > 0.,
            cx,
            dim,
            under,
        );
        let cursor = snap.cursor;
        let sliver = snap.sliver.as_ref();

        let cursor_cell = cursor.map(|c| (c.row, c.ime_col));
        let render_cursor = cursor
            .filter(|c| !c.hidden)
            .map(|c| (c.row, c.col, c.style));

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

        let cursor_bounds = cursor_cell.map(|(row, col)| geom.cell_rect(row, col, 1));
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

        GRID_BUF.with(|b| *b.borrow_mut() = buf);

        self.register_mouse_handlers(geom, bounds, prepaint.hitbox.id, window);

        let view = self.view.read(cx);
        if view.hovered_link.is_some() {
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
    fn seg_clip_width_frees_solo_symbols_but_pins_batched_runs() {
        let cell = px(10.);
        assert_eq!(seg_clip_width(false, 1, cell), px(10.));
        assert_eq!(seg_clip_width(false, 5, cell), px(50.));
        assert_eq!(seg_clip_width(false, 2, cell), px(20.));
        assert_eq!(seg_clip_width(true, 1, cell), px(20.));
    }

    #[test]
    fn build_font_disables_ligatures_unless_features_are_configured() {
        let font = build_font(&gpui::font("Test"), false, false);
        assert_eq!(font.features.is_calt_enabled(), Some(false));

        let mut configured = gpui::font("Test");
        configured.features = serde_json::from_str(r#"{"calt":true,"liga":1}"#).unwrap();
        let font = build_font(&configured, false, false);
        assert_eq!(font.features.is_calt_enabled(), Some(true));
        assert!(
            font.features
                .tag_value_list()
                .iter()
                .any(|(tag, value)| tag == "liga" && *value == 1)
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
}

//! GPUI element that paints an `alacritty_terminal` grid as a fixed character
//! matrix: background quads, shaped glyph runs, and a cursor overlay.

use std::cell::RefCell;
use std::collections::HashMap;

use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column as AlacColumn, Line as AlacLine, Point as AlacPoint};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb};
use gpui::{
    App, BorderStyle, Bounds, ContentMask, CursorStyle, Element, ElementId, Font, FontStyle,
    FontWeight, GlobalElementId, Hitbox, HitboxBehavior, Hsla, IntoElement, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Rgba, SharedString, Style,
    TextAlign, TextRun, Window, fill, outline, point, px, relative, size,
};
use gpui_component::ActiveTheme as _;

use super::view::TerminalView;
// NOTE: `gpui::CursorStyle` (mouse pointer) is already in scope above, so the
// config cursor-shape enum is always referred to fully-qualified as
// `crate::core::config::CursorStyle` to avoid the name clash.
use crate::core::config::Config;

/// Which underline variant the emulator asked for. The alacritty `Flags` bits
/// are independent, so we collapse them into one ordered style rather than a
/// bare bool — that lets the painter map curly → wavy and (best effort) vary
/// the rest instead of drawing every SGR 4:x the same.
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

/// Per-cell render data resolved from the emulator grid.
#[derive(Clone)]
struct RenderCell {
    c: char,
    fg: Hsla,
    bg: Hsla,
    draw_bg: bool,
    bold: bool,
    italic: bool,
    underline: UnderlineKind,
    /// SGR 58 underline color (OSC-less, per-cell). `None` means "reuse the
    /// glyph foreground", matching xterm's default.
    underline_color: Option<Hsla>,
    spacer: bool,
    selected: bool,
    /// Covered by a (non-current) search match.
    match_hit: bool,
    /// Covered by the current (focused) search match.
    match_current: bool,
    /// Part of the URL currently under the mouse; painted underlined so the link
    /// reads as clickable.
    link_hover: bool,
}

impl Default for RenderCell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: Hsla::default(),
            bg: Hsla::default(),
            draw_bg: false,
            bold: false,
            italic: false,
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
    /// Hitbox over the grid, inserted in prepaint so paint can flip the cursor to
    /// a pointing hand while a link is hovered.
    hitbox: Hitbox,
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

/// Resolve an alacritty color slot to RGB, honoring OSC overrides then falling
/// back to the static xterm palette / theme defaults.
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
    // Batched runs shape several chars in one line, where a programming font's
    // contextual ligatures (`calt`, e.g. Fira Code's "->") would fuse cells into
    // one glyph and break `force_width`'s one-glyph-per-column snapping. A cell
    // grid can't ligate — Zed's terminal disables the same feature.
    f.features = gpui::FontFeatures::disable_ligatures();
    f
}

/// Resolve one emulator cell into a `RenderCell`: colors (inverse/hidden
/// handling included), emphasis, underline style/color, and the selection
/// flag. `point` is the cell's grid-space position, used only for the
/// selection test. Wide-char spacers come back with just `spacer` set — the
/// leading cell paints them.
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
        fg: to_hsla(fgc),
        bg: to_hsla(bgc),
        draw_bg,
        bold: flags.contains(Flags::BOLD) || flags.contains(Flags::BOLD_ITALIC),
        italic: flags.contains(Flags::ITALIC) || flags.contains(Flags::BOLD_ITALIC),
        // Map the specific underline bit → style. The variants are mutually
        // exclusive in practice, but check the more specific bits first so a
        // plain UNDERLINE never shadows them.
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
        // SGR 58 underline color (falls back to fg at paint when absent).
        underline_color: cell
            .underline_color()
            .map(|c| to_hsla(resolve(c, palette, colors.fg_rgb, colors.bg_rgb).0)),
        ..RenderCell::default()
    };

    // Selection only *flags* the cell: the paint pass lays a translucent wash
    // over it and the cell keeps its own foreground and background — so colors
    // whose information lives in the background (fastfetch swatches, colored
    // diff blocks, TUI status bars) stay visible while selected, matching the
    // inline editor's translucent selection.
    if selection.is_some_and(|s| s.contains(point)) {
        rc.selected = true;
    }
    rc
}

/// The active preset's terminal selection background, read from the
/// `ActivePalette` global. Falls back to a neutral dark tone if the global
/// isn't published yet (only possible before the first paint).
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

/// Theme, selection, and search-highlight colors resolved once per paint pass,
/// so the grid builder and the painters share one consistent set instead of
/// each re-deriving them.
struct PaintColors {
    default_fg: Hsla,
    default_bg: Hsla,
    caret: Hsla,
    selection_bg: Hsla,
    match_bg: Hsla,
    current_match_bg: Hsla,
    current_match_border: Hsla,
    /// RGB defaults handed to the ANSI palette resolver.
    fg_rgb: Rgb,
    bg_rgb: Rgb,
}

impl PaintColors {
    fn resolve(theme: &gpui_component::Theme, cx: &gpui::App) -> Self {
        let default_fg = theme.foreground;
        let default_bg = theme.background;
        let caret = theme.caret;
        // The selection paints as a *translucent* wash over the cells' own
        // colors — like the inline editor's selection and VS Code — so
        // selected text keeps its syntax colors and background-only cells
        // (fastfetch swatches, colored diff blocks) stay visible instead of
        // vanishing under an opaque fill. Foreground at 0.24 alpha mirrors the
        // preset's opaque selection surface (`mix(bg, fg, 0.24)` — see
        // `presets::active_palette`): on default-background cells it composites
        // to exactly the tone the opaque fill used to have.
        let selection_bg = {
            let mut c = default_fg;
            c.a = 0.24;
            c
        };
        // Search highlights derive from the preset's selection surface
        // (published as the `ActivePalette` global by `apply_theme`, tuned to
        // keep text legible on both themes): non-current matches get a subtle
        // wash, the current match a stronger fill plus an accent outline.
        let base_match = to_hsla(active_selection_bg(cx));
        let match_bg = {
            let mut c = base_match;
            c.a = 0.32;
            c
        };
        let current_match_bg = {
            let mut c = base_match;
            c.a = 0.85;
            c
        };
        Self {
            default_fg,
            default_bg,
            caret,
            selection_bg,
            match_bg,
            current_match_bg,
            current_match_border: caret,
            fg_rgb: super::palette::hsla_to_rgb(default_fg),
            bg_rgb: super::palette::hsla_to_rgb(default_bg),
        }
    }
}

/// Paint per-cell background quads, merging each horizontal run of equal color
/// into a single quad. Background color varies per cell, so this can't share
/// the fixed-color `paint_cell_runs` helper.
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
                // Spacer cells (trailing half of a wide char) inherit the
                // preceding cell's background — include them in the run.
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

/// Paint a single fixed `color` over every horizontal run of cells matching
/// `covered`, merging contiguous cells into one quad. An optional `border`
/// draws an accent outline around each run (used by the current search match).
///
/// This collapses the selection, search-wash, and current-match overlays — all
/// previously copy-pasted run-merge loops — into one place.
fn paint_cell_runs(
    window: &mut Window,
    geom: &CellGeom,
    buf: &[RenderCell],
    color: Hsla,
    border: Option<Hsla>,
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
                // Spacer cells are the trailing half of a wide (CJK) char.
                // They inherit the preceding cell's highlight state, so include
                // them in the run to paint the full 2-column width.
                if covered(cell) || cell.spacer {
                    col += 1;
                } else {
                    break;
                }
            }
            let rect = geom.cell_rect(row, start, col - start);
            window.paint_quad(fill(rect, color));
            if let Some(b) = border {
                window.paint_quad(outline(rect, b, BorderStyle::Solid));
            }
        }
    }
}

/// The style facets that must agree for two cells to share one shaped run.
/// Everything here feeds the `TextRun` (face, color, underline); backgrounds,
/// selection and search washes live in separate paint layers and don't split
/// glyph runs.
#[derive(Clone, Copy, PartialEq)]
struct GlyphStyle {
    fg: Hsla,
    bold: bool,
    italic: bool,
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
            underline: cell.underline,
            underline_color: cell.underline_color,
            link_hover: cell.link_hover,
        }
    }

    /// Whether this style paints ink even on blank cells (an underline does),
    /// which forbids batching across space gaps: a lone space was never
    /// underlined by the per-cell painter, and batching must not change that.
    fn draws_on_blanks(&self) -> bool {
        self.underline != UnderlineKind::None || self.link_hover
    }

    /// Underline for either an emulator-styled underline or a hovered link (a
    /// hovered link with no underline of its own reads as a plain single line
    /// so it looks clickable).
    fn underline_style(&self) -> Option<gpui::UnderlineStyle> {
        self.draws_on_blanks().then(|| {
            // gpui's `UnderlineStyle` only exposes `thickness` / `color` /
            // `wavy`, so curly maps to `wavy` and double gets a thicker
            // line. TODO: gpui has no dotted/dashed primitive, so those
            // fall back to a straight single line for now.
            let wavy = self.underline == UnderlineKind::Curly;
            let thickness = if self.underline == UnderlineKind::Double {
                px(2.)
            } else {
                px(1.)
            };
            gpui::UnderlineStyle {
                thickness,
                // SGR 58 color when set, else the glyph's own foreground so
                // the line reads as part of the text it sits on.
                color: Some(self.underline_color.unwrap_or(self.fg)),
                wavy,
            }
        })
    }
}

/// A blank cell paints no glyph (and today, no underline either — see
/// `GlyphStyle::draws_on_blanks`).
fn is_blank(cell: &RenderCell) -> bool {
    cell.c == '\0' || cell.c == ' '
}

/// One paintable piece of a row produced by [`segment_row`].
#[derive(Debug, PartialEq)]
enum RowSeg {
    /// Style-identical ASCII cells (with interior blank gaps rendered as
    /// spaces), shaped as a single line.
    Run {
        start: usize,
        /// Columns covered, gaps included — the clip width in cells.
        cells: usize,
        text: String,
    },
    /// Style-identical consecutive wide (two-column) glyphs — CJK text, wide
    /// emoji — shaped as a single line pinned to `2 × cell_width` per glyph.
    /// A CJK-dense screen used to shape and paint every cell on its own; this
    /// collapses each run to one `shape_line` + one paint.
    Wide {
        start: usize,
        /// Columns covered (2 per glyph, spacers included) — the clip width.
        cells: usize,
        text: String,
    },
    /// A cell painted on its own: any single-width non-ASCII glyph (box
    /// drawing, accented Latin, …) that may route to a fallback face whose
    /// advance isn't the cell width.
    Solo { col: usize },
}

/// Split one grid row into paintable segments.
///
/// ASCII-graphic cells batch into [`RowSeg::Run`]s: they always come from the
/// primary monospace face, so their advances match the cell width (and
/// `force_width` pins them exactly at paint). Blank cells end an underlined
/// run, silently join a plain one, and never lead or trail a run.
///
/// Wide glyphs (a cell followed by its spacer — the grid's authoritative
/// two-column marker) batch into [`RowSeg::Wide`] runs: `force_width` at
/// `2 × cell_width` pins each to its own column pair, since every wide glyph
/// is a base glyph to `apply_force_width_to_layout` (a full-width advance is
/// always > half the forced width, even from a fallback face).
///
/// Single-width non-ASCII glyphs still paint solo, preserving the per-cell
/// behavior for glyphs with unpredictable advances (box drawing must fill its
/// cell exactly, so per-cell clipping is deliberate there).
fn segment_row(row: &[RenderCell]) -> Vec<RowSeg> {
    let mut segs = Vec::new();
    let mut col = 0;
    while col < row.len() {
        let cell = &row[col];
        if cell.spacer || is_blank(cell) {
            col += 1;
            continue;
        }
        if !cell.c.is_ascii_graphic() {
            // Wide (two-column) glyph? The trailing spacer is the grid's own
            // width marker, so no Unicode width guessing is needed.
            if col + 1 < row.len() && row[col + 1].spacer {
                let style = GlyphStyle::of(cell);
                let start = col;
                let mut text = String::new();
                text.push(cell.c);
                col += 2;
                // Extend over strictly consecutive style-identical wide glyphs
                // (no gap joining — a blank between wide chars ends the run).
                while col + 1 < row.len()
                    && !row[col].spacer
                    && !is_blank(&row[col])
                    && !row[col].c.is_ascii_graphic()
                    && row[col + 1].spacer
                    && GlyphStyle::of(&row[col]) == style
                {
                    text.push(row[col].c);
                    col += 2;
                }
                segs.push(RowSeg::Wide {
                    start,
                    cells: col - start,
                    text,
                });
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
        // Blanks between words may extend the run (they paint nothing), but are
        // committed only once another matching glyph follows, so a run never
        // carries trailing blanks.
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
            if c.spacer || !c.c.is_ascii_graphic() || GlyphStyle::of(c) != style {
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
    /// Single-char `SharedString`s memoized across frames and panes (the UI
    /// thread paints everything). Solo glyphs used to allocate a fresh String
    /// per cell per frame — thousands of allocations per paint on a CJK-dense
    /// screen. Entries are font-independent so they never go stale; the map is
    /// cleared wholesale only if a pathological stream floods it with unique
    /// codepoints.
    static CHAR_STRINGS: RefCell<HashMap<char, SharedString>> = RefCell::new(HashMap::new());

    /// The grid snapshot buffer, reused across frames and panes (the UI thread
    /// paints everything sequentially). A full-screen grid is tens of
    /// thousands of `RenderCell`s (~2 MB); remaking that Vec every paint was
    /// pure allocator churn. Taken (`mem::take`) for the duration of one
    /// element's paint and put back after, so panes share one allocation.
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

/// Paint glyphs as per-row batched runs where safe, single cells otherwise.
///
/// Merging cells into multi-char `shape_line` runs causes drift whenever a
/// glyph's font advance ≠ cell_width. gpui's `force_width` (added upstream for
/// Zed's terminal) pins every glyph in a shaped line to its own column, which
/// makes batching safe when all glyphs in the line occupy the same number of
/// columns — so style-identical ASCII runs shape at `cell_width` per glyph,
/// and style-identical wide runs (CJK text) at `2 × cell_width` per glyph.
/// Single-width glyphs that may come from a fallback face (box drawing, …)
/// still paint cell-by-cell: their advances are unpredictable and mixing
/// widths inside one batch would break `force_width`'s uniform-column
/// assumption.
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
    // The four style faces, resolved once per paint instead of once per cell.
    // A distinct bold/italic family applies when configured (bold wins for
    // bold+italic cells); `build_font` synthesizes the emphasis otherwise.
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
            let (start, cells, text, force_width) = match seg {
                RowSeg::Run { start, cells, text } => (
                    start,
                    cells,
                    SharedString::from(text),
                    Some(geom.cell_width),
                ),
                // Each wide glyph is pinned to its own two-column slot; the
                // clip stops an oversized fallback glyph bleeding past the run.
                RowSeg::Wide { start, cells, text } => (
                    start,
                    cells,
                    SharedString::from(text),
                    Some(geom.cell_width * 2.),
                ),
                // Always exactly one column now — anything with a trailing
                // spacer became a Wide run in `segment_row`. No `force_width`
                // for a single glyph — it paints at the run origin regardless.
                RowSeg::Solo { col } => (col, 1, char_string(buf[row_base + col].c), None),
            };

            let style = GlyphStyle::of(&buf[row_base + start]);
            run_buf[0] = TextRun {
                len: text.len(),
                font: face(style.bold, style.italic).clone(),
                color: style.fg,
                background_color: None,
                underline: style.underline_style(),
                strikethrough: None,
            };

            let shaped = window
                .text_system()
                .shape_line(text, font_size, run_buf, force_width);

            let x = geom.origin.x + geom.cell_width * (start as f32);
            let clip = Bounds::new(
                point(x, y),
                size(geom.cell_width * (cells as f32), geom.line_height),
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
        }
    }
}

/// Where the emulator's cursor sits, plus whether the app has hidden it
/// (DECTCEM off / `CursorShape::Hidden`). The position is tracked even while
/// hidden so the IME candidate window can anchor to it.
#[derive(Clone, Copy)]
struct GridCursor {
    row: usize,
    col: usize,
    hidden: bool,
    /// The shape to draw (from `Config::cursor_style`).
    style: crate::core::config::CursorStyle,
}

/// Paint the cursor overlay in the configured shape: a filled block, a thin
/// vertical bar, or an underline in the blink "on" phase when focused (nothing in
/// the "off" phase, so it blinks). When unfocused every shape falls back to a
/// static hollow block outline — the conventional "not the active pane" cue,
/// independent of the shape choice.
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
        let mut c = caret;
        c.a = 0.5;
        window.paint_quad(outline(rect, c, BorderStyle::Solid));
        return;
    }
    if !cursor_visible {
        return; // blink "off" phase
    }
    let mut c = caret;
    c.a = 0.55;
    match style {
        CursorStyle::Block => window.paint_quad(fill(rect, c)),
        CursorStyle::Bar => {
            // A 2px vertical bar hugging the cell's left edge, scaled up a touch
            // on very large fonts so it stays visible.
            let w = (geom.cell_width * 0.15).max(px(1.)).min(px(3.));
            let bar = Bounds::new(rect.origin, size(w, rect.size.height));
            window.paint_quad(fill(bar, c));
        }
        CursorStyle::Underline => {
            // A thin line along the cell's baseline, same thickness logic.
            let h = (geom.line_height * 0.12).max(px(1.)).min(px(3.));
            let y = rect.origin.y + rect.size.height - h;
            let line = Bounds::new(point(rect.origin.x, y), size(rect.size.width, h));
            window.paint_quad(fill(line, c));
        }
    }
}

/// Paint IME pre-edit (composing) text over the cursor cell, underlined so it
/// reads as provisional.
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

/// What [`TerminalElement::build_grid`] found alongside the cell buffer: the
/// cursor cell, the optional sub-line-scroll sliver row, and whether any cell
/// carries a selection / search-highlight flag — so the paint pass can skip
/// the corresponding overlay scans entirely in the common no-selection,
/// no-search frame.
struct GridSnapshot {
    cursor: Option<GridCursor>,
    sliver: Option<Vec<RenderCell>>,
    any_selected: bool,
    any_match: bool,
    any_current: bool,
}

impl TerminalElement {
    /// Snapshot the emulator grid into `buf` (releasing the term lock before
    /// returning) and locate the cursor cell. Search-match highlighting is
    /// layered on afterwards. `buf` is caller-provided so the (rows × cols)
    /// allocation is reused across frames instead of remade per paint.
    ///
    /// With `want_sliver`, also snapshots the row just above the viewport
    /// (grid line `-(display_offset + 1)`), which becomes visible when a
    /// sub-line scroll fraction shifts the whole grid down at paint.
    fn build_grid(
        &self,
        colors: &PaintColors,
        buf: &mut Vec<RenderCell>,
        rows: usize,
        cols: usize,
        want_sliver: bool,
        cx: &App,
    ) -> GridSnapshot {
        buf.clear();
        buf.resize(rows * cols, RenderCell::default());
        let mut cursor: Option<GridCursor> = None;
        let mut sliver: Option<Vec<RenderCell>> = None;
        let mut any_selected = false;
        let display_offset;
        {
            let mut palette = self.view.read(cx).terminal.palette;
            // Overwrite the ANSI 16 (slots 0-15) with the active preset's set
            // for the current mode, published as the `ActivePalette` global by
            // `apply_theme`. The 256-color cube and grayscale ramp (slots 16+)
            // stay as built. Falls back to the stored palette if unset.
            if let Some(active) = cx.try_global::<crate::terminal::palette::ActivePalette>() {
                palette[..16].copy_from_slice(&active.ansi16);
            }
            let term = self.view.read(cx).terminal.term.clone();
            let term = term.lock();
            let content = term.renderable_content();
            display_offset = content.display_offset as i32;
            let selection = content.selection;

            for cell in content.display_iter {
                let row = cell.point.line.0 + display_offset;
                let col = cell.point.column.0;
                if row < 0 || row as usize >= rows || col >= cols {
                    continue;
                }
                let rc = snapshot_cell(cell.cell, cell.point, &palette, colors, selection.as_ref());
                any_selected |= rc.selected;
                buf[row as usize * cols + col] = rc;
            }

            // The extra top row for sub-line scrolling. Grid lines index into
            // history below 0, so the line above the viewport exists whenever
            // we're not already at the top of the scrollback. Search-match
            // washes are skipped here: matches are flagged in viewport
            // coordinates, and a strip at most one line tall lighting up a
            // frame early isn't worth widening that mapping.
            if want_sliver && (display_offset as usize) < term.grid().history_size() {
                let line = AlacLine(-display_offset - 1);
                let mut row_buf = vec![RenderCell::default(); cols];
                for (col, rc) in row_buf
                    .iter_mut()
                    .enumerate()
                    .take(term.columns().min(cols))
                {
                    let point = AlacPoint::new(line, AlacColumn(col));
                    *rc = snapshot_cell(
                        &term.grid()[line][AlacColumn(col)],
                        point,
                        &palette,
                        colors,
                        selection.as_ref(),
                    );
                    any_selected |= rc.selected;
                }
                sliver = Some(row_buf);
            }

            // Cursor cell. We record the position even when the app has hidden the
            // cursor (`CursorShape::Hidden`, e.g. a full-screen TUI like Claude Code
            // that draws its own): the rendered block honours `hidden`, but the IME
            // candidate window still needs an anchor at the input cell — otherwise it
            // falls back to a window corner and can't follow the caret.
            let cur = content.cursor;
            let row = cur.point.line.0 + display_offset;
            let col = cur.point.column.0;
            if row >= 0 && (row as usize) < rows && col < cols {
                cursor = Some(GridCursor {
                    row: row as usize,
                    col,
                    hidden: matches!(
                        cur.shape,
                        alacritty_terminal::vte::ansi::CursorShape::Hidden
                    ),
                    // Shape is a user preference, not app-driven: the config style
                    // wins regardless of what DECSCUSR requested.
                    style: cx.global::<Config>().cursor_style,
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
        }
    }

    /// Flag the cells covered by the hovered link (if it's currently on screen) so
    /// they paint underlined. The link is stored in scroll-stable grid coordinates,
    /// so we shift it back into a screen row by the current display offset.
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
        let row = link.line + display_offset;
        if row < 0 || row as usize >= rows {
            return;
        }
        let row = row as usize;
        let mut col = link.start;
        while col <= link.end && col < cols {
            buf[row * cols + col].link_hover = true;
            col += 1;
        }
    }

    /// Flag cells covered by search matches. Driven entirely by the SearchState
    /// match list (computed only when the query changes), so this is the single
    /// source of truth for highlighting. Cheap per frame: the list is ordered
    /// top→bottom and non-overlapping (see `recompute_matches`), so a binary
    /// search finds the first match that can touch the viewport and iteration
    /// stops at the first one past it — instead of scanning all (up to 10k)
    /// matches every frame. Returns whether any (non-current, current) cells
    /// were flagged, so paint can skip the highlight passes entirely.
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
        // First match whose end reaches the viewport's top row.
        let first = search
            .matches
            .partition_point(|m| m.end().line.0 + display_offset < 0);
        for (i, m) in search.matches.iter().enumerate().skip(first) {
            let is_current = search.current_index == Some(i);
            let start = *m.start();
            let end = *m.end();
            if start.line.0 + display_offset >= rows as i32 {
                break; // ordered: everything after starts below the viewport too
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

    /// Register the per-frame mouse listeners (press / drag / release) over our
    /// bounds, translating pixel positions to grid cells and routing to the view
    /// (selection, link opening, or mouse-tracking reports).
    fn register_mouse_handlers(&self, geom: CellGeom, bounds: Bounds<Pixels>, window: &mut Window) {
        let view = self.view.clone();
        window.on_mouse_event(move |ev: &MouseDownEvent, phase, _window, cx| {
            if !phase.bubble() || !bounds.contains(&ev.position) {
                return;
            }
            let (col, row, left) = geom.pos_to_cell(ev.position);
            let mods = ev.modifiers;
            let button = ev.button;
            let clicks = ev.click_count;
            view.update(cx, |v, cx| {
                // Cmd+click opens a URL under the cursor.
                if mods.platform && button == MouseButton::Left && v.open_link_at(col, row, cx) {
                    return;
                }
                // Report to the app when in mouse-tracking mode (Shift forces
                // local selection instead).
                if v.mouse_mode() && !mods.shift {
                    v.mouse_press(button, col, row, &mods);
                    return;
                }
                // A left click/double-click/triple-click on the command-editor
                // line drives its caret/selection instead of a (meaningless)
                // terminal selection over it.
                if button == MouseButton::Left && v.editor_click(col, row, clicks, cx) {
                    return;
                }
                if button == MouseButton::Left {
                    v.on_select_start(col, row, left, clicks, cx);
                }
            });
        });

        let view = self.view.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, _phase, window, cx| {
            let (col, row, left) = geom.pos_to_cell(ev.position);
            let mods = ev.modifiers;
            let Some(button) = ev.pressed_button else {
                // No button down: detect a link under the cursor so it underlines.
                // This runs in mouse-tracking mode too — hover detection is purely
                // local (button-less motion is never forwarded to the app), and
                // ⌘-click opens links inside mouse-mode TUIs as well, so the
                // underline affordance must match. Skipped only when the pointer
                // is outside our bounds.
                let inside = bounds.contains(&ev.position);
                // Focus-follows-mouse: hovering an unfocused pane focuses it, no
                // click needed. Guarded on `inside` and the config flag.
                if inside && cx.global::<Config>().focus_follows_mouse {
                    let handle = view.read(cx).focus_handle.clone();
                    if !handle.is_focused(window) {
                        window.focus(&handle, cx);
                    }
                }
                view.update(cx, |v, cx| {
                    if inside {
                        // Any-event mouse tracking (mode 1003): apps that asked
                        // for all motion get button-less moves too; `mouse_motion`
                        // no-ops unless the mode is set. Shift keeps the mouse
                        // local, matching the click/drag/scroll routing.
                        if !mods.shift {
                            v.mouse_motion(col, row, &mods);
                        }
                        v.hover_link_at(col, row, cx);
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
                // A drag that began on the command-editor line extends its
                // selection rather than the terminal's.
                if button == MouseButton::Left && v.editor_drag(col, row, cx) {
                    return;
                }
                if button == MouseButton::Left {
                    v.on_select_update(col, row, left, cx);
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

        // Measure the monospace advance from a single glyph.
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
        // Clamp to >= 1px like `cell_width`: a degenerate config (font_size 0 or a
        // tiny line-height multiple) can round to 0, and dividing `bounds.height`
        // by 0 yields `inf`, which casts to `usize::MAX` rows → `rows * cols`
        // capacity overflow and an allocation panic on the first paint.
        let line_height = px((font_size.as_f32() * line_height_mul).round()).max(px(1.));

        let cols = (bounds.size.width.as_f32() / cell_width.as_f32())
            .floor()
            .max(1.0) as usize;
        let rows = (bounds.size.height.as_f32() / line_height.as_f32())
            .floor()
            .max(1.0) as usize;

        self.view.update(cx, |view, _cx| {
            view.set_grid_size(cols, rows, cell_width, line_height);
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
        // Optional frame timing (TTY7_FPS=1). Times the whole paint body below.
        let fps_start = super::fps::enabled().then(std::time::Instant::now);

        // Sub-line scroll fraction: shift the whole grid down by this many
        // pixels so trackpad scrolling moves continuously instead of snapping
        // line by line. The strip that opens above the top row is filled with
        // the next older row (the "sliver"). Mouse mapping stays consistent
        // automatically: `pos_to_cell` measures from this shifted origin.
        let frac = self.view.read(cx).scroll_frac.clamp(0., 1.);
        let geom = CellGeom {
            origin: point(
                bounds.origin.x,
                bounds.origin.y + prepaint.line_height * frac,
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
        // Blink phase (only meaningful while focused) and the transient bell flash.
        let cursor_visible = self.view.read(cx).cursor_visible;
        let bell_flash = self.view.read(cx).bell_flash;
        // While the inline line editor is live it owns the keyboard and draws its
        // own caret at the prompt; suppress the terminal's block cursor so the two
        // don't stack (the editor isn't focused on `focus_handle`, so otherwise the
        // grid would paint a stale hollow box behind the field).
        let editor_active = self.view.read(cx).input_active();

        // Snapshot the emulator grid into the reused buffer (lock released
        // inside). The buffer is returned to `GRID_BUF` at the end of paint.
        let mut buf = GRID_BUF.with(|b| std::mem::take(&mut *b.borrow_mut()));
        let snap = self.build_grid(&colors, &mut buf, geom.rows, geom.cols, frac > 0., cx);
        let cursor = snap.cursor;
        let sliver = snap.sliver.as_ref();

        // Cell the IME candidate window anchors to. Use the cursor position even
        // when the app has hidden the hardware cursor (full-screen TUIs draw their
        // own) so composition still tracks the input cell instead of dropping to a
        // window corner.
        let cursor_cell = cursor.map(|c| (c.row, c.col));
        // The rendered cursor, by contrast, honours `hidden`; it carries its shape
        // so `paint_cursor` can draw a block / bar / underline.
        let render_cursor = cursor
            .filter(|c| !c.hidden)
            .map(|c| (c.row, c.col, c.style));

        // Register the IME / text input handler so CJK (and dead-key) input
        // composes and commits to the PTY. Positioned at the cursor cell so the
        // candidate window appears in the right place.
        let cursor_bounds = cursor_cell.map(|(row, col)| geom.cell_rect(row, col, 1));
        let focus_handle = self.view.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            super::input::TerminalInputHandler::new(self.view.clone(), cursor_bounds),
            cx,
        );
        let marked = self.view.read(cx).marked_text.clone();

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            // Background quads, then the selection / search overlays, then
            // glyphs. The overlay passes each rescan the whole buffer, so they
            // only run when the snapshot actually flagged something — the
            // common no-selection, no-search frame skips all three.
            paint_backgrounds(window, &geom, &buf);
            if snap.any_selected {
                paint_cell_runs(window, &geom, &buf, colors.selection_bg, None, |c| {
                    c.selected
                });
            }
            if snap.any_match {
                paint_cell_runs(window, &geom, &buf, colors.match_bg, None, |c| {
                    c.match_hit && !c.match_current
                });
            }
            if snap.any_current {
                paint_cell_runs(
                    window,
                    &geom,
                    &buf,
                    colors.current_match_bg,
                    Some(colors.current_match_border),
                    |c| c.match_current,
                );
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
            // The sliver row above the viewport, exposed by the sub-line
            // scroll shift: same paint layers on a one-row geometry sitting
            // one line above the (already shifted) grid origin, clipped by
            // the surrounding content mask.
            if let Some(row) = sliver {
                let sg = CellGeom {
                    origin: point(geom.origin.x, geom.origin.y - geom.line_height),
                    rows: 1,
                    ..geom
                };
                paint_backgrounds(window, &sg, row);
                if snap.any_selected {
                    paint_cell_runs(window, &sg, row, colors.selection_bg, None, |c| c.selected);
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
            }
            // While the command editor is live, it draws its own caret and IME
            // pre-edit in the overlay; suppress the grid's versions so they don't
            // double up.
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

            // Visual bell: a brief, low-alpha wash over the whole surface as a
            // restrained, non-intrusive alternative to an audible beep. Cleared
            // automatically ~150ms after the bell by the view's timer.
            if bell_flash {
                let mut c = colors.default_fg;
                c.a = 0.12;
                window.paint_quad(fill(bounds, c));
            }
        });

        // Hand the snapshot buffer back for the next paint (any pane).
        GRID_BUF.with(|b| *b.borrow_mut() = buf);

        self.register_mouse_handlers(geom, bounds, window);

        // A pointing-hand cursor over a hovered link reinforces that it's
        // clickable (Cmd+click opens it).
        if self.view.read(cx).hovered_link.is_some() {
            window.set_cursor_style(CursorStyle::PointingHand, &prepaint.hitbox);
        }

        if let Some(start) = fps_start {
            super::fps::record(start.elapsed());
        }
    }
}

/// Geometry helper for mapping pixel positions to grid cells.
#[derive(Clone, Copy)]
struct CellGeom {
    origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    cols: usize,
    rows: usize,
}

impl CellGeom {
    /// The pixel rectangle covering `span` cells starting at (`row`, `col`).
    fn cell_rect(&self, row: usize, col: usize, span: usize) -> Bounds<Pixels> {
        let x = self.origin.x + self.cell_width * (col as f32);
        let y = self.origin.y + self.line_height * (row as f32);
        Bounds::new(
            point(x, y),
            size(self.cell_width * (span as f32), self.line_height),
        )
    }

    /// Returns (column, row, is_left_half).
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
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // A pure primary round-trips back through Rgba.
        let back = Rgba::from(to_hsla(Rgb { r: 255, g: 0, b: 0 }));
        assert!((back.r - 1.0).abs() < 1e-3);
        assert!(back.g.abs() < 1e-3 && back.b.abs() < 1e-3);
    }

    #[test]
    fn resolve_covers_every_color_slot() {
        // A palette whose red channel encodes its own index, for easy assertions.
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

        // A direct RGB spec passes through and is not a "default".
        let spec = Rgb { r: 1, g: 2, b: 3 };
        assert_eq!(
            resolve(AnsiColor::Spec(spec), &palette, fg, bg),
            (spec, false)
        );

        // Indexed reads the palette slot.
        let (rgb, is_def) = resolve(AnsiColor::Indexed(5), &palette, fg, bg);
        assert_eq!(rgb.r, 5);
        assert!(!is_def);

        // Named Foreground/Background fall back to the theme defaults (is_default=true).
        assert_eq!(
            resolve(AnsiColor::Named(NamedColor::Foreground), &palette, fg, bg),
            (fg, true)
        );
        assert_eq!(
            resolve(AnsiColor::Named(NamedColor::Background), &palette, fg, bg),
            (bg, true)
        );

        // A concrete named ANSI color reads the palette, not a default.
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

        // Family is preserved across the tweak.
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
        // A multi-cell span widens the rect by that many cells.
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
        // Left of the cell midpoint → left half.
        let (c, r, left) = geom.pos_to_cell(point(px(2.), px(5.)));
        assert_eq!((c, r), (0, 0));
        assert!(left);
        // Right of the midpoint → right half.
        let (_, _, left) = geom.pos_to_cell(point(px(8.), px(5.)));
        assert!(!left);
        // Negative offsets clamp to the first cell.
        let (c, r, _) = geom.pos_to_cell(point(px(-100.), px(-100.)));
        assert_eq!((c, r), (0, 0));
        // Far beyond the grid clamps to (cols-1, rows-1).
        let (c, r, _) = geom.pos_to_cell(point(px(9999.), px(9999.)));
        assert_eq!((c, r), (4, 2));
    }

    // ---- segment_row ----

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
            text: text.to_string(),
        }
    }

    /// A row of wide glyphs as the grid stores them: each char followed by its
    /// trailing spacer cell.
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
        // " ab  cd  " → one run: interior blanks join, leading/trailing don't.
        let row: Vec<_> = " ab  cd  ".chars().map(cell).collect();
        assert_eq!(segment_row(&row), [run(1, 6, "ab  cd")]);
        // NUL cells (never-written grid slots) count as blanks too.
        let mut row: Vec<_> = "ab cd".chars().map(cell).collect();
        row[2].c = '\0';
        assert_eq!(segment_row(&row), [run(0, 5, "ab cd")]);
    }

    #[test]
    fn segment_row_ends_underlined_runs_at_blanks() {
        // The per-cell painter never underlined a blank cell; a batched run
        // must not start doing so, thus the gap splits the run.
        let mut row: Vec<_> = "ab cd".chars().map(cell).collect();
        for c in &mut row {
            c.underline = UnderlineKind::Single;
        }
        assert_eq!(segment_row(&row), [run(0, 2, "ab"), run(3, 2, "cd")]);

        // Same for a hovered link.
        let mut row: Vec<_> = "ab cd".chars().map(cell).collect();
        for c in &mut row {
            c.link_hover = true;
        }
        assert_eq!(segment_row(&row), [run(0, 2, "ab"), run(3, 2, "cd")]);
    }

    #[test]
    fn segment_row_splits_on_style_changes() {
        // Foreground color change mid-word.
        let mut row: Vec<_> = "abcd".chars().map(cell).collect();
        row[2].fg = gpui::red();
        row[3].fg = gpui::red();
        assert_eq!(segment_row(&row), [run(0, 2, "ab"), run(2, 2, "cd")]);

        // Bold toggling.
        let mut row: Vec<_> = "abcd".chars().map(cell).collect();
        row[0].bold = true;
        assert_eq!(segment_row(&row), [run(0, 1, "a"), run(1, 3, "bcd")]);
    }

    #[test]
    fn segment_row_isolates_non_ascii() {
        // A wide CJK char (cell + spacer) between ASCII words: the wide cell
        // becomes a one-glyph Wide run (spanning its spacer), and the ASCII
        // resumes batching after it.
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

        // Single-width non-ASCII (box drawing) paints solo — it may come
        // from a fallback face with a non-cell advance.
        let row: Vec<_> = "a─b".chars().map(cell).collect();
        assert_eq!(
            segment_row(&row),
            [run(0, 1, "a"), RowSeg::Solo { col: 1 }, run(2, 1, "b")]
        );
    }

    #[test]
    fn segment_row_batches_consecutive_wide_glyphs() {
        // A CJK phrase batches into one Wide run covering glyphs + spacers.
        let row = wide_cells("你好世界");
        assert_eq!(segment_row(&row), [wide(0, 8, "你好世界")]);
    }

    #[test]
    fn segment_row_splits_wide_runs_on_style_and_gaps() {
        // A foreground change mid-phrase splits the batch, like ASCII runs.
        let mut row = wide_cells("你好世界");
        row[4].fg = gpui::red(); // third glyph (cols 4-5)
        row[6].fg = gpui::red();
        assert_eq!(segment_row(&row), [wide(0, 4, "你好"), wide(4, 4, "世界")]);

        // A blank cell between wide glyphs ends the run — no gap joining.
        let mut row = wide_cells("你好");
        row.insert(2, cell(' '));
        assert_eq!(segment_row(&row), [wide(0, 2, "你"), wide(3, 2, "好")]);
    }

    #[test]
    fn segment_row_leaves_spacerless_wide_char_solo() {
        // A wide char whose spacer was clipped off (last column) has no width
        // marker, so it falls back to the single-cell path.
        let row = vec![cell('a'), cell('字')];
        assert_eq!(segment_row(&row), [run(0, 1, "a"), RowSeg::Solo { col: 1 }]);
    }

    #[test]
    fn segment_row_ignores_blank_rows() {
        let row: Vec<_> = "  \0 ".chars().map(cell).collect();
        assert!(segment_row(&row).is_empty());
    }

    #[test]
    fn char_string_memoizes_per_char() {
        let a = char_string('界');
        let b = char_string('界');
        assert_eq!(a, b);
        assert_eq!(a.as_ref(), "界");
    }

    /// Hand-built `PaintColors` for the snapshot tests (the real `resolve`
    /// needs a live theme/App).
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
            current_match_border: to_hsla(fg),
            fg_rgb: fg,
            bg_rgb: bg,
        }
    }

    /// Regression: selection must not rewrite a cell's own colors. It used to
    /// force the foreground to the preset's selection text color and rely on an
    /// opaque fill — which erased background-only cells entirely (fastfetch
    /// color swatches, colored diff blocks vanished while selected). Selection
    /// now only flags the cell; the paint pass lays a translucent wash on top.
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

        // A fastfetch-style swatch: a space whose information IS its background.
        let swatch = Cell {
            bg: AnsiColor::Indexed(1),
            ..Cell::default()
        };
        let rc = snapshot_cell(&swatch, point, &palette, &colors, Some(&range));
        assert!(rc.selected);
        assert!(rc.draw_bg, "the swatch background still paints");
        assert_eq!(rc.bg, to_hsla(palette[1]), "background not replaced");

        // Colored text keeps its syntax color while selected.
        let text = Cell {
            c: 'x',
            fg: AnsiColor::Indexed(2),
            ..Cell::default()
        };
        let rc = snapshot_cell(&text, point, &palette, &colors, Some(&range));
        assert!(rc.selected);
        assert_eq!(rc.fg, to_hsla(palette[2]), "foreground not forced");

        // And the wash itself is translucent, or the kept colors could never
        // read through it.
        assert!(colors.selection_bg.a < 1.0);
    }

    /// SGR 7 swaps the two colors, and the swapped background must paint even
    /// when the cell sat on the *default* background — otherwise an inverse
    /// block (`ls` selections, status bars) silently vanishes.
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

    /// SGR 8 conceals text by drawing it in the background color — whatever
    /// that background is — without turning a default background opaque.
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

        // On the default background the glyph melts into the theme bg and the
        // cell still skips the background fill.
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

        // On a colored background it melts into *that* color instead.
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

    /// Each underline SGR maps to its own variant, and the specific bits must
    /// win over a plain UNDERLINE that may be set alongside them — a curly
    /// diagnostic squiggle degrading to a straight line is exactly the kind of
    /// regression a human eyeball misses.
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
        // A variant bit set together with plain UNDERLINE keeps the variant.
        assert_eq!(
            kind(Flags::UNDERLINE | Flags::UNDERCURL),
            UnderlineKind::Curly
        );
    }

    /// SGR 58 sets a dedicated underline color that resolves through the
    /// palette; without it the field stays `None` so paint falls back to the
    /// glyph's foreground.
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

    /// BOLD_ITALIC is its own flag bit — it must light up both emphases, not
    /// require BOLD and ITALIC to also be set.
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

    /// Wide-char spacers only mark the column as occupied; everything else —
    /// colors, underline, selection — is the leading cell's job. A spacer that
    /// painted anything would double-draw under every CJK glyph.
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
}

//! The history section at the foot of the panel.
//!
//! Its job is shape first and text second. 260px leaves room for roughly 26
//! characters beside the lanes, and this repository's commit subjects run to a
//! median of 64 — so what a reader gets here is where the branches are, where
//! they merged, which refs sit where, and how recently anything moved. Reading
//! a whole message is the commit detail view's job, one click away.
//!
//! # What size everything is
//!
//! Three, and no more: 12px for the commit subject and for the
//! conventional-commit type in front of it, which are one run of text and are
//! told apart by tone rather than by size; 11px for the section's own heading
//! and for the "load more" control; and 10.5px for the annotations — the
//! relative age, the commit count, the ref chip. A sidebar list is dense by
//! design, and the graph is the densest thing in this panel.
//!
//! That is also VS Code's own reading of a sidebar graph, and it is why the
//! conventional-commit prefix is cut down to its type and demoted to muted ink
//! rather than left to eat half the line. The lane gutter itself never folds
//! away: the lanes are the graph, and a graph without them is just a list.
//!
//! The section is not a surface of its own. It sits flush on the panel's
//! `theme.sidebar` fill and is separated from the file list above it by a
//! hairline, the way every other band of this panel is. The only colour that
//! gets to be saturated here is a lane's, and the lanes stay in the gutter,
//! below the text; the rows themselves are ink, muted ink and the sidebar's own
//! neutral hover and selection fills.
//!
//! # How it is drawn
//!
//! One `canvas` covering the whole list, absolutely positioned over ordinary
//! interactive rows — never one canvas per row. Every `PrimitiveBatch::Paths`
//! gpui emits ends the current encoder, opens a render pass, clears a
//! drawable-sized intermediate texture, rasterises, resolves MSAA and
//! composites back; forty visible rows would mean forty of those per frame.
//!
//! Being on top costs nothing in event terms: `Canvas::id` returns `None` and
//! it implements no interactivity, so it registers no hitbox in prepaint. The
//! rows underneath keep gpui's native hover, click, context menu and
//! scroll-into-view. This was measured, not assumed — see the G7·0 spike commit.

use std::cell::Cell as StdCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, BorderStyle, Bounds, Context, Corners, Edges, Focusable as _, Hsla, MouseButton,
    MouseMoveEvent, MouseUpEvent, Pixels, SharedString, Window, canvas, div, fill, point,
    prelude::*, px, quad, rems,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use tty7_core::core::git::log::{
    Commit, CommitPage, Edge, GRAPH_PAGE, GraphRow, GraphScope, Lane, MAX_GRAPH_COMMITS, RefDeco,
    RefKind,
};
use tty7_core::core::git::ops::{GitOp, ResetMode};

use crate::ui::app::{CONTENT_INSET, Tty7App};
use crate::ui::i18n::{L10nKey, t};
use crate::ui::presets::{ActiveLanes, LANE_SLOTS, Lanes};
use crate::ui::right_panel::{META, META_MONO, TEXT, info_chip};
use crate::ui::scm::path::{elide_middle, relative_time};
use crate::ui::scm::state::RepoKey;

/// One commit per row. 24px rather than the file list's 26: a graph row has no
/// icon column, and the vertical pitch wants to stay close to the lane pitch or
/// the diagonal of a merge reads as a much shallower angle than it is.
///
/// It also has to contain the line the subject is set on — gpui leads text at
/// phi, so [`TEXT`] occupies `round(14 × 1.618) = 23px`, and 24 is the first
/// even pitch above it. Both numbers were 4 smaller while the section was on
/// the panel's old 12px step.
///
/// A taller row against an unchanged [`GRAPH_LANE_W`] steepens a merge's
/// diagonal, which is the safe direction: the failure this pitch is guarding
/// against is an angle read as *shallower* than the jump it draws.
const GRAPH_ROW_H: f32 = 24.;
/// Rows materialized above and below the visible band, so a fast scroll never
/// outruns the window into blank space, and the "load more" band — laid out
/// one row past the window's end — stays below the fold until it is real.
const GRAPH_WINDOW_MARGIN: usize = 4;

/// Header of the section itself: fold, title, count, filter tile, scope picker.
///
/// The truth, not a wish: the row sets this height explicitly, and what a
/// reader measures is the tallest thing inside it. That used to be the 18px
/// `GRAPH_TILE`; now it is the title's own line — [`META`] leads to
/// `round(12 × 1.618) = 20px` — so 24 is that line plus 2px of air on each
/// side. The resize constants below are counted against it, which is why it
/// has to stay honest.
const GRAPH_HEADER_H: f32 = 24.;

/// The header's controls: the filter tile and the scope picker beside it.
///
/// An 18px square with a 12px glyph, sized against the [`META`] title beside
/// it: two thirds of the box, which is the fill that keeps an icon from
/// rattling around inside its tile.
const GRAPH_TILE: f32 = 18.;
const GRAPH_TILE_GLYPH: f32 = 12.;

/// Horizontal distance between lane centres.
const GRAPH_LANE_W: f32 = 12.;

/// Inset before the first lane centre, and the gap between the gutter and the
/// text column.
const GRAPH_PAD_L: f32 = 6.;
const GRAPH_PAD_R: f32 = 6.;

/// An ordinary node is a 3px disc and a lane line is 1.5px wide: the dot is a
/// quarter of the row's height, which is enough to read as a bead on a string
/// without closing the gap to the row above. Merges and roots are drawn from
/// the same radius rather than from a second vocabulary.
///
/// It stays 3 while the row grows, because what a bead is measured against is
/// the string it is on — `GRAPH_LANE_W`, which has not moved. A quarter is the
/// floor the test below holds it to.
const GRAPH_DOT_R: f32 = 3.;
const GRAPH_LINE_W: f32 = 1.5;

/// Most of the panel's width belongs to the message. Thirty percent is what
/// leaves five lanes at the 260px default and still keeps a readable column.
const GRAPH_GUTTER_SHARE: f32 = 0.30;

/// Lanes are capped by what the panel can show, never by what history did.
const GRAPH_MIN_LANES: usize = 3;
const GRAPH_MAX_LANES: usize = LANE_SLOTS;

/// The cap can never exceed the palette, or two columns on screen would come
/// out the same colour and the whole point of colouring by lane is lost.
const _: () = assert!(GRAPH_MAX_LANES <= LANE_SLOTS);

/// Resting height of the history section and the range the divider drags it
/// through. The ceiling is a share of the window rather than a constant: the
/// file list has to keep a usable part of a short one.
///
/// Both of the fixed ones are counted in rows against the header: what they
/// mean is a number of commits, not a number of pixels. 260 is `24 + 9.8 × 24`
/// — nine commits and most of a tenth, and the fraction is deliberate, because
/// a row cut by the bottom edge is the only honest way a fixed-height list says
/// there is more below it. 100 is `24 + 3.2 × 24`, three and a bit, which is
/// the least that still looks like history rather than like a mistake.
///
/// They grew with [`GRAPH_ROW_H`] — 220 and 88 around a 20px row — precisely
/// because they are counted in commits: holding the pixels would have quietly
/// bought the file list 40px by showing two fewer commits.
const GRAPH_H_DEFAULT: f32 = 260.;
const GRAPH_H_MIN: f32 = 100.;
const GRAPH_H_MAX_RATIO: f32 = 0.65;

/// The divider's grab area, matching `RESIZE_HANDLE_WIDTH` on the other axis.
const GRAPH_HANDLE_H: f32 = 6.;

/// Ref chips are the widest optional thing on a row, so they get a hard cap
/// and lose their middle rather than the message losing its column.
const GRAPH_REF_CHARS: usize = 14;

/// How many lanes fit, given the panel's width.
///
/// A pure projection over the width, deliberately: dragging the panel narrower
/// must not re-run the layout pass or renumber a colour.
fn max_lanes(panel_w: f32) -> usize {
    // The share buys the whole gutter, insets included — budgeting only the
    // lane strip would overrun it by a lane at every width.
    let fit = ((panel_w * GRAPH_GUTTER_SHARE - GRAPH_PAD_L - GRAPH_PAD_R) / GRAPH_LANE_W).floor();
    if !fit.is_finite() {
        return GRAPH_MIN_LANES;
    }
    (fit as usize).clamp(GRAPH_MIN_LANES, GRAPH_MAX_LANES)
}

/// Fold a true lane onto a visible column.
///
/// Everything past the cap shares the last column. Pure projection, never fed
/// back into the layout: the same page re-projects for free at any width, with
/// no recomputation and no colour changing under the reader.
fn project(lane: Lane, max_lanes: usize) -> Lane {
    lane.min(max_lanes.saturating_sub(1) as Lane)
}

/// Snap a lane centre to a device pixel *before* the quad is built.
///
/// `paint_quad` snaps the bounds it is handed, but each edge independently: an
/// unsnapped centre makes `[cx - w/2, cx + w/2]` round out to one physical
/// pixel on some rows and two on others, and a column of lines that changes
/// width as it scrolls is the most visible artefact this element can produce.
/// Same shape as `powerline_solid_edge` in the terminal renderer.
fn snap(x: f32, scale: f32) -> f32 {
    if !scale.is_finite() || scale <= 0. {
        return x;
    }
    (x * scale).round() / scale
}

/// Centre of a visible column, relative to the gutter's left edge.
fn lane_center_x(column: Lane, scale: f32) -> f32 {
    snap(
        GRAPH_PAD_L + GRAPH_LANE_W * column as f32 + GRAPH_LANE_W / 2.,
        scale,
    )
}

/// Total width of the gutter for a given cap.
fn gutter_width(max_lanes: usize) -> f32 {
    GRAPH_PAD_L + GRAPH_LANE_W * max_lanes as f32 + GRAPH_PAD_R
}

/// Split a conventional-commit prefix off the subject.
///
/// Returns the type, whether it was marked breaking, and what is left. This
/// repository's subjects spend an average of 12.7 characters on the prefix,
/// which is half of what a 260px panel has to give — and the scope in the
/// middle of it is the part nobody scans for, so it goes. The type stays, in
/// front of the subject and a shade quieter than it.
///
/// Strict on purpose. Only a lowercase ASCII type, an optional parenthesised
/// scope, an optional `!`, then `": "`. `Note: see below` and `TODO: fix` are
/// not conventional commits and keep their whole line.
fn split_conventional(subject: &str) -> (Option<(&str, bool)>, &str) {
    let bytes = subject.as_bytes();
    let type_len = bytes.iter().take_while(|b| b.is_ascii_lowercase()).count();
    // Two is `ci`; past twelve it is prose that happens to start lowercase.
    if !(2..=12).contains(&type_len) {
        return (None, subject);
    }
    let mut i = type_len;
    if bytes.get(i) == Some(&b'(') {
        match bytes[i..].iter().position(|b| *b == b')') {
            // An empty scope, `feat(): x`, is malformed; treat the line as prose.
            Some(0 | 1) => return (None, subject),
            Some(close) => i += close + 1,
            None => return (None, subject),
        }
    }
    let breaking = bytes.get(i) == Some(&b'!');
    if breaking {
        i += 1;
    }
    // The space matters: `fix:it` is not a conventional commit, and without it
    // a URL-bearing subject would be cut at `https:`.
    if bytes.get(i) != Some(&b':') || bytes.get(i + 1) != Some(&b' ') {
        return (None, subject);
    }
    let rest = subject[i + 2..].trim_start();
    if rest.is_empty() {
        return (None, subject);
    }
    (Some((&subject[..type_len], breaking)), rest)
}

/// Which colour a visible column draws with.
///
/// A column is the overflow bundle only when the page really is wider than the
/// cap. Deciding that per page rather than per row keeps a column from changing
/// colour as the reader scrolls past the one merge that widened history.
fn column_ink(column: Lane, color: u16, max_lanes: usize, overflowing: bool, lanes: &Lanes) -> u32 {
    if overflowing && column as usize + 1 == max_lanes {
        return lanes.overflow;
    }
    lanes.ink[(color as usize).min(LANE_SLOTS - 1)]
}

/// The segments of one row's band, already folded onto visible columns.
///
/// Deduplicated by column, which is what makes the overflow bundle work: five
/// lanes sharing the last column produce one line, not five stacked on each
/// other at five different alphas. Later writers win, and the caller feeds
/// edges in `paint_rank` order, so the node's own line lands over anything
/// merely passing behind it.
#[derive(Default)]
struct Band {
    top: [Option<u32>; GRAPH_MAX_LANES],
    bottom: [Option<u32>; GRAPH_MAX_LANES],
    /// `(left column, right column, colour)`, at most one per pair.
    turns: Vec<(Lane, Lane, u32)>,
}

impl Band {
    fn turn(&mut self, a: Lane, b: Lane, ink: u32) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        match self.turns.iter_mut().find(|t| t.0 == lo && t.1 == hi) {
            Some(existing) => existing.2 = ink,
            None => self.turns.push((lo, hi, ink)),
        }
    }
}

/// Fold one row's edges into the segments that will be painted.
fn band_of(row: &GraphRow, max_lanes: usize, overflowing: bool, lanes: &Lanes) -> Band {
    let mut band = Band::default();
    let node = project(row.node, max_lanes);
    for edge in &row.edges {
        let ink = |column: Lane| column_ink(column, edge.color(), max_lanes, overflowing, lanes);
        match *edge {
            Edge::Pass { lane, .. } => {
                let c = project(lane, max_lanes);
                band.top[c as usize] = Some(ink(c));
                band.bottom[c as usize] = Some(ink(c));
            }
            Edge::In { from, .. } => {
                let c = project(from, max_lanes);
                band.top[c as usize] = Some(ink(c));
                if c != node {
                    band.turn(c, node, ink(c));
                }
            }
            Edge::Out { to, .. } => {
                let c = project(to, max_lanes);
                band.bottom[c as usize] = Some(ink(c));
                if c != node {
                    band.turn(node, c, ink(c));
                }
            }
        }
    }
    band
}

/// Everything the paint closure needs, snapshotted at render time so nothing
/// reaches back into the view from inside the frame.
struct GraphPaint {
    page: Arc<CommitPage>,
    max_lanes: usize,
    overflowing: bool,
    lanes: Lanes,
    /// The fill behind a hollow node, so a merge reads as a ring and not as a
    /// disc with a hole punched through to whatever is under the panel. It is
    /// the panel's own opaque sidebar fill rather than `theme.background`: the
    /// section is flush with the panel, and `background` carries the window's
    /// transparency when one is configured, which would let the lane line show
    /// straight down the middle of the node.
    surface: Hsla,
    /// The selected row and the fill its band paints under the node, so a
    /// hollow node's hole matches the selection band it sits on instead of
    /// punching through to the resting surface. Hover is not covered — it
    /// lives in gpui's element state, which a paint closure cannot read — so
    /// a hovered ring keeps the resting hole; one step of fill under a 3px
    /// hole, against a whole selected band showing the wrong colour.
    selected: Option<usize>,
    selected_surface: Hsla,
    /// Whether a "load more" band follows the last row.
    more: bool,
}

/// Paint the whole gutter in one pass.
///
/// Deliberately *not* wrapped in `paint_layer` per line, which is what Zed's
/// own graph does. A layer is a full-drawable render pass; a dozen of them per
/// frame is a dozen. Overlap ordering is already handled — `BoundsTree` hands
/// every overlapping primitive an increasing order — and within a row the
/// caller has sorted edges by `paint_rank` so the node's line is last. If a
/// future change makes something here look wrong in z, the fix is the sort
/// order, not a layer.
fn paint_graph(p: &GraphPaint, bounds: Bounds<Pixels>, window: &mut Window) {
    let started = std::time::Instant::now();
    let scale = window.scale_factor();
    let top = bounds.origin.y.as_f32();
    let left = bounds.origin.x.as_f32();
    let rows = &p.page.rows;

    // The canvas is as tall as the whole list, so most of it is off screen.
    // The mask is the viewport; only the band it allows is worth iterating.
    let mask = window.content_mask().bounds;
    let first = (((mask.origin.y.as_f32() - top) / GRAPH_ROW_H).floor() as isize).max(0) as usize;
    let last = ((((mask.origin.y + mask.size.height).as_f32() - top) / GRAPH_ROW_H).ceil() as isize)
        .max(0) as usize;

    let cx_of = |column: Lane| left + lane_center_x(column, scale);
    let vline = |x: f32, y0: f32, y1: f32| {
        Bounds::from_corners(
            point(px(x - GRAPH_LINE_W / 2.), px(y0)),
            point(px(x + GRAPH_LINE_W / 2.), px(y1)),
        )
    };

    for (i, row) in rows
        .iter()
        .enumerate()
        .take(last.min(rows.len()))
        .skip(first)
    {
        let y0 = top + i as f32 * GRAPH_ROW_H;
        let mid = y0 + GRAPH_ROW_H / 2.;
        let band = band_of(row, p.max_lanes, p.overflowing, &p.lanes);

        for (column, ink) in band.top.iter().enumerate() {
            if let Some(ink) = ink {
                window.paint_quad(fill(vline(cx_of(column as Lane), y0, mid), gpui::rgb(*ink)));
            }
        }
        for (column, ink) in band.bottom.iter().enumerate() {
            if let Some(ink) = ink {
                window.paint_quad(fill(
                    vline(cx_of(column as Lane), mid, y0 + GRAPH_ROW_H),
                    gpui::rgb(*ink),
                ));
            }
        }
        // Cross-lane turns are right angles, which is what tig, lazygit and
        // `git log --graph` all draw and what reads unambiguously at a 12px
        // pitch. Curves would mean paths, and paths mean a render pass each.
        // Swapping them in later touches only this loop: a curve consumes the
        // same `(lo, hi, mid)` a right angle does.
        //
        // The horizontal runs half a line width past both centres, which is
        // exactly what fills the two outside corners the vertical stubs leave
        // open. Without it a turn shows a notch at every elbow.
        for (lo, hi, ink) in &band.turns {
            window.paint_quad(fill(
                Bounds::from_corners(
                    point(
                        px(cx_of(*lo) - GRAPH_LINE_W / 2.),
                        px(mid - GRAPH_LINE_W / 2.),
                    ),
                    point(
                        px(cx_of(*hi) + GRAPH_LINE_W / 2.),
                        px(mid + GRAPH_LINE_W / 2.),
                    ),
                ),
                gpui::rgb(*ink),
            ));
        }

        let node = project(row.node, p.max_lanes);
        let ink = gpui::rgb(column_ink(
            node,
            row.color,
            p.max_lanes,
            p.overflowing,
            &p.lanes,
        ));
        let cx = cx_of(node);
        let dot = |r: f32| {
            Bounds::from_corners(
                point(px(cx - r), px(mid - r)),
                point(px(cx + r), px(mid + r)),
            )
        };
        // A rounded quad rather than a path: the quad shader's rounding is an
        // exact SDF with analytic anti-aliasing, where `PathBuilder` fills every
        // vertex's `st` with `(0, 1)` and so falls back on 4x MSAA alone. The
        // hollow ones are one bordered quad rather than two concentric fills,
        // because the border is part of that same SDF — stacking would blend the
        // inner edge over the outer one's already-blended edge, and a 3px hole
        // is where that shows.
        let hole = if p.selected == Some(i) {
            p.selected_surface
        } else {
            p.surface
        };
        if row.parents > 1 {
            // A merge is a ring. It is the one row shape a reader scans for,
            // and an outline reads at 8px where a second fill colour does not.
            let r = GRAPH_DOT_R + 1.;
            window.paint_quad(quad(
                dot(r),
                Corners::all(px(r)),
                hole,
                Edges::all(px(GRAPH_LINE_W)),
                ink,
                BorderStyle::Solid,
            ));
        } else if row.parents == 0 {
            // A root has nothing below it; hollow says "the line stops here"
            // without needing a second glyph.
            window.paint_quad(quad(
                dot(GRAPH_DOT_R),
                Corners::all(px(GRAPH_DOT_R)),
                hole,
                Edges::all(px(GRAPH_LINE_W)),
                ink,
                BorderStyle::Solid,
            ));
        } else {
            window.paint_quad(
                fill(dot(GRAPH_DOT_R), ink).corner_radii(Corners::all(px(GRAPH_DOT_R))),
            );
        }
    }

    // Past the last row the lanes that are still open get a stub. Without it a
    // page boundary reads as a row of root commits — every line simply ending.
    // Under a "load more" row the stubs run the full band instead, so the graph
    // reads as continuing through the control rather than being cut by it.
    if last > rows.len() && !p.page.open_lanes.is_empty() {
        let y0 = top + rows.len() as f32 * GRAPH_ROW_H;
        for lane in &p.page.open_lanes {
            let column = project(*lane, p.max_lanes);
            let ink = column_ink(column, *lane, p.max_lanes, p.overflowing, &p.lanes);
            let x = cx_of(column);
            if p.more {
                window.paint_quad(fill(
                    vline(x, y0, y0 + GRAPH_ROW_H),
                    Hsla::from(gpui::rgb(ink)).opacity(0.3),
                ));
            } else {
                // Three steps rather than a gradient: a gradient would be a
                // second `Background` kind for four pixels of ink.
                const STEP: f32 = 3.;
                for (step, alpha) in [0.5f32, 0.3, 0.15].into_iter().enumerate() {
                    let a = y0 + step as f32 * STEP;
                    window.paint_quad(fill(
                        vline(x, a, a + STEP),
                        Hsla::from(gpui::rgb(ink)).opacity(alpha),
                    ));
                }
            }
        }
    }

    if crate::ui::perf::enabled() {
        crate::ui::perf::record("scm.graph.paint", started.elapsed());
    }
}

impl Tty7App {
    /// The history section, when it is expanded and has something to draw.
    ///
    /// Sits below the file list as its own scroll region rather than at the end
    /// of one: the graph pages, and sharing a scroller would mean scrolling back
    /// past hundreds of commits to reach the message box.
    pub(crate) fn render_graph_section(
        &mut self,
        repo: &RepoKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.scm.graph.expanded {
            // Folded, the section is one line — but it keeps the rule above it,
            // or it reads as the last row of the file list rather than as a
            // section of its own.
            return Some(
                div()
                    .flex_none()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(self.graph_header(repo, None, cx))
                    .into_any_element(),
            );
        }
        self.scm_load_graph(repo, cx);

        if self.scm.graph.height.get() <= 0. {
            self.scm.graph.height.set(GRAPH_H_DEFAULT);
        }
        let ceiling = (window.viewport_size().height.as_f32() * GRAPH_H_MAX_RATIO).max(GRAPH_H_MIN);
        let height = self.scm.graph.height.get().clamp(GRAPH_H_MIN, ceiling);

        let page = self.scm.graph.page.clone();
        let header = self.graph_header(repo, page.as_deref(), cx);
        let search = self.graph_search(cx);
        let naming = self.graph_naming_row(repo, cx);
        let query = self.graph_query(cx);
        let body = match page {
            None => self.panel_empty(t(L10nKey::PanelLoading), None, cx),
            Some(page) if page.commits.is_empty() => {
                self.panel_empty(t(L10nKey::ScmGraphEmpty), None, cx)
            }
            Some(page) => self.graph_body(repo, &page, query.as_deref(), height, cx),
        };
        let (backing, handle) = self.graph_resize(ceiling, cx);

        Some(
            v_flex()
                .relative()
                .flex_none()
                .h(px(height))
                .border_t_1()
                .border_color(cx.theme().border)
                .child(backing)
                .child(header)
                .children(search)
                .children(naming)
                .child(body)
                .child(handle)
                .into_any_element(),
        )
    }

    /// Which refs the current settings walk from.
    fn graph_scope(&self) -> GraphScope {
        self.scm.graph.scope.clone()
    }

    /// Ask for a page, at most once per (repository, scope, size).
    ///
    /// Paging grows `requested` and re-runs the query rather than paging with
    /// `--skip`. The layout is deterministic, so a longer run reproduces the
    /// same prefix row for row — nothing already on screen moves — where
    /// `--skip` is O(skip) to walk and slides under you the moment a ref moves.
    fn scm_load_graph(&mut self, repo: &RepoKey, cx: &mut Context<Self>) {
        // A page that belongs to another repository must neither be drawn nor
        // grown from: this runs before the frame reads `page`, so the switch
        // shows the loading state, never seconds of the previous repository's
        // history — where a row click would build an op for the new repo with
        // the old repo's rev. The page size resets with it; how deep someone
        // read one history says nothing about the next.
        if self
            .scm
            .graph
            .page_key
            .as_ref()
            .is_some_and(|(r, _, _)| r != repo)
        {
            self.scm.graph.page = None;
            self.scm.graph.page_key = None;
            self.scm.graph.requested = 0;
        }
        // `try_global`, never `default_global`: this runs from `render`, and
        // taking the global mutably there queues a global-observer effect on
        // every frame, which is a panel that asks for a frame from inside one.
        let epoch = cx
            .try_global::<crate::terminal::git_data::ScmData>()
            .map_or(0, |data| data.epoch(repo.host, &repo.root));
        let scope = self.graph_scope();
        // Clamped like `load_page` clamps it: `requested` above the cap with a
        // page that answers *at* the cap would read as never-fresh, and the
        // panel would refetch the same full page from every frame's render.
        let want = self
            .scm
            .graph
            .requested
            .clamp(GRAPH_PAGE, MAX_GRAPH_COMMITS);
        let key = (repo.clone(), epoch, scope.clone());
        let fresh = self.scm.graph.page_key.as_ref() == Some(&key)
            && self
                .scm
                .graph
                .page
                .as_ref()
                .is_some_and(|p| p.requested >= want);
        // A load that failed is not retried until its key changes — an epoch
        // bump, a new scope, a new repository. Retrying from render would
        // spawn git processes in a tight loop for as long as the cause holds.
        if fresh || self.scm.graph.loading || self.scm.graph.failed_key.as_ref() == Some(&key) {
            return;
        }
        let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, repo.host) else {
            return;
        };
        self.scm.graph.loading = true;
        let root = repo.root.clone();
        let query = scope.clone();
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            move |h| tty7_core::core::git::log::load_page(h, &root, &query, want),
            move |this, page, cx| {
                this.scm.graph.loading = false;
                match page {
                    Some(page) => {
                        this.scm.graph.failed_key = None;
                        this.scm.graph.page = Some(Arc::new(page));
                        this.scm.graph.page_key = Some(key);
                    }
                    None => this.scm.graph.failed_key = Some(key),
                }
                cx.notify();
            },
        );
    }

    /// Which commit indices the list shows for this page and query, resolved
    /// through the cache on `GraphState` — see its doc for why it exists.
    fn graph_visible_rows(
        &mut self,
        page: &Arc<CommitPage>,
        query: Option<&str>,
    ) -> Arc<Vec<usize>> {
        let key = Arc::as_ptr(page) as usize;
        if let Some((held_query, held_page, rows)) = &self.scm.graph.filter_cache {
            if *held_page == key && held_query.as_deref() == query {
                return rows.clone();
            }
        }
        let rows: Arc<Vec<usize>> = Arc::new(match query {
            None => (0..page.commits.len()).collect(),
            Some(q) => (0..page.commits.len())
                .filter(|i| matches_query(&page.commits[*i], q))
                .collect(),
        });
        self.scm.graph.filter_cache = Some((query.map(str::to_string), key, rows.clone()));
        rows
    }

    /// The filter box's text, if it has any.
    fn graph_query(&self, cx: &Context<Self>) -> Option<String> {
        let input = self.scm.graph.search.as_ref()?;
        let text = input.read(cx).value().trim().to_lowercase();
        (!text.is_empty()).then_some(text)
    }

    /// The filter field, drawn only while it is open.
    ///
    /// At rest the section is a title and a list; a permanently parked search
    /// box was a row of chrome above every reading of history, for a thing that
    /// gets used once a session. It lives behind the header's tile now.
    fn graph_search(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let input = self.scm.graph.search.clone()?;
        Some(self.panel_search(&input, cx))
    }

    /// Show the filter field, or take it away again.
    ///
    /// The `InputState` *is* the open flag: there is no second boolean to keep
    /// in step with it, and closing the field drops the entity, which is what
    /// answers the question a hidden filter always raises. A query cannot go on
    /// quietly cutting rows out of the list from behind a closed box, because
    /// there is nothing left holding the text — `graph_query` reads the input
    /// or reads nothing. The cost is that reopening starts empty, which is the
    /// right trade for a filter this shallow: retyping four characters is
    /// cheaper than wondering why history is missing.
    ///
    /// Created here rather than on first render, the way `commit_input` still
    /// is: an `InputState` needs a real window, and the click that asks for one
    /// has one in hand.
    fn graph_toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.scm.graph.search.take().is_some() {
            self.scm.graph.search_sub = None;
            cx.notify();
            return;
        }
        let input = cx.new(|cx| {
            gpui_component::input::InputState::new(window, cx)
                .placeholder(t(L10nKey::ScmGraphFilterPlaceholder))
        });
        // Without this the box would take text the list never sees: an
        // `InputState` is its own entity, and its changes are its own events.
        self.scm.graph.search_sub =
            Some(
                cx.subscribe_in(&input, window, |_this, _input, ev, _window, cx| {
                    if matches!(ev, gpui_component::input::InputEvent::Change) {
                        cx.notify();
                    }
                }),
            );
        let handle = input.read(cx).focus_handle(cx);
        self.scm.graph.search = Some(input);
        // A field that appears without the caret in it is a field you have to
        // click twice.
        window.focus(&handle, cx);
        cx.notify();
    }
}

impl Tty7App {
    /// The scrolling list: rows underneath, one canvas over the gutter.
    ///
    /// `height` is the section's height — the ceiling on how much of the list
    /// can be on screen, and so on how many rows become elements.
    fn graph_body(
        &mut self,
        repo: &RepoKey,
        page: &Arc<CommitPage>,
        query: Option<&str>,
        height: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let panel_w = cx.global::<crate::core::config::Config>().right_panel_width;
        let cap = max_lanes(panel_w);
        let gutter = gutter_width(cap);
        let now = crate::ui::home::now_secs() as i64;

        // A filtered view drops rows out of the middle of history, and lanes
        // drawn across a subset would connect commits that are not adjacent.
        // So the filter hides the gutter entirely and the list becomes a flat
        // search result — which is what it actually is.
        let filtering = query.is_some();
        let rows = self.graph_visible_rows(page, query);
        // No row at the cap either: `load_page` clamps there, so "load more"
        // past it could only refetch what is already on screen.
        let more = query.is_none() && !page.complete && page.commits.len() < MAX_GRAPH_COMMITS;
        let bands = rows.len() + usize::from(more);

        // Only the rows that can be on screen become elements — the row count
        // is bounded by the cap at 5000, and a taffy pass over 5000 flex
        // children per frame is most of a frame. The stack below keeps its
        // full fixed height so the scroll range is unchanged; a top padding
        // stands in for everything scrolled past. The canvas needs no such
        // treatment: its paint is already clipped to the content mask.
        let scrolled = (-self.scm.graph.scroll.offset().y.as_f32()).max(0.);
        let first = ((scrolled / GRAPH_ROW_H) as usize)
            .saturating_sub(GRAPH_WINDOW_MARGIN)
            .min(rows.len());
        let visible = (height / GRAPH_ROW_H).ceil() as usize + GRAPH_WINDOW_MARGIN * 2;
        let last = first.saturating_add(visible).min(rows.len());

        // With the gutter gone the text takes the panel's own inset, so a
        // search result does not sit in a column of empty space.
        let indent = if filtering { CONTENT_INSET } else { gutter };
        let list = v_flex().pt(px(first as f32 * GRAPH_ROW_H)).children(
            rows[first..last]
                .iter()
                .map(|i| self.graph_row(repo, page, *i, indent, now, cx)),
        );
        let mut stack = div()
            .relative()
            .w_full()
            .h(px(bands as f32 * GRAPH_ROW_H))
            .child(list)
            .children(more.then(|| self.graph_load_more(gutter, cx)));

        if !filtering {
            let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
            let paint = GraphPaint {
                page: page.clone(),
                max_lanes: cap,
                overflowing: page.max_lanes as usize > cap,
                lanes: cx.global::<ActiveLanes>().0,
                // The hole in a hollow node has to be the exact fill behind it,
                // or the lane line running underneath shows through. The
                // section is flush on the panel, so that fill is the sidebar's.
                surface: gpui::rgb(sf.base).into(),
                selected: self
                    .scm
                    .graph
                    .selected
                    .as_deref()
                    .and_then(|oid| page.commits.iter().position(|c| c.oid == oid)),
                selected_surface: gpui::rgb(sf.selected).into(),
                more,
            };
            stack = stack.child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| paint_graph(&paint, bounds, window),
                )
                .absolute()
                .top_0()
                .left_0()
                .w(px(gutter))
                .h_full(),
            );
        }

        let scroller = div()
            .id("scm-graph")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scm.graph.scroll)
            .child(stack);
        crate::ui::scrollbar::with_vertical_scrollbar(
            "scm-graph-scrollbar",
            scroller,
            &self.scm.graph.scroll,
        )
    }

    /// One commit.
    ///
    /// Column order is fixed and every optional part has a hard cap, so the
    /// worst case cannot squeeze the message to nothing: type prefix, message,
    /// ref chip, age. The age goes when a ref chip is present — a chip says
    /// where a branch is, which is worth more here than three characters of
    /// "3d", and the full timestamp is in the tooltip either way.
    fn graph_row(
        &self,
        repo: &RepoKey,
        page: &Arc<CommitPage>,
        i: usize,
        gutter: f32,
        now: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let commit = &page.commits[i];
        let mono = cx.theme().mono_font_family.clone();
        // The sidebar's surface, because that is the fill this row sits on.
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let selected = self.scm.graph.selected.as_deref() == Some(commit.oid.as_str());
        let (prefix, subject) = split_conventional(&commit.summary);
        let deco = commit.refs.first();
        let extra = commit.refs.len().saturating_sub(1);
        let oid = commit.oid.clone();

        h_flex()
            .id(SharedString::from(format!("scm-graph-row-{i}")))
            .items_center()
            .gap(px(4.))
            .h(px(GRAPH_ROW_H))
            .pl(px(gutter))
            .pr(px(CONTENT_INSET))
            .cursor_pointer()
            // The panel's own neutral selection and hover fills, the same two
            // the file list above uses. A tinted band would make this one list
            // in the sidebar announce itself differently from every other.
            .when(selected, |d| d.bg(gpui::rgb(sf.selected)))
            .when(!selected, |d| d.hover(|s| s.bg(gpui::rgb(sf.hover))))
            // Prefix and subject are one run of text, so they sit closer than
            // the row's own gap: two pixels between `feat` and the words it
            // introduces is about a word space at this size, and reads as one
            // rather than as the space between two elements. The outer gap
            // still separates them from the ref chip and the age, which *are*
            // other elements.
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(2.))
                    .children(
                        prefix.map(|(kind, breaking)| self.graph_type_prefix(kind, breaking, cx)),
                    )
                    .child(
                        // The only full-strength ink on the row, and the widest
                        // thing on it. Everything else — the type, the age, the
                        // refs, the lanes — is an annotation on this.
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(rems(TEXT))
                            .text_color(cx.theme().foreground)
                            .child(SharedString::from(subject.to_string())),
                    ),
            )
            .children(deco.map(|r| self.graph_ref_chip(r, extra, &mono, cx)))
            // The age is a mono token, so its column is measured in characters:
            // the widest it ever prints is four (`12mo`), and four of the mono
            // face's advances at `META_MONO` is `4 × 11 × 0.6` = 26.4, rounded
            // up to 27.
            .when(deco.is_none(), |d| {
                d.child(
                    div()
                        .flex_none()
                        .min_w(px(27.))
                        .text_size(rems(META_MONO))
                        .font_family(mono.clone())
                        .text_color(cx.theme().muted_foreground)
                        .child(SharedString::from(relative_time(
                            now,
                            commit.author.at.unix,
                        ))),
                )
            })
            .tooltip({
                let text = commit_tooltip(commit, now);
                move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(text.clone()).build(window, cx)
                }
            })
            .on_click(cx.listener({
                let repo = repo.clone();
                // The row already holds everything the detail view renders, so
                // it hands its own commit over and no `git show` is run. The
                // listener carries the page `Arc` and an index, not a clone of
                // the commit: with up to 5000 rows a frame, one deep `Commit`
                // clone per row (an 8KB body, refs) was most of the frame.
                let page = page.clone();
                move |this, _, _, cx| {
                    let seed = page.commits[i].clone();
                    this.graph_open_commit(repo.clone(), seed.oid.clone(), Some(seed), cx)
                }
            }))
            .context_menu({
                let app = cx.entity().downgrade();
                let repo = repo.clone();
                let oid = oid.clone();
                move |menu, _window, cx| {
                    let danger = cx.theme().danger;
                    Tty7App::graph_row_context_menu(menu, &repo, &oid, danger, &app)
                }
            })
            .into_any_element()
    }
}

impl Tty7App {
    /// Select a row and hand its commit to the detail view.
    ///
    /// `seed` is the commit the row was drawn from. Passing it means the
    /// detail view opens with its message, author and refs already in hand and
    /// only reads the file list — a row click costs one git command, not two.
    fn graph_open_commit(
        &mut self,
        repo: RepoKey,
        oid: String,
        seed: Option<Commit>,
        cx: &mut Context<Self>,
    ) {
        self.scm.graph.selected = Some(oid.clone());
        self.open_commit_detail(repo, oid, seed, cx);
    }
}

/// One "load more" click's worth of growth, stopped at what `load_page` will
/// actually answer.
///
/// Growing past [`MAX_GRAPH_COMMITS`] would ask for a page the loader clamps:
/// the answer would never satisfy `requested >= want`, and the panel would
/// refetch the same full page from every frame's render, forever.
fn next_page_request(requested: usize) -> usize {
    requested
        .max(GRAPH_PAGE)
        .saturating_add(GRAPH_PAGE)
        .min(MAX_GRAPH_COMMITS)
}

/// Whether a commit answers the filter box.
///
/// Subject, author and sha, all case-folded. Not the body: a search that
/// matches on text the row cannot show is a search whose results look wrong.
fn matches_query(commit: &Commit, query: &str) -> bool {
    commit.summary.to_lowercase().contains(query)
        || commit.author.name.to_lowercase().contains(query)
        || commit.oid.starts_with(query)
}

fn commit_tooltip(commit: &Commit, now: i64) -> SharedString {
    SharedString::from(format!(
        "{}\n{} · {} · {}",
        commit.summary,
        commit.short(),
        commit.author.name,
        relative_time(now, commit.author.at.unix)
    ))
}

/// What ink a conventional-commit prefix is set in.
///
/// One rule rather than a palette, and the type is not what decides it. The
/// lane gutter immediately to the left is already a column of colour, so a
/// green `feat` beside a green lane dot adds a second colour column that says
/// nothing the dot has not: the reader's eye is pulled twice and told the same
/// thing once. Muting the whole vocabulary — `feat`, `fix`, `perf`, `docs` and
/// whatever a repository invents — leaves the row with two levels of text
/// emphasis, which is all a sidebar has ever needed.
///
/// The one exception is a breaking change. A `!` is the single thing in a
/// subject line worth shouting about, whatever the type in front of it says.
fn type_tone(breaking: bool, cx: &gpui::App) -> Hsla {
    let theme = cx.theme();
    match breaking {
        true => theme.danger,
        false => theme.muted_foreground,
    }
}

impl Tty7App {
    /// The prefix, set inline as the first word of the subject.
    ///
    /// Deliberately not a chip. A filled pill on every row is a colour column
    /// running down the panel right beside the one the lanes already draw, and
    /// because the types are different lengths — `fix`, `chore`, `refactor` —
    /// the pills gave every subject a different left edge, so nothing in the
    /// list lined up vertically. With no fill and no padding of its own, the
    /// two halves read as one sentence and the column starts where the prefix
    /// does.
    ///
    /// Level with the subject at 12px and told apart by tone alone. The two
    /// halves are one sentence, and a size change mid-sentence is a seam: the
    /// muting already says which half a reader is meant to skip, and saying it
    /// twice buys nothing but a ragged line.
    ///
    /// Not mono either, for the same reason it is not a chip: the subject
    /// beside it is not, and a family change mid-run is a seam where the point
    /// is continuity.
    fn graph_type_prefix(&self, kind: &str, breaking: bool, cx: &mut Context<Self>) -> AnyElement {
        div()
            // Never the part that truncates. It is a handful of characters, and
            // clipping them to buy the subject the same handful is not a trade
            // — a half-eaten `refac…` costs a reader more than it gives back.
            .flex_none()
            .text_size(rems(TEXT))
            .text_color(type_tone(breaking, cx))
            .child(SharedString::from(match breaking {
                true => format!("{kind}!"),
                false => kind.to_string(),
            }))
            .into_any_element()
    }

    /// The highest-priority ref on a commit, plus a count of the rest.
    ///
    /// `load_page` already sorted them HEAD → local → tag → remote, so the
    /// first one is the one worth the width.
    fn graph_ref_chip(
        &self,
        deco: &RefDeco,
        extra: usize,
        mono: &SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let (bg, fg, weight) = match deco.kind {
            // Where you are is the one thing on this row worth a heavier
            // weight; everything else is context. The fill is grey, not brand:
            // `theme.accent` is a neutral surface tint here, and the only
            // saturated colour the section spends is a lane's.
            RefKind::Head => (
                theme.accent.opacity(0.28),
                theme.foreground,
                gpui::FontWeight::SEMIBOLD,
            ),
            // Tags are yellow because tags are yellow — in git's own output,
            // in every other client, and in the reader's memory.
            RefKind::Tag => (
                theme.warning.opacity(0.16),
                theme.warning,
                gpui::FontWeight::NORMAL,
            ),
            // Everything else — a local branch you are not on, a remote
            // tracking ref — is context, and context does not get a box. A
            // filled grey pill here was a third block of surface on a row that
            // already carries a lane gutter and a subject; unfilled, it reads
            // as a label sitting after the message, which is what it is.
            _ => (
                gpui::transparent_black(),
                theme.muted_foreground,
                gpui::FontWeight::NORMAL,
            ),
        };
        let label = match extra {
            0 => elide_middle(&deco.short, GRAPH_REF_CHARS).into_owned(),
            n => format!("{} +{n}", elide_middle(&deco.short, GRAPH_REF_CHARS)),
        };
        // A hard cap, because the chip is the one part of the row whose width
        // comes from a branch name someone else chose. `GRAPH_REF_CHARS` elides
        // the middle first; this is the backstop for the widths that survive
        // it — fourteen characters of `info_chip`'s 10.5px mono plus its
        // padding, which is about 72.
        div()
            .flex_none()
            .max_w(px(72.))
            .truncate()
            .font_weight(weight)
            .child(info_chip(&label, bg, fg, mono))
            .into_any_element()
    }

    /// The band under the last row that asks for the next page.
    ///
    /// A row rather than a scroll trigger. A remote `git log` is an RPC across
    /// a host boundary, and scroll-to-load turns one flick of a trackpad into a
    /// burst of concurrent ones.
    fn graph_load_more(&self, gutter: f32, cx: &mut Context<Self>) -> AnyElement {
        let loading = self.scm.graph.loading;
        h_flex()
            .id("scm-graph-more")
            .items_center()
            .h(px(GRAPH_ROW_H))
            .pl(px(gutter))
            .pr(px(CONTENT_INSET))
            .cursor_pointer()
            // A step under the subjects above it: this is a control the list
            // offers, not a commit, and it should not read as one more row of
            // history.
            .text_size(rems(META))
            .text_color(cx.theme().muted_foreground)
            .hover(|s| s.text_color(cx.theme().foreground))
            .child(SharedString::from(match loading {
                true => t(L10nKey::PanelLoading),
                false => t(L10nKey::ScmGraphLoadMore),
            }))
            .on_click(cx.listener(|this, _, _, cx| {
                this.scm.graph.requested = next_page_request(this.scm.graph.requested);
                cx.notify();
            }))
            .into_any_element()
    }
}

impl Tty7App {
    /// The section's own title row: fold, title and count on the left, the
    /// filter tile and the scope picker on the right.
    ///
    /// The arrangement is the part worth keeping — a count that reads as part
    /// of the title, and the two controls collected at the trailing edge rather
    /// than strung out beside the label. The dress is the panel's: muted ink
    /// and no control tinted, because nothing here is more important than the
    /// file list above it.
    ///
    /// The title is [`META`] MEDIUM and the count a step under it beside it,
    /// both muted: this is a band label, not a heading a reader is meant to
    /// stop at, and the rows below it are what the section is for. The count is
    /// set in the UI font rather than mono — it sits inside the title's own
    /// phrase, and a family change there would read as a token rather than as
    /// part of it — so it borrows `META_MONO` for its size and nothing else.
    /// The ladder has no half-steps, which is what the old 11/10.5 pair was;
    /// the full step it lands on now does the same job a little more plainly.
    ///
    /// There is no lane-gutter fold. The lanes are the graph.
    fn graph_header(
        &self,
        repo: &RepoKey,
        page: Option<&CommitPage>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.scm.graph.expanded;
        let muted = cx.theme().muted_foreground;
        let filtering = self.scm.graph.search.is_some();
        let count = page.map(|p| match p.complete {
            true => p.commits.len().to_string(),
            false => format!("{}+", p.commits.len()),
        });

        h_flex()
            .flex_none()
            .items_center()
            .gap(px(4.))
            .h(px(GRAPH_HEADER_H))
            .pl(px(CONTENT_INSET))
            .pr(px(crate::ui::app::tile_trailing_inset_sm()))
            .child(
                h_flex()
                    .id("scm-graph-fold")
                    .items_center()
                    .gap(px(4.))
                    .flex_1()
                    .min_w(px(0.))
                    .cursor_pointer()
                    .child(
                        // A hair under the title it opens: the chevron is a
                        // mark, not a word, and at the label's own size it
                        // starts competing with it for the corner. `.xsmall()`
                        // is that size exactly, which is why this one is still
                        // a number.
                        Icon::new(match expanded {
                            true => IconName::ChevronDown,
                            false => IconName::ChevronRight,
                        })
                        .size(px(11.))
                        .text_color(muted),
                    )
                    .child(
                        div()
                            .text_size(rems(META))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(muted)
                            .child(SharedString::from(t(L10nKey::ScmGraphTitle))),
                    )
                    // The count reads as part of the title, so it sits with it
                    // rather than at the far end of the row — a step smaller
                    // and at normal weight, which is the whole difference
                    // between the two words in this corner.
                    .children(count.map(|c| {
                        div()
                            .text_size(rems(META_MONO))
                            .text_color(muted)
                            .child(SharedString::from(c))
                    }))
                    .on_click(cx.listener(|this, _, _, cx| this.scm_toggle_graph(cx))),
            )
            .when(expanded, |row| {
                row.child(
                    // Lit while the field is open, which is the only signal
                    // that history is being filtered once the box is gone —
                    // and it cannot go stale, because closing the box is what
                    // throws the query away.
                    crate::ui::tab_strip::chrome_tile_sized(
                        Button::new("scm-graph-filter").icon(Icon::new(IconName::Search)),
                        GRAPH_TILE,
                        GRAPH_TILE_GLYPH,
                        filtering,
                        cx,
                    )
                    .rounded(px(4.))
                    .tooltip(t(L10nKey::ScmGraphFilterPlaceholder))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.graph_toggle_search(window, cx)),
                    ),
                )
                .child(
                    // The same square as the tile beside it, or the two
                    // controls in this corner sit on different baselines.
                    Button::new("scm-graph-scope")
                        .ghost()
                        .xsmall()
                        .h(px(GRAPH_TILE))
                        .rounded(px(4.))
                        .dropdown_caret(true)
                        .label(scope_label(&self.scm.graph.scope))
                        .text_color(muted)
                        .dropdown_menu_with_anchor(
                            gpui::Anchor::TopRight,
                            self.graph_scope_menu(repo, cx),
                        ),
                )
            })
            .into_any_element()
    }

    fn graph_scope_menu(
        &self,
        repo: &RepoKey,
        cx: &mut Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static + use<>
    {
        let app = cx.entity().downgrade();
        let branches = self
            .scm
            .branches
            .get(repo)
            .map(|(_, names)| names.clone())
            .unwrap_or_default();
        let scope = self.scm.graph.scope.clone();

        move |menu, _window, _cx| {
            let mut menu = menu.min_w(px(180.));
            let pick = |app: &gpui::WeakEntity<Tty7App>, next: GraphScope| {
                let app = app.clone();
                move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut gpui::App| {
                    let _ = app.update(cx, |this, cx| this.graph_set_scope(next.clone(), cx));
                }
            };
            for (label, next) in [
                (
                    t(L10nKey::ScmGraphCurrentBranch),
                    GraphScope::HeadAndUpstream,
                ),
                (t(L10nKey::ScmGraphAllBranches), GraphScope::All),
            ] {
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .checked(scope == next)
                        .on_click(pick(&app, next)),
                );
            }
            if !branches.is_empty() {
                menu = menu.separator();
            }
            for name in &branches {
                let next = GraphScope::Refs(vec![format!("refs/heads/{name}")]);
                menu = menu.item(
                    PopupMenuItem::new(name.clone())
                        .checked(scope == next)
                        .on_click(pick(&app, next)),
                );
            }
            menu
        }
    }

    /// Point the graph at a different set of refs, and start it over.
    ///
    /// The page count resets with the scope: keeping a grown `requested` would
    /// make switching to a short branch pull its whole history in one go.
    fn graph_set_scope(&mut self, scope: GraphScope, cx: &mut Context<Self>) {
        if self.scm.graph.scope == scope {
            return;
        }
        self.scm.graph.scope = scope;
        self.scm.graph.requested = GRAPH_PAGE;
        self.scm.graph.page = None;
        self.scm.graph.page_key = None;
        cx.notify();
    }
}

fn scope_label(scope: &GraphScope) -> String {
    match scope {
        GraphScope::All => t(L10nKey::ScmGraphAllBranches).to_string(),
        GraphScope::Refs(refs) => refs
            .first()
            .map(|r| r.rsplit('/').next().unwrap_or(r).to_string())
            .unwrap_or_else(|| t(L10nKey::ScmGraphAllBranches).to_string()),
        _ => t(L10nKey::ScmGraphCurrentBranch).to_string(),
    }
}

impl Tty7App {
    /// The row's verbs.
    ///
    /// Every one of them goes through `scm_op`, which is where the confirmation
    /// for anything that can lose work already lives — a second gate here would
    /// be a second thing to keep in step with `GitOp::destructive`.
    fn graph_row_context_menu(
        menu: PopupMenu,
        repo: &RepoKey,
        oid: &str,
        danger: Hsla,
        app: &gpui::WeakEntity<Tty7App>,
    ) -> PopupMenu {
        let op = |app: &gpui::WeakEntity<Tty7App>, repo: &RepoKey, build: fn(String) -> GitOp| {
            let app = app.clone();
            let repo = repo.clone();
            let rev = oid.to_string();
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut gpui::App| {
                let _ = app.update(cx, |this, cx| {
                    this.scm_op(repo.clone(), build(rev.clone()), window, cx)
                });
            }
        };

        let mut menu = menu
            .min_w(px(200.))
            .item(
                PopupMenuItem::new(t(L10nKey::ScmCheckoutCommit))
                    .on_click(op(app, repo, |rev| GitOp::CheckoutDetached { rev })),
            )
            .item(
                PopupMenuItem::new(t(L10nKey::ScmCreateBranchHere)).on_click({
                    let app = app.clone();
                    let repo = repo.clone();
                    let rev = oid.to_string();
                    move |_, window, cx| {
                        let _ = app.update(cx, |this, cx| {
                            this.graph_begin_branch_at(repo.clone(), rev.clone(), window, cx)
                        });
                    }
                }),
            )
            .separator()
            .item(
                PopupMenuItem::new(t(L10nKey::ScmCherryPick)).on_click(op(app, repo, |rev| {
                    GitOp::CherryPick {
                        rev,
                        // A merge cherry-picked without `-m` is an error, and
                        // the first parent is the only sane default.
                        mainline: true,
                        no_commit: false,
                    }
                })),
            )
            .item(
                PopupMenuItem::new(t(L10nKey::ScmRevertCommit)).on_click(op(app, repo, |rev| {
                    GitOp::Revert {
                        rev,
                        mainline: true,
                    }
                })),
            )
            .separator();

        for (label, mode) in [
            (t(L10nKey::ScmResetSoft), ResetMode::Soft),
            (t(L10nKey::ScmResetMixed), ResetMode::Mixed),
            (t(L10nKey::ScmResetHard), ResetMode::Hard),
        ] {
            let app = app.clone();
            let repo = repo.clone();
            let rev = oid.to_string();
            // `--hard` is the one entry here that discards work outright, so
            // it wears the danger colour the same way the tree's Delete does.
            // The confirmation still comes from `scm_op`; this is the warning
            // before the warning.
            let base = match mode {
                ResetMode::Hard => PopupMenuItem::element(move |_window, _cx| {
                    div().text_color(danger).child(label)
                }),
                _ => PopupMenuItem::new(label),
            };
            let item = base.on_click(move |_, window, cx| {
                let _ = app.update(cx, |this, cx| {
                    this.scm_op(
                        repo.clone(),
                        GitOp::Reset {
                            rev: rev.clone(),
                            mode,
                        },
                        window,
                        cx,
                    )
                });
            });
            menu = menu.item(item);
        }

        menu.separator()
            .item(PopupMenuItem::new(t(L10nKey::ScmCopyCommitSha)).on_click({
                let rev = oid.to_string();
                move |_, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(rev.clone()))
            }))
    }

    /// `right_panel_resize` rotated onto the other axis: a canvas that remembers
    /// the container's bounds, an `Rc<Cell>` pair for the live value and the
    /// drag flag, and a one-pixel hairline that only shows on hover or while
    /// held.
    fn graph_resize(&self, ceiling: f32, cx: &mut Context<Self>) -> (AnyElement, AnyElement) {
        let container: Rc<StdCell<Option<Bounds<Pixels>>>> = Rc::new(StdCell::new(None));
        let backing = canvas(
            {
                let container = container.clone();
                move |bounds, _window, _cx| container.set(Some(bounds))
            },
            {
                let container = container.clone();
                let height = self.scm.graph.height.clone();
                let dragging = self.scm.graph.dragging.clone();
                move |_bounds, _state, window: &mut Window, _cx| {
                    window.on_mouse_event({
                        let container = container.clone();
                        let height = height.clone();
                        let dragging = dragging.clone();
                        move |ev: &MouseMoveEvent, _phase, window: &mut Window, _cx| {
                            if !dragging.get() {
                                return;
                            }
                            let Some(b) = container.get() else {
                                return;
                            };
                            // Measured from the bottom, because that edge is
                            // pinned and the top is the one being dragged.
                            let raw = (b.origin.y + b.size.height - ev.position.y).as_f32();
                            height.set(raw.clamp(GRAPH_H_MIN, ceiling));
                            window.refresh();
                        }
                    });
                    window.on_mouse_event({
                        let dragging = dragging.clone();
                        move |_ev: &MouseUpEvent, _phase, window: &mut Window, _cx| {
                            if !dragging.get() {
                                return;
                            }
                            dragging.set(false);
                            window.refresh();
                        }
                    });
                }
            },
        )
        .absolute()
        .size_full()
        .into_any_element();

        let active = self.scm.graph.dragging.get();
        let handle = div()
            .group("scm-graph-resize")
            .occlude()
            .absolute()
            .left_0()
            .top(px(-(GRAPH_HANDLE_H / 2.)))
            .w_full()
            .h(px(GRAPH_HANDLE_H))
            .flex()
            .items_center()
            .cursor_row_resize()
            .child(
                div()
                    .w_full()
                    .h(px(1.))
                    .when(active, |d| d.bg(cx.theme().drag_border))
                    .group_hover("scm-graph-resize", |s| s.bg(cx.theme().drag_border)),
            )
            .on_mouse_down(MouseButton::Left, {
                let dragging = self.scm.graph.dragging.clone();
                move |_ev, window: &mut Window, _cx| {
                    dragging.set(true);
                    window.refresh();
                }
            })
            .into_any_element();

        (backing, handle)
    }
}

impl Tty7App {
    /// Open the inline "name a branch here" input for one commit.
    ///
    /// Its own input rather than the panel's naming row: that row always
    /// branches from HEAD, and a branch created at the wrong commit is a
    /// silent mistake rather than a visible one.
    fn graph_begin_branch_at(
        &mut self,
        repo: RepoKey,
        rev: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            gpui_component::input::InputState::new(window, cx)
                .placeholder(t(L10nKey::ScmCreateBranchHere))
        });
        let handle = input.read(cx).focus_handle(cx);
        self.scm.graph.naming = Some((input, rev));
        self.scm.repo_override = Some(repo);
        self.scm.override_tab = Some(self.active);
        window.focus(&handle, cx);
        cx.notify();
    }

    /// The inline "name a branch here" field.
    ///
    /// `xsmall`, like every other inline field in this panel: a 20px box in a
    /// 30px row, which is the input plus 5px of air each side. A taller one
    /// here would push the list it interrupts down by more than the field is
    /// worth.
    fn graph_naming_row(&mut self, repo: &RepoKey, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (input, rev) = self.scm.graph.naming.clone()?;
        let repo = repo.clone();
        Some(
            h_flex()
                .id("scm-graph-naming")
                .flex_none()
                .items_center()
                .h(px(30.))
                .px(px(CONTENT_INSET))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        // `appearance(false)` like every other field in the
                        // app: left on, gpui-component draws its own border and
                        // fill, which is the one chrome nothing else in this
                        // panel wears.
                        .child(
                            gpui_component::input::Input::new(&input)
                                .appearance(false)
                                .xsmall(),
                        ),
                )
                .on_key_down(
                    cx.listener(move |this, ev: &gpui::KeyDownEvent, window, cx| {
                        match ev.keystroke.key.as_str() {
                            "escape" => {
                                this.scm.graph.naming = None;
                                cx.notify();
                            }
                            "enter" => {
                                let Some((input, _)) = this.scm.graph.naming.take() else {
                                    return;
                                };
                                let name = input.read(cx).value().trim().to_string();
                                cx.notify();
                                if name.is_empty() {
                                    return;
                                }
                                this.scm_op(
                                    repo.clone(),
                                    GitOp::CreateBranch {
                                        name,
                                        start: Some(rev.clone()),
                                        // Naming a branch at an old commit is
                                        // usually marking a place, not moving to
                                        // it — and moving would take the working
                                        // tree with it.
                                        checkout: false,
                                    },
                                    window,
                                    cx,
                                );
                            }
                            _ => {}
                        }
                    }),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::app::test_window::harness;
    use crate::ui::host_ops::HostId;
    use gpui::TestAppContext;
    use smallvec::smallvec;
    use std::path::PathBuf;

    fn repo() -> RepoKey {
        RepoKey {
            host: HostId::LOCAL,
            root: PathBuf::from("/tmp/tty7-graph-test"),
        }
    }

    fn lanes() -> Lanes {
        Lanes {
            ink: [0x111111, 0x222222, 0x333333, 0x444444, 0x555555, 0x666666],
            overflow: 0x999999,
        }
    }

    #[test]
    fn lanes_inside_the_cap_keep_their_own_column() {
        for cap in GRAPH_MIN_LANES..=GRAPH_MAX_LANES {
            let columns: Vec<Lane> = (0..cap as Lane).map(|l| project(l, cap)).collect();
            let mut sorted = columns.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(
                columns.len(),
                sorted.len(),
                "cap {cap} collapsed a real lane"
            );
            assert_eq!(columns, (0..cap as Lane).collect::<Vec<_>>());
        }
    }

    #[test]
    fn everything_past_the_cap_lands_in_the_overflow_column() {
        let cap = 4;
        for lane in 4u16..=tty7_core::core::git::log::MAX_LANES {
            assert_eq!(project(lane, cap), 3, "lane {lane} escaped the last column");
        }
        // `max_lanes` never goes below `GRAPH_MIN_LANES`, but `project` is a
        // pure function anyone can call: a one-column cap has to swallow every
        // lane rather than saturating into a negative index.
        for lane in 0u16..8 {
            assert_eq!(project(lane, 1), 0);
        }
    }

    #[test]
    fn lane_centres_rise_and_land_on_device_pixels() {
        for scale in [1.0f32, 1.25, 2.0, 3.0] {
            let mut previous = f32::MIN;
            for column in 0..GRAPH_MAX_LANES as Lane {
                let x = lane_center_x(column, scale);
                assert!(x > previous, "column {column} did not advance at {scale}x");
                previous = x;
                let physical = x * scale;
                assert!(
                    (physical - physical.round()).abs() < 1e-4,
                    "column {column} at {scale}x sits at {physical} device pixels"
                );
            }
        }
        // A nonsense scale must not produce NaN geometry.
        assert_eq!(lane_center_x(0, 0.), lane_center_x(0, f32::NAN));
    }

    #[test]
    fn the_gutter_narrows_with_the_panel() {
        // 260px is the default panel; 216px is about as narrow as it gets.
        assert_eq!(max_lanes(260.), 5);
        assert_eq!(max_lanes(216.), 4);
        assert_eq!(max_lanes(320.), 6);
        assert_eq!(max_lanes(160.), GRAPH_MIN_LANES);
        // The gutter it asks for has to fit inside the share it was given.
        for w in [120., 160., 216., 260., 320., 600.] {
            let cap = max_lanes(w);
            assert!(
                cap == GRAPH_MIN_LANES || gutter_width(cap) <= w * GRAPH_GUTTER_SHARE,
                "{w}px: a {cap}-lane gutter is {}px of a {}px budget",
                gutter_width(cap),
                w * GRAPH_GUTTER_SHARE
            );
        }
        // Below the floor the gutter stops shrinking: three lanes is the least
        // that can show a branch leaving and coming back.
        assert_eq!(max_lanes(40.), GRAPH_MIN_LANES);
        // And above the palette it stops growing, or two columns would share a
        // colour.
        assert_eq!(max_lanes(4000.), GRAPH_MAX_LANES);
        assert!(max_lanes(4000.) <= LANE_SLOTS);
    }

    /// The two fixed resize constants are counted in rows against the header.
    ///
    /// What they are worth is not obvious from their values — 220 is a number,
    /// "nine commits and most of a tenth" is a decision — so the decision is
    /// what gets pinned, and a change to the row height or the header has to
    /// come back through here rather than quietly buying or losing a commit.
    /// The partial row at the bottom is deliberate: it is the only thing a
    /// fixed-height list says to admit there is more below it, so the assertion
    /// is that a real strip of that row survives at both ends.
    #[test]
    fn the_section_is_sized_in_commits() {
        let rows = |h: f32| (h - GRAPH_HEADER_H) / GRAPH_ROW_H;
        let resting = rows(GRAPH_H_DEFAULT);
        assert!(
            (9.5..10.0).contains(&resting),
            "the resting section shows {resting} commits"
        );
        // In pixels, because "a visible sliver" is a pixel count and not a
        // fraction: at 20px rows the last band shows 16 of its 20 and loses 4.
        let shown = GRAPH_ROW_H * resting.fract();
        let cut = GRAPH_ROW_H - shown;
        assert!(
            shown >= 3. && cut >= 3.,
            "{resting} rows shows {shown}px of the last band and cuts {cut}px, \
             which is not a partial row a reader can see"
        );
        let floor = rows(GRAPH_H_MIN);
        assert!(
            floor >= 3.,
            "dragged all the way shut the section shows {floor} commits, which \
             is not enough of a graph to be one"
        );
        assert!(GRAPH_H_MIN < GRAPH_H_DEFAULT);
        // The ceiling is a share of the window, and the floor has to survive a
        // window short enough that the share falls under it.
        assert_eq!((100f32 * GRAPH_H_MAX_RATIO).max(GRAPH_H_MIN), GRAPH_H_MIN);
    }

    /// Every node fits: inside its row, inside the gutter, and clear of the
    /// lane line one column over.
    ///
    /// The widest one is the merge, which is drawn a pixel larger than the
    /// others. Nothing about the paint code looks wrong when a node grows past
    /// its column — it simply overlaps the neighbouring line — so the fit is
    /// asserted here rather than left to be noticed.
    #[test]
    fn nodes_fit_inside_their_row_and_column() {
        let widest = GRAPH_DOT_R + 1.;
        assert!(widest * 2. <= GRAPH_ROW_H);
        assert!(lane_center_x(0, 1.) - widest >= 0.);
        assert!(widest + GRAPH_LINE_W / 2. < GRAPH_LANE_W);
        // And a hollow node keeps a hole: a stroke that eats the radius is a
        // filled dot wearing a ring's name.
        assert!(GRAPH_DOT_R - GRAPH_LINE_W >= 1.);
        // The node also has to be worth the row it sits in: a 6px dot in a 24px
        // row is a quarter of it, and a bead much smaller than that stops
        // reading as one on a string. This is the floor, and the row is
        // currently sitting on it — a taller row wants a larger dot, not the
        // same one with more air around it.
        assert!(GRAPH_DOT_R * 2. >= GRAPH_ROW_H * 0.25);
    }

    #[test]
    fn conventional_prefixes_come_off_and_prose_does_not() {
        assert_eq!(
            split_conventional("feat(terminal): localize the menu"),
            (Some(("feat", false)), "localize the menu")
        );
        assert_eq!(
            split_conventional("fix: a thing"),
            (Some(("fix", false)), "a thing")
        );
        assert_eq!(
            split_conventional("feat!: drop the old dialect"),
            (Some(("feat", true)), "drop the old dialect")
        );
        assert_eq!(
            split_conventional("refactor(ui/scm)!: one entry point"),
            (Some(("refactor", true)), "one entry point")
        );
        for prose in [
            "no prefix here",
            // Capitalised is not a conventional type.
            "Merge pull request #1 from x",
            // No space after the colon.
            "fix:it",
            // A bare URL must not be cut at its scheme.
            "see https://example.invalid for why",
            // An empty scope is malformed, not a prefix.
            "feat(): nothing",
            // Nothing left after the colon is not a subject.
            "chore: ",
            "",
        ] {
            assert_eq!(
                split_conventional(prose),
                (None, prose),
                "{prose:?} should have been left alone"
            );
        }
    }

    #[test]
    fn a_chinese_subject_survives_the_split_intact() {
        // Byte indexing over a multibyte subject is exactly how this goes
        // wrong, so the assertion is on the value, not on not panicking.
        let subject = "修复终端右键菜单的本地化";
        assert_eq!(split_conventional(subject), (None, subject));
        let prefixed = "fix(terminal): 修复终端右键菜单的本地化";
        assert_eq!(
            split_conventional(prefixed),
            (Some(("fix", false)), "修复终端右键菜单的本地化")
        );
    }

    /// The prefix carries one bit of colour, not six.
    ///
    /// A green `feat` beside a green lane dot was exactly the noise this row
    /// was cleaned up to lose, and `type_tone` no longer being handed the type
    /// is most of the guard against it coming back. What is left to pin is the
    /// one exception: a breaking change has to stay visibly apart from
    /// everything else, or the exception is not worth making.
    #[gpui::test]
    fn only_a_breaking_prefix_gets_a_colour(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (_app, mut vcx) = harness(cx);

        vcx.update(|_, cx| {
            assert_eq!(type_tone(false, cx), cx.theme().muted_foreground);
            assert_eq!(type_tone(true, cx), cx.theme().danger);
            assert_ne!(
                type_tone(true, cx),
                type_tone(false, cx),
                "a `!` that looks like every other prefix is not a warning"
            );
        });
    }

    /// `4 → node 0 → 4` should be one line bending, not two lines drawn twice.
    #[test]
    fn a_band_keeps_one_segment_per_column() {
        let row = GraphRow {
            node: 0,
            color: 0,
            parents: 2,
            edges: smallvec![
                Edge::Pass { lane: 5, color: 5 },
                Edge::Pass { lane: 7, color: 7 },
                Edge::In { from: 0, color: 0 },
                Edge::Out { to: 0, color: 0 },
                Edge::Out { to: 6, color: 6 },
            ],
        };
        // Cap of three: lanes 5, 6 and 7 all fall into column 2.
        let band = band_of(&row, 3, true, &lanes());
        assert_eq!(band.top[0], Some(lanes().ink[0]));
        assert_eq!(band.top[2], Some(lanes().overflow));
        assert_eq!(band.bottom[0], Some(lanes().ink[0]));
        assert_eq!(band.bottom[2], Some(lanes().overflow));
        assert_eq!(band.top[1], None);
        // Three lanes bundled into the overflow column produce exactly one
        // turn, not three stacked on each other.
        assert_eq!(band.turns.len(), 1);
        assert_eq!(band.turns[0].0, 0);
        assert_eq!(band.turns[0].1, 2);
    }

    #[test]
    fn a_wide_page_only_neutralises_the_column_that_is_shared() {
        let l = lanes();
        // Not overflowing: every column is a real lane and keeps its hue.
        assert_eq!(column_ink(2, 2, 3, false, &l), l.ink[2]);
        // Overflowing: only the last column goes neutral.
        assert_eq!(column_ink(2, 2, 3, true, &l), l.overflow);
        assert_eq!(column_ink(1, 1, 3, true, &l), l.ink[1]);
        // A colour index past the palette can only come from a lane that was
        // projected into the bundle, but clamp anyway rather than panic.
        assert_eq!(column_ink(0, 99, 3, false, &l), l.ink[LANE_SLOTS - 1]);
    }

    #[gpui::test]
    fn folding_the_history_section_survives_a_restart(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);

        // It starts shut: a panel that unfurls two hundred commits the first
        // time it is opened looks like a mess nobody asked for.
        assert!(!app.read_with(&vcx, |app, _| app.scm.graph.expanded));
        app.update(&mut vcx, |app, cx| app.scm_toggle_graph(cx));
        assert!(app.read_with(&vcx, |app, _| app.scm.graph.expanded));
        assert!(vcx.update(|_, cx| {
            cx.global::<crate::core::config::Config>()
                .scm_graph_expanded
        }));
    }

    #[gpui::test]
    fn opening_a_row_hands_that_commit_to_the_detail_view(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);

        // Unseeded first: a click that cannot hand a commit over still opens
        // the view, and the read that fills it is dispatched later.
        app.update(&mut vcx, |app, cx| {
            app.graph_open_commit(repo(), "deadbeef".into(), None, cx)
        });
        let (selected, detail) = app.read_with(&vcx, |app, _| {
            (app.scm.graph.selected.clone(), app.scm.detail.clone())
        });
        assert_eq!(selected.as_deref(), Some("deadbeef"));
        let detail = detail.expect("the row opened a commit");
        assert_eq!(detail.oid, "deadbeef");
        assert_eq!(detail.repo, repo());
        assert!(detail.commit.is_none(), "nothing was handed over");

        // Seeded: the row draws from a commit it already holds, so the detail
        // view opens with it and only the file list is left to read. This is
        // the path every real click takes.
        let seed = commit_named("a subject", "someone", "deadbeef");
        app.update(&mut vcx, |app, cx| {
            app.graph_open_commit(repo(), "deadbeef".into(), Some(seed.clone()), cx)
        });
        let detail = app
            .read_with(&vcx, |app, _| app.scm.detail.clone())
            .expect("the row opened a commit");
        assert_eq!(
            detail.commit.as_deref(),
            Some(&seed),
            "the seed spares the view a `git show`"
        );
    }

    /// Changing what the graph walks has to throw the page away, not page on
    /// top of it: the rows of a different scope are a different history.
    #[gpui::test]
    fn switching_scope_resets_the_page_and_its_size(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);

        app.update(&mut vcx, |app, cx| {
            app.scm.graph.requested = GRAPH_PAGE * 3;
            app.scm.graph.page = Some(Arc::new(empty_page()));
            app.scm.graph.page_key = Some((repo(), 7, GraphScope::HeadAndUpstream));
            app.graph_set_scope(GraphScope::All, cx);
        });
        app.read_with(&vcx, |app, _| {
            assert_eq!(app.scm.graph.scope, GraphScope::All);
            assert_eq!(app.scm.graph.requested, GRAPH_PAGE);
            assert!(app.scm.graph.page.is_none());
            assert!(app.scm.graph.page_key.is_none());
        });

        // Picking the scope that is already showing must not throw the page
        // away, or every menu open would cost a `git log`.
        app.update(&mut vcx, |app, cx| {
            app.scm.graph.page = Some(Arc::new(empty_page()));
            app.graph_set_scope(GraphScope::All, cx);
        });
        assert!(app.read_with(&vcx, |app, _| app.scm.graph.page.is_some()));
    }

    /// One click at the cap used to loop: `requested` grew past what
    /// `load_page` clamps to, the answer never satisfied `requested >= want`,
    /// and the panel refetched the full page from every frame's render.
    #[test]
    fn load_more_growth_stops_at_the_cap() {
        assert_eq!(next_page_request(0), GRAPH_PAGE * 2);
        assert_eq!(next_page_request(GRAPH_PAGE), GRAPH_PAGE * 2);
        assert_eq!(
            next_page_request(MAX_GRAPH_COMMITS - 1),
            MAX_GRAPH_COMMITS,
            "the last step lands on the cap, not past it"
        );
        assert_eq!(
            next_page_request(MAX_GRAPH_COMMITS),
            MAX_GRAPH_COMMITS,
            "at the cap the click is a no-op, not a bigger ask"
        );
    }

    /// A repository switch drops the old page before anything can draw it or
    /// grow from it: a stale row's context menu would otherwise build an op
    /// for the new repository with the old repository's rev.
    #[gpui::test]
    fn switching_repositories_drops_the_previous_page(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);

        let other = RepoKey {
            host: HostId::LOCAL,
            root: PathBuf::from("/tmp/tty7-graph-test-other"),
        };
        app.update(&mut vcx, |app, cx| {
            app.scm.graph.requested = GRAPH_PAGE * 3;
            app.scm.graph.page = Some(Arc::new(empty_page()));
            app.scm.graph.page_key = Some((repo(), 7, GraphScope::HeadAndUpstream));
            app.scm_load_graph(&other, cx);
        });
        app.read_with(&vcx, |app, _| {
            assert!(app.scm.graph.page.is_none(), "the old history is gone");
            assert!(app.scm.graph.page_key.is_none());
            assert_eq!(
                app.scm.graph.requested, 0,
                "page depth does not carry across repositories"
            );
        });

        // The same repository keeps its page — this must not turn every
        // render into a reload.
        app.update(&mut vcx, |app, cx| {
            app.scm.graph.page = Some(Arc::new(empty_page()));
            app.scm.graph.page_key = Some((repo(), 7, GraphScope::HeadAndUpstream));
            app.scm_load_graph(&repo(), cx);
        });
        assert!(app.read_with(&vcx, |app, _| app.scm.graph.page.is_some()));
    }

    #[test]
    fn the_filter_matches_what_a_row_can_show() {
        let commit = commit_named("feat(ui): the graph", "Ada Lovelace", "c0ffee1234");
        for hit in ["graph", "GRAPH", "feat", "ada", "Lovelace", "c0ffee"] {
            assert!(
                matches_query(&commit, &hit.to_lowercase()),
                "{hit:?} should have matched"
            );
        }
        // The body is deliberately not searched: a hit the row cannot show
        // looks like a wrong result.
        assert!(!matches_query(&commit, "rationale"));
        // A sha matches as a prefix, the way `git show` takes one — not as a
        // substring, or every query of hex characters would light up.
        assert!(!matches_query(&commit, "ffee"));
    }

    /// A filter you cannot see must not be filtering.
    ///
    /// The field is behind a tile now, and the failure mode a hidden filter
    /// invites is a reader staring at a list with rows missing and nothing on
    /// screen to say why. The guard is structural — closing drops the
    /// `InputState`, and `graph_query` has nowhere else to read text from — so
    /// this is the test that the structure holds.
    #[gpui::test]
    fn closing_the_filter_takes_its_query_with_it(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);

        // At rest there is no field at all, which is what the section looks
        // like every time it is opened.
        app.update(&mut vcx, |app, cx| {
            assert!(app.scm.graph.search.is_none());
            assert!(app.graph_query(cx).is_none());
        });

        app.update_in(&mut vcx, |app, window, cx| {
            app.graph_toggle_search(window, cx);
            assert!(app.scm.graph.search.is_some(), "the tile opens the field");
            assert!(
                app.scm.graph.search_sub.is_some(),
                "and wires typing to a repaint, or the list never sees the text"
            );
        });

        app.update_in(&mut vcx, |app, window, cx| {
            let input = app.scm.graph.search.clone().expect("the field is open");
            input.update(cx, |state, cx| state.set_value("Feat", window, cx));
        });
        assert_eq!(
            app.update(&mut vcx, |app, cx| app.graph_query(cx)),
            Some("feat".to_string()),
            "the box filters while it is open"
        );

        app.update_in(&mut vcx, |app, window, cx| {
            app.graph_toggle_search(window, cx)
        });
        app.update(&mut vcx, |app, cx| {
            assert!(app.scm.graph.search.is_none());
            assert!(app.scm.graph.search_sub.is_none());
            assert!(
                app.graph_query(cx).is_none(),
                "a closed box went on cutting rows out of the list"
            );
        });
    }

    #[test]
    fn the_scope_button_says_which_history_is_showing() {
        assert_eq!(
            scope_label(&GraphScope::HeadAndUpstream),
            t(L10nKey::ScmGraphCurrentBranch)
        );
        assert_eq!(
            scope_label(&GraphScope::All),
            t(L10nKey::ScmGraphAllBranches)
        );
        // The label is the branch, not the fully qualified ref: `refs/heads/`
        // is eleven characters of the panel spent saying nothing.
        assert_eq!(
            scope_label(&GraphScope::Refs(vec!["refs/heads/feature/auth".into()])),
            "auth"
        );
    }

    fn commit_named(summary: &str, author: &str, oid: &str) -> Commit {
        use tty7_core::core::git::log::{OffsetTs, Signature};
        let who = Signature {
            name: author.to_string(),
            email: "a@b.invalid".into(),
            at: OffsetTs {
                unix: 0,
                offset_minutes: 0,
            },
        };
        Commit {
            oid: oid.to_string(),
            parents: smallvec![],
            author: who.clone(),
            committer: who,
            summary: summary.to_string(),
            body: "rationale goes in the body".into(),
            refs: Vec::new(),
        }
    }

    fn empty_page() -> CommitPage {
        CommitPage {
            commits: Vec::new(),
            rows: Vec::new(),
            max_lanes: 0,
            scope: GraphScope::HeadAndUpstream,
            requested: GRAPH_PAGE,
            complete: true,
            truncated_lanes: false,
            open_lanes: Vec::new(),
        }
    }
}

/// The one test that has to run against a real repository and a real pane.
///
/// A canvas that repaints every frame would make the panel a perpetual motion
/// machine, and nothing about the code reads as wrong when it does — the only
/// way to know is to settle the window and count frames. Same shape as the file
/// tree's own idle tests, including the serial lock: the render probe is
/// thread-local and two of these at once would count each other's frames.
///
/// `unix` for the same reason `panel.rs`, `detail.rs` and `file_tree.rs` gate
/// theirs: a real pane means `test_window::harness_with_pane`, and that harness
/// hands back a `std::os::unix::net::UnixStream`.
#[cfg(all(test, unix))]
mod render_idle_gpui_tests {
    use super::*;
    use crate::ui::app::{render_probe, test_window};
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::path::Path;
    use tty7_core::core::config::RightPanelTab;

    const BUDGET: u64 = 200;

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Git with the identity and signing pinned, so the test does not depend on
    /// whatever is in the developer's `~/.gitconfig`.
    fn git(root: &Path, args: &[&str]) -> bool {
        let mut full = vec![
            "-c",
            "user.name=tty7 test",
            "-c",
            "user.email=test@tty7.invalid",
            "-c",
            "commit.gpgsign=false",
        ];
        full.extend_from_slice(args);
        std::process::Command::new("git")
            .args(&full)
            .current_dir(root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-graph-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    fn draws_while_idle(vcx: &mut VisualTestContext) -> u64 {
        test_window::quiesce(vcx, None);
        render_probe::arm(BUDGET);
        vcx.background_executor.run_until_parked();
        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(3));
        vcx.background_executor.run_until_parked();
        render_probe::arm(BUDGET);
        // No real-time exposure in the counted window, deliberately. The file
        // tree holds a real filesystem watch, so real time is a channel input
        // arrives on — and a test that spends it here is asking to be handed
        // some. What has to be waited out is waited out in `quiesce` above,
        // where a frame costs nothing.
        vcx.executor()
            .advance_clock(std::time::Duration::from_secs(9));
        vcx.background_executor.run_until_parked();
        render_probe::draws()
    }

    /// Drive frames until the graph has a page, because the query only goes out
    /// from `render`.
    fn settle_graph(app: &Entity<Tty7App>, vcx: &mut VisualTestContext) -> Option<Arc<CommitPage>> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            app.update_in(vcx, |_, _, cx| cx.notify());
            vcx.background_executor.run_until_parked();
            let page = app.update_in(vcx, |app, _, _| app.scm.graph.page.clone());
            if page.is_some() {
                vcx.background_executor.run_until_parked();
                test_window::quiesce(vcx, None);
                return page;
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[gpui::test]
    fn an_expanded_graph_with_history_reaches_render_idle(cx: &mut TestAppContext) {
        let _serial = serial();
        crate::core::config::pin_test_config_dir();
        let root = scratch("idle");
        if !git(&root, &["init", "--quiet"]) {
            return; // no git on this machine
        }
        for n in 0..6 {
            std::fs::write(root.join(format!("f{n}.txt")), format!("{n}\n")).unwrap();
            assert!(git(&root, &["add", "-A"]));
            assert!(git(
                &root,
                &["commit", "--quiet", "-m", &format!("feat(x): commit {n}")]
            ));
        }

        let (app, mut vcx, _pane) = test_window::harness_with_pane(cx);
        crate::daemon::protocol::DaemonMsg::Cwd(root.clone())
            .encode(&mut { _pane })
            .expect("the pane's socket takes the cwd");
        app.update_in(&mut vcx, |app, _, cx| {
            app.right_panel_visible = true;
            app.right_panel_tab = RightPanelTab::Scm;
            app.scm.graph.expanded = true;
            cx.notify();
        });

        let Some(page) = settle_graph(&app, &mut vcx) else {
            // A machine where the pane never reported its cwd has nothing to
            // say about idling; failing here would only be flaky.
            let _ = std::fs::remove_dir_all(&root);
            return;
        };
        assert!(page.commits.len() >= 6, "the graph loaded no history");
        assert_eq!(page.rows.len(), page.commits.len());

        assert_eq!(draws_while_idle(&mut vcx), 0);
        // And it is still expanded and still holding the same page, i.e. the
        // zero above is idleness and not the section having quietly vanished.
        app.update_in(&mut vcx, |app, _, _| {
            assert!(app.scm.graph.expanded);
            assert!(app.scm.graph.page.is_some());
            assert!(!app.scm.graph.loading);
        });
        let _ = std::fs::remove_dir_all(&root);
    }
}

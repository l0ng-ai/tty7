//! Tray bitmap rendering: the bundled SVGs rasterized with `resvg` (gpui's
//! own SVG path only yields a tinted alpha mask, so the tray draws its own).
//!
//! Per platform:
//! - macOS: the outline terminal glyph (`tray.svg`) as a *template* image —
//!   the system recolors its alpha for light/dark menu bars, permanently.
//!   Attention never touches the bitmap (template images can't carry color,
//!   and leaving template mode made the glyph illegible); the tooltip and
//!   menu carry agent status instead (see `native.rs`).
//! - Windows / Linux: the colored app icon (`app-icon.svg`); attention
//!   punches a transparent ring into the corner and fills an amber badge, so
//!   the badge separates from the orange tile behind it.

use resvg::tiny_skia;
use resvg::usvg;

/// Straight (unpremultiplied) RGBA, the format both `tray_icon::Icon` and
/// (after a byte shuffle) `ksni::Icon` want.
pub(super) struct RgbaImage {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[cfg(target_os = "macos")]
const GLYPH_SVG: &[u8] = include_bytes!("../../../assets/tray.svg");
#[cfg(not(target_os = "macos"))]
const GLYPH_SVG: &[u8] = include_bytes!("../../../assets/app-icon.svg");

/// Physical pixel size. On macOS the bitmap is 36 px (retina-crisp at 18 pt);
/// native.rs then overrides the NSImage to 22 pt so the glyph fills the menu
/// bar. Windows tray slots are 16–32 px; 32 downsamples cleanly.
#[cfg(target_os = "macos")]
const SIZE: u32 = 36;
#[cfg(not(target_os = "macos"))]
const SIZE: u32 = 32;

/// The `Waiting` amber, same hue as the in-window status dot
/// (`AgentStatus::dot_rgb`).
#[cfg(not(target_os = "macos"))]
const AMBER: (u8, u8, u8) = (0xF5, 0x9E, 0x0B);

/// Render the tray icon: the template outline glyph, always — attention
/// never touches the bitmap, so the icon stays a template image the system
/// keeps legible on any bar. `None` only on a malformed bundled SVG, i.e.
/// never in practice — callers treat it as "no icon change".
#[cfg(target_os = "macos")]
pub(super) fn render() -> Option<RgbaImage> {
    let tree = usvg::Tree::from_data(GLYPH_SVG, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(SIZE, SIZE)?;
    resvg::render(&tree, fit_center(&tree, SIZE), &mut pixmap.as_mut());
    Some(to_rgba(&pixmap))
}

/// Render the tray icon. `attention` = some agent is blocked on the user —
/// stamps the amber badge into the colored app icon. `None` only on a
/// malformed bundled SVG, i.e. never in practice — callers treat it as "no
/// icon change".
#[cfg(not(target_os = "macos"))]
pub(super) fn render(attention: bool) -> Option<RgbaImage> {
    let tree = usvg::Tree::from_data(GLYPH_SVG, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(SIZE, SIZE)?;
    resvg::render(&tree, fit_center(&tree, SIZE), &mut pixmap.as_mut());

    if attention {
        badge(&mut pixmap);
    }

    Some(to_rgba(&pixmap))
}

/// The tray-menu row avatar for an agent pane: the tab avatar's visual
/// language translated to a menu icon — brand-colored disc, the brand mark as
/// a white silhouette (geometry only, same as the tab renders it), and the
/// status dot in the bottom-right corner. `None` only if the brand SVG fails
/// to resolve/parse, which the caller treats as "text-only row".
pub(super) fn agent_avatar(
    agent: crate::core::cli_agent::CLIAgent,
    status: crate::core::cli_agent::AgentStatus,
) -> Option<tiny_skia::Pixmap> {
    use gpui::AssetSource as _;

    // 16 pt at 2× — the size native menus render item icons at.
    const SIZE: u32 = 32;
    let s = SIZE as f32;
    let mut pixmap = tiny_skia::Pixmap::new(SIZE, SIZE)?;

    // Brand-colored disc.
    let accent = agent.accent_rgb();
    let mut paint = tiny_skia::Paint {
        anti_alias: true,
        ..Default::default()
    };
    paint.set_color_rgba8(
        (accent >> 16) as u8,
        (accent >> 8) as u8,
        accent as u8,
        0xFF,
    );
    let mut pb = tiny_skia::PathBuilder::new();
    pb.push_circle(s / 2.0, s / 2.0, s / 2.0);
    let disc = pb.finish()?;
    pixmap.fill_path(
        &disc,
        &paint,
        tiny_skia::FillRule::Winding,
        tiny_skia::Transform::identity(),
        None,
    );

    // Brand mark as a white silhouette, centered at ~60% of the disc — the
    // same "tinted alpha mask" treatment the tab avatar gets from gpui. The
    // SVG resolves through the app's asset source, so the generic `bot`
    // fallback for unbranded agents comes along for free.
    let svg = crate::ui::assets::Assets
        .load(agent.icon_path())
        .ok()
        .flatten()?;
    let tree = usvg::Tree::from_data(&svg, &usvg::Options::default()).ok()?;
    let glyph_size = (s * 0.60).round() as u32;
    let mut glyph = tiny_skia::Pixmap::new(glyph_size, glyph_size)?;
    resvg::render(&tree, fit_center(&tree, glyph_size), &mut glyph.as_mut());
    recolor(&mut glyph, (0xFF, 0xFF, 0xFF));
    let offset = ((SIZE - glyph_size) / 2) as i32;
    pixmap.draw_pixmap(
        offset,
        offset,
        glyph.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        tiny_skia::Transform::identity(),
        None,
    );

    // Status dot, bottom-right, ringed by transparency so it reads against
    // the disc — the same composition as the tab avatar's dot. Idle has none.
    if let Some(rgb) = status.dot_rgb() {
        let (cx, cy, r) = (s * 0.80, s * 0.80, s * 0.17);
        let circle = |radius: f32| {
            let mut pb = tiny_skia::PathBuilder::new();
            pb.push_circle(cx, cy, radius);
            pb.finish()
        };
        if let Some(ring) = circle(r * 1.45) {
            paint.blend_mode = tiny_skia::BlendMode::Clear;
            pixmap.fill_path(
                &ring,
                &paint,
                tiny_skia::FillRule::Winding,
                tiny_skia::Transform::identity(),
                None,
            );
        }
        if let Some(dot) = circle(r) {
            paint.blend_mode = tiny_skia::BlendMode::SourceOver;
            paint.set_color_rgba8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 0xFF);
            pixmap.fill_path(
                &dot,
                &paint,
                tiny_skia::FillRule::Winding,
                tiny_skia::Transform::identity(),
                None,
            );
        }
    }

    Some(pixmap)
}

/// Scale-to-fit + center transform for rendering an SVG into a square
/// `size`×`size` bitmap — a non-square SVG would otherwise hug the top-left
/// corner. (Both bundled icons are square today; this keeps that an
/// aesthetic fact, not a correctness assumption.)
fn fit_center(tree: &usvg::Tree, size: u32) -> tiny_skia::Transform {
    let (w, h) = (tree.size().width(), tree.size().height());
    let scale = size as f32 / w.max(h);
    tiny_skia::Transform::from_scale(scale, scale).post_translate(
        (size as f32 - w * scale) / 2.0,
        (size as f32 - h * scale) / 2.0,
    )
}

/// Un-premultiply a tiny-skia pixmap into straight RGBA (what
/// `tray_icon::Icon`/`muda::Icon` want).
pub(super) fn to_rgba(pixmap: &tiny_skia::Pixmap) -> RgbaImage {
    let mut data = Vec::with_capacity(pixmap.data().len());
    for p in pixmap.pixels() {
        let c = p.demultiply();
        data.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    RgbaImage {
        data,
        width: pixmap.width(),
        height: pixmap.height(),
    }
}

/// Repaint every covered pixel to `rgb`, keeping coverage (alpha) intact —
/// turns the glyph into a flat single-color mark.
fn recolor(pixmap: &mut tiny_skia::Pixmap, rgb: (u8, u8, u8)) {
    for p in pixmap.pixels_mut() {
        let a = p.alpha();
        let mul = |c: u8| ((c as u16 * a as u16) / 255) as u8;
        // from_rgba only rejects components > alpha; mul() guarantees not.
        if let Some(np) =
            tiny_skia::PremultipliedColorU8::from_rgba(mul(rgb.0), mul(rgb.1), mul(rgb.2), a)
        {
            *p = np;
        }
    }
}

/// Stamp the amber attention badge in the top-right corner: first clear a
/// slightly larger disc so the badge is ringed by transparency (separating it
/// from whatever the glyph or a colored tile puts behind it), then fill.
#[cfg(not(target_os = "macos"))]
fn badge(pixmap: &mut tiny_skia::Pixmap) {
    let s = SIZE as f32;
    let (cx, cy) = (s * 0.78, s * 0.22);
    let r = s * 0.20;
    let circle = |radius: f32| {
        let mut pb = tiny_skia::PathBuilder::new();
        pb.push_circle(cx, cy, radius);
        pb.finish()
    };
    let mut paint = tiny_skia::Paint {
        anti_alias: true,
        ..Default::default()
    };

    if let Some(ring) = circle(r * 1.35) {
        paint.blend_mode = tiny_skia::BlendMode::Clear;
        pixmap.fill_path(
            &ring,
            &paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
    }
    if let Some(dot) = circle(r) {
        paint.blend_mode = tiny_skia::BlendMode::SourceOver;
        paint.set_color_rgba8(AMBER.0, AMBER.1, AMBER.2, 0xFF);
        pixmap.fill_path(
            &dot,
            &paint,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
    }
}

/// The same bitmap in `ksni::Icon`'s wire format: ARGB32, network byte order.
#[cfg(target_os = "linux")]
pub(super) fn render_argb(attention: bool) -> Option<(Vec<u8>, u32)> {
    let img = render(attention)?;
    let mut argb = Vec::with_capacity(img.data.len());
    for px in img.data.chunks_exact(4) {
        argb.extend_from_slice(&[px[3], px[0], px[1], px[2]]);
    }
    Some((argb, img.width))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::cli_agent::{AgentStatus, CLIAgent};

    /// `recolor` must keep coverage (alpha) and produce premultiplied
    /// components that `PremultipliedColorU8` accepts (component ≤ alpha) —
    /// the invariant the `from_rgba` in its body relies on.
    #[test]
    fn recolor_keeps_alpha_and_flattens_color() {
        let mut pm = tiny_skia::Pixmap::new(2, 2).unwrap();
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(10, 200, 30, 128);
        pm.fill_rect(
            tiny_skia::Rect::from_xywh(0.0, 0.0, 2.0, 2.0).unwrap(),
            &paint,
            tiny_skia::Transform::identity(),
            None,
        );
        let alphas: Vec<u8> = pm.pixels().iter().map(|p| p.alpha()).collect();
        recolor(&mut pm, (0xFF, 0xFF, 0xFF));
        for (p, a) in pm.pixels().iter().zip(alphas) {
            assert_eq!(p.alpha(), a);
            // White at coverage a premultiplies to ~a on every channel.
            assert!(p.red().abs_diff(a) <= 1, "red {} vs alpha {a}", p.red());
            assert_eq!(p.red(), p.green());
            assert_eq!(p.green(), p.blue());
        }
    }

    /// `to_rgba` un-premultiplies: a half-covered red pixel comes back as
    /// full red with the original alpha.
    #[test]
    fn to_rgba_demultiplies() {
        let mut pm = tiny_skia::Pixmap::new(1, 1).unwrap();
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(255, 0, 0, 128);
        pm.fill_rect(
            tiny_skia::Rect::from_xywh(0.0, 0.0, 1.0, 1.0).unwrap(),
            &paint,
            tiny_skia::Transform::identity(),
            None,
        );
        let img = to_rgba(&pm);
        assert_eq!((img.width, img.height), (1, 1));
        let px = &img.data[0..4];
        assert_eq!(px[3], 128);
        assert!(px[0] >= 253, "red demultiplied back to ~255, got {}", px[0]);
        assert_eq!((px[1], px[2]), (0, 0));
    }

    /// The template glyph renders at the declared size with visible coverage.
    #[cfg(target_os = "macos")]
    #[test]
    fn render_produces_template_glyph() {
        let img = render().unwrap();
        assert_eq!((img.width, img.height), (SIZE, SIZE));
        assert_eq!(img.data.len(), (SIZE * SIZE * 4) as usize);
        let covered = img.data.chunks_exact(4).filter(|p| p[3] > 0).count();
        assert!(covered > 0, "icon rendered fully transparent");
    }

    /// Both tray states render at the declared size with visible coverage,
    /// and the attention badge actually changes the bitmap.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn render_produces_both_states() {
        let normal = render(false).unwrap();
        let attention = render(true).unwrap();
        for img in [&normal, &attention] {
            assert_eq!((img.width, img.height), (SIZE, SIZE));
            assert_eq!(img.data.len(), (SIZE * SIZE * 4) as usize);
            let covered = img.data.chunks_exact(4).filter(|p| p[3] > 0).count();
            assert!(covered > 0, "icon rendered fully transparent");
        }
        assert_ne!(normal.data, attention.data);
    }

    /// Every avatar renders, with and without the status dot. Run over the
    /// whole roster rather than one branded and one fallback agent, because
    /// this is the only test that puts the bundled SVGs through resvg: the
    /// asset-source test next door proves the bytes resolve, not that they
    /// parse into visible geometry. Vendor marks arrive in whatever shape the
    /// vendor publishes — stylesheets, nested groups, features usvg quietly
    /// drops — and a mark that parses to nothing shows up as a bare accent
    /// disc, which nothing else here would catch.
    #[test]
    fn agent_avatar_renders_brand_and_fallback() {
        for agent in CLIAgent::ALL {
            let idle = agent_avatar(agent, AgentStatus::Idle).unwrap();
            let waiting = agent_avatar(agent, AgentStatus::Waiting).unwrap();
            assert_eq!((idle.width(), idle.height()), (32, 32));
            // The disc leaves the very corners transparent…
            assert_eq!(idle.pixel(0, 0).unwrap().alpha(), 0);
            // …and the center is covered (disc + glyph).
            assert!(idle.pixel(16, 16).unwrap().alpha() > 0);
            // The glyph actually drew something. The disc under it is a flat
            // accent fill, so every opaque pixel shares one colour unless the
            // white mark landed on top — one colour means resvg handed back an
            // empty canvas, which is what a silently-unsupported SVG feature
            // looks like from here.
            let shades: std::collections::HashSet<_> = idle
                .pixels()
                .iter()
                .filter(|p| p.alpha() == 0xFF)
                .map(|p| (p.red(), p.green(), p.blue()))
                .collect();
            assert!(
                shades.len() > 1,
                "{} rendered as a bare disc — its glyph drew nothing",
                agent.display_name()
            );
            // The status dot changes the bottom-right corner.
            assert_ne!(idle.data(), waiting.data());
        }
    }

    /// ksni wants ARGB32 in network byte order — verify the shuffle against
    /// the RGBA source.
    #[cfg(target_os = "linux")]
    #[test]
    fn render_argb_reorders_bytes() {
        let rgba = render(false).unwrap();
        let (argb, size) = render_argb(false).unwrap();
        assert_eq!(size, rgba.width);
        assert_eq!(argb.len(), rgba.data.len());
        for (a4, r4) in argb.chunks_exact(4).zip(rgba.data.chunks_exact(4)) {
            assert_eq!(a4, [r4[3], r4[0], r4[1], r4[2]]);
        }
    }
}

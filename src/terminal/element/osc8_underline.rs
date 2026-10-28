//! An OSC 8 link's resting decoration: iTerm2's faint dotted underline, so a
//! `#1234` that is a link reads as one before the pointer finds it. It rides
//! the SGR 4:4 dotted painter, so it costs what an app's own dotted run does.

use alacritty_terminal::term::cell::Cell;

use super::{HELD_BACK_LINK_ALPHA, RenderCell, UnderlineKind};

/// A cell the application underlined, or gave an underline colour, keeps
/// its own decoration.
pub(super) fn mark(rc: &mut RenderCell, cell: &Cell) {
    if rc.underline == UnderlineKind::None
        && rc.underline_color.is_none()
        && cell.hyperlink().is_some()
    {
        rc.underline = UnderlineKind::Dotted;
        rc.underline_color = Some(rc.fg.opacity(HELD_BACK_LINK_ALPHA));
        rc.osc8_dots = true;
    }
}

/// Under the pointer the dots give way to the hovered link's solid line.
pub(super) fn unmark(rc: &mut RenderCell) {
    if std::mem::take(&mut rc.osc8_dots) {
        rc.underline = UnderlineKind::None;
        rc.underline_color = None;
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::index::{Column, Line, Point};
    use alacritty_terminal::term::cell::{Flags, Hyperlink};

    use tty7_vt::model::Rgb;

    use super::super::{
        GlyphStyle, PaintColors, invert_cursor_cell, snapshot_cell, tests::test_colors,
    };
    use super::*;

    fn linked(flags: Flags) -> Cell {
        let mut cell = Cell {
            c: '#',
            flags,
            ..Cell::default()
        };
        cell.set_hyperlink(Some(Hyperlink::new(
            None::<&str>,
            "https://example.com/3328".to_string(),
        )));
        cell
    }

    fn paint_fixture() -> ([Rgb; 256], PaintColors) {
        ([Rgb { r: 0, g: 0, b: 0 }; 256], test_colors())
    }

    #[test]
    fn a_hyperlink_rests_under_faint_dots() {
        let (palette, colors) = paint_fixture();
        let rc = snapshot_cell(
            &linked(Flags::empty()),
            Point::new(Line(0), Column(0)),
            &palette,
            &colors,
            None,
        );
        assert_eq!(rc.underline, UnderlineKind::Dotted);
        assert_eq!(
            rc.underline_color,
            Some(rc.fg.opacity(HELD_BACK_LINK_ALPHA))
        );
    }

    #[test]
    fn an_sgr_underlined_link_keeps_its_own_line_only() {
        let (palette, colors) = paint_fixture();
        let rc = snapshot_cell(
            &linked(Flags::UNDERLINE),
            Point::new(Line(0), Column(0)),
            &palette,
            &colors,
            None,
        );
        assert_eq!(rc.underline, UnderlineKind::Single);
        assert_eq!(rc.underline_color, None);
        assert!(!rc.osc8_dots);
    }

    #[test]
    fn hovering_swaps_the_dots_for_a_solid_line() {
        let (palette, colors) = paint_fixture();
        let mut rc = snapshot_cell(
            &linked(Flags::empty()),
            Point::new(Line(0), Column(0)),
            &palette,
            &colors,
            None,
        );
        unmark(&mut rc);
        rc.link_hover = true;
        assert_eq!(rc.underline, UnderlineKind::None);
        let solid = GlyphStyle::of(&rc)
            .underline_style()
            .expect("a solid underline");
        assert!(!solid.wavy);
    }

    #[test]
    fn the_dots_stay_faint_under_a_block_cursor() {
        let (palette, colors) = paint_fixture();
        let rc = snapshot_cell(
            &linked(Flags::empty()),
            Point::new(Line(0), Column(0)),
            &palette,
            &colors,
            None,
        );
        let mut buf = vec![rc];
        invert_cursor_cell(&mut buf, 1, 0, 0, &colors);
        let under = buf[0].underline_color.expect("the dots keep a colour");
        assert_eq!(under.a, buf[0].fg.a * HELD_BACK_LINK_ALPHA);
    }
}

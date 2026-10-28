//! An OSC 8 link's resting decoration: iTerm2's faint dotted underline, so a
//! `#1234` that is a link reads as one before the pointer finds it. It rides
//! the SGR 4:4 dotted painter, so it costs what an app's own dotted run does.

use tty7_vt::model::Cell;

use super::{HELD_BACK_LINK_ALPHA, RenderCell, UnderlineKind};

/// A cell the application underlined, or gave an underline colour, keeps
/// its own decoration.
pub(super) fn mark(rc: &mut RenderCell, cell: &Cell) {
    if rc.underline == UnderlineKind::None
        && rc.underline_color.is_none()
        && cell.hyperlink.is_some()
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
    use tty7_vt::model::{GridPoint, Rgb, StyleColor};

    use super::super::{
        GlyphStyle, PaintColors, invert_cursor_cell, snapshot_cell, tests::test_colors,
    };
    use super::*;

    fn linked(underline: tty7_vt::model::Underline) -> Cell {
        Cell {
            c: '#',
            underline,
            // Any link anchor: the frame carries a side-table index, and
            // presence alone decides the resting dots.
            hyperlink: Some(0),
            ..Cell::default()
        }
    }

    fn paint_fixture() -> ([Rgb; 256], PaintColors) {
        ([Rgb { r: 0, g: 0, b: 0 }; 256], test_colors())
    }

    fn at_origin(cell: &Cell) -> RenderCell {
        let (palette, colors) = paint_fixture();
        snapshot_cell(
            cell,
            GridPoint { line: 0, col: 0 },
            &palette,
            &colors,
            None,
        )
    }

    #[test]
    fn a_hyperlink_rests_under_faint_dots() {
        let rc = at_origin(&linked(tty7_vt::model::Underline::None));
        assert_eq!(rc.underline, UnderlineKind::Dotted);
        assert_eq!(
            rc.underline_color,
            Some(rc.fg.opacity(HELD_BACK_LINK_ALPHA))
        );
    }

    #[test]
    fn an_sgr_underlined_link_keeps_its_own_line_only() {
        let rc = at_origin(&linked(tty7_vt::model::Underline::Single));
        assert_eq!(rc.underline, UnderlineKind::Single);
        assert_eq!(rc.underline_color, None);
        assert!(!rc.osc8_dots);
    }

    #[test]
    fn an_sgr_underline_color_beats_the_link_to_the_line() {
        // SGR 58 without SGR 4: the app asked for a colour under an
        // ununderlined cell, and the link's dots defer to it.
        let mut cell = linked(tty7_vt::model::Underline::None);
        cell.underline_color = StyleColor::Palette(3);
        let rc = at_origin(&cell);
        assert_eq!(rc.underline, UnderlineKind::None);
        assert!(rc.underline_color.is_some());
        assert!(!rc.osc8_dots);
    }

    #[test]
    fn hovering_swaps_the_dots_for_a_solid_line() {
        let mut rc = at_origin(&linked(tty7_vt::model::Underline::None));
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
        let rc = at_origin(&linked(tty7_vt::model::Underline::None));
        let mut buf = vec![rc];
        invert_cursor_cell(&mut buf, 1, 0, 0, &test_colors());
        let under = buf[0].underline_color.expect("the dots keep a colour");
        assert_eq!(under.a, buf[0].fg.a * HELD_BACK_LINK_ALPHA);
    }
}

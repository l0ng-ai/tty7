use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

pub struct Assets;

const STOCK_PREFIX: &str = "stock/";

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(downstream) = path.strip_prefix(STOCK_PREFIX) {
            return gpui_component_assets::Assets.load(downstream);
        }
        if let Some(bytes) = agent_icon(path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        gpui_component_assets::Assets.list(path)
    }
}

fn agent_icon(path: &str) -> Option<&'static [u8]> {
    let bytes: &'static [u8] = match path {
        "icons/terminal.svg" => include_bytes!("../../assets/icons/terminal.svg"),
        "icons/git-branch.svg" => include_bytes!("../../assets/icons/git-branch.svg"),
        // Deliberately not `refresh.svg`: the panel header already carries a
        // refresh tile, and the same glyph meaning two different things one row
        // apart reads as a bug.
        "icons/git-sync.svg" => include_bytes!("../../assets/icons/git-sync.svg"),
        "icons/git-commit.svg" => include_bytes!("../../assets/icons/git-commit.svg"),
        "icons/panel-left.svg" => include_bytes!("../../assets/icons/panel-left.svg"),
        "icons/panel-right.svg" => include_bytes!("../../assets/icons/panel-right.svg"),
        "icons/plus.svg" => include_bytes!("../../assets/icons/plus.svg"),
        "icons/ellipsis.svg" => include_bytes!("../../assets/icons/ellipsis.svg"),
        "icons/folder-closed.svg" => include_bytes!("../../assets/icons/folder-closed.svg"),
        "icons/folder-open.svg" => include_bytes!("../../assets/icons/folder-open.svg"),
        "icons/info.svg" => include_bytes!("../../assets/icons/info.svg"),
        "icons/eye.svg" => include_bytes!("../../assets/icons/eye.svg"),
        "icons/search.svg" => include_bytes!("../../assets/icons/search.svg"),
        "icons/copy.svg" => include_bytes!("../../assets/icons/copy.svg"),
        "icons/folder.svg" => include_bytes!("../../assets/icons/folder.svg"),
        "icons/file.svg" => include_bytes!("../../assets/icons/file.svg"),
        "icons/circle-info.svg" => include_bytes!("../../assets/icons/circle-info.svg"),
        "icons/machine-local.svg" => include_bytes!("../../assets/icons/machine-local.svg"),
        "icons/machine-remote.svg" => include_bytes!("../../assets/icons/machine-remote.svg"),
        "icons/refresh.svg" => include_bytes!("../../assets/icons/refresh.svg"),
        "icons/agents/claude.svg" => include_bytes!("../../assets/icons/agents/claude.svg"),
        "icons/agents/codex.svg" => include_bytes!("../../assets/icons/agents/codex.svg"),
        "icons/agents/gemini.svg" => include_bytes!("../../assets/icons/agents/gemini.svg"),
        "icons/agents/amp.svg" => include_bytes!("../../assets/icons/agents/amp.svg"),
        "icons/agents/opencode.svg" => include_bytes!("../../assets/icons/agents/opencode.svg"),
        "icons/agents/copilot.svg" => include_bytes!("../../assets/icons/agents/copilot.svg"),
        "icons/agents/cursor.svg" => include_bytes!("../../assets/icons/agents/cursor.svg"),
        "icons/agents/goose.svg" => include_bytes!("../../assets/icons/agents/goose.svg"),
        "icons/agents/droid.svg" => include_bytes!("../../assets/icons/agents/droid.svg"),
        "icons/agents/grok.svg" => include_bytes!("../../assets/icons/agents/grok.svg"),
        "icons/agents/pi.svg" => include_bytes!("../../assets/icons/agents/pi.svg"),
        "icons/agents/omp.svg" => include_bytes!("../../assets/icons/agents/omp.svg"),
        "icons/agents/qwen.svg" => include_bytes!("../../assets/icons/agents/qwen.svg"),
        _ => return None,
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stock_prefix_bypasses_the_overrides() {
        for name in ["search", "ellipsis"] {
            let overridden = Assets
                .load(&format!("icons/{name}.svg"))
                .unwrap()
                .expect("tty7 override present");
            let stock = Assets
                .load(&format!("{STOCK_PREFIX}icons/{name}.svg"))
                .unwrap()
                .expect("downstream glyph present");
            assert_ne!(
                overridden, stock,
                "`{name}` should resolve to different art with and without `{STOCK_PREFIX}`"
            );
        }
    }

    #[test]
    fn every_agent_icon_resolves() {
        for agent in crate::core::cli_agent::CLIAgent::ALL {
            let path = agent.icon_path();
            assert!(
                Assets.load(path).unwrap().is_some(),
                "{} points at {path}, which nothing serves",
                agent.display_name()
            );
        }
    }

    #[test]
    fn every_git_icon_resolves() {
        // An SVG on disk that nobody added to the match above silently renders
        // as nothing, which is exactly the kind of miss no one notices.
        for path in [
            "icons/git-branch.svg",
            "icons/git-sync.svg",
            "icons/git-commit.svg",
        ] {
            assert!(
                Assets.load(path).unwrap().is_some(),
                "{path} is not registered in `agent_icon`"
            );
        }
    }

    fn glyph(name: &str) -> String {
        let bytes = Assets
            .load(&format!("icons/{name}.svg"))
            .unwrap()
            .unwrap_or_else(|| panic!("nothing serves `icons/{name}.svg`"));
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn attr<'a>(svg: &'a str, name: &str) -> &'a str {
        svg.split_once(&format!("{name}=\""))
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(value, _)| value)
            .unwrap_or_else(|| panic!("no `{name}` in: {svg}"))
    }

    #[test]
    fn the_plus_is_drawn_on_the_bound_the_rest_of_the_set_uses() {
        // Two bare strokes fail quietly. A tightened `plus` does not look
        // broken, it just reads a size smaller than the tiles beside it — which
        // is how it spent a while being scaled up at the call site instead. The
        // icons tty7 draws itself put 19.3 units of ink in a 24 viewBox (a 17.2
        // shape straddled by the family's stroke). A cross gets to sit a little
        // inside that, but not at the 14.1 it had, and not at stock lucide's 14
        // — so a re-tightened path or a re-sync from upstream lands here rather
        // than in someone's peripheral vision.
        let plus = glyph("plus");
        let stroke: f32 = attr(&plus, "stroke-width").parse().unwrap();

        // Each arm is `M<x> <y><axis><len>`: the vertical first, then the
        // horizontal. Both are checked, because a cross edited on one axis
        // only is exactly the kind of miss that survives a glance.
        let arms: Vec<&str> = plus
            .split("d=\"")
            .skip(1)
            .map(|rest| rest.split_once('"').expect("unterminated `d`").0)
            .collect();
        let arm = |d: &str| -> (char, f32, f32, f32) {
            let axis = d
                .chars()
                .find(|c| matches!(c, 'v' | 'h'))
                .unwrap_or_else(|| panic!("plus.svg's `{d}` is neither a `v` nor an `h` run"));
            let (from, len) = d.trim_start_matches('M').split_once(axis).unwrap();
            let (x, y) = from.split_once(' ').unwrap();
            (
                axis,
                x.parse().unwrap(),
                y.parse().unwrap(),
                len.parse().unwrap(),
            )
        };
        assert_eq!(arms.len(), 2, "plus.svg is not two paths: {plus}");
        let (v_axis, v_x, v_top, v_len) = arm(arms[0]);
        let (h_axis, h_left, h_y, h_len) = arm(arms[1]);
        assert_eq!(
            (v_axis, h_axis),
            ('v', 'h'),
            "plus.svg should be a vertical arm then a horizontal one: {plus}"
        );

        for (label, across, along, len) in [
            ("vertical", v_x, v_top, v_len),
            ("horizontal", h_y, h_left, h_len),
        ] {
            assert!(
                (across - 12.).abs() < 0.01 && (along + len / 2. - 12.).abs() < 0.01,
                "plus.svg's {label} arm runs {along}..{} at {across} and so is not \
                 centred in the viewBox",
                along + len
            );
        }
        assert!(
            (v_len - h_len).abs() < 0.01,
            "plus.svg's arms are {v_len} and {h_len} long, so it is not square"
        );

        let ink = v_len + stroke;
        assert!(
            ink / 19.3 >= 0.85,
            "plus.svg puts {ink} units of ink in the box where the rest of the \
             set puts 19.3, so it will read a size small beside them"
        );

        // The weight is the one part of this that is not free to be chosen.
        // `plus` is the only glyph in the set drawn off the family's own
        // `stroke-width`, and the amount it is off by is not a taste call: it
        // is what the three chrome tiles were already rendering. Scaling a
        // glyph up buys stroke along with extent, so the old art drew
        // `TILE_GLYPH_LINE / TILE_GLYPH` wider at those call sites; dropping
        // the scale-up without putting that back would have thinned the `+` by
        // a fifth even as it got *longer*. The art carries it instead, which is
        // what lets the same asset serve the tiles that never had a `_LINE`
        // step to grow into.
        //
        // Note what this does not license: any *more* weight than that. `plus`
        // is drawn beside stock lucide hairlines as well as beside the set's
        // own closed shapes — `minus` and `undo-2` in the Source Control row
        // strip at `TILE_GLYPH_XS`, `search` in the switcher's gutter — and a
        // cross heavier than the glyph next to it reads as the emphasised
        // control in the row, which is the same failure as reading small.
        let family: f32 = attr(&glyph("panel-left"), "stroke-width").parse().unwrap();
        let shipped = family * crate::ui::app::TILE_GLYPH_LINE / crate::ui::app::TILE_GLYPH;
        assert!(
            (stroke - shipped).abs() < 0.01,
            "plus.svg strokes {stroke} where the set strokes {family}; off the \
             family weight it should be off it by exactly the {}/{} the call \
             site used to scale it by, which is {shipped}",
            crate::ui::app::TILE_GLYPH_LINE,
            crate::ui::app::TILE_GLYPH,
        );

        // Extent and weight are only two thirds of it; the last is landing on
        // the pixel grid. The set's other glyphs are closed shapes carrying
        // solid fills, so a soft edge costs them little — a cross is two
        // hairlines and nothing else, and an arm end that straddles pixel rows
        // turns the tip into a smudge. A round cap reaches stroke/2 past the
        // path, so that is where the whole pixel has to land.
        let tip = (v_top - stroke / 2.) * crate::ui::app::TILE_GLYPH / 24.;
        assert!(
            (tip - tip.round()).abs() < 0.01,
            "plus.svg's cap tip lands at {tip} CSS px, not on a whole pixel"
        );
    }

    #[test]
    fn stock_prefix_works_for_unoverridden_glyphs() {
        assert_eq!(
            Assets.load("stock/icons/check.svg").unwrap(),
            Assets.load("icons/check.svg").unwrap(),
        );
    }
}

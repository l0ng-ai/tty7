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

    #[test]
    fn the_plus_is_drawn_on_the_bound_the_rest_of_the_set_uses() {
        // Two bare strokes fail quietly. A tightened `plus` does not look
        // broken, it just reads a size smaller than the tiles beside it — which
        // is how it spent a while being scaled up at the call site instead. The
        // icons tty7 draws itself put 19.3 units of ink in a 24 viewBox (a 17.2
        // shape straddled by a 2.1 stroke). A cross gets to sit a little inside
        // that, but not at the 14.1 it had, and not at stock lucide's 14 — so a
        // re-tightened path or a re-sync from upstream lands here rather than in
        // someone's peripheral vision.
        let svg =
            String::from_utf8(Assets.load("icons/plus.svg").unwrap().unwrap().to_vec()).unwrap();
        let field = |after: &str, upto: char| -> &str {
            svg.split_once(after)
                .and_then(|(_, rest)| rest.split_once(upto))
                .map(|(n, _)| n)
                .unwrap_or_else(|| panic!("plus.svg has no `{after}…{upto}`: {svg}"))
        };
        // The vertical arm, as `M12 <top>v<len>`.
        let (top, len) = field("d=\"M12 ", '"')
            .split_once('v')
            .unwrap_or_else(|| panic!("plus.svg's vertical arm is not a `v` run: {svg}"));
        let (top, len): (f32, f32) = (top.parse().unwrap(), len.parse().unwrap());
        let stroke: f32 = field("stroke-width=\"", '"').parse().unwrap();

        assert!(
            (top + len / 2. - 12.).abs() < 0.01,
            "plus.svg's arm runs {top}..{} and so is not centred in the viewBox",
            top + len
        );
        let ink = len + stroke;
        assert!(
            ink / 19.3 >= 0.85,
            "plus.svg puts {ink} units of ink in the box where the rest of the \
             set puts 19.3, so it will read a size small beside them"
        );

        // Extent is only half of it; the other half is landing on the pixel
        // grid. The set's other glyphs are closed shapes carrying solid fills,
        // so a soft edge costs them little — a cross is two hairlines and
        // nothing else, and a stroke that straddles pixel columns turns the
        // whole glyph into a smudge.
        //
        // The arm sits at the middle of the box, 6.5 CSS px into the 13, so a
        // whole-CSS-pixel stroke is the only one whose edges land on whole
        // device pixels. That argued for 1 or 2 and nothing between — but the
        // range was walked on a real screen and the eye disagreed with the
        // arithmetic: 1 still read thin against the closed shapes, 2 read
        // heavy-handed, and the fractional weight in between is fine, because
        // at 2x its edge columns come out ~75% lit rather than the ~35% that
        // made the first attempt look smeared. Sharpness was worth less than
        // this note assumed; the number below is eyeballed, not derived, so
        // keep the band wide and re-check on a screen before moving it.
        let css_px = stroke * crate::ui::app::TILE_GLYPH / 24.;
        assert!(
            (1.5..=1.9).contains(&css_px),
            "plus.svg strokes {css_px} CSS px at the {}px glyph; 1.0 was judged \
             thin and 2.0 heavy on a real screen, so it belongs in 1.5..=1.9",
            crate::ui::app::TILE_GLYPH
        );
        // Same argument for the arm ends: a round cap reaches stroke/2 past the
        // path, so the tip wants to land on a whole pixel too.
        let tip = (top - stroke / 2.) * crate::ui::app::TILE_GLYPH / 24.;
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

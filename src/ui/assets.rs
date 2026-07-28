//! The app's [`gpui::AssetSource`]: tty7's own bundled icons layered over
//! gpui-component's icon set.
//!
//! gpui-component ships the generic UI glyphs (close, chevrons, `bot`, …) via
//! [`gpui_component_assets::Assets`]. tty7 adds a small set of third-party
//! coding-agent brand marks (`icons/agents/*.svg`) for the tab avatars — see
//! [`crate::core::cli_agent::CLIAgent::icon_path`]. Rather than fork the
//! upstream asset crate to carry app-specific brand art, this source resolves
//! tty7's icons first and delegates everything else downstream, so both sets
//! load through the single `AssetSource` gpui allows.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

/// tty7's asset source. Registered once in `main` via `with_assets`.
pub struct Assets;

/// Prefix that opts a single call site *out* of the overrides in [`agent_icon`]
/// and takes gpui-component's own glyph instead: `stock/icons/search.svg`.
///
/// Needed because the overrides are keyed on the asset path, which makes them
/// app-wide (see [`agent_icon`]). Most of them are wanted everywhere, but the
/// detail panel's set is drawn for 18px tiles sitting beside solid dock glyphs,
/// and a few of those shapes are too heavy at the 16px the Settings page uses —
/// its `⋯` in particular, whose filled `r=2` dots smear into three blobs there.
/// Rather than fork the whole set under a second name, those call sites ask for
/// stock by path.
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
        // Only gpui-component enumerates its icons; tty7's brand marks are
        // referenced by explicit path, never listed, so the downstream set is
        // the whole answer.
        gpui_component_assets::Assets.list(path)
    }
}

/// The bytes of a bundled agent brand mark, or `None` if `path` isn't one of
/// ours. Kept as an explicit match (rather than `rust-embed`) because the set is
/// tiny and fixed, and `include_bytes!` needs no extra build dependency.
///
/// Note that matching on the *path* makes every arm here an app-wide override,
/// not a local one: `gpui_component_macros::icon_named!` derives `IconName` from
/// the downstream asset filenames, so `IconName::Search.path()` is literally
/// `"icons/search.svg"` and every `Icon::new(IconName::Search)` in the tree —
/// tty7's and gpui-component's own — resolves through the arm below. Adding a
/// name that upstream also ships redraws it everywhere; check the call sites
/// before doing so, and prefer a name upstream *doesn't* use (`circle-info`)
/// when only one place should change. A call site that wants the downstream
/// glyph despite an override here asks for it by [`STOCK_PREFIX`].
fn agent_icon(path: &str) -> Option<&'static [u8]> {
    let bytes: &'static [u8] = match path {
        // Flush `>_` prompt glyph for the plain-shell tab avatar (Lucide's
        // unboxed `terminal`, which gpui-component doesn't bundle — it only
        // ships the boxed `square-terminal`).
        "icons/terminal.svg" => include_bytes!("../../assets/icons/terminal.svg"),
        // A git glyph on the detail-panel spec below (gpui-component bundles
        // none). Serves both the sidebar row's branch line and the Changes tab.
        // Drawn as a commit graph — a trunk with a node at each end, branching
        // once — rather than lucide's long arc slung between two floating rings:
        // that one is the loosest, most lopsided shape in a row of four. Nodes
        // are filled, because at the sidebar's 11px a stroked ring's hole
        // collapses into a blur.
        "icons/git-branch.svg" => include_bytes!("../../assets/icons/git-branch.svg"),
        // Dock glyphs for the window chrome: a rounded frame with one inset solid
        // block marking which dock is open. gpui-component only ships the hollow
        // line `panel-left/right`, which says a panel exists but not which one is
        // showing, so tty7 carries its own. The block is a flat fill, not a wash —
        // nothing in this set relies on partial alpha surviving rasterisation.
        "icons/panel-left.svg" => include_bytes!("../../assets/icons/panel-left.svg"),
        "icons/panel-right.svg" => include_bytes!("../../assets/icons/panel-right.svg"),
        // The chrome glyphs beside those dock tiles. Lucide's `ellipsis` strokes
        // three `r=1` circles, and at 18px the cap overlaps its own fill and the
        // dots blur into grey smudges, so these are filled to the node rule below.
        "icons/plus.svg" => include_bytes!("../../assets/icons/plus.svg"),
        "icons/ellipsis.svg" => include_bytes!("../../assets/icons/ellipsis.svg"),
        // The detail panel's own set: four tab tiles at 18px and four controls at
        // 13px, all in one panel, so they're drawn to one spec instead of taken
        // from lucide as-is.
        //
        // The spec is "humanist": full, rounded, near-square. Every glyph here
        // draws the *conventional* thing — a magnifier is a magnifier, a folder
        // is a folder, a sheet has a dog-ear. Several rounds of this set tried
        // swapping those metaphors for cleverer ones (terminal-native objects,
        // brand-derived panels, deliberately unclosed forms) and every one of
        // them cost more recognition than it bought character. What carries the
        // set is how the familiar shape is *drawn*, not which shape it is.
        //
        //   stroke      2.1, flat across the set, round caps and joins
        //   radius      3.4–4.4 — generous, matching the app's own 10px panels
        //   span        3.4→20.6, near-square bounding box; circles widen ~4%,
        //               since a round shape reads smaller at equal geometry
        //   curvature   one step fuller than a mechanical arc would give (`eye`,
        //               `folder`'s shoulder), which is what makes the set feel
        //               drawn rather than constructed
        //   nodes       filled, r ≥ 1.6 — a stroked dot hazes below 16px
        //
        // The five craft rules the set is held to, in the order they're usually
        // violated:
        //
        //   1. Optical weight, not geometric: round shapes are drawn ~4% larger
        //      than square ones so a row of mixed glyphs reads level.
        //   2. Tangent, never intrusion: `search`'s handle leaves the circle at
        //      its 45° tangent point rather than aiming at the centre — an
        //      overlap there shows as a lump at 64px.
        //   3. Even interior rhythm: within one glyph, gaps between strokes are
        //      equal (`file`'s two text rules are spaced as far apart as each is
        //      from the frame). Uneven interior air is the single biggest source
        //      of "drawn sloppily".
        //   4. One terminal treatment: every end is round, every corner shares
        //      the radius band. Don't mix a hard-folded corner into this set.
        //   5. Equal apparent size, not equal bounds: `folder` is wide and short
        //      so it's drawn wider; `file` is narrow and tall so it's drawn
        //      taller. They occupy the same visual area, not the same rectangle.
        //
        // The stock lucide glyphs share none of that: they mix stroke weights,
        // sit a 21-wide circle next to an 18-wide folder, and leave so much dead
        // space inside the frame that a row reads as four glyphs from four sets.
        //
        // Shape choices worth keeping: `info` is a panel with a title bar ruled
        // across the top and one content line under it — a picture of what the
        // tab actually opens (cwd, shell, branch, changes) — rather than the
        // circled `i`, which is the most-drawn icon there is and says "help" as
        // readily as "details"; the title rule is also what keeps it from
        // colliding with `panel-left`, which is the same frame with a block in
        // it. Outline's last row is cut short, because three full-width rules
        // read as a hamburger menu rather than a list. `copy`'s back sheet wraps
        // three sides and stops on its own curves instead of poking two raw stubs
        // out of an L.
        "icons/list.svg" => include_bytes!("../../assets/icons/list.svg"),
        "icons/folder-closed.svg" => include_bytes!("../../assets/icons/folder-closed.svg"),
        "icons/folder-open.svg" => include_bytes!("../../assets/icons/folder-open.svg"),
        "icons/info.svg" => include_bytes!("../../assets/icons/info.svg"),
        "icons/eye.svg" => include_bytes!("../../assets/icons/eye.svg"),
        "icons/search.svg" => include_bytes!("../../assets/icons/search.svg"),
        "icons/copy.svg" => include_bytes!("../../assets/icons/copy.svg"),
        // `folder` and `file` carry no detail-panel role of their own — they're
        // here because overriding `folder-open` above would otherwise split the
        // file tree down the middle, drawing expanded rows on this spec and
        // collapsed ones (and every file) from stock lucide.
        //
        // `folder` is the file tree's collapsed row; `folder-closed` is the
        // Files *tab* label, and carries one inner rule so the two names stop
        // resolving to one identical drawing. The rule is legible there because
        // the tab renders at 18px — at the tree's 13px it would only crowd the
        // box, which is why the collapsed row doesn't wear it.
        "icons/folder.svg" => include_bytes!("../../assets/icons/folder.svg"),
        "icons/file.svg" => include_bytes!("../../assets/icons/file.svg"),
        // The circled `i` that `info.svg` used to be, kept under its own name for
        // the Settings nav's About row — there the glyph labels a section rather
        // than a detail tab, and "panel with two lines written in it" says nothing
        // about *About*. No upstream `IconName` maps here, so it's referenced by
        // path (see `settings.rs`).
        "icons/circle-info.svg" => include_bytes!("../../assets/icons/circle-info.svg"),
        // The Files tab's remote (SFTP) mode needs a refresh it doesn't need
        // locally: the local tree runs a recursive filesystem watcher and
        // invalidates itself, a remote listing has nothing watching it. Drawn to
        // the circle rule above (r 8.6) so it sits level with the `eye` beside it
        // rather than lucide's r=9 `rotate-cw`, whose arrow head is also a size
        // step heavier than anything else in the row.
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
        // The one mark not taken from the vendor directly: xAI publishes its
        // symbol only as a ~2:1 landscape lockup that turns to mush as a 16px
        // silhouette, so this is lobehub/lobe-icons' square transcription (MIT),
        // drawn for exactly this avatar use. Its notice rides in the SVG.
        "icons/agents/grok.svg" => include_bytes!("../../assets/icons/agents/grok.svg"),
        // Pi's own mark, from pi.dev — the one file that arrives with theme
        // logic attached (`logo-auto.svg` carries a `prefers-color-scheme`
        // style block). The geometry is kept as published and rescaled to the
        // 24x24 grid; the CSS is dropped, since these avatars are tinted by the
        // app and no other mark here brings a stylesheet. Details in the SVG.
        "icons/agents/pi.svg" => include_bytes!("../../assets/icons/agents/pi.svg"),
        _ => return None,
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Settings page reaches for stock glyphs by path; if that stopped
    /// bypassing the overrides it would silently pick up the detail panel's
    /// heavier redraws again, which is the regression this prefix exists to undo.
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

    /// Every agent avatar must resolve to real bytes. Adding a brand mark means
    /// touching two files — the SVG and the arm above — and forgetting the
    /// second one costs the agent its avatar with nothing to show for it.
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

    /// A `stock/` path for a glyph tty7 never overrode still has to resolve —
    /// the prefix is a bypass, not a separate asset set.
    #[test]
    fn stock_prefix_works_for_unoverridden_glyphs() {
        assert_eq!(
            Assets.load("stock/icons/check.svg").unwrap(),
            Assets.load("icons/check.svg").unwrap(),
        );
    }
}

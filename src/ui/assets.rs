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

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
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
fn agent_icon(path: &str) -> Option<&'static [u8]> {
    let bytes: &'static [u8] = match path {
        // Flush `>_` prompt glyph for the plain-shell tab avatar (Lucide's
        // unboxed `terminal`, which gpui-component doesn't bundle — it only
        // ships the boxed `square-terminal`).
        "icons/terminal.svg" => include_bytes!("../../assets/icons/terminal.svg"),
        // Lucide's `git-branch`, for the sidebar row's branch line (gpui-component
        // doesn't bundle a git glyph).
        "icons/git-branch.svg" => include_bytes!("../../assets/icons/git-branch.svg"),
        "icons/agents/claude.svg" => include_bytes!("../../assets/icons/agents/claude.svg"),
        "icons/agents/codex.svg" => include_bytes!("../../assets/icons/agents/codex.svg"),
        "icons/agents/gemini.svg" => include_bytes!("../../assets/icons/agents/gemini.svg"),
        "icons/agents/amp.svg" => include_bytes!("../../assets/icons/agents/amp.svg"),
        "icons/agents/opencode.svg" => include_bytes!("../../assets/icons/agents/opencode.svg"),
        "icons/agents/copilot.svg" => include_bytes!("../../assets/icons/agents/copilot.svg"),
        "icons/agents/cursor.svg" => include_bytes!("../../assets/icons/agents/cursor.svg"),
        "icons/agents/goose.svg" => include_bytes!("../../assets/icons/agents/goose.svg"),
        "icons/agents/droid.svg" => include_bytes!("../../assets/icons/agents/droid.svg"),
        _ => return None,
    };
    Some(bytes)
}

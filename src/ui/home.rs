//! The home page: what the window shows when zero tabs are open.
//!
//! Zero tabs is a legitimate state, not an error — closing the last tab lands
//! here (and quitting from here restores here). The body renders the tty7
//! logotype drawn in half-block characters plus a keyboard-shortcut watermark
//! in the VS Code empty-workspace tradition. The logo uses the terminal's own
//! font and theme colors, so it re-skins with everything else; the shortcuts
//! resolve through the live keymap (`effective_key`), so a user remap shows up
//! here automatically. Enter, a click, or ⌘T spawns a fresh terminal.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, App, Context, KeyDownEvent, Keystroke, MouseButton,
    MouseDownEvent, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::kbd::Kbd;
use gpui_component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};

use crate::core::session::{SessionPane, SessionTab};
use crate::ui::app::Tty7App;

/// The "tty7" logotype in half-block characters. Rendered line-by-line in the
/// terminal font with a 1.0 line height so the blocks stack seamlessly; the
/// trailing blinking cursor is appended to the last line at render time.
const LOGO: [&str; 4] = [
    " ▄▄▄ ▄▄▄ ▄  ▄ ▄▄▄▄",
    "  █   █  █  █    █",
    "  █   █  ▀▄▄█   █",
    "  ▀▄  ▀▄ ▄▄▄▀  █  ",
];

/// Logo cell size (px). Text size == line height so half-blocks join vertically.
const LOGO_PX: f32 = 20.0;

/// The curated shortcuts taught on the home page: (action name, label). A
/// deliberate subset — the full table lives in Settings → Keybindings; this is
/// a watermark, not documentation.
const HOME_SHORTCUTS: [(&str, &str); 7] = [
    ("NewTab", "New Tab"),
    ("ReopenClosedTab", "Reopen Closed Tab"),
    // The way to another workspace — or another machine — now that this page
    // no longer lists them. Without this row an empty window says nothing about
    // where the rest of the user's work went.
    ("ToggleSwitcher", "Switch Workspace"),
    ("TogglePalette", "Command Palette"),
    ("SplitRight", "Split Right"),
    ("SplitDown", "Split Down"),
    // "Settings…" everywhere: the menu bar, the tray, the palette and this page
    // used to offer four different names for the same destination.
    ("OpenSettings", "Settings…"),
];

/// Longest label shown for a recently-closed tab before ellipsizing, matching
/// the tab strip's clamp spirit (a runaway title must not stretch the page).
const CLOSED_LABEL_MAX: usize = 20;

/// Display label for a recently-closed tab: the user-set name if present,
/// otherwise the directory name of its first leaf's saved cwd. `None` when
/// neither is known (an unnamed tab that never reported a cwd).
fn closed_tab_label(tab: &SessionTab) -> Option<String> {
    if let Some(name) = tab.name.as_ref() {
        let name = name.trim();
        if !name.is_empty() {
            return Some(clamp_label(name));
        }
    }
    first_leaf_cwd(&tab.pane)
        .and_then(|p| p.file_name())
        .map(|s| clamp_label(&s.to_string_lossy()))
}

/// The first leaf (in layout order) that saved a cwd, depth-first.
fn first_leaf_cwd(pane: &SessionPane) -> Option<&std::path::PathBuf> {
    match pane {
        SessionPane::Leaf { cwd, .. } => cwd.as_ref(),
        SessionPane::Split { a, b, .. } => first_leaf_cwd(a).or_else(|| first_leaf_cwd(b)),
    }
}

fn clamp_label(s: &str) -> String {
    if s.chars().count() > CLOSED_LABEL_MAX {
        format!("{}…", s.chars().take(CLOSED_LABEL_MAX).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Longest workspace path shown before the front is elided. Named for the
/// picker this page used to hold; the switcher inherited both the constant and
/// the reason for it.
pub(crate) const PICKER_PATH_MAX: usize = 34;

/// Now, in Unix seconds — the clock every "2 minutes ago" in the app is
/// measured against. A clock that cannot be read reads as the epoch, which
/// [`relative_time`] renders as "just now" rather than as a negative age.
pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Human-readable age of a workspace's last activity. Coarse on purpose — the
/// user is picking between "the one from lunchtime" and "the one from last
/// week", not reading a log.
pub(crate) fn relative_time(now: u64, then: u64) -> String {
    // A future timestamp (clock change, edited file) reads as current rather
    // than rendering a negative age.
    if then == 0 || then >= now {
        return "just now".to_string();
    }
    let secs = now - then;
    match secs {
        s if s < 60 => "just now".to_string(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 7200 => "1 hour ago".to_string(),
        s if s < 86_400 => format!("{} hours ago", s / 3600),
        s if s < 172_800 => "yesterday".to_string(),
        s if s < 604_800 => format!("{} days ago", s / 86_400),
        _ => "over a week ago".to_string(),
    }
}

/// A workspace's directory, shortened for the picker's dim subtitle: `$HOME`
/// collapses to `~`, and a still-too-long path keeps its tail (the part that
/// identifies the project) with an elided front.
pub(crate) fn display_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    let shortened = match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && text.starts_with(&home) => {
            format!("~{}", &text[home.len()..])
        }
        _ => text.to_string(),
    };
    if shortened.chars().count() <= PICKER_PATH_MAX {
        return shortened;
    }
    let tail: String = shortened
        .chars()
        .skip(shortened.chars().count() - PICKER_PATH_MAX)
        .collect();
    format!("…{tail}")
}

/// The display string ("⌘T") for an action's effective (default or
/// user-remapped) binding. Formatted by gpui-component's `Kbd` so platform
/// conventions stay consistent app-wide — but rendered as bare text, not the
/// `Kbd` element: its keycap chrome (filled box + border) reads far heavier
/// than this watermark page on dark themes. Multi-chord specs show their
/// first chord — enough for a hint.
fn key_hint(action: &str, cx: &App) -> Option<String> {
    let spec = crate::ui::keymap::effective_key(action, cx)?;
    let first = spec.split_whitespace().next()?;
    let stroke = Keystroke::parse(first).ok()?;
    Some(Kbd::format(&stroke))
}

impl Tty7App {
    /// Render the home page (called by `render` when `tabs` is empty).
    pub(crate) fn render_home(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (muted, foreground, accent) = (theme.muted_foreground, theme.foreground, theme.primary);

        // The logotype: quiet muted lines in the terminal's own font, with a
        // blinking block cursor after the last line — the page's only motion
        // and only accent color, as a terminal's resting state should be.
        let mut logo = v_flex()
            .font_family(self.font_family.clone())
            .text_size(px(LOGO_PX))
            .line_height(px(LOGO_PX))
            .text_color(muted);
        let (last, head) = LOGO.split_last().expect("LOGO is non-empty");
        for line in head {
            logo = logo.child(*line);
        }
        logo = logo.child(h_flex().child(*last).child(
            div().text_color(accent).child("▌").with_animation(
                "home-cursor-blink",
                Animation::new(Duration::from_millis(1200)).repeat(),
                // A terminal cursor snaps, it doesn't fade: hard on/off.
                |cursor, delta| cursor.opacity(if delta < 0.5 { 1.0 } else { 0.0 }),
            ),
        ));

        // Shortcut watermark. The Reopen row doubles as the undo affordance:
        // when something was just closed it names it and brightens, so an
        // accidental ⌘W on the last tab reads its own rescue on arrival.
        let closed_hint = self.closed.last().and_then(closed_tab_label);
        let mut list = v_flex().gap_2().w(px(300.)).text_sm().text_color(muted);
        for (action, label) in HOME_SHORTCUTS {
            let (label, emphasized) = match (&closed_hint, action) {
                (Some(name), "ReopenClosedTab") => (format!("Reopen \u{201c}{name}\u{201d}"), true),
                _ => (label.to_string(), false),
            };
            list = list.child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .when(emphasized, |row| row.text_color(foreground))
                    .child(label)
                    // Bare key glyphs in the terminal's own mono font: quiet,
                    // and visibly "of the terminal" rather than UI chrome.
                    .children(
                        key_hint(action, cx)
                            .map(|keys| div().font_family(self.font_family.clone()).child(keys)),
                    ),
            );
        }

        // Nothing in the middle any more. The picker and the "connect to
        // another machine" wizard both used to live here, and both were
        // answering the question `ui::switcher` now owns — from the title-bar
        // chip, which is on screen in *every* window rather than only in an
        // empty one. Keeping a second copy here would mean two surfaces to keep
        // in step and two places to learn.
        let status = self.render_remote_status_strip(cx);

        v_flex()
            .id("home-page")
            .track_focus(&self.home_focus)
            .size_full()
            .items_center()
            .justify_center()
            .gap(px(48.))
            // The empty window's whole job is to hand out a shell: a bare click
            // or Enter spawns one, no target to aim for.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| this.new_tab(window, cx)),
            )
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key == "enter" && !ev.keystroke.modifiers.modified() {
                    this.new_tab(window, cx);
                }
            }))
            .child(logo)
            .children(status)
            .child(list)
            // Ease the page in rather than popping it — closing the last tab
            // should feel like arriving somewhere, not like a glitch.
            .with_animation(
                "home-fade-in",
                Animation::new(Duration::from_millis(150)),
                |page, delta| page.opacity(delta),
            )
    }

    // ----- connect to another machine --------------------------

    /// The status strip a remote window wears when it is not attached.
    ///
    /// One sits at the top of the window in every state that is not
    /// `Attached`, and this is why: a window that has lost its machine must keep
    /// showing what it had and say so, rather than close or empty itself. A
    /// local window and a healthy remote one say nothing — a permanent "you are
    /// fine" banner is noise.
    fn render_remote_status_strip(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let machine = self.remote_machine_label(cx);
        let status = self.remote_status(cx)?;
        let message = status.strip_message(&machine)?;
        // A failure state is a resting state, so it always offers the next
        // move. The button belongs here and not only on a window with tabs —
        // this is the *empty* remote window, which is precisely the one with no
        // other way out.
        let action = status.action_label();
        let theme = cx.theme();
        Some(
            h_flex()
                .items_center()
                .gap_2()
                .px(px(12.))
                .py(px(6.))
                .rounded(px(10.))
                .bg(theme.popover)
                .border_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(gpui_component::Icon::new(IconName::Globe))
                .child(message)
                .when_some(action, |this, label| {
                    this.child(
                        Button::new("home-remote-status-action")
                            .label(label)
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _window, cx| this.remote_retry(cx)))
                            // The page spawns a terminal on any bare left click.
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()),
                    )
                }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn leaf(cwd: Option<&str>) -> SessionPane {
        SessionPane::Leaf {
            cwd: cwd.map(PathBuf::from),
            pane_id: None,
            ssh_spec: None,
            agent: None,
            agent_session_id: None,
            agent_launch_argv: None,
        }
    }

    #[test]
    fn closed_tab_label_prefers_the_user_set_name() {
        let tab = SessionTab {
            name: Some("build".into()),
            sidebar_group: None,
            pane: leaf(Some("/work/getty")),
        };
        assert_eq!(closed_tab_label(&tab).as_deref(), Some("build"));
    }

    #[test]
    fn closed_tab_label_falls_back_to_the_first_leaf_cwd_dir_name() {
        let tab = SessionTab {
            name: None,
            sidebar_group: None,
            pane: leaf(Some("/work/getty")),
        };
        assert_eq!(closed_tab_label(&tab).as_deref(), Some("getty"));

        // Whitespace-only names don't count as names.
        let tab = SessionTab {
            name: Some("   ".into()),
            sidebar_group: None,
            pane: leaf(Some("/work/getty")),
        };
        assert_eq!(closed_tab_label(&tab).as_deref(), Some("getty"));
    }

    #[test]
    fn closed_tab_label_searches_splits_for_the_first_cwd() {
        let tab = SessionTab {
            name: None,
            sidebar_group: None,
            pane: SessionPane::Split {
                axis: crate::core::session::SessionAxis::Horizontal,
                ratio: 0.5,
                a: Box::new(leaf(None)),
                b: Box::new(leaf(Some("/tmp/demo"))),
            },
        };
        assert_eq!(closed_tab_label(&tab).as_deref(), Some("demo"));
    }

    #[test]
    fn closed_tab_label_is_none_when_nothing_is_known() {
        // No name, no cwd — and "/" has no file name either.
        let unnamed = SessionTab {
            name: None,
            sidebar_group: None,
            pane: leaf(None),
        };
        assert_eq!(closed_tab_label(&unnamed), None);
        let root = SessionTab {
            name: None,
            sidebar_group: None,
            pane: leaf(Some("/")),
        };
        assert_eq!(closed_tab_label(&root), None);
    }

    #[test]
    fn closed_tab_label_clamps_runaway_names() {
        let tab = SessionTab {
            name: Some("a".repeat(40)),
            sidebar_group: None,
            pane: leaf(None),
        };
        let label = closed_tab_label(&tab).unwrap();
        assert_eq!(label.chars().count(), CLOSED_LABEL_MAX + 1);
        assert!(label.ends_with('…'));
    }

    #[test]
    fn relative_time_reads_coarsely_across_the_ranges() {
        let now = 10_000_000u64;
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now, now - 30), "just now");
        assert_eq!(relative_time(now, now - 120), "2 min ago");
        assert_eq!(relative_time(now, now - 3600), "1 hour ago");
        assert_eq!(relative_time(now, now - 4 * 3600), "4 hours ago");
        assert_eq!(relative_time(now, now - 90_000), "yesterday");
        assert_eq!(relative_time(now, now - 3 * 86_400), "3 days ago");
        assert_eq!(relative_time(now, now - 30 * 86_400), "over a week ago");
    }

    #[test]
    fn relative_time_never_renders_a_negative_age() {
        let now = 1_000_000u64;
        // A never-stamped workspace, and one whose clock ran ahead (a system
        // time change, or a hand-edited session file).
        assert_eq!(relative_time(now, 0), "just now");
        assert_eq!(relative_time(now, now + 5_000), "just now");
    }

    #[test]
    fn display_path_collapses_home_and_elides_from_the_front() {
        // SAFETY: single-threaded test; HOME is restored right after.
        let saved = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", "/Users/tester") };

        assert_eq!(
            display_path(std::path::Path::new("/Users/tester/repo/tty7")),
            "~/repo/tty7"
        );
        // Outside home, the path is left alone.
        assert_eq!(display_path(std::path::Path::new("/opt/work")), "/opt/work");

        // A long path keeps its *tail* — the part that names the project.
        let long = display_path(std::path::Path::new(
            "/Users/tester/very/deeply/nested/projects/area/thing",
        ));
        assert!(long.starts_with('…'), "{long} should be front-elided");
        assert!(long.ends_with("thing"), "{long} must keep the tail");
        assert_eq!(long.chars().count(), PICKER_PATH_MAX + 1);

        match saved {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    fn logo_rows_never_exceed_the_first_row_width() {
        // The logotype renders as stacked left-aligned text lines; the first
        // row spans the full logotype, so a longer row below it would poke out
        // of the block and skew the art.
        let width = LOGO[0].chars().count();
        for row in &LOGO {
            assert!(row.chars().count() <= width, "row {row:?} exceeds {width}");
        }
    }
}

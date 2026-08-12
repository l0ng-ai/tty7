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
use crate::ui::i18n::{L10nKey, t, t_fmt, t_plural};

const LOGO: [&str; 4] = [
    " ▄▄▄ ▄▄▄ ▄  ▄ ▄▄▄▄",
    "  █   █  █  █    █",
    "  █   █  ▀▄▄█   █",
    "  ▀▄  ▀▄ ▄▄▄▀  █  ",
];

const LOGO_PX: f32 = 20.0;

/// What a window with no tabs open can actually do. `SplitRight`/`SplitDown`
/// were listed here too, but both need a pane to split and return without a
/// word when there is none — the home page was advertising two chords that do
/// nothing from the only screen that offers them. `ReopenClosedTab` earns its
/// row only while something is on the closed stack.
const HOME_SHORTCUTS: [&str; 5] = [
    "NewTab",
    "ReopenClosedTab",
    "ToggleSwitcher",
    "TogglePalette",
    "OpenSettings",
];

const CLOSED_LABEL_MAX: usize = 20;

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

pub(crate) const PICKER_PATH_MAX: usize = 34;

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

const DAY: u64 = 86_400;
const WEEK: u64 = 7 * DAY;
/// A twelfth of a year, not 30 days. Dividing by 30 would report "0 months
/// ago" for the five days between the last whole week and the first whole
/// 30-day month; a twelfth tiles the year with no such seam.
const MONTH: u64 = 31_536_000 / 12;
const YEAR: u64 = 365 * DAY;

pub(crate) fn relative_time(now: u64, then: u64) -> String {
    if then == 0 || then >= now {
        return t(L10nKey::HomeTimeJustNow).to_string();
    }
    let secs = now - then;
    match secs {
        s if s < 60 => t(L10nKey::HomeTimeJustNow).to_string(),
        s if s < 3_600 => t_plural(L10nKey::HomeTimeMinutesAgo, (s / 60) as usize, &[]),
        s if s < 7_200 => t(L10nKey::HomeTimeHourAgo).to_string(),
        s if s < DAY => t_plural(L10nKey::HomeTimeHoursAgo, (s / 3_600) as usize, &[]),
        s if s < 2 * DAY => t(L10nKey::HomeTimeYesterday).to_string(),
        s if s < WEEK => t_plural(L10nKey::HomeTimeDaysAgo, (s / DAY) as usize, &[]),
        // The ladder used to stop here, so a workspace last opened a year ago
        // and one opened eight days ago both read "over a week ago". In the
        // switcher, where recency is the whole reason the line is there, that
        // collapsed the interesting half of the range into one label.
        s if s < MONTH => t_plural(L10nKey::HomeTimeWeeksAgo, (s / WEEK) as usize, &[]),
        s if s < YEAR => t_plural(L10nKey::HomeTimeMonthsAgo, (s / MONTH) as usize, &[]),
        _ => t(L10nKey::HomeTimeOverYearAgo).to_string(),
    }
}

pub(crate) fn display_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    // Same home-abbreviation the Info panel and tab strip use: HOME with a
    // USERPROFILE fallback, separators normalized, case folded (#544).
    let shortened = crate::ui::path_display::abbreviate_home(&text).into_owned();
    if shortened.chars().count() <= PICKER_PATH_MAX {
        return shortened;
    }
    let tail: String = shortened
        .chars()
        .skip(shortened.chars().count() - PICKER_PATH_MAX)
        .collect();
    format!("…{}", snap_to_separator(&tail))
}

/// Drops a leading half-component from a front-elided path.
///
/// Cutting at a character count lands mid-name as often as not, and the
/// remainder reads as a directory that exists: "…eeply/nested/projects" offers
/// "eeply" with the same weight as "nested". A partial name carries no
/// information the rest of the path does not, so it goes — unless it is the
/// longer half, which happens when a single component overruns the whole
/// budget and there is nothing else left to show.
fn snap_to_separator(tail: &str) -> &str {
    let Some(cut) = tail.find('/') else {
        return tail;
    };
    let (dropped, kept) = tail.split_at(cut);
    if dropped.is_empty() || dropped.chars().count() <= kept.chars().count() {
        kept
    } else {
        tail
    }
}

pub(crate) fn key_hint(action: &str, cx: &App) -> Option<String> {
    let spec = crate::ui::keymap::effective_key(action, cx)?;
    let first = spec.split_whitespace().next()?;
    let stroke = Keystroke::parse(first).ok()?;
    Some(Kbd::format(&stroke))
}

fn home_shortcut_label(action: &str, closed: Option<&str>) -> String {
    let label = match action {
        "NewTab" => crate::ui::i18n::t(crate::ui::i18n::L10nKey::HomeNewTab),
        "ReopenClosedTab" => crate::ui::i18n::t(crate::ui::i18n::L10nKey::HomeReopenClosedTab),
        "ToggleSwitcher" => crate::ui::i18n::t(crate::ui::i18n::L10nKey::HomeSwitchWorkspace),
        "TogglePalette" => crate::ui::i18n::t(crate::ui::i18n::L10nKey::HomeCommandPalette),
        "SplitRight" => crate::ui::i18n::t(crate::ui::i18n::L10nKey::HomeSplitRight),
        "SplitDown" => crate::ui::i18n::t(crate::ui::i18n::L10nKey::HomeSplitDown),
        "OpenSettings" => crate::ui::i18n::t(crate::ui::i18n::L10nKey::HomeSettings),
        _ => action,
    };
    if action == "ReopenClosedTab" {
        if let Some(name) = closed {
            return t_fmt(L10nKey::HomeReopenNamed, &[("name", name)]);
        }
    }
    label.to_string()
}

impl Tty7App {
    pub(crate) fn render_home(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (muted, foreground, accent) = (theme.muted_foreground, theme.foreground, theme.primary);

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
                |cursor, delta| cursor.opacity(if delta < 0.5 { 1.0 } else { 0.0 }),
            ),
        ));

        let closed_hint = self.closed.last().and_then(closed_tab_label);
        let nothing_to_reopen = self.closed.is_empty();
        let mut list = v_flex().gap_2().w(px(300.)).text_sm().text_color(muted);
        for action in HOME_SHORTCUTS {
            if action == "ReopenClosedTab" && nothing_to_reopen {
                continue;
            }
            let emphasized = closed_hint.is_some() && action == "ReopenClosedTab";
            let label = home_shortcut_label(action, closed_hint.as_deref());
            list = list.child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .when(emphasized, |row| row.text_color(foreground))
                    .child(label)
                    .children(
                        key_hint(action, cx)
                            .map(|keys| div().font_family(self.font_family.clone()).child(keys)),
                    ),
            );
        }

        let status = self.render_remote_status_strip(cx);
        let failure = self.startup_error.clone().map(|text| {
            div()
                .max_w(px(420.))
                .text_sm()
                .text_center()
                .text_color(cx.theme().danger)
                .child(text)
        });

        v_flex()
            .id("home-page")
            .track_focus(&self.home_focus)
            .size_full()
            .items_center()
            .justify_center()
            .gap(px(48.))
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
            .children(failure)
            .children(status)
            .child(list)
            .with_animation(
                "home-fade-in",
                Animation::new(Duration::from_millis(crate::ui::tab_strip::TRANSITION_MS))
                    .with_easing(gpui::ease_out_quint()),
                |page, delta| page.opacity(delta),
            )
    }

    fn render_remote_status_strip(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let machine = self.remote_machine_label(cx);
        let status = self.remote_status(cx)?;
        let message = status.strip_message(&machine)?;
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
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()),
                    )
                }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::i18n::set_locale;
    use std::path::PathBuf;

    fn leaf(cwd: Option<&str>) -> SessionPane {
        SessionPane::Leaf {
            shell: None,
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
            tree_id: None,
            sidebar_group: None,
            pane: leaf(Some("/work/getty")),
        };
        assert_eq!(closed_tab_label(&tab).as_deref(), Some("build"));
    }

    #[test]
    fn closed_tab_label_falls_back_to_the_first_leaf_cwd_dir_name() {
        let tab = SessionTab {
            name: None,
            tree_id: None,
            sidebar_group: None,
            pane: leaf(Some("/work/getty")),
        };
        assert_eq!(closed_tab_label(&tab).as_deref(), Some("getty"));

        let tab = SessionTab {
            name: Some("   ".into()),
            tree_id: None,
            sidebar_group: None,
            pane: leaf(Some("/work/getty")),
        };
        assert_eq!(closed_tab_label(&tab).as_deref(), Some("getty"));
    }

    #[test]
    fn closed_tab_label_searches_splits_for_the_first_cwd() {
        let tab = SessionTab {
            name: None,
            tree_id: None,
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
        let unnamed = SessionTab {
            name: None,
            tree_id: None,
            sidebar_group: None,
            pane: leaf(None),
        };
        assert_eq!(closed_tab_label(&unnamed), None);
        let root = SessionTab {
            name: None,
            tree_id: None,
            sidebar_group: None,
            pane: leaf(Some("/")),
        };
        assert_eq!(closed_tab_label(&root), None);
    }

    #[test]
    fn closed_tab_label_clamps_runaway_names() {
        let tab = SessionTab {
            name: Some("a".repeat(40)),
            tree_id: None,
            sidebar_group: None,
            pane: leaf(None),
        };
        let label = closed_tab_label(&tab).unwrap();
        assert_eq!(label.chars().count(), CLOSED_LABEL_MAX + 1);
        assert!(label.ends_with('…'));
    }

    #[test]
    fn relative_time_reads_coarsely_across_the_ranges() {
        set_locale("en");
        let now = 10_000_000u64;
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now, now - 30), "just now");
        assert_eq!(relative_time(now, now - 120), "2 min ago");
        assert_eq!(relative_time(now, now - 3600), "1 hour ago");
        assert_eq!(relative_time(now, now - 4 * 3600), "4 hours ago");
        assert_eq!(relative_time(now, now - 90_000), "yesterday");
        assert_eq!(relative_time(now, now - 3 * 86_400), "3 days ago");
        assert_eq!(relative_time(now, now - 10 * 86_400), "1 week ago");
        assert_eq!(relative_time(now, now - 31 * 86_400), "1 month ago");

        set_locale("zh-CN");
        assert_eq!(relative_time(now, now - 30), "刚刚");
        assert_eq!(relative_time(now, now - 120), "2 分钟前");
        assert_eq!(relative_time(now, now - 3600), "1 小时前");
        assert_eq!(relative_time(now, now - 90_000), "昨天");
    }

    /// Every step of the ladder has to hand off to the next one without
    /// leaving a gap that rounds down to zero — "0 months ago" is the kind of
    /// label a coarser boundary produces and nobody notices until it ships.
    #[test]
    fn relative_time_never_counts_down_to_zero_between_ranges() {
        set_locale("en");
        // Far enough from the epoch that subtracting years stays positive.
        let now = 400_000_000u64;
        for days in 7..=400u64 {
            let label = relative_time(now, now - days * 86_400);
            assert!(
                !label.starts_with('0'),
                "{days} days ago rendered as {label:?}"
            );
        }
        // The handoffs themselves, spelled out.
        assert_eq!(relative_time(now, now - 6 * 86_400), "6 days ago");
        assert_eq!(relative_time(now, now - 7 * 86_400), "1 week ago");
        // Weeks run to a twelfth of a year, so day 30 is still four weeks.
        assert_eq!(relative_time(now, now - 30 * 86_400), "4 weeks ago");
        assert_eq!(relative_time(now, now - 31 * 86_400), "1 month ago");
        assert_eq!(relative_time(now, now - 364 * 86_400), "11 months ago");
        assert_eq!(relative_time(now, now - 365 * 86_400), "over a year ago");
        assert_eq!(relative_time(now, now - 3_000 * 86_400), "over a year ago");
    }

    #[test]
    fn relative_time_never_renders_a_negative_age() {
        set_locale("en");
        let now = 1_000_000u64;
        assert_eq!(relative_time(now, 0), "just now");
        assert_eq!(relative_time(now, now + 5_000), "just now");
    }

    #[test]
    fn display_path_collapses_home_and_elides_from_the_front() {
        let saved = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", "/Users/tester") };

        assert_eq!(
            display_path(std::path::Path::new("/Users/tester/repo/tty7")),
            "~/repo/tty7"
        );
        assert_eq!(display_path(std::path::Path::new("/opt/work")), "/opt/work");

        let long = display_path(std::path::Path::new(
            "/Users/tester/very/deeply/nested/projects/area/thing",
        ));
        assert!(long.starts_with('…'), "{long} should be front-elided");
        assert!(long.ends_with("thing"), "{long} must keep the tail");
        // Snapping to a separator can only shorten what the char budget kept.
        assert!(long.chars().count() <= PICKER_PATH_MAX + 1);

        // A cut that lands mid-name drops the fragment rather than passing it
        // off as a directory.
        let midname = display_path(std::path::Path::new(
            "/Users/tester/verylongish/deeply/nested/projects/area/thing",
        ));
        assert_eq!(midname, "…/deeply/nested/projects/area/thing");

        match saved {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// The fragment only goes when the rest of the path can stand without it.
    #[test]
    fn a_path_that_is_one_huge_name_keeps_what_it_can() {
        // Nothing to snap to.
        assert_eq!(snap_to_separator("abcdefghij"), "abcdefghij");
        // The fragment is the longer half, so dropping it would leave almost
        // nothing — better a partial name than "…/ui".
        assert_eq!(
            snap_to_separator("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/ui"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/ui"
        );
        // Already on a boundary: nothing is dropped.
        assert_eq!(snap_to_separator("/src/ui/i18n"), "/src/ui/i18n");
    }

    #[test]
    fn every_home_shortcut_ships_with_a_chord_to_show() {
        let defaults = crate::ui::keymap::default_bindings();
        for action in HOME_SHORTCUTS {
            let key = defaults
                .iter()
                .find(|(a, _)| *a == action)
                .unwrap_or_else(|| panic!("{action} is not a bindable action"))
                .1;
            assert!(
                !key.is_empty(),
                "{action} has no default chord, so its home row would read as a bare label"
            );
        }
    }

    #[test]
    fn the_home_list_leaves_out_what_an_empty_window_cannot_do() {
        for action in ["SplitRight", "SplitDown", "CloseActiveTab", "RenameTab"] {
            assert!(
                !HOME_SHORTCUTS.contains(&action),
                "{action} needs a pane, and the home page is what a window shows without one"
            );
        }
    }

    #[test]
    fn logo_rows_never_exceed_the_first_row_width() {
        let width = LOGO[0].chars().count();
        for row in &LOGO {
            assert!(row.chars().count() <= width, "row {row:?} exceeds {width}");
        }
    }
}

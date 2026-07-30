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

const LOGO: [&str; 4] = [
    " ▄▄▄ ▄▄▄ ▄  ▄ ▄▄▄▄",
    "  █   █  █  █    █",
    "  █   █  ▀▄▄█   █",
    "  ▀▄  ▀▄ ▄▄▄▀  █  ",
];

const LOGO_PX: f32 = 20.0;

const HOME_SHORTCUTS: [(&str, &str); 7] = [
    ("NewTab", "New Tab"),
    ("ReopenClosedTab", "Reopen Closed Tab"),
    ("ToggleSwitcher", "Switch Workspace"),
    ("TogglePalette", "Command Palette"),
    ("SplitRight", "Split Right"),
    ("SplitDown", "Split Down"),
    ("OpenSettings", "Settings…"),
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

pub(crate) fn relative_time(now: u64, then: u64) -> String {
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

fn key_hint(action: &str, cx: &App) -> Option<String> {
    let spec = crate::ui::keymap::effective_key(action, cx)?;
    let first = spec.split_whitespace().next()?;
    let stroke = Keystroke::parse(first).ok()?;
    Some(Kbd::format(&stroke))
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
                    .children(
                        key_hint(action, cx)
                            .map(|keys| div().font_family(self.font_family.clone()).child(keys)),
                    ),
            );
        }

        let status = self.render_remote_status_strip(cx);

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
            .children(status)
            .child(list)
            .with_animation(
                "home-fade-in",
                Animation::new(Duration::from_millis(150)),
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
        assert_eq!(long.chars().count(), PICKER_PATH_MAX + 1);

        match saved {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
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

//! The tab strip rendered into the title bar: one chip per tab (context icon,
//! label, close affordance), inline rename, drag-to-reorder, and the "+"
//! new-tab button. Split out of `app.rs` as an `impl Tty7App` block (the same
//! pattern `settings` uses) so the window-shell file stays focused on tab/pane
//! orchestration rather than chrome rendering.

use gpui::{
    App, Context, FontWeight, MouseButton, MouseDownEvent, SharedString, Window, div, prelude::*,
    px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex};

use crate::ui::app::{Tab, Tty7App};
use crate::ui::hints::tab_badge_label;

/// Derive a short tab label from a terminal's raw title.
///
/// Shells emit OSC titles like `user@host:~/projects/app`; we show just the
/// meaningful tail — the current directory's name (or the running command). We
/// strip the `user@host:` prefix, then take the last path component, keeping
/// `~` for the home directory.
fn short_title(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    // Drop a leading `user@host:` if present (only when it precedes a path).
    let after_host = match raw.split_once(':') {
        Some((head, tail)) if head.contains('@') => tail,
        _ => raw,
    };
    let path = after_host.trim();
    // Home directory shows as `~`; otherwise use the last path segment.
    let name = if path == "~" {
        "~"
    } else {
        path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path)
    };
    let mut name = name.to_string();
    // Final safety clamp for unusually long names.
    if name.chars().count() > 24 {
        name = format!("{}…", name.chars().take(24).collect::<String>());
    }
    name
}

/// Pick a small monochrome line icon for a tab from its terminal title. A clean
/// terminal glyph is the default; a few high-confidence contexts (remote shell,
/// version control) get a dedicated icon. Kept deliberately minimal so the strip
/// reads modern rather than busy.
fn icon_for_title(raw: &str) -> IconName {
    let label = short_title(raw);
    // First whitespace token, lowercased — matches a bare command like `ssh host`.
    let cmd = label
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match cmd.as_str() {
        "ssh" | "mosh" => IconName::Globe,
        "git" | "lazygit" | "gitui" => IconName::Github,
        _ => IconName::SquareTerminal,
    }
}

/// Drag payload for reordering tabs. Carries the source index and a label so the
/// drag preview can show the tab being moved.
#[derive(Clone)]
struct DragTab {
    index: usize,
    label: SharedString,
}

impl Render for DragTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_1()
            .rounded_lg()
            .bg(cx.theme().secondary)
            .border_1()
            .border_color(cx.theme().border)
            .text_sm()
            .text_color(cx.theme().foreground)
            .child(self.label.clone())
    }
}

impl Tty7App {
    /// The display label for a tab: the user-set name if present, otherwise the
    /// focused terminal's title (shortened), falling back to
    /// "Session N" when there's no title yet.
    pub(crate) fn tab_label(&self, tab: &Tab, index: usize, cx: &App) -> String {
        if let Some(name) = tab.name.as_ref() {
            let trimmed = name.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        let raw = tab.leaf_title(cx);
        let label = short_title(&raw);
        if label.trim().is_empty() {
            format!("Session {}", index + 1)
        } else {
            label
        }
    }

    /// A small monochrome line icon for a tab: a gear for settings, otherwise
    /// derived from the focused terminal's title (remote/VCS/default terminal).
    fn tab_icon(&self, tab: &Tab, cx: &App) -> IconName {
        if tab.is_settings() {
            return IconName::Settings;
        }
        icon_for_title(&tab.leaf_title(cx))
    }

    pub(crate) fn tab_strip(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let active = self.active;
        // While the bare ⌘/Ctrl hold is armed (see `ui::hints`), each of the
        // first nine chips swaps its close affordance for a ⌘N badge — same
        // slot, so nothing shifts when the hints appear.
        let show_badges = self.mod_hint_badges;
        // The titlebar lays out on its main axis by content width, so the strip
        // never inherits a window-bounded width to shrink within — `flex_shrink`
        // on the chips would never fire. Derive an explicit cap from the live
        // viewport instead: total width minus the traffic-light pad (80), the
        // strip's own left margin (8), and a small right buffer. Capped this way,
        // a crowded strip becomes a bounded flex container and the chips shrink.
        let avail = (window.viewport_size().width - px(100.)).max(px(120.));
        let mut strip = h_flex()
            .items_center()
            .gap_1p5()
            .ml_2()
            .min_w_0()
            .max_w(avail)
            .overflow_hidden();

        for (i, tab) in self.tabs.iter().enumerate() {
            let is_active = i == active;
            let label = self.tab_label(tab, i, cx);
            // A small leading glyph hinting the tab's context (dir / tool / settings).
            let icon = self.tab_icon(tab, cx);

            // Inline rename input for this tab, if it's the one being renamed.
            let rename_input = self
                .renaming
                .as_ref()
                .filter(|r| r.index == i)
                .map(|r| r.input.clone());
            // Clean label (no pane-count suffix) for the rename prefill / drag preview.
            let drag_label: SharedString = label.clone().into();

            // Either the editable input (while renaming) or the clickable,
            // draggable label.
            let label_region = match rename_input {
                Some(input) => div()
                    .id(("tab-rename", i))
                    .flex_1()
                    .min_w_0()
                    // Swallow mouse-downs (incl. double-click word-select inside
                    // the input) so they never reach the enclosing TitleBar and
                    // zoom/maximize the window.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(Input::new(&input).appearance(false))
                    .into_any_element(),
                None => div()
                    .id(("tab-label", i))
                    .flex_1()
                    .min_w_0()
                    // Ellipsis-truncate the label so a shrunken chip degrades
                    // gracefully instead of hard-clipping mid-glyph.
                    .truncate()
                    .text_sm()
                    // Active tab carries a hair more weight so hierarchy reads
                    // from the type, not from colour alone.
                    .when(is_active, |d| d.font_weight(FontWeight::MEDIUM))
                    .child(label)
                    // Single click activates; double click starts a rename.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            // Swallow the event so it never reaches the enclosing
                            // TitleBar, whose double-click handler would otherwise
                            // zoom/maximize the window on a rename double-click.
                            cx.stop_propagation();
                            if ev.click_count >= 2 {
                                this.start_rename(i, window, cx);
                            } else {
                                this.activate(i, window, cx);
                            }
                        }),
                    )
                    // Drag the tab by its label to reorder it.
                    .on_drag(
                        DragTab {
                            index: i,
                            label: drag_label.clone(),
                        },
                        |drag, _, _, cx| {
                            cx.stop_propagation();
                            cx.new(|_| drag.clone())
                        },
                    )
                    .into_any_element(),
            };

            let chip = h_flex()
                .id(("tab-chip", i))
                // The strip lives inside gpui-component's `TitleBar`, which marks
                // its whole area as `WindowControlArea::Drag`. On Windows that maps
                // to `HTCAPTION`, so unless an element on top registers a
                // mouse-blocking hitbox, the OS swallows clicks as window-drags and
                // our `on_mouse_down` never fires. `occlude()` makes the chip a
                // `BlockMouse` hitbox so hit-testing stops here (its label/close
                // children paint above it, so they still click through). No-op on
                // macOS, where titlebar dragging doesn't gate child hit-testing.
                .occlude()
                // A group so this chip's close affordance can reveal on hover
                // (progressive disclosure) without affecting sibling tabs.
                .group(SharedString::from(format!("tab-chip-{i}")))
                .items_center()
                .justify_between()
                .gap_1p5()
                .h(px(30.))
                // Size to content instead of a fixed width so a short label
                // ("~") doesn't claim as much room as a long one — but cap the
                // width and keep a generous floor, and let it shrink when the
                // strip gets crowded so chips stay inside the titlebar. The
                // floor is deliberately roomy (Safari-ish) so a chip reads as a
                // substantial tab rather than a cramped pill.
                .min_w(px(150.))
                .max_w(px(260.))
                .flex_shrink(1.)
                .pl_3()
                .pr_1p5()
                .rounded_lg()
                // Active tab: a soft lifted fill, no border — reads native
                // (Safari/Arc) rather than as a hard-edged box. Inactive: quiet
                // muted text with a barely-there fill on hover for feedback.
                .when(is_active, |s| {
                    s.bg(cx.theme().secondary).text_color(cx.theme().foreground)
                })
                .when(!is_active, |s| {
                    s.text_color(cx.theme().muted_foreground)
                        .hover(|s| s.bg(cx.theme().muted))
                })
                // Drop target: dropping a dragged tab here moves it to this slot.
                .drag_over::<DragTab>(|s, _, _, cx| s.bg(cx.theme().drag_border.opacity(0.2)))
                .on_drop(cx.listener(move |this, drag: &DragTab, _window, cx| {
                    this.move_tab(drag.index, i, cx);
                }))
                // A click anywhere on the chip activates the tab. Clicks on the
                // label or close button are handled by those children (which stop
                // propagation), so this fires for the rest — icon, padding, the
                // bare chip — making the whole tab a switch target, not just text.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.activate(i, window, cx);
                    }),
                )
                // Leading context glyph: brighter (foreground) on the active tab,
                // muted on the others — hierarchy stays monochrome.
                .child(div().flex_shrink_0().flex().child(
                    Icon::new(icon).size(px(15.)).text_color(if is_active {
                        cx.theme().foreground
                    } else {
                        cx.theme().muted_foreground
                    }),
                ))
                // Clickable / editable label region.
                .child(label_region)
                // Trailing slot: normally the close affordance — always shown on
                // the active tab; on the others it stays out of the way
                // (opacity 0) and fades in on chip hover, so a row of tabs reads
                // clean instead of three-icons-per-chip busy. Space is reserved
                // either way, so nothing shifts on hover. While the shortcut
                // hints are armed, the same slot shows the tab's ⌘N badge instead.
                .child(if show_badges && i < 9 {
                    // Bare digit, no keycap box — the hint blends into the chip
                    // rather than reading as another button. Sized to the exact
                    // 20px square of the close button it stands in for, so the
                    // swap can never change the chip's width (an ellipsized
                    // label would otherwise reflow and the strip would jitter).
                    div()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(20.))
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if is_active {
                            cx.theme().foreground
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(tab_badge_label(i))
                        .into_any_element()
                } else {
                    div()
                        .flex_shrink_0()
                        .when(!is_active, |s| {
                            s.opacity(0.)
                                .group_hover(SharedString::from(format!("tab-chip-{i}")), |s| {
                                    s.opacity(1.)
                                })
                        })
                        .child(
                            Button::new(("tab-close", i))
                                .icon(IconName::Close)
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.close_tab(i, window, cx);
                                })),
                        )
                        .into_any_element()
                });

            strip = strip.child(chip);
        }

        // "+" new-tab button — a title-bar tile in the tab row's rhythm.
        strip =
            strip.child(
                self.title_bar_tile("tab-add", IconName::Plus, cx, |this, window, cx| {
                    this.new_tab(window, cx);
                }),
            );

        strip
    }

    /// A minimal clickable icon tile sized to sit in the title bar's rhythm:
    /// chip-height (30px) box, chip-sized (15px) glyph, quiet hover. We hand-roll
    /// it because gpui-component's Button locks its glyph to 0.75× the box, so it
    /// can't hit a 30px target with a 15px glyph — Button's xsmall would float a
    /// 20px square beside the 30px chips. `occlude()` makes the tile a `BlockMouse`
    /// hitbox — like the chips — so on Windows the click isn't swallowed by the
    /// TitleBar's `HTCAPTION` drag area; mouse-down + stop_propagation also keeps
    /// the click off the TitleBar's zoom-on-double-click handler. Shared by the
    /// "+" new-tab button and the split-pane buttons.
    fn title_bar_tile<F>(
        &self,
        id: &'static str,
        icon: IconName,
        cx: &mut Context<Self>,
        on_click: F,
    ) -> impl IntoElement + use<F>
    where
        F: Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    {
        div()
            .id(id)
            .occlude()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .size(px(30.))
            .rounded_lg()
            .text_color(cx.theme().muted_foreground)
            .hover(|s| s.bg(cx.theme().muted))
            .child(Icon::new(icon).size(px(15.)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    on_click(this, window, cx);
                }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_title_strips_user_host_and_keeps_last_segment() {
        assert_eq!(short_title("user@host:~/projects/app"), "app");
        assert_eq!(short_title("/usr/local/bin"), "bin");
        assert_eq!(short_title("plain"), "plain");
    }

    #[test]
    fn short_title_keeps_home_tilde_and_handles_trailing_slash() {
        assert_eq!(short_title("user@host:~"), "~");
        assert_eq!(short_title("~"), "~");
        assert_eq!(short_title("a/b/c/"), "c");
    }

    #[test]
    fn short_title_blank_input_is_empty_and_long_names_are_clamped() {
        assert_eq!(short_title("   "), "");
        let long = "a".repeat(40);
        let out = short_title(&long);
        // Clamp is 24 chars plus a single ellipsis.
        assert_eq!(out.chars().count(), 25);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn icon_for_title_maps_known_commands_else_terminal() {
        assert!(matches!(icon_for_title("ssh box"), IconName::Globe));
        assert!(matches!(icon_for_title("git status"), IconName::Github));
        assert!(matches!(
            icon_for_title("vim file"),
            IconName::SquareTerminal
        ));
    }
}

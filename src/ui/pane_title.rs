//! The slim row above each pane of a split tab, naming it (#1147).
//!
//! A split running two agents and a dev server used to be three terminals
//! with nothing to tell them apart: the tab could say what one of them was,
//! and the Info panel only ever spoke for the focused one. The row says, for
//! every pane at once, what it is called, what agent runs in it and what that
//! agent is doing — and it is also the handle the pane is picked up by, so a
//! split that shows rows has no hidden grip besides.
//!
//! The name comes off the same ladder a tab's does ([`TabView::label`]): a
//! name the user gave, then the title the pane reports, then its agent, then
//! where it is. A pane named after its directory says only the last part of
//! it: the row is narrow, and the tab already carries the rest.

use std::collections::HashMap;
use std::path::Path;

use gpui::{
    AnyElement, App, Context, EntityId, FontWeight, MouseButton, SharedString, Window, div,
    prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent};
use gpui_component::menu::DropdownMenu as _;
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme as _, IconName, Sizable as _, h_flex};

use crate::core::actions::{CloseActiveTab, RenamePane, SplitDown, SplitRight, ToggleMaximizePane};
use crate::ui::app::{PaneRenaming, Tty7App, slot_pane_id, slot_pane_name};
use crate::ui::i18n::{L10nKey, t};
use crate::ui::machine_mirror::{TabLabel, TabView};
use crate::ui::pane::PaneSlot;

/// The row's height. Tall enough for a 24px button with a pixel to spare on
/// each side, short enough to cost a split one terminal row and change.
pub(crate) const PANE_TITLE_HEIGHT: f32 = 26.;

/// The agent's avatar beside the name.
const AVATAR: f32 = 14.;

/// What every row of one tab is drawn with.
#[derive(Clone, Copy)]
struct RowStyle {
    /// Unfocused panes are dimmed, so their rows read muted too.
    dim_inactive: bool,
    /// The row picks its pane up: there is somewhere else to put it.
    movable: bool,
    now_ms: u64,
}

/// What a pane's row calls it — the tab ladder, with a directory cut to its
/// last part. See the module docs.
pub(crate) fn pane_label(view: &TabView, index: usize, home: Option<&Path>) -> String {
    if let TabLabel::Cwd(cwd) = view.label()
        && let Some(last) = last_component(cwd)
    {
        return last;
    }
    crate::ui::tab_strip::label_of(view, index, home)
}

/// The last part of a path, either separator, ignoring a trailing one. `None`
/// for a root, which has no last part to show.
fn last_component(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_end_matches(['/', '\\']);
    let last = trimmed.rsplit(['/', '\\']).next()?.trim();
    (!last.is_empty()).then(|| last.to_string())
}

/// One pane as the label ladder reads it.
fn pane_view(slot: &PaneSlot, cx: &App) -> (TabView, Option<std::path::PathBuf>) {
    let id = tty7_core::core::machine::TabId::new();
    match slot {
        PaneSlot::Ready(view) => {
            let view = view.read(cx);
            (
                TabView {
                    id,
                    name: view.pane_name().map(str::to_string),
                    title: crate::terminal::view::DEFAULT_TITLE.to_string(),
                    osc_title: view.stated_title().map(str::to_string),
                    cwd: view.cwd().map(|p| p.display().to_string()),
                    agent: view.agent(),
                    status: view.agent_session().map(|s| s.status),
                    live: !view.terminal.exited,
                    panes: 1,
                },
                view.display_home(cx),
            )
        }
        PaneSlot::Connecting(pending) => {
            let spawn = &pending.read(cx).spawn;
            (
                TabView {
                    id,
                    name: spawn.name.clone(),
                    title: String::new(),
                    osc_title: None,
                    cwd: spawn
                        .working_directory
                        .as_ref()
                        .map(|p| p.display().to_string()),
                    agent: spawn.agent,
                    status: None,
                    live: false,
                    panes: 1,
                },
                None,
            )
        }
    }
}

impl Tty7App {
    /// Whether the active tab draws title rows: the setting says so for a tab
    /// of this many panes, or one of its panes has its row open as a rename
    /// box — renaming a pane whose row is hidden shows the row for as long as
    /// the box is up.
    pub(crate) fn pane_titles_shown(&self, cx: &App) -> bool {
        let Some(tab) = self.tabs.get(self.active) else {
            return false;
        };
        let leaves = tab.pane.leaves();
        let mode = cx.global::<crate::core::config::Config>().pane_title_bar;
        mode.shown_for(leaves.len())
            || self
                .pane_renaming
                .as_ref()
                .is_some_and(|r| leaves.iter().any(|l| l.entity_id() == r.slot))
    }

    /// The title rows for the active tab's panes, by slot.
    pub(crate) fn pane_title_rows(
        &self,
        dim_inactive: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> HashMap<EntityId, AnyElement> {
        let Some(tab) = self.tabs.get(self.active) else {
            return HashMap::new();
        };
        let now_ms = crate::core::config::unix_now().saturating_mul(1000);
        let leaves = tab.pane.leaves();
        // A lone pane has nowhere to be dragged to; its row is only a label.
        let movable = leaves.len() > 1;
        leaves
            .into_iter()
            .enumerate()
            .map(|(index, slot)| {
                let id = slot.entity_id();
                let row = self.pane_title_row(
                    &slot,
                    index,
                    RowStyle {
                        dim_inactive,
                        movable,
                        now_ms,
                    },
                    window,
                    cx,
                );
                (id, row)
            })
            .collect()
    }

    fn pane_title_row(
        &self,
        slot: &PaneSlot,
        index: usize,
        style: RowStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let RowStyle {
            dim_inactive,
            movable,
            now_ms,
        } = style;
        let id = slot.entity_id();
        let focused = slot.contains_focused(window, cx);
        let theme = cx.theme();
        let (fg, muted, fill, divider) = (
            theme.foreground,
            theme.muted_foreground,
            theme.secondary,
            theme.sidebar_border,
        );
        // Ink follows the terminal under it: with dimming on, only the focused
        // pane reads at full strength.
        let ink = match focused || !dim_inactive {
            true => fg,
            false => muted,
        };
        let (view, home) = pane_view(slot, cx);
        let label = pane_label(&view, index, home.as_deref());
        let session = slot.terminal().and_then(|t| t.read(cx).agent_session());
        let status = view.agent.map(|_| {
            session
                .as_ref()
                .map_or(crate::core::cli_agent::AgentStatus::Idle, |s| s.status)
        });
        let pane_id = slot_pane_id(slot, cx);
        let renaming = self
            .pane_renaming
            .as_ref()
            .filter(|r| r.slot == id)
            .map(|r| r.input.clone());

        let avatar = view.agent.map(|agent| {
            self.tab_avatar_plain(
                ("pane-title-avatar", id),
                Some(agent),
                status,
                None,
                AVATAR,
                cx,
            )
        });
        // The agent's name rides beside a label that is something else; a
        // label that already is the agent's name would only say it twice.
        let secondary = view
            .agent
            .filter(|_| !matches!(view.label(), TabLabel::Agent(_)))
            .map(|agent| SharedString::from(agent.display_name().to_string()));
        let status_word = crate::ui::tab_strip::agent_status_label(status).map(|word| {
            let age = session
                .as_ref()
                .and_then(|s| s.status_age_secs(now_ms))
                .map(crate::ui::home::compact_age);
            match age {
                Some(age) => format!("{word} · {age}"),
                None => word.to_string(),
            }
        });
        let surface = match focused {
            true => fill,
            false => theme.background,
        };
        let dot = self.row_status_dot(status, 0, surface);

        let name: AnyElement = match renaming {
            Some(input) => div()
                .id(("pane-title-rename", id.as_u64() as usize))
                .flex_1()
                .min_w_0()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(Input::new(&input).appearance(false).xsmall().px_0())
                .into_any_element(),
            None => h_flex()
                .flex_1()
                .min_w_0()
                .gap(px(6.))
                .items_center()
                .child(
                    div()
                        .flex_shrink(1.)
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(ink)
                        .child(SharedString::from(label)),
                )
                .when_some(secondary, |row, agent| {
                    row.child(
                        div()
                            .flex_shrink(2.)
                            .min_w_0()
                            .truncate()
                            .text_color(muted)
                            .child(agent),
                    )
                })
                .when_some(status_word, |row, word| {
                    row.child(
                        h_flex()
                            .flex_shrink_0()
                            .gap(px(4.))
                            .items_center()
                            .text_color(muted)
                            .children(dot)
                            .child(SharedString::from(word)),
                    )
                })
                .into_any_element(),
        };

        let focus = slot.focus_handle(cx);
        let menu_focus = focus.clone();
        let app = cx.entity().downgrade();
        let more = div()
            .flex_shrink_0()
            // Ahead of the button, so the pane is the focused one by the time
            // its menu acts: every entry there acts on the focused pane.
            .capture_any_mouse_down(move |_, window, cx| window.focus(&focus, cx))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                crate::ui::tab_strip::hit_target(
                    Button::new(("pane-title-more", id.as_u64() as usize))
                        .icon(IconName::Ellipsis)
                        .ghost()
                        .xsmall(),
                )
                .tooltip(t(L10nKey::PaneTitleMenuTooltip))
                .dropdown_menu(move |menu, _window, _cx| {
                    let app = app.clone();
                    menu.action_context(menu_focus.clone())
                        .item(
                            gpui_component::menu::PopupMenuItem::new(t(L10nKey::AppMenuRenamePane))
                                .action(Box::new(RenamePane))
                                .on_click(move |_, window, cx| {
                                    let _ = app.update(cx, |this, cx| {
                                        this.start_pane_rename(Some(id), window, cx)
                                    });
                                }),
                        )
                        .separator()
                        .menu(t(L10nKey::AppMenuSplitRight), Box::new(SplitRight))
                        .menu(t(L10nKey::AppMenuSplitDown), Box::new(SplitDown))
                        .menu(t(L10nKey::AppMenuZoomPane), Box::new(ToggleMaximizePane))
                        .separator()
                        .menu(t(L10nKey::AppMenuClosePaneTab), Box::new(CloseActiveTab))
                }),
            );

        let drag = self.pane_drag.clone();
        let row_focus = slot.focus_handle(cx);
        let lifted = crate::ui::pane_drag::lifted(&self.pane_drag).is_some();
        h_flex()
            .id(("pane-title", id.as_u64() as usize))
            .flex_shrink_0()
            .w_full()
            .h(px(PANE_TITLE_HEIGHT))
            .pl(px(8.))
            .pr(px(1.))
            .gap(px(6.))
            .items_center()
            .text_size(gpui::rems(12. / 16.))
            .when(focused, |row| row.bg(fill))
            .border_b(crate::ui::theme::hairline(window))
            .border_color(divider)
            .when(movable && !lifted, |row| {
                row.map(crate::ui::reorder::cursor_grab).on_drag(
                    crate::ui::pane_drag::DragPane,
                    move |_, _, _, cx| {
                        cx.stop_propagation();
                        crate::ui::pane_drag::begin(&drag, id);
                        cx.new(|_| crate::ui::pane_drag::DragPane)
                    },
                )
            })
            .tooltip(|window, cx| {
                Tooltip::new(t(L10nKey::PaneTitleRenameTooltip)).build(window, cx)
            })
            // The row is not the terminal: a press here focuses the pane and
            // must not start a selection in it.
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                cx.stop_propagation();
                window.focus(&row_focus, cx);
            })
            .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                if ev.click_count() >= 2 {
                    this.start_pane_rename(Some(id), window, cx);
                }
            }))
            .children(avatar)
            .child(name)
            .when_some(pane_id, |row, pane| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .text_color(muted)
                        .font_features(crate::ui::theme::tabular_figures())
                        .child(SharedString::from(format!("%{pane}"))),
                )
            })
            .child(more)
            .into_any_element()
    }

    /// Opens the rename box on a pane's title row: the one named by `slot`, or
    /// the active tab's focused pane.
    pub(crate) fn start_pane_rename(
        &mut self,
        slot: Option<EntityId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let leaves = tab.pane.leaves();
        let target = match slot {
            Some(id) => leaves.iter().position(|l| l.entity_id() == id),
            None => tab
                .pane
                .focused_or_first_slot(window, cx)
                .and_then(|f| leaves.iter().position(|l| l.entity_id() == f.entity_id())),
        };
        let Some(index) = target else {
            return;
        };
        let leaf = leaves[index].clone();
        // Zoomed, the pane is alone and has no row to put the box in.
        self.maximized = None;
        let prefill = match slot_pane_name(&leaf, cx) {
            Some(name) => name,
            None => {
                let (view, home) = pane_view(&leaf, cx);
                pane_label(&view, index, home.as_deref())
            }
        };
        let input = Self::rename_box(prefill.clone(), window, cx);
        let subs = vec![cx.subscribe_in(
            &input,
            window,
            |this, _input, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    this.commit_pane_rename(window, cx)
                }
                _ => {}
            },
        )];
        self.pane_renaming = Some(PaneRenaming::new(leaf.entity_id(), input, prefill, subs));
        cx.notify();
    }

    fn commit_pane_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(renaming) = self.pane_renaming.take() else {
            return;
        };
        let value = renaming.input.read(cx).value().to_string();
        let slot = self
            .tabs
            .iter()
            .flat_map(|t| t.pane.leaves())
            .find(|l| l.entity_id() == renaming.slot);
        if let Some(slot) = slot {
            if let Some(name) = crate::ui::app::rename_outcome_for(&value, renaming.prefill()) {
                crate::ui::app::set_slot_pane_name(&slot, name, cx);
                self.save_session(cx);
            }
            let handle = slot.focus_handle(cx);
            window.focus(&handle, cx);
        } else {
            self.focus_active(window, cx);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::cli_agent::CLIAgent;

    fn view() -> TabView {
        TabView {
            id: tty7_core::core::machine::TabId::new(),
            name: None,
            title: String::new(),
            osc_title: None,
            cwd: None,
            agent: None,
            status: None,
            live: true,
            panes: 1,
        }
    }

    /// A given name first, then what the pane titles itself, then its agent,
    /// then where it is — the ladder a tab is named by.
    #[test]
    fn a_pane_label_falls_back_in_the_tab_order() {
        let full = TabView {
            name: Some("reviewer".into()),
            osc_title: Some("✳ fixing the parser".into()),
            agent: Some(CLIAgent::Claude),
            cwd: Some("/work/tty7".into()),
            ..view()
        };
        assert_eq!(pane_label(&full, 0, None), "reviewer");

        let titled = TabView {
            name: None,
            ..full.clone()
        };
        assert_eq!(pane_label(&titled, 0, None), "fixing the parser");

        let agent = TabView {
            osc_title: None,
            ..titled.clone()
        };
        assert_eq!(pane_label(&agent, 0, None), CLIAgent::Claude.display_name());

        let place = TabView {
            agent: None,
            ..agent
        };
        assert_eq!(pane_label(&place, 0, None), "tty7", "only the last part");
    }

    #[test]
    fn a_blank_name_is_no_name() {
        let blank = TabView {
            name: Some("   ".into()),
            cwd: Some("C:\\src\\app\\".into()),
            ..view()
        };
        assert_eq!(pane_label(&blank, 0, None), "app");
    }

    #[test]
    fn the_last_component_takes_either_separator_and_skips_a_root() {
        assert_eq!(last_component("/a/b/c/").as_deref(), Some("c"));
        assert_eq!(last_component("~/repo").as_deref(), Some("repo"));
        assert_eq!(last_component("C:\\x\\y").as_deref(), Some("y"));
        assert_eq!(last_component("/"), None);
    }
}

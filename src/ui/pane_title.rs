//! The strip along the top of a pane that says which pane it is (#1147).
//!
//! A split tab is where the tab itself stops being enough: two agents side by
//! side share one tab label, and the status dot on it is only the most urgent
//! of theirs. Each pane gets a strip of its own instead, naming it the way a tab
//! is named - the name someone gave it, or failing that what it is running -
//! with its own agent's status beside the name. Double-clicking the strip
//! renames the pane, the same box a tab's rename uses.
//!
//! The name lives on the pane's record in the machine tree, so it is read from
//! the window's mirror of that tree rather than kept on the view: it survives
//! the view, the window and a restart, and a `tty7 pane rename` from anywhere
//! reaches every window showing the pane.

use std::collections::HashMap;

use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, EntityId, FontWeight, InteractiveElement as _,
    IntoElement as _, MouseButton, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, prelude::*, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex};

use crate::core::config::{Config, PaneTitleBar};
use crate::core::session::WorkspaceStore;
use crate::terminal::view::TerminalView;
use crate::ui::app::{Rename, Tab, Tty7App, rename_outcome};
use crate::ui::i18n::{L10nKey, t};
use crate::ui::machine_mirror::MachineMirrors;
use crate::ui::pane::PaneSlot;

/// How tall a pane's title strip is. Enough for one line of the UI's small
/// text and the status dot, and no more: every pixel of it is a pixel of
/// terminal the pane gives up, in every pane of the tab.
pub(crate) const PANE_TITLE_HEIGHT: f32 = 20.;

/// Whether a tab's panes draw their title strips.
///
/// Under the default, only while the tab is split: a tab of one pane says
/// everything the strip would on the tab itself. A zoomed pane is the one pane
/// on screen, so it is drawn the same way a tab of one pane is.
pub(crate) fn shows_title_strips(mode: PaneTitleBar, panes: usize, zoomed: bool) -> bool {
    match mode {
        PaneTitleBar::Never => false,
        PaneTitleBar::Always => true,
        PaneTitleBar::Splits => panes > 1 && !zoomed,
    }
}

/// A pane rename in progress: the box in the pane's strip.
pub(crate) struct PaneRenaming {
    /// The pane's view, which is what its strip is drawn for.
    pub(crate) view: EntityId,
    /// The pane on its machine, which is what the name is kept against.
    pane_id: u64,
    pub(crate) input: Entity<InputState>,
    /// What the box was prefilled with: the label on screen. Committing it
    /// untouched names nothing, exactly as a tab's rename does - see
    /// [`rename_outcome`].
    prefill: String,
    _subs: Vec<Subscription>,
}

impl Tty7App {
    /// The name someone gave the pane `view` shows, if anyone did.
    pub(crate) fn pane_name(&self, view: &TerminalView, cx: &App) -> Option<String> {
        if !cx.has_global::<WorkspaceStore>() {
            return None;
        }
        let host = WorkspaceStore::host_of(cx, self.workspace);
        MachineMirrors::pane_name(cx, host, view.pane_id).map(str::to_string)
    }

    /// What a pane's strip calls it: its name, or the tab's own ladder -
    /// title, agent, directory - read off this one pane, so a pane nobody
    /// named reads the way it would as a tab of its own.
    pub(crate) fn pane_label(
        &self,
        tab: &Tab,
        index: usize,
        view: &TerminalView,
        cx: &App,
    ) -> String {
        let reading = tty7_core::core::tab_view::TabView {
            id: tab.tree_id.get(),
            name: self.pane_name(view, cx),
            title: crate::terminal::view::DEFAULT_TITLE.to_string(),
            osc_title: view.stated_title().map(str::to_string),
            cwd: view.cwd().map(|p| p.display().to_string()),
            agent: view.agent(),
            status: view.agent_session().map(|s| s.status),
            live: true,
            panes: 1,
        };
        crate::ui::tab_strip::label_of(&reading, index, view.display_home(cx).as_deref())
    }

    /// The strips for the panes of `tab`, by pane. Every pane gets one while
    /// `shown`; otherwise only a pane being renamed does, so a rename asked for
    /// with the strips off still has somewhere to type.
    pub(crate) fn pane_title_strips(
        &self,
        tab: &Tab,
        shown: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> HashMap<EntityId, AnyElement> {
        let mut strips = HashMap::new();
        for (index, slot) in tab.pane.leaves().into_iter().enumerate() {
            let id = slot.entity_id();
            let editing = self
                .pane_renaming
                .as_ref()
                .filter(|r| r.view == id)
                .map(|r| r.input.clone());
            if !shown && editing.is_none() {
                continue;
            }
            strips.insert(
                id,
                self.pane_title_strip(tab, index, &slot, editing, window, cx),
            );
        }
        strips
    }

    fn pane_title_strip(
        &self,
        tab: &Tab,
        index: usize,
        slot: &PaneSlot,
        editing: Option<Entity<InputState>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = slot.entity_id();
        let focused = slot.contains_focused(window, cx);
        let (label, status) = match slot {
            PaneSlot::Ready(view) => {
                let view = view.read(cx);
                let status = view.agent().map(|_| {
                    view.agent_session()
                        .map(|s| s.status)
                        .unwrap_or(crate::core::cli_agent::AgentStatus::Idle)
                });
                (self.pane_label(tab, index, view, cx), status)
            }
            PaneSlot::Connecting(pending) => (pending.read(cx).machine.to_string(), None),
        };
        // Unfocused strips fade with their panes, and only then: with
        // `dim_inactive_panes` off the panes all read alike, and a strip that
        // still singled one out would be saying something the panes are not.
        let quiet = !focused && cx.global::<Config>().dim_inactive_panes;
        let theme = cx.theme();
        let ink = match quiet {
            true => theme.muted_foreground,
            false => theme.foreground,
        };
        let surface = theme.background;
        let rule = theme.sidebar_border;
        let dot = self.row_status_dot(status, 0, surface);
        // What a screen reader says for the strip: the whole name, and the
        // state the dot shows only as a colour.
        let a11y_name = match crate::ui::tab_strip::agent_status_label(status) {
            Some(state) => format!("{label}, {state}"),
            None => label.clone(),
        };
        let name: AnyElement = match editing {
            Some(input) => div()
                .id(("pane-title-rename", id.as_u64() as usize))
                .flex_1()
                .min_w_0()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(Input::new(&input).appearance(false).px_0().xsmall())
                .into_any_element(),
            None => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .when(focused, |d| d.font_weight(FontWeight::MEDIUM))
                .child(label)
                .into_any_element(),
        };
        let slot = slot.clone();
        h_flex()
            .id(("pane-title", id.as_u64() as usize))
            .role(gpui::Role::Label)
            .aria_label(SharedString::from(a11y_name))
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(PANE_TITLE_HEIGHT))
            .px(px(8.))
            .border_b(crate::ui::theme::hairline(window))
            .border_color(rule)
            .bg(surface)
            .text_xs()
            .text_color(ink)
            // The name keeps to the half of the strip left of the grip, which
            // sits in the middle of the pane's top edge: the two never overlap,
            // however long the name or narrow the pane.
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(6.))
                    .children(dot)
                    .child(name),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(crate::ui::pane_drag::GRIP_TARGET.0 + 8.)),
            )
            .child(div().flex_1())
            .tooltip(|window, cx| {
                gpui_component::tooltip::Tooltip::new(t(L10nKey::PaneTitleTooltip))
                    .build(window, cx)
            })
            // A press on the strip is a press on its pane: it takes the focus,
            // as a press on the terminal below it would, and goes no further.
            .on_mouse_down(MouseButton::Left, {
                let slot = slot.clone();
                move |_, window, cx| {
                    cx.stop_propagation();
                    let handle = slot.focus_handle(cx);
                    window.focus(&handle, cx);
                }
            })
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                let double = matches!(ev, ClickEvent::Mouse(e) if e.down.click_count >= 2);
                if double && let PaneSlot::Ready(view) = &slot {
                    this.start_pane_rename(view.clone(), window, cx);
                }
            }))
            .into_any_element()
    }

    /// Opens the rename box on the focused pane, for the palette, the key
    /// binding and the pane's context menu.
    pub(crate) fn rename_focused_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
        else {
            return;
        };
        self.start_pane_rename(view, window, cx);
    }

    pub(crate) fn start_pane_rename(
        &mut self,
        view: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // One box at a time: a rename already open is settled as if it had
        // lost focus, rather than dropped with whatever was typed into it.
        if let Some(open) = self.pane_renaming.take() {
            self.apply_pane_rename(open, cx);
        }
        if !crate::ui::tree_sync::names_panes_on(cx, self.workspace) {
            log::info!(
                "workspace {}: its machine's server predates pane names; not renaming",
                self.workspace
            );
            return;
        }
        let Some((tab, index)) = self.tabs.iter().find_map(|tab| {
            tab.pane
                .leaves()
                .iter()
                .position(|l| l.entity_id() == view.entity_id())
                .map(|index| (tab, index))
        }) else {
            return;
        };
        let current = self.pane_label(tab, index, view.read(cx), cx);
        let prefill = current.clone();
        let input = Self::rename_box(current, window, cx);
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
        self.pane_renaming = Some(PaneRenaming {
            view: view.entity_id(),
            pane_id: view.read(cx).pane_id,
            input,
            prefill,
            _subs: subs,
        });
        cx.notify();
    }

    fn commit_pane_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(renaming) = self.pane_renaming.take() else {
            return;
        };
        self.apply_pane_rename(renaming, cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Ends a pane rename whose pane has left every tab - closed, or its tab
    /// closed under it - so the box is not left waiting to name a pane the
    /// machine no longer holds.
    pub(crate) fn forget_lost_pane_rename(&mut self) {
        let lost = self.pane_renaming.as_ref().is_some_and(|r| {
            !self
                .tabs
                .iter()
                .any(|t| t.pane.leaves().iter().any(|l| l.entity_id() == r.view))
        });
        if lost {
            self.pane_renaming = None;
        }
    }

    fn apply_pane_rename(&mut self, renaming: PaneRenaming, cx: &mut Context<Self>) {
        let value = renaming.input.read(cx).value();
        let name = match rename_outcome(&value, &renaming.prefill) {
            Rename::Unchanged => None,
            Rename::Cleared => Some(None),
            Rename::Named(name) => Some(Some(name)),
        };
        if let Some(name) = name {
            crate::ui::tree_sync::rename_pane(cx, self.workspace, renaming.pane_id, name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_default_only_a_split_tab_draws_strips_and_not_while_zoomed() {
        let mode = PaneTitleBar::default();
        assert!(
            !shows_title_strips(mode, 1, false),
            "one pane: the tab says it"
        );
        assert!(shows_title_strips(mode, 2, false));
        assert!(shows_title_strips(mode, 4, false));
        assert!(
            !shows_title_strips(mode, 2, true),
            "a zoomed pane is the only one on screen"
        );
    }

    #[test]
    fn always_and_never_mean_what_they_say() {
        for (panes, zoomed) in [(1, false), (2, false), (2, true)] {
            assert!(shows_title_strips(PaneTitleBar::Always, panes, zoomed));
            assert!(!shows_title_strips(PaneTitleBar::Never, panes, zoomed));
        }
    }
}

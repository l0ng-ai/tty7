use gpui::{
    AnyElement, App, Context, EntityId, FocusHandle, Global, Subscription, WeakEntity,
    WeakFocusHandle, Window, div, prelude::*, px,
};
use gpui_component::{ActiveTheme as _, FocusTrapElement as _, Sizable as _, WindowExt as _};
use gpui_component::{checkbox::Checkbox, h_flex};

use crate::core::session::WorkspaceStore;
use crate::terminal::view::TerminalView;
use crate::ui::app::Tty7App;
use crate::ui::dialog::{self, Tone};
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::windows::WindowRegistry;

/// One group across the GUI's windows. Never hold a pane open for broadcasting,
/// or let a replacement pane inherit the old pane's selection.
#[derive(Default)]
pub(crate) struct MultiExec {
    targets: Vec<WeakEntity<TerminalView>>,
    paused: bool,
}

impl Global for MultiExec {}

impl MultiExec {
    pub(crate) fn active(cx: &App) -> Option<&Self> {
        cx.try_global::<Self>()
            .filter(|mode| !mode.targets.is_empty())
    }

    fn contains(&self, id: EntityId) -> bool {
        self.targets.iter().any(|target| target.entity_id() == id)
    }

    pub(crate) fn is_target(cx: &App, id: EntityId) -> bool {
        Self::active(cx).is_some_and(|mode| mode.contains(id))
    }

    pub(crate) fn pause_if_target(cx: &mut App, id: EntityId) {
        if Self::active(cx).is_some_and(|mode| !mode.paused && mode.contains(id)) {
            cx.global_mut::<Self>().paused = true;
        }
    }

    /// Deliver a terminal input to the other selected panes. The caller still
    /// handles the source, unless a target became unavailable. In that case
    /// suppress this input everywhere and require an explicit exit: silently
    /// dropping a recipient could run a command on only some of the machines.
    pub(crate) fn mirror(
        source: &TerminalView,
        cx: &mut Context<TerminalView>,
        apply: impl Fn(&mut TerminalView, &mut Context<TerminalView>),
    ) -> bool {
        let id = cx.entity_id();
        let Some(mode) = Self::active(cx).filter(|mode| mode.contains(id)) else {
            return true;
        };
        if source.multi_exec_overlay_focused() {
            return true;
        }
        if mode.paused {
            return false;
        }
        let peers = mode
            .targets
            .iter()
            .filter(|target| target.entity_id() != id)
            .map(|target| {
                target
                    .upgrade()
                    .filter(|view| view.read(cx).multi_exec_available(cx))
            })
            .collect::<Option<Vec<_>>>();
        let Some(peers) = peers.filter(|_| source.multi_exec_available(cx)) else {
            Self::pause_if_target(cx, id);
            return false;
        };
        for peer in peers {
            peer.update(cx, |view, cx| apply(view, cx));
        }
        true
    }
}

pub(crate) struct TargetPicker {
    focus: FocusHandle,
    previous_focus: Option<WeakFocusHandle>,
    rows: Vec<TargetRow>,
    _focus_out: Subscription,
}

struct TargetRow {
    view: WeakEntity<TerminalView>,
    title: String,
    location: String,
    checked: bool,
}

impl Tty7App {
    pub(crate) fn toggle_multi_exec(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if MultiExec::active(cx).is_some() {
            cx.set_global(MultiExec::default());
            return;
        }
        if self.multi_exec_picker.is_some() {
            self.close_multi_exec_picker(window, cx);
            return;
        }

        let mut rows = Vec::new();
        self.multi_exec_targets(true, cx, &mut rows);
        for (workspace, app) in WindowRegistry::open_windows(cx) {
            if workspace != self.workspace
                && let Some(app) = app.upgrade()
            {
                app.read(cx).multi_exec_targets(false, cx, &mut rows);
            }
        }
        if rows.len() < 2 {
            window.push_notification(t(L10nKey::MultiExecNeedsTargets), cx);
            return;
        }
        // The default group is the current tab, at the top of the list.
        rows.sort_by_key(|row| !row.checked);
        let focus = cx.focus_handle();
        let previous_focus = window.focused(cx).map(|handle| handle.downgrade());
        let focus_out = cx.on_focus_out(&focus, window, |app, _, _, cx| {
            app.multi_exec_picker = None;
            cx.notify();
        });
        focus.focus(window, cx);
        self.multi_exec_picker = Some(TargetPicker {
            focus,
            previous_focus,
            rows,
            _focus_out: focus_out,
        });
        cx.notify();
    }

    fn multi_exec_targets(&self, here: bool, cx: &App, rows: &mut Vec<TargetRow>) {
        let workspace = WorkspaceStore::all(cx)
            .get(self.workspace)
            .and_then(|entry| crate::ui::machine_mirror::display_name(cx, entry))
            .unwrap_or_else(|| t(L10nKey::WindowUntitled).to_owned());
        for (tab_index, tab) in self.tabs.iter().enumerate() {
            for (pane_index, terminal) in tab.pane.terminals().into_iter().enumerate() {
                let view = terminal.read(cx);
                if !view.multi_exec_available(cx) {
                    continue;
                }
                let location = t_fmt(
                    L10nKey::MultiExecLocation,
                    &[
                        ("workspace", &workspace),
                        ("tab", &(tab_index + 1).to_string()),
                        ("pane", &(pane_index + 1).to_string()),
                    ],
                );
                rows.push(TargetRow {
                    view: terminal.downgrade(),
                    title: view
                        .stated_title()
                        .map(str::to_owned)
                        .or_else(|| view.cwd().map(|path| path.display().to_string()))
                        .unwrap_or_else(|| view.title.clone()),
                    location: match &tab.name {
                        Some(name) => format!("{location} · {name}"),
                        None => location,
                    },
                    checked: here && tab_index == self.active,
                });
            }
        }
    }

    pub(crate) fn close_multi_exec_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = self.multi_exec_picker.take() {
            if let Some(handle) = picker.previous_focus.and_then(|handle| handle.upgrade()) {
                handle.focus(window, cx);
            } else {
                self.focus_active(window, cx);
            }
            cx.notify();
        }
    }

    fn start_multi_exec(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picker) = self.multi_exec_picker.as_ref() else {
            return;
        };
        let targets: Vec<_> = picker
            .rows
            .iter()
            .filter(|row| row.checked)
            .map(|row| row.view.clone())
            .collect();
        if targets.len() < 2 {
            return;
        }
        self.close_multi_exec_picker(window, cx);
        if targets.iter().any(|target| {
            target
                .upgrade()
                .is_none_or(|view| !view.read(cx).multi_exec_available(cx))
        }) {
            window.push_notification(t(L10nKey::MultiExecTargetsChanged), cx);
            return;
        }
        cx.set_global(MultiExec {
            targets,
            paused: false,
        });
    }

    pub(crate) fn render_multi_exec_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let picker = self.multi_exec_picker.as_ref()?;
        let title = t(L10nKey::MultiExecTitle);
        let rungs = dialog::popover_rungs(cx);
        let muted = cx.theme().muted_foreground;
        let count = picker.rows.iter().filter(|row| row.checked).count();
        let rows = picker.rows.iter().enumerate().map(|(i, row)| {
            Checkbox::new(("multi-exec-target", i))
                .small()
                .checked(row.checked)
                .w_full()
                .p(px(8.))
                .aria_label(format!("{} · {}", row.location, row.title))
                .child(div().truncate().child(row.title.clone()))
                .child(
                    div()
                        .truncate()
                        .text_xs()
                        .text_color(muted)
                        .child(row.location.clone()),
                )
                .on_click(cx.listener(move |app, checked: &bool, _, cx| {
                    if let Some(row) = app
                        .multi_exec_picker
                        .as_mut()
                        .and_then(|p| p.rows.get_mut(i))
                    {
                        row.checked = *checked;
                        cx.notify();
                    }
                }))
        });
        let card = dialog::card("multi-exec-picker-card", title, 460., cx)
            .track_focus(&picker.focus)
            .on_key_down(cx.listener(|app, event: &gpui::KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => app.close_multi_exec_picker(window, cx),
                    "enter" if !event.is_held => app.start_multi_exec(window, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(dialog::header(title, cx))
            .child(
                dialog::body()
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child(t(L10nKey::MultiExecChooseTargets)),
                    )
                    .child(
                        gpui_component::v_flex()
                            .id("multi-exec-targets")
                            .max_h(px(280.))
                            .overflow_y_scroll()
                            .children(rows),
                    ),
            )
            .child(
                dialog::footer(cx)
                    .child(dialog::button(
                        "multi-exec-cancel",
                        t(L10nKey::Cancel),
                        Tone::Secondary,
                        true,
                        rungs,
                        cx,
                        cx.listener(|app, _, window, cx| app.close_multi_exec_picker(window, cx)),
                    ))
                    .child(dialog::button(
                        "multi-exec-start",
                        t(L10nKey::MultiExecStart),
                        Tone::Primary,
                        count >= 2,
                        rungs,
                        cx,
                        cx.listener(|app, _, window, cx| app.start_multi_exec(window, cx)),
                    )),
            )
            .focus_trap("multi-exec-focus-trap", &picker.focus);
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .bg(crate::ui::presets::scrim_fill(cx))
                .flex()
                .flex_col()
                .items_center()
                .pt(px(crate::ui::switcher::CARD_TOP))
                .child(card)
                .into_any_element(),
        )
    }

    pub(crate) fn render_multi_exec_status(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let mode = MultiExec::active(cx)?;
        let caption = if mode.paused {
            t(L10nKey::MultiExecPaused).to_owned()
        } else {
            t_fmt(
                L10nKey::MultiExecActive,
                &[("count", &mode.targets.len().to_string())],
            )
        };
        let theme = cx.theme();
        Some(
            h_flex()
                .flex_none()
                .items_center()
                .gap_2()
                .h(px(34.))
                .px(px(12.))
                .border_b_1()
                .border_color(theme.border)
                .bg(crate::ui::theme::workspace_surface_color(cx))
                .child(
                    div()
                        .flex_none()
                        .size(px(6.))
                        .rounded_full()
                        .bg(theme.warning),
                )
                .child(div().flex_1().truncate().text_sm().child(caption))
                .child(dialog::button(
                    "multi-exec-exit",
                    t(L10nKey::MultiExecExit),
                    Tone::Secondary,
                    true,
                    dialog::popover_rungs(cx),
                    cx,
                    |_, _, cx| cx.set_global(MultiExec::default()),
                ))
                .into_any_element(),
        )
    }
}

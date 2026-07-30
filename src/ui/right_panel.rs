use gpui::{AnyElement, Context, Window, div, prelude::*, px};
use gpui_component::button::Button;
use gpui_component::input::Input;
use gpui_component::{
    ActiveTheme as _, Icon, IconName, InteractiveElementExt as _, Sizable as _, h_flex, v_flex,
};
use std::path::PathBuf;
use std::sync::Arc;

use crate::core::config::{Config, RightPanelTab};
use crate::daemon::protocol::PaneProcs;
use crate::terminal::git_diff::{DiffSnapshot, MAX_RENDERED_FILES};
use crate::ui::app::{
    CONTENT_INSET, TILE_GLYPH_SM, TILE_SIZE_SM, Tty7App, tile_trailing_inset,
    tile_trailing_inset_sm,
};
use crate::ui::scrollbar::with_vertical_scrollbar;

pub(crate) const MIN_WIDTH: f32 = 216.;
pub(crate) const MAX_WIDTH_RATIO: f32 = 0.5;

const RESIZE_HANDLE_WIDTH: f32 = 8.;

#[derive(Default)]
pub(crate) struct RightPanelState {
    pub(crate) diff_cwd: Option<(crate::ui::host_ops::HostId, PathBuf)>,
    pub(crate) diff: Option<Option<Arc<DiffSnapshot>>>,
    pub(crate) diff_pending: Option<(crate::ui::host_ops::HostId, PathBuf)>,
    pub(crate) procs_pane: Option<u64>,
    pub(crate) procs: Option<PaneProcs>,
    pub(crate) procs_loading: bool,
    pub(crate) procs_gen: u64,
    pub(crate) procs_forwards: Option<crate::ui::app::ForwardRoute>,
    pub(crate) scroll: gpui::ScrollHandle,
    pub(crate) tree_scroll: gpui::ScrollHandle,
}

const PROCS_POLL: std::time::Duration = std::time::Duration::from_millis(2000);

impl Tty7App {
    pub(crate) fn right_panel_open(&self, _cx: &gpui::App) -> bool {
        self.right_panel_visible && !self.tabs.is_empty()
    }

    pub(crate) fn right_panel_px(&self, window: &Window, _cx: &gpui::App) -> f32 {
        let max = (window.viewport_size().width.as_f32() * MAX_WIDTH_RATIO).max(MIN_WIDTH);
        self.right_panel_width.get().clamp(MIN_WIDTH, max)
    }

    pub(crate) fn toggle_right_panel(&mut self, cx: &mut Context<Self>) {
        let next = !self.right_panel_visible;
        self.right_panel_visible = next;
        self.update_config(cx, |cfg| cfg.right_panel_visible = next);
        cx.notify();
    }

    pub(crate) fn set_right_panel_tab(&mut self, tab: RightPanelTab, cx: &mut Context<Self>) {
        self.right_panel_tab = tab;
        self.right_panel_visible = true;
        self.update_config(cx, |cfg| {
            cfg.right_panel_tab = tab;
            cfg.right_panel_visible = true;
        });
        cx.notify();
    }

    pub(crate) fn render_right_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let panel_open = self.right_panel_open(cx);
        if let Some(open) = self.sftp_panel.open_pane_id
            && (!panel_open || self.remote_files_pane(window, cx).map(|(id, _)| id) != Some(open))
        {
            self.sftp_close_browser(cx);
        }
        if !panel_open {
            return None;
        }
        let width = self.right_panel_px(window, cx);
        let tab = self.right_panel_tab;

        let body = match tab {
            RightPanelTab::Info => self.render_panel_info(window, cx),
            RightPanelTab::Outline => self.render_panel_outline(window, cx),
            RightPanelTab::Changes => self.render_panel_changes(window, cx),
            RightPanelTab::Files => self.render_panel_files(window, cx),
        };
        let (backing, handle) = self.right_panel_resize(cx);

        Some(
            v_flex()
                .id("right-panel")
                .relative()
                .flex_none()
                .w(px(width))
                .h_full()
                .child(backing)
                .bg(cx.theme().sidebar)
                .border_l_1()
                .border_color(cx.theme().sidebar_border)
                .children(cfg!(target_os = "macos").then(|| {
                    let row = h_flex()
                        .id("right-panel-titlebar-drag")
                        .flex_none()
                        .h(px(crate::ui::app::TITLE_BAR_HEIGHT))
                        .border_b_1()
                        .border_color(cx.theme().transparent);
                    crate::ui::app::window_move_gesture(
                        row,
                        "right-panel-titlebar-drag",
                        window,
                        cx,
                    )
                    .on_double_click(|_, window, _| window.titlebar_double_click())
                    .items_center()
                    .gap(px(2.))
                    .pl(px(tile_trailing_inset()))
                    .children(self.right_panel_tabs(cx))
                    .child(div().flex_1())
                    .child(self.window_chrome(window, cx))
                }))
                .child(body)
                .children(self.sftp_transfers_footer(cx))
                .child(handle)
                .into_any_element(),
        )
    }

    fn right_panel_resize(&self, cx: &mut Context<Self>) -> (AnyElement, AnyElement) {
        use gpui::{Bounds, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, canvas};
        use std::cell::Cell as StdCell;
        use std::rc::Rc;

        let container: Rc<StdCell<Option<Bounds<Pixels>>>> = Rc::new(StdCell::new(None));
        let backing = canvas(
            {
                let container = container.clone();
                move |bounds, _window, _cx| container.set(Some(bounds))
            },
            {
                let container = container.clone();
                let width_cell = self.right_panel_width.clone();
                let dragging = self.right_panel_dragging.clone();
                move |_bounds, _state, window, _cx| {
                    window.on_mouse_event({
                        let container = container.clone();
                        let width_cell = width_cell.clone();
                        let dragging = dragging.clone();
                        move |ev: &MouseMoveEvent, _phase, window, _cx| {
                            if !dragging.get() {
                                return;
                            }
                            let Some(b) = container.get() else {
                                return;
                            };
                            let right = b.origin.x + b.size.width;
                            let raw = (right - ev.position.x).as_f32();
                            let max = (window.viewport_size().width.as_f32() * MAX_WIDTH_RATIO)
                                .max(MIN_WIDTH);
                            width_cell.set(raw.clamp(MIN_WIDTH, max));
                            window.refresh();
                        }
                    });
                    window.on_mouse_event({
                        let width_cell = width_cell.clone();
                        let dragging = dragging.clone();
                        move |_ev: &MouseUpEvent, _phase, window, cx| {
                            if !dragging.get() {
                                return;
                            }
                            dragging.set(false);
                            let w = width_cell.get();
                            let cfg = cx.global_mut::<Config>();
                            if cfg.right_panel_width != w {
                                cfg.right_panel_width = w;
                                cfg.save();
                            }
                            window.refresh();
                        }
                    });
                }
            },
        )
        .absolute()
        .size_full()
        .into_any_element();

        let active = self.right_panel_dragging.get();
        let handle = div()
            .group("right-panel-resize")
            .occlude()
            .absolute()
            .top_0()
            .left(px(-(RESIZE_HANDLE_WIDTH / 2.)))
            .w(px(RESIZE_HANDLE_WIDTH))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_col_resize()
            .child(
                div()
                    .w(px(1.))
                    .h_full()
                    .when(active, |d| d.bg(cx.theme().drag_border))
                    .group_hover("right-panel-resize", |s| s.bg(cx.theme().drag_border)),
            )
            .on_mouse_down(MouseButton::Left, {
                let dragging = self.right_panel_dragging.clone();
                move |_ev, window, _cx| {
                    dragging.set(true);
                    window.refresh();
                }
            })
            .into_any_element();

        (backing, handle)
    }

    pub(crate) fn panel_title(
        &self,
        text: &str,
        count: Option<String>,
        trailing: Option<AnyElement>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tabs = (!cfg!(target_os = "macos")).then(|| self.right_panel_tabs(cx));
        let has_trailing = trailing.is_some();
        if tabs.is_none() && !has_trailing {
            return div().flex_none().into_any_element();
        }
        let row = crate::ui::app::window_move_gesture(
            h_flex().id("panel-title"),
            "panel-title-drag",
            window,
            cx,
        );
        row.flex_none()
            .h(px(if tabs.is_some() {
                crate::ui::app::TITLE_BAR_HEIGHT
            } else {
                32.
            }))
            .items_center()
            .pl(px(CONTENT_INSET))
            .pr(px(match (&tabs, has_trailing) {
                (Some(_), _) => tile_trailing_inset(),
                (None, true) => tile_trailing_inset_sm(),
                (None, false) => CONTENT_INSET,
            }))
            .when(tabs.is_some(), |this| {
                this.border_b_1().border_color(cx.theme().sidebar_border)
            })
            .child(
                h_flex()
                    .flex_shrink_0()
                    .items_baseline()
                    .gap(px(7.))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(cx.theme().secondary_foreground)
                            .child(text.to_uppercase()),
                    )
                    .when_some(count, |this, c| {
                        this.child(
                            div()
                                .text_size(px(11.))
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_color(cx.theme().muted_foreground.opacity(0.75))
                                .child(c),
                        )
                    }),
            )
            .child(div().flex_1().min_w_0())
            .when_some(trailing, |this, t| this.child(t))
            .when_some(tabs, |this, tiles| {
                this.child(
                    h_flex()
                        .flex_shrink_0()
                        .items_center()
                        .gap(px(2.))
                        .when(has_trailing, |this| this.ml(px(6.)))
                        .children(tiles),
                )
            })
            .into_any_element()
    }

    pub(crate) fn panel_search(
        &self,
        input: &gpui::Entity<gpui_component::input::InputState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .h(px(30.))
            .px(px(CONTENT_INSET))
            .child(
                Icon::new(IconName::Search)
                    .small()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(input).appearance(false).xsmall()),
            )
            .into_any_element()
    }

    fn panel_scroll(&self, inner: AnyElement, title: AnyElement) -> AnyElement {
        let body = div()
            .id("right-panel-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.right_panel.scroll)
            .child(inner);
        v_flex()
            .flex_1()
            .min_h_0()
            .child(title)
            .child(with_vertical_scrollbar(
                "right-panel-body-scrollbar",
                body,
                &self.right_panel.scroll,
            ))
            .into_any_element()
    }

    fn panel_empty(&self, text: &str, hint: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .px(px(CONTENT_INSET))
            .py(px(4.))
            .gap(px(3.))
            .text_size(px(12.))
            .text_color(muted)
            .child(text.to_string())
            .children(hint.map(|h| {
                div()
                    .text_size(px(11.))
                    .text_color(muted.opacity(0.75))
                    .child(h.to_string())
            }))
            .into_any_element()
    }

    fn render_panel_info(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let title = self.panel_title("Info", None, None, window, cx);
        let mut rows: Vec<(&'static str, String)> = Vec::new();
        let mut cwd_for_actions: Option<PathBuf> = None;
        let mut pane_id: Option<u64> = None;
        let mut forwards_pane: Option<u64> = None;

        if let Some(tab) = self.tabs.get(self.active) {
            if let Some(leaf) = tab.detail_pane(window, cx) {
                let view = leaf.read(cx);
                pane_id = Some(view.pane_id);
                if let Some(cwd) = view
                    .git_status_cwd()
                    .map(|p| p.to_path_buf())
                    .or_else(|| view.cwd())
                {
                    rows.push(("cwd", compact_path(&cwd)));
                    cwd_for_actions = Some(cwd);
                }
                let shell = match view.shell_spec().map(|s| s.program.clone()) {
                    Some(program) => crate::core::shells::default_shell_name(Some(&program)),
                    None => self.default_shell_label(cx),
                };
                rows.push(("shell", shell));
                if let Some(ssh) = view.ssh_spec() {
                    rows.push(("ssh", ssh.host.clone()));
                }
                let connected_ssh = view
                    .remote_context()
                    .is_some_and(|c| c.kind == crate::daemon::protocol::RemoteKind::NativeSsh)
                    && matches!(
                        view.ssh_phase(),
                        Some(crate::daemon::protocol::SshPhase::Connected)
                    );
                if connected_ssh || view.workspace().is_some() {
                    forwards_pane = Some(view.pane_id);
                }
            }
            if let Some(git) = tab.git_status(Some(window), cx) {
                rows.push(("branch", git.branch.clone()));
                rows.push(("changes", format!("+{} −{}", git.added, git.removed)));
            }
            if let Some(agent) = tab.agent(cx) {
                let name = agent.display_name();
                let status = match tab.agent_status(cx) {
                    Some(s) => format!("{name} · {}", agent_status_label(s)),
                    None => name.to_string(),
                };
                rows.push(("agent", status));
            }
        }

        if rows.is_empty() {
            return self.panel_scroll(
                self.panel_empty(
                    "No active session.",
                    Some("Open a tab to see its shell, directory, and processes here."),
                    cx,
                ),
                title,
            );
        }

        let route = forwards_pane.map(|id| self.forward_route(id, cx));
        self.sync_procs(pane_id, route, cx);

        let mono = cx.theme().mono_font_family.clone();
        let mut list = v_flex().px(px(CONTENT_INSET)).py(px(2.)).gap(px(3.));
        for (k, v) in rows {
            list = list.child(
                h_flex()
                    .items_baseline()
                    .gap(px(9.))
                    .py(px(1.))
                    .text_size(px(12.))
                    .child(
                        div()
                            .flex_none()
                            .w(px(46.))
                            .text_color(cx.theme().muted_foreground)
                            .child(k),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(mono.clone())
                            .text_color(cx.theme().foreground)
                            .child(v),
                    ),
            );
        }

        let inner = v_flex()
            .child(self.panel_subtitle("Session", false, None, cx))
            .child(list)
            .when_some(cwd_for_actions, |this, cwd| {
                this.child(self.cwd_actions(cwd, cx))
            })
            .children(self.procs_section(pane_id, cx))
            .children(self.ports_section(pane_id, cx))
            .children(self.forwards_section(forwards_pane, cx))
            .into_any_element();
        self.panel_scroll(inner, title)
    }

    fn cwd_actions(&self, cwd: PathBuf, cx: &mut Context<Self>) -> AnyElement {
        let reveal_label = reveal_label();
        h_flex()
            .gap(px(2.))
            .px(px(tile_trailing_inset_sm()))
            .pt(px(6.))
            .child(
                crate::ui::tab_strip::chrome_tile_sized(
                    Button::new("panel-info-reveal").icon(Icon::new(IconName::FolderOpen)),
                    TILE_SIZE_SM,
                    TILE_GLYPH_SM,
                    false,
                    cx,
                )
                .rounded_md()
                .tooltip(reveal_label)
                .on_click({
                    let cwd = cwd.clone();
                    move |_, _window, cx| cx.reveal_path(&cwd)
                }),
            )
            .child(
                crate::ui::tab_strip::chrome_tile_sized(
                    Button::new("panel-info-copy-path").icon(Icon::new(IconName::Copy)),
                    TILE_SIZE_SM,
                    TILE_GLYPH_SM,
                    false,
                    cx,
                )
                .rounded_md()
                .tooltip("Copy Path")
                .on_click(move |_, _window, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                        cwd.display().to_string(),
                    ));
                }),
            )
            .into_any_element()
    }

    pub(crate) fn panel_subtitle(
        &self,
        text: &str,
        divider: bool,
        trailing: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .when(divider, |d| {
                d.mt(px(6.)).border_t_1().border_color(cx.theme().border)
            })
            .items_center()
            .justify_between()
            .pl(px(CONTENT_INSET))
            .pr(px(if trailing.is_some() {
                CONTENT_INSET - crate::ui::app::TILE_PAD
            } else {
                CONTENT_INSET
            }))
            .pt(px(match (divider, trailing.is_some()) {
                (true, false) => 12.,
                (true, true) => 8.,
                (false, false) => 10.,
                (false, true) => 6.,
            }))
            .pb(px(if trailing.is_some() { 0. } else { 4. }))
            .child(
                div()
                    .text_size(px(10.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .child(text.to_uppercase()),
            )
            .when_some(trailing, |this, t| this.child(t))
            .into_any_element()
    }

    fn procs_section(&self, pane_id: Option<u64>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let procs = &self.procs(pane_id)?.procs;
        if procs.len() < 2 {
            return None;
        }
        let mono = cx.theme().mono_font_family.clone();
        let mut list = v_flex().px(px(CONTENT_INSET)).py(px(1.)).gap(px(2.));
        for p in procs {
            list = list.child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .pl(px(f32::from(p.depth) * 10.))
                            .text_size(px(12.))
                            .font_family(mono.clone())
                            .text_color(if p.foreground {
                                cx.theme().foreground
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child(p.name.clone()),
                    )
                    .child(info_chip(
                        &p.pid.to_string(),
                        cx.theme().accent,
                        cx.theme().muted_foreground,
                        &mono,
                    )),
            );
        }
        Some(
            v_flex()
                .child(self.panel_subtitle("Processes", true, None, cx))
                .child(list)
                .into_any_element(),
        )
    }

    fn ports_section(&self, pane_id: Option<u64>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ports = &self.procs(pane_id)?.ports;
        if ports.is_empty() {
            return None;
        }
        let mono = cx.theme().mono_font_family.clone();
        let mut list = v_flex().px(px(CONTENT_INSET)).py(px(1.)).gap(px(2.));
        for p in ports {
            list = list.child(
                h_flex()
                    .items_center()
                    .gap(px(8.))
                    .child(info_chip(
                        &p.port.to_string(),
                        cx.theme().accent,
                        cx.theme().foreground,
                        &mono,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .font_family(mono.clone())
                            .text_color(cx.theme().muted_foreground)
                            .child(p.name.clone()),
                    ),
            );
        }
        Some(
            v_flex()
                .child(self.panel_subtitle("Ports", true, None, cx))
                .child(list)
                .into_any_element(),
        )
    }

    fn procs(&self, pane_id: Option<u64>) -> Option<&PaneProcs> {
        (pane_id.is_some() && self.right_panel.procs_pane == pane_id)
            .then_some(self.right_panel.procs.as_ref())?
    }

    fn sync_procs(
        &mut self,
        pane_id: Option<u64>,
        forwards: Option<crate::ui::app::ForwardRoute>,
        cx: &mut Context<Self>,
    ) {
        let Some(pane_id) = pane_id else { return };
        self.right_panel.procs_forwards = forwards.clone();
        if self.right_panel.procs_pane != Some(pane_id) {
            self.right_panel.procs_pane = Some(pane_id);
            self.right_panel.procs = None;
            self.loopback_panel.managed.clear();
            self.right_panel.procs_gen += 1;
            self.right_panel.procs_loading = false;
        }
        if !self.right_panel.procs_loading {
            self.right_panel.procs_loading = true;
            let generation = self.right_panel.procs_gen;
            self.spawn_procs_query(pane_id, generation, forwards, cx);
        }
    }

    fn spawn_procs_query(
        &mut self,
        pane_id: u64,
        generation: u64,
        forwards: Option<crate::ui::app::ForwardRoute>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let route = forwards.clone();
            let (procs, managed) = cx
                .background_executor()
                .spawn(async move {
                    let procs = crate::terminal::RemoteTerminal::query_procs(pane_id);
                    let managed = route.map(|r| r.list()).unwrap_or_default();
                    (procs, managed)
                })
                .await;
            let keep_polling = this
                .update(cx, |app, cx| {
                    if app.right_panel.procs_gen != generation {
                        return false;
                    }
                    app.right_panel.procs = Some(procs);
                    if forwards.is_some() {
                        app.loopback_panel.managed = managed;
                    }
                    cx.notify();
                    let wanted =
                        app.right_panel_visible && app.right_panel_tab == RightPanelTab::Info;
                    if !wanted {
                        app.right_panel.procs_loading = false;
                    }
                    wanted
                })
                .unwrap_or(false);
            if !keep_polling {
                return;
            }
            cx.background_executor().timer(PROCS_POLL).await;
            let _ = this.update(cx, |app, cx| {
                if app.right_panel.procs_gen != generation {
                    return;
                }
                let wanted = app.right_panel_visible && app.right_panel_tab == RightPanelTab::Info;
                if wanted {
                    let forwards = app.right_panel.procs_forwards.clone();
                    app.spawn_procs_query(pane_id, generation, forwards, cx);
                } else {
                    app.right_panel.procs_loading = false;
                }
            });
        })
        .detach();
    }

    fn render_panel_outline(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let Some(leaf) = self
            .tabs
            .get(self.active)
            .and_then(|t| t.detail_pane(window, cx))
        else {
            let title = self.panel_title("Outline", None, None, window, cx);
            return self.panel_scroll(
                self.panel_empty(
                    "No active session.",
                    Some("Open a tab to see its shell, directory, and processes here."),
                    cx,
                ),
                title,
            );
        };
        let count = leaf.read(cx).command_marks().len();
        if count == 0 {
            let title = self.panel_title("Outline", None, None, window, cx);
            return self.panel_scroll(
                self.panel_empty(
                    "No commands recorded for this pane.",
                    Some("Run a command — shell integration marks each one so you can jump back to it."),
                    cx,
                ),
                title,
            );
        }
        let title = self.panel_title("Outline", Some(count.to_string()), None, window, cx);

        let mono = cx.theme().mono_font_family.clone();
        let mut list = v_flex().px(px(CONTENT_INSET - 4.)).py(px(2.)).gap(px(1.));
        let marks = leaf.read(cx).command_marks();
        for mark in marks.iter().rev() {
            let row = mark.row;
            let leaf = leaf.clone();
            let failed = mark.exit.is_some_and(|c| c != 0);
            let running = !mark.done;
            let dot = {
                let d = div().flex_none().size(px(7.)).rounded_full();
                if failed {
                    d.bg(cx.theme().danger)
                } else if running {
                    d.bg(cx.theme().muted_foreground)
                } else {
                    d.border_1()
                        .border_color(cx.theme().muted_foreground.opacity(0.55))
                }
            };
            list = list.child(
                h_flex()
                    .id(gpui::SharedString::from(format!("panel-mark-{row}")))
                    .items_center()
                    .gap(px(8.))
                    .px(px(4.))
                    .py(px(3.))
                    .rounded(px(5.))
                    .cursor_pointer()
                    .hover(|s| s.bg(gpui::rgb(sf.hover)))
                    .on_click(cx.listener(move |_this, _, _window, cx| {
                        leaf.update(cx, |view, cx| {
                            view.scroll_to_mark(row, cx);
                        });
                    }))
                    .child(dot)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .font_family(mono.clone())
                            .text_color(if failed {
                                cx.theme().danger
                            } else {
                                cx.theme().foreground
                            })
                            .child(one_line(&mark.text)),
                    )
                    .when_some(mark.exit.filter(|c| *c != 0), |this, code| {
                        this.child(
                            div()
                                .flex_none()
                                .text_size(px(10.5))
                                .font_family(mono.clone())
                                .text_color(cx.theme().danger)
                                .child(code.to_string()),
                        )
                    }),
            );
        }
        self.panel_scroll(list.into_any_element(), title)
    }

    fn render_panel_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let target = self
            .tabs
            .get(self.active)
            .and_then(|t| t.detail_pane(window, cx))
            .and_then(|leaf| {
                let v = leaf.read(cx);
                let cwd = v
                    .git_status_cwd()
                    .map(|p| p.to_path_buf())
                    .or_else(|| v.host_cwd())?;
                Some((v.host(cx)?, cwd))
            });

        let Some((host, cwd)) = target else {
            let title = self.panel_title("Changes", None, None, window, cx);
            return self.panel_scroll(
                self.panel_empty(
                    "No working directory.",
                    Some("This pane has not reported one yet."),
                    cx,
                ),
                title,
            );
        };
        let key = (host.id(), cwd.clone());
        if self.right_panel.diff_cwd.as_ref() != Some(&key) {
            self.right_panel.diff_cwd = Some(key);
            self.right_panel.diff = None;
            self.spawn_right_panel_diff(host.clone(), cwd.clone(), cx);
        } else if self.right_panel.diff.is_none() && self.right_panel.diff_pending.is_none() {
            self.spawn_right_panel_diff(host.clone(), cwd.clone(), cx);
        }

        let count = match &self.right_panel.diff {
            Some(Some(snap)) => {
                let n = snap.files.len() + snap.untracked_count();
                (n > 0).then(|| n.to_string())
            }
            _ => None,
        };
        let title = self.panel_title("Changes", count, None, window, cx);
        let mono = cx.theme().mono_font_family.clone();

        let inner = match &self.right_panel.diff {
            None => self.panel_empty("Loading…", None, cx),
            Some(None) => self.panel_empty(
                "Not a git repository.",
                Some("cd into one and this tab lists its uncommitted changes."),
                cx,
            ),
            Some(Some(snap)) if snap.files.is_empty() && snap.untracked.is_empty() => self
                .panel_empty(
                    "No uncommitted changes.",
                    Some("The working tree is clean."),
                    cx,
                ),
            Some(Some(snap)) => {
                let snap = Arc::clone(snap);
                let untracked = snap.untracked_count();
                let focused = self.diff_overlay_focus(host.id(), &cwd).map(str::to_string);
                let shown = snap.files.len().min(MAX_RENDERED_FILES);
                let mut list = v_flex().px(px(CONTENT_INSET - 4.)).py(px(2.)).gap(px(1.));
                for file in snap.files.iter().take(shown) {
                    let path = file.path.clone();
                    let (added, removed) = (file.added, file.removed);
                    let selected = focused.as_deref() == Some(path.as_str());
                    list = list.child(
                        h_flex()
                            .id(gpui::SharedString::from(format!("panel-change-{path}")))
                            .items_center()
                            .gap(px(8.))
                            .px(px(4.))
                            .py(px(3.))
                            .rounded(px(5.))
                            .cursor_pointer()
                            .hover(|s| s.bg(gpui::rgb(sf.hover)))
                            .when(selected, |s| s.bg(gpui::rgb(sf.selected)))
                            .on_click({
                                let host_id = host.id();
                                let cwd = cwd.clone();
                                let path = path.clone();
                                cx.listener(move |this, _, window, cx| {
                                    this.toggle_diff_overlay_at(
                                        host_id,
                                        cwd.clone(),
                                        Some(path.clone()),
                                        window,
                                        cx,
                                    );
                                })
                            })
                            .child(git_badge("M", cx.theme().muted_foreground, &mono))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.))
                                    .font_family(mono.clone())
                                    .text_color(cx.theme().foreground)
                                    .child(path),
                            )
                            .when(added > 0, |this| {
                                this.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(11.))
                                        .font_family(mono.clone())
                                        .text_color(cx.theme().success)
                                        .child(format!("+{added}")),
                                )
                            })
                            .when(removed > 0, |this| {
                                this.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(11.))
                                        .font_family(mono.clone())
                                        .text_color(cx.theme().danger)
                                        .child(format!("−{removed}")),
                                )
                            }),
                    );
                }
                if snap.files.len() > shown {
                    let rest = snap.files.len() - shown;
                    list = list.child(
                        div()
                            .px(px(4.))
                            .py(px(3.))
                            .text_size(px(11.5))
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "… and {rest} more changed file{} — run `git diff` to see them.",
                                if rest == 1 { "" } else { "s" }
                            )),
                    );
                }
                if untracked > 0 {
                    list = list.child(
                        h_flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(4.))
                            .py(px(3.))
                            .child(git_badge(
                                "U",
                                cx.theme().muted_foreground.opacity(0.75),
                                &mono,
                            ))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("{untracked} untracked")),
                            ),
                    );
                }
                list.into_any_element()
            }
        };
        self.panel_scroll(inner, title)
    }

    fn spawn_right_panel_diff(
        &mut self,
        host: crate::ui::host_ops::SharedHost,
        cwd: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if self.right_panel.diff_pending.is_some() {
            return;
        }
        self.right_panel.diff_pending = Some((host.id(), cwd.clone()));
        self.spawn_shared_diff_probe(host, cwd, cx);
    }

    pub(crate) fn right_panel_refresh_changes(&mut self, cx: &mut Context<Self>) {
        if self.right_panel.diff_pending.is_some() {
            return;
        }
        let Some((id, cwd)) = self.right_panel.diff_cwd.clone() else {
            return;
        };
        let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, id) else {
            return;
        };
        let Some(Some(snap)) = &self.right_panel.diff else {
            return;
        };
        let Some(status) = cx
            .try_global::<crate::terminal::git_status::GitStatusCache>()
            .and_then(|cache| cache.status_for(id, &cwd))
        else {
            return;
        };
        let stale = status.branch != snap.branch || (status.added, status.removed) != snap.totals();
        if stale {
            self.spawn_right_panel_diff(host, cwd, cx);
        }
    }

    fn render_panel_files(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let remote = self.remote_files_pane(window, cx);
        let host = remote.as_ref().map(|(_, host)| host.clone());
        if self.sftp_sync_pane(remote.map(|(id, _)| id), window, cx) {
            return self.render_panel_sftp(host.unwrap_or_default(), window, cx);
        }

        let title = self.panel_title("Files", None, None, window, cx);
        let search = self.panel_search(&self.file_search.clone(), cx);
        let rows = self.render_file_tree_rows(window, cx);
        v_flex()
            .flex_1()
            .min_h_0()
            .child(title)
            .child(search)
            .child(rows)
            .into_any_element()
    }

    fn remote_files_pane(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(u64, String)> {
        use crate::daemon::protocol::{RemoteKind, SshPhase};
        let leaf = self.tabs.get(self.active)?.detail_pane(window, cx)?;
        let view = leaf.read(cx);
        let remote = view.remote_context()?;
        if remote.kind != RemoteKind::NativeSsh
            || !matches!(view.ssh_phase(), Some(SshPhase::Connected))
        {
            return None;
        }
        Some((view.pane_id, remote.target))
    }
}

pub(crate) fn git_badge(letter: &str, color: gpui::Hsla, mono: &gpui::SharedString) -> AnyElement {
    div()
        .flex_none()
        .w(px(14.))
        .text_center()
        .text_size(px(10.5))
        .font_family(mono.clone())
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(color)
        .child(letter.to_string())
        .into_any_element()
}

pub(crate) fn info_chip(
    text: &str,
    bg: gpui::Hsla,
    fg: gpui::Hsla,
    mono: &gpui::SharedString,
) -> AnyElement {
    div()
        .flex_none()
        .px(px(5.))
        .py(px(1.5))
        .rounded(px(4.))
        .bg(bg)
        .text_size(px(10.5))
        .font_family(mono.clone())
        .text_color(fg)
        .child(text.to_string())
        .into_any_element()
}

pub fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Reveal in Finder"
    } else {
        "Open Folder"
    }
}

fn agent_status_label(status: crate::core::cli_agent::AgentStatus) -> &'static str {
    use crate::core::cli_agent::AgentStatus::*;
    match status {
        Idle => "idle",
        Working => "working",
        Waiting => "waiting",
        Done => "done",
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn compact_path(path: &std::path::Path) -> String {
    let s = path.to_string_lossy().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && s.starts_with(&home) => s.replacen(&home, "~", 1),
        _ => s,
    }
}

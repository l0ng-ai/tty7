//! Pane-contextual SSH loopback forward controls.
//!
//! Settings owns persistent preferences; this module owns the live forwarding
//! dashboard that only makes sense beside a concrete SSH pane.

use gpui::{AnyElement, Context, Div, FontWeight, SharedString, div, prelude::*, px};
use gpui_component::Selectable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};

use crate::daemon::protocol::{LoopbackForwardInfo, RemoteContext};
use crate::ui::app::Tty7App;

impl Tty7App {
    pub(crate) fn render_loopback_forward_overlay(
        &self,
        pane_id: u64,
        remote: &RemoteContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let foreground = cx.theme().foreground;
        let pane_forwards = self.loopback_forwards_for_pane(pane_id);
        let active_count = pane_forwards.len();
        let panel_open = self.loopback_panel.open_pane_id == Some(pane_id);
        let label = if active_count == 0 {
            "Ports".to_string()
        } else {
            format!("Ports {active_count}")
        };

        div()
            .absolute()
            .top_2()
            .right_4()
            .flex()
            .flex_col()
            .items_end()
            .gap_2()
            .child(
                Button::new(("ssh-forward-chip", pane_id))
                    .label(label)
                    .small()
                    .selected(panel_open)
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.toggle_loopback_forward_panel(pane_id, cx)
                    })),
            )
            .when(panel_open, |this| {
                this.child(self.render_loopback_forward_panel(pane_id, remote, &pane_forwards, cx))
            })
            .text_color(foreground)
            .into_any_element()
    }

    fn loopback_forwards_for_pane(&self, pane_id: u64) -> Vec<LoopbackForwardInfo> {
        self.loopback_panel
            .forwards
            .iter()
            .filter(|forward| forward.id.pane_id == pane_id)
            .cloned()
            .collect()
    }

    fn render_loopback_forward_panel(
        &self,
        pane_id: u64,
        remote: &RemoteContext,
        forwards: &[LoopbackForwardInfo],
        cx: &mut Context<Self>,
    ) -> Div {
        let popover = cx.theme().popover;
        let border = cx.theme().border;
        let foreground = cx.theme().foreground;
        let muted_foreground = cx.theme().muted_foreground;
        let refresh = Button::new(("ssh-forward-refresh", pane_id))
            .label("Refresh")
            .small()
            .on_click(cx.listener(|this, _, _w, cx| this.refresh_loopback_forwards(cx)));
        let close = Button::new(("ssh-forward-panel-close", pane_id))
            .label("Close")
            .small()
            .on_click(cx.listener(|this, _, _w, cx| this.close_loopback_forward_panel(cx)));

        let body = if forwards.is_empty() {
            v_flex().child(
                div()
                    .text_sm()
                    .text_color(muted_foreground)
                    .child("No active forwards for this host."),
            )
        } else {
            let mut list = v_flex().gap_2();
            for forward in forwards {
                list = list.child(self.render_loopback_forward_row(forward, cx));
            }
            list
        };

        v_flex()
            .w(px(460.))
            .max_h(px(420.))
            .gap_3()
            .p_3()
            .overflow_hidden()
            .bg(popover)
            .border_1()
            .border_color(border)
            .rounded_lg()
            .shadow_lg()
            .child(
                h_flex()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(foreground)
                                    .child("SSH forwards"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted_foreground)
                                    .child(remote.target.clone()),
                            ),
                    )
                    .child(h_flex().gap_2().child(refresh).child(close)),
            )
            .child(self.render_loopback_forward_form(pane_id, cx))
            .child(body)
    }

    fn render_loopback_forward_form(&self, pane_id: u64, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let host_input = self.loopback_panel.host_input.clone();
        let port_input = self.loopback_panel.port_input.clone();
        let editing = self.loopback_panel.editing.clone();
        let title = if editing.is_some() {
            "Edit forward"
        } else {
            "Add forward"
        };
        let save_label = if editing.is_some() { "Save" } else { "Add" };
        let cancel =
            editing.is_some().then(|| {
                Button::new(("ssh-forward-cancel", pane_id))
                    .label("Cancel")
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.cancel_loopback_forward_edit(window, cx)
                    }))
            });

        let host = div()
            .w(px(180.))
            .child(Input::new(&host_input).small())
            .into_any_element();
        let port = div()
            .w(px(92.))
            .child(Input::new(&port_input).small())
            .into_any_element();

        v_flex()
            .gap_2()
            .py_1()
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        v_flex().gap_0p5().child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.foreground)
                                .child(title),
                        ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new(("ssh-forward-save", pane_id))
                                    .label(save_label)
                                    .small()
                                    .primary()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.save_loopback_forward_form(pane_id, window, cx)
                                    })),
                            )
                            .when_some(cancel, |row, button| row.child(button)),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(host)
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(":"),
                    )
                    .child(port),
            )
    }

    fn render_loopback_forward_row(
        &self,
        forward: &LoopbackForwardInfo,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = cx.theme();
        let id = forward.id.clone();
        let remote = format!("{}:{}", forward.id.remote_host, forward.id.remote_port);
        let local = format!("http://127.0.0.1:{}", forward.local_port);
        let local_url = local.clone();
        let details = format!(
            "idle {} · age {}",
            human_duration(forward.idle_secs),
            human_duration(forward.age_secs)
        );
        let close_id = SharedString::from(format!(
            "ssh-forward-close-{}-{}-{}-{}",
            forward.id.pane_id, forward.id.target, forward.id.remote_host, forward.id.remote_port
        ));
        let edit_id = SharedString::from(format!(
            "ssh-forward-edit-{}-{}-{}-{}",
            forward.id.pane_id, forward.id.target, forward.id.remote_host, forward.id.remote_port
        ));
        let open_id = SharedString::from(format!(
            "ssh-forward-open-{}-{}-{}-{}-{}",
            forward.id.pane_id,
            forward.id.target,
            forward.id.remote_host,
            forward.id.remote_port,
            forward.local_port
        ));

        h_flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .child(
                v_flex()
                    .gap_0p5()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().text_sm().text_color(theme.foreground).child(remote))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("->"),
                            )
                            .child(
                                div()
                                    .id(open_id)
                                    .text_sm()
                                    .text_color(theme.accent)
                                    .cursor_pointer()
                                    .hover(|style| style.bg(theme.accent.opacity(0.08)).underline())
                                    .child(local)
                                    .on_click(cx.listener(move |_this, _, _window, cx| {
                                        cx.open_url(&local_url);
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(details),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new(edit_id)
                            .label("Edit")
                            .small()
                            .on_click(cx.listener({
                                let id = id.clone();
                                move |this, _, window, cx| {
                                    this.edit_loopback_forward(id.clone(), window, cx)
                                }
                            })),
                    )
                    .child(
                        Button::new(close_id)
                            .label("Close")
                            .small()
                            .on_click(cx.listener(move |this, _, _w, cx| {
                                this.close_loopback_forward(id.clone(), cx)
                            })),
                    ),
            )
    }
}

fn human_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 60 * 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

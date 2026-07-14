//! Pane-contextual SSH loopback forward controls.
//!
//! Settings owns persistent preferences; this module owns the live forwarding
//! dashboard that only makes sense beside a concrete SSH pane.

use gpui::{AnyElement, Context, Div, Entity, FontWeight, div, prelude::*, px};
use gpui_component::Selectable as _;
use gpui_component::badge::Badge;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};

use crate::daemon::protocol::{ForwardStatus, ManagedForward, RemoteContext, SshForwardKind};
use crate::terminal::view::TerminalView;
use crate::ui::app::Tty7App;

impl Tty7App {
    /// The in-pane native-SSH notice (PRD FR-E4): a dead pane shows a
    /// bottom-centered "Disconnected — ⌘⇧R to reconnect" bar. Live/connecting
    /// panes show nothing here — the tab status dot already carries the phase and
    /// the daemon prints connect progress/failures into the buffer. Returns
    /// `None` for a non-native or still-alive pane.
    pub(crate) fn render_ssh_status_strip(
        &self,
        leaf: &Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let view = leaf.read(cx);
        view.ssh_phase()?;
        if !view.ssh_disconnected() {
            return None;
        }
        let host = view
            .terminal
            .ssh_endpoint()
            .map(|(h, _)| h)
            .or_else(|| view.remote_context().map(|c| c.target))
            .unwrap_or_default();

        let theme = cx.theme();

        // The failure reason is already printed into the terminal buffer
        // (top-left, in red) by the daemon, so a top-left overlay would sit right
        // on top of it. Dock the (actionable) reconnect notice at the
        // bottom-center — clear of the output, a familiar "connection lost,
        // reconnect" spot.
        let bar = h_flex()
            .occlude()
            .items_center()
            .gap_2()
            .px_3()
            .py_1p5()
            .rounded_lg()
            .bg(theme.popover)
            .border_1()
            .border_color(theme.danger.opacity(0.4))
            .shadow_md()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.foreground)
                    .child(if host.is_empty() {
                        "Disconnected".to_string()
                    } else {
                        format!("Disconnected from {host}")
                    }),
            )
            .child(div().child("· ⌘⇧R"))
            .child(
                Button::new("ssh-reconnect")
                    .label("Reconnect")
                    .primary()
                    .small()
                    .on_click(
                        cx.listener(|this, _, window, cx| this.restart_ssh_session(window, cx)),
                    ),
            );
        Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_4()
                .flex()
                .justify_center()
                .child(bar)
                .into_any_element(),
        )
    }

    /// The in-pane "confirm close of a live SSH session" sheet (PRD FR-E3),
    /// centered over the terminal. Enter/Close closes; Esc/Keep cancels. Returns
    /// `None` when no confirmation is pending.
    pub(crate) fn render_ssh_close_confirm_overlay(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.ssh_close_confirm?;
        let theme = cx.theme();
        let card = v_flex()
            .w(px(360.))
            .gap_3()
            .p_4()
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .rounded_lg()
            .shadow_lg()
            .occlude()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Close this SSH session?"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("The connection is live. Closing will end it."),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("ssh-close-cancel")
                            .label("Keep")
                            .small()
                            .on_click(
                                cx.listener(|this, _, _window, cx| this.cancel_ssh_close(cx)),
                            ),
                    )
                    .child(
                        Button::new("ssh-close-confirm")
                            .label("Close")
                            .primary()
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm_ssh_close(window, cx)
                            })),
                    ),
            );
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .child(card)
                .into_any_element(),
        )
    }

    /// Pane-contextual action buttons for a connected native-SSH pane, pinned
    /// top-right of the terminal body: a **tunnel** icon that toggles the port
    /// forwarding panel and an **SFTP** icon that toggles the file browser. The
    /// panels themselves are unchanged; these are just discoverable entry points
    /// beside the top-left ` SSH ` status strip (status vs. actions). The tunnel
    /// icon carries a small count badge when one or more forwards are active.
    ///
    /// The caller gates this to a connected native pane (see `app.rs` render), so
    /// the buttons never appear for a plain foreground `ssh` or a still-connecting
    /// session.
    pub(crate) fn render_loopback_forward_overlay(
        &self,
        pane_id: u64,
        remote: &RemoteContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let foreground = cx.theme().foreground;
        let active_count = self
            .loopback_panel
            .managed
            .iter()
            .filter(|m| m.pane_id == pane_id)
            .count();
        let panel_open = self.loopback_panel.open_pane_id == Some(pane_id);
        let sftp_open = self.sftp_panel.open_pane_id == Some(pane_id);

        // Tunnel (port forwarding). ExternalLink is the closest network/arrows
        // glyph the icon set ships — it reads as "traffic forwarded out".
        let tunnel_button = Button::new(("ssh-forward-icon", pane_id))
            .icon(IconName::ExternalLink)
            .ghost()
            .small()
            .selected(panel_open)
            .tooltip("Port forwarding")
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.toggle_loopback_forward_panel(pane_id, cx)
            }));
        // A tiny count badge when ≥1 forward is active; the bare icon otherwise.
        let tunnel: AnyElement = if active_count > 0 {
            Badge::new()
                .count(active_count)
                .child(tunnel_button)
                .into_any_element()
        } else {
            tunnel_button.into_any_element()
        };

        // SFTP (file browser). Folder is the natural glyph.
        let sftp_button = Button::new(("ssh-sftp-icon", pane_id))
            .icon(IconName::Folder)
            .ghost()
            .small()
            .selected(sftp_open)
            .tooltip("SFTP")
            .on_click(cx.listener(move |this, _, window, cx| this.toggle_sftp(window, cx)));

        div()
            .absolute()
            .top_2()
            .right_4()
            .flex()
            .flex_col()
            .items_end()
            .gap_2()
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .child(tunnel)
                    .child(sftp_button),
            )
            .when(panel_open, |this| {
                this.child(self.render_loopback_forward_panel(pane_id, remote, cx))
            })
            .text_color(foreground)
            .into_any_element()
    }

    /// The port-forwarding panel: a single unified forwards list plus one L/R/D add
    /// form (Tabby-like). Auto forwards created by Cmd-clicking a `localhost:PORT`
    /// link (FR-F4) arrive as plain Local rows in this same list.
    fn render_loopback_forward_panel(
        &self,
        pane_id: u64,
        remote: &RemoteContext,
        cx: &mut Context<Self>,
    ) -> Div {
        let popover = cx.theme().popover;
        let border = cx.theme().border;
        let foreground = cx.theme().foreground;
        let muted_foreground = cx.theme().muted_foreground;
        let close = Button::new(("ssh-forward-panel-close", pane_id))
            .icon(IconName::Close)
            .ghost()
            .small()
            .tooltip("Close")
            .on_click(cx.listener(|this, _, _w, cx| this.close_loopback_forward_panel(cx)));

        v_flex()
            .w(px(460.))
            .max_h(px(560.))
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
                    .child(close),
            )
            .child(self.render_managed_forwards_section(pane_id, cx))
    }

    /// The single port-forwarding section for a native-SSH pane: an add form with a
    /// Local/Remote/Dynamic kind selector and the live forward rows (including the
    /// auto localhost-link forwards, which read as Local rows).
    fn render_managed_forwards_section(&self, pane_id: u64, cx: &mut Context<Self>) -> Div {
        let foreground = cx.theme().foreground;
        let muted_foreground = cx.theme().muted_foreground;
        let managed: Vec<ManagedForward> = self
            .loopback_panel
            .managed
            .iter()
            .filter(|m| m.pane_id == pane_id)
            .cloned()
            .collect();

        let body = if managed.is_empty() {
            v_flex().child(
                div()
                    .text_sm()
                    .text_color(muted_foreground)
                    .child("No forwards yet."),
            )
        } else {
            let mut list = v_flex().gap_2();
            for forward in &managed {
                list = list.child(self.render_managed_forward_row(forward, cx));
            }
            list
        };

        v_flex()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(foreground)
                    .child("Port forwarding"),
            )
            .child(self.render_managed_forward_form(pane_id, cx))
            .child(body)
    }

    fn render_managed_forward_form(&self, pane_id: u64, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let kind = self.loopback_panel.mf_kind;
        let editing = self.loopback_panel.mf_editing.is_some();
        let selected = match kind {
            SshForwardKind::Local => 0,
            SshForwardKind::Remote => 1,
            SshForwardKind::Dynamic => 2,
        };
        // Dynamic (SOCKS) forwards have no fixed target — grey the target inputs.
        let needs_target = kind != SshForwardKind::Dynamic;

        let bind_host = div()
            .w(px(150.))
            .child(Input::new(&self.loopback_panel.mf_bind_host).small());
        let bind_port = div()
            .w(px(80.))
            .child(Input::new(&self.loopback_panel.mf_bind_port).small());
        let target_host = div()
            .w(px(150.))
            .child(Input::new(&self.loopback_panel.mf_target_host).small());
        let target_port = div()
            .w(px(80.))
            .child(Input::new(&self.loopback_panel.mf_target_port).small());
        let description = div()
            .w_full()
            .child(Input::new(&self.loopback_panel.mf_description).small());

        v_flex()
            .gap_2()
            .py_1()
            .child(self.segmented(
                "ssh-managed-forward-kind",
                &["Local", "Remote", "Dynamic"],
                selected,
                cx,
                move |this, ix, _window, cx| {
                    let kind = match ix {
                        1 => SshForwardKind::Remote,
                        2 => SshForwardKind::Dynamic,
                        _ => SshForwardKind::Local,
                    };
                    this.set_managed_forward_kind(kind, cx);
                },
            ))
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .child(div().w(px(48.)).text_xs().text_color(muted).child("bind"))
                    .child(bind_host)
                    .child(div().text_sm().text_color(muted).child(":"))
                    .child(bind_port),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .opacity(if needs_target { 1.0 } else { 0.4 })
                    .child(
                        div()
                            .w(px(48.))
                            .text_xs()
                            .text_color(muted)
                            .child(if needs_target { "target" } else { "SOCKS" }),
                    )
                    .child(target_host)
                    .child(div().text_sm().text_color(muted).child(":"))
                    .child(target_port),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(description)
                    .when(editing, |row| {
                        row.child(
                            Button::new(("ssh-managed-forward-cancel", pane_id))
                                .label("Cancel")
                                .small()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.cancel_managed_forward_edit(window, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new(("ssh-managed-forward-add", pane_id))
                            .label(if editing { "Save" } else { "Add" })
                            .small()
                            .primary()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.add_managed_forward(pane_id, window, cx)
                            })),
                    ),
            )
    }

    fn render_managed_forward_row(&self, forward: &ManagedForward, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (badge, badge_color) = match forward.kind {
            SshForwardKind::Local => ("L", theme.info),
            SshForwardKind::Remote => ("R", theme.warning),
            SshForwardKind::Dynamic => ("D", theme.success),
        };
        let bind = format!("{}:{}", forward.bind_host, forward.bind_port);
        let flow = if forward.kind == SshForwardKind::Dynamic {
            format!("{bind}  (SOCKS)")
        } else {
            format!("{bind} -> {}:{}", forward.target_host, forward.target_port)
        };
        let (status_text, status_color) = match &forward.status {
            ForwardStatus::Listening => ("listening".to_string(), theme.success),
            ForwardStatus::Error(msg) => (format!("error: {msg}"), theme.danger),
        };
        let pane_id = forward.pane_id;
        let forward_id = forward.id;
        let forward_for_edit = forward.clone();

        h_flex()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .child(
                div()
                    .flex_none()
                    .w(px(20.))
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .bg(badge_color.opacity(0.15))
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .text_color(badge_color)
                    .child(badge),
            )
            .child(
                v_flex()
                    .gap_0p5()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().text_color(theme.foreground).child(flow))
                    .when_some(forward.description.clone(), |el, desc| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(desc),
                        )
                    })
                    .child(div().text_xs().text_color(status_color).child(status_text)),
            )
            .child(
                Button::new(("ssh-managed-forward-edit", forward_id as usize))
                    .label("Edit")
                    .small()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.edit_managed_forward(forward_for_edit.clone(), window, cx)
                    })),
            )
            .child(
                Button::new(("ssh-managed-forward-del", forward_id as usize))
                    .label("Delete")
                    .small()
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.remove_managed_forward(pane_id, forward_id, cx)
                    })),
            )
    }
}

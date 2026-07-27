//! Pane-contextual SSH loopback forward controls.
//!
//! Settings owns persistent preferences; this module owns the live forwarding
//! dashboard that only makes sense beside a concrete SSH pane.
//!
//! The dashboard is a **band in the detail panel's Info tab**, not a popover over
//! the terminal: a pane's forwards are one of its facts, so they belong beside its
//! cwd, processes and ports rather than in a floating panel of their own. The
//! rendering helpers here are called from `right_panel`'s Info body.

use gpui::{AnyElement, Context, Div, Entity, FontWeight, Stateful, div, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use crate::daemon::protocol::{ForwardStatus, ManagedForward, SshForwardKind};
use crate::terminal::view::TerminalView;
use crate::ui::app::{CONTENT_INSET, Tty7App};

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

    /// The Info tab's **Forwards** band: what this pane routes across its
    /// connection, sitting under Ports, which says what it listens on locally.
    /// `None` for anything but a connected native-SSH pane — the band doesn't
    /// exist rather than showing an empty section on every local shell.
    ///
    /// The rows are the daemon's list, re-fetched on the Info tab's own poll (see
    /// `right_panel::sync_procs`), so a forward that dies out from under us turns
    /// red here without anyone clicking anything.
    pub(crate) fn forwards_section(
        &self,
        pane_id: Option<u64>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let pane_id = pane_id?;
        let open = self.loopback_panel.form_pane_id == Some(pane_id);
        // The `+` toggles the add form open. It's the band's only control, so it
        // takes the header's trailing slot rather than a row of its own.
        let add = crate::ui::tab_strip::chrome_tile(
            Button::new(("ssh-forward-add-toggle", pane_id))
                .icon(Icon::empty().path("icons/plus.svg").size(px(13.))),
            open,
            cx,
        )
        .xsmall()
        .w(px(24.))
        .h(px(24.))
        .rounded_md()
        .tooltip(if open { "Cancel" } else { "Add forward" })
        .on_click(cx.listener(move |this, _, window, cx| {
            this.toggle_managed_forward_form(pane_id, window, cx)
        }))
        .into_any_element();

        let managed: Vec<ManagedForward> = self
            .loopback_panel
            .managed
            .iter()
            .filter(|m| m.pane_id == pane_id)
            .cloned()
            .collect();

        let mono = cx.theme().mono_font_family.clone();
        // Rows inset themselves rather than the list, so the hover capsule bleeds
        // into the same 12px gutter the Changes rows use.
        let mut list = v_flex().px(px(CONTENT_INSET - 4.)).py(px(2.)).gap(px(1.));
        for forward in &managed {
            list = list.child(self.forward_row(forward, &mono, cx));
        }

        Some(
            v_flex()
                .child(self.panel_subtitle("Forwards", true, Some(add), cx))
                // The empty line is suppressed while the form is open: the form
                // *is* the answer to "nothing here yet".
                .when(managed.is_empty() && !open, |this| {
                    this.child(
                        div()
                            .px(px(CONTENT_INSET))
                            .py(px(2.))
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("None."),
                    )
                })
                .when(!managed.is_empty(), |this| this.child(list))
                .when(open, |this| this.child(self.forward_form(pane_id, cx)))
                .into_any_element(),
        )
    }

    /// One forward, in the Info list's language: a mono kind letter, the bound
    /// port as the same chip a listening port gets, and the destination trailing
    /// it. Click to load it into the form (edit = re-establish); the `×` revealed
    /// on hover tears it down. A description, where one was typed, takes a second
    /// muted line — the only thing on the row that isn't derivable from the rule.
    fn forward_row(
        &self,
        forward: &ManagedForward,
        mono: &gpui::SharedString,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let letter = match forward.kind {
            SshForwardKind::Local => "L",
            SshForwardKind::Remote => "R",
            SshForwardKind::Dynamic => "D",
        };
        let errored = matches!(forward.status, ForwardStatus::Error(_));
        // A bind host worth naming is one that isn't the loopback default —
        // `0.0.0.0` means "reachable from the network", which the row must not
        // hide behind a bare port number.
        let bind = if matches!(forward.bind_host.as_str(), "127.0.0.1" | "localhost" | "") {
            forward.bind_port.to_string()
        } else {
            format!("{}:{}", forward.bind_host, forward.bind_port)
        };
        // The tail carries the error where there is one: an error is what you
        // need to read, and the destination is still on the row you clicked from.
        let tail = match &forward.status {
            ForwardStatus::Error(msg) => msg.clone(),
            ForwardStatus::Listening => match forward.kind {
                SshForwardKind::Dynamic => "SOCKS".to_string(),
                _ => format!("→ {}:{}", forward.target_host, forward.target_port),
            },
        };
        let pane_id = forward.pane_id;
        let forward_id = forward.id;
        let forward_for_edit = forward.clone();
        let group = gpui::SharedString::from(format!("panel-forward-{forward_id}"));

        h_flex()
            .id(("panel-forward", forward_id as usize))
            .group(group.clone())
            .items_center()
            .gap(px(8.))
            .px(px(4.))
            .py(px(3.))
            .rounded(px(5.))
            .cursor_pointer()
            .hover(|s| s.bg(gpui::rgb(sf.hover)))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.edit_managed_forward(forward_for_edit.clone(), window, cx)
            }))
            .child(crate::ui::right_panel::git_badge(
                letter,
                if errored { theme.danger } else { muted },
                mono,
            ))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(1.))
                    .child(
                        h_flex()
                            .items_center()
                            .gap(px(6.))
                            .child(crate::ui::right_panel::info_chip(
                                &bind,
                                theme.accent,
                                theme.foreground,
                                mono,
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.))
                                    .font_family(mono.clone())
                                    .text_color(if errored { theme.danger } else { muted })
                                    .child(tail),
                            ),
                    )
                    .when_some(forward.description.clone(), |this, desc| {
                        this.child(
                            div()
                                .truncate()
                                .text_size(px(11.))
                                .text_color(muted)
                                .child(desc),
                        )
                    }),
            )
            .child(
                // Revealed on row hover — the same progressive disclosure the
                // sidebar's rows use, so a list of forwards stays a list.
                div()
                    .flex_shrink_0()
                    .opacity(0.)
                    .group_hover(group, |s| s.opacity(1.))
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        crate::ui::tab_strip::chrome_tile(
                            Button::new(("panel-forward-del", forward_id as usize))
                                .icon(IconName::Close)
                                .xsmall(),
                            false,
                            cx,
                        )
                        .w(px(18.))
                        .h(px(18.))
                        .rounded(px(4.))
                        .tooltip("Remove")
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                this.remove_managed_forward(pane_id, forward_id, cx)
                            },
                        )),
                    ),
            )
    }

    /// The add/edit form, inline under the band. The 460px three-column layout the
    /// old popover used doesn't survive a 260px column, so the fields stack: kind,
    /// then one line each for bind and target, each `host : port`.
    fn forward_form(&self, pane_id: u64, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        // The form is inside the right panel, i.e. on the sunk rail — not on the
        // settings sheet the segmented control otherwise assumes.
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let kind = self.loopback_panel.mf_kind;
        let editing = self.loopback_panel.mf_editing.is_some();
        let selected = match kind {
            SshForwardKind::Local => 0,
            SshForwardKind::Remote => 1,
            SshForwardKind::Dynamic => 2,
        };
        // Dynamic (SOCKS) forwards have no fixed target — grey the target line.
        let needs_target = kind != SshForwardKind::Dynamic;

        // `host : port` on one line, the port sized to four digits and the host
        // taking what's left.
        let pair = |label: &'static str,
                    host: &Entity<gpui_component::input::InputState>,
                    port: &Entity<gpui_component::input::InputState>| {
            h_flex()
                .items_center()
                .gap(px(4.))
                .child(
                    div()
                        .flex_none()
                        .w(px(30.))
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().child(Input::new(host).xsmall()))
                .child(div().text_size(px(11.)).text_color(muted).child(":"))
                .child(div().w(px(52.)).child(Input::new(port).xsmall()))
        };

        v_flex()
            .px(px(CONTENT_INSET))
            .pt(px(6.))
            .pb(px(2.))
            .gap(px(5.))
            .child(self.segmented_on(
                sf,
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
            .child(pair(
                "bind",
                &self.loopback_panel.mf_bind_host,
                &self.loopback_panel.mf_bind_port,
            ))
            .child(
                div()
                    .opacity(if needs_target { 1.0 } else { 0.4 })
                    .child(pair(
                        if needs_target { "to" } else { "SOCKS" },
                        &self.loopback_panel.mf_target_host,
                        &self.loopback_panel.mf_target_port,
                    )),
            )
            .child(Input::new(&self.loopback_panel.mf_description).xsmall())
            .child(
                h_flex()
                    .justify_end()
                    .gap(px(4.))
                    .pt(px(1.))
                    .child(
                        Button::new(("ssh-managed-forward-cancel", pane_id))
                            .label("Cancel")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_managed_forward_form(window, cx)
                            })),
                    )
                    .child(
                        Button::new(("ssh-managed-forward-add", pane_id))
                            .label(if editing { "Save" } else { "Add" })
                            .primary()
                            .xsmall()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.add_managed_forward(pane_id, window, cx)
                            })),
                    ),
            )
    }
}

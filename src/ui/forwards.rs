use gpui::{AnyElement, Context, Div, Entity, FontWeight, Stateful, div, prelude::*, px, rems};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};

use crate::daemon::protocol::{ForwardStatus, ManagedForward, SshForwardKind};
use crate::terminal::view::TerminalView;
use crate::ui::app::{CONTENT_INSET, TILE_GLYPH_SM, TILE_SIZE_SM, Tty7App};
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::right_panel::{META, TEXT, TEXT_MONO};

/// How far a forward's target endpoint fades when the rule is Dynamic and has
/// no target to name. The rules editor in Settings and the live Forwards panel
/// draw the same row, so they fade it by the same amount.
pub(crate) const NO_TARGET_FADE: f32 = 0.4;

impl Tty7App {
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

        // Why it ended. The strip used to say only that it *had* ended, so a
        // rejected key and a dropped network read identically — and the reason
        // scrolled away with the pane's own output.
        let reason = match view.ssh_phase() {
            Some(crate::daemon::protocol::SshPhase::Failed { reason }) if !reason.is_empty() => {
                Some(reason)
            }
            _ => None,
        };
        // A saved connection is editable; a one-off `user@host` is not, and
        // offering to edit one would open an empty page.
        let profile = view
            .ssh_spec()
            .and_then(|s| s.profile_id.clone())
            .and_then(|id| uuid::Uuid::parse_str(&id).ok());

        let theme = cx.theme();

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
            // Off the right panel's ramp on purpose: this bar floats over the
            // terminal, not inside the panel, and it is sized against the
            // terminal's own text. `app.rs` draws it, `render_panel_info` does
            // not.
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.foreground)
                    .child(if host.is_empty() {
                        t(L10nKey::ForwardDisconnected).to_string()
                    } else {
                        t_fmt(L10nKey::ForwardDisconnectedFrom, &[("host", &host)])
                    }),
            )
            .children(reason.map(|reason| {
                // The reason is whatever ssh said, and it runs past 360px more
                // often than not — a truncated "…" was the only account of a
                // rejected key the pane had left, its own output having
                // scrolled away. Hovering gives the whole sentence back.
                let full = reason.clone();
                div()
                    .id("ssh-strip-reason")
                    .max_w(px(360.))
                    .truncate()
                    .text_color(theme.danger)
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(full.clone()).build(window, cx)
                    })
                    .child(reason)
            }))
            // The chord was spelled "⌘⇧R" here. That is one platform's
            // rendering of one default: off macOS it names a modifier the
            // keyboard does not have, and either way it kept saying ⌘⇧R after
            // the user had rebound Reconnect to something else. Ask the keymap,
            // and say nothing at all when the action carries no binding.
            .children(
                crate::ui::home::key_hint("RestartSshSession", cx)
                    .map(|keys| div().child(format!("· {keys}"))),
            )
            .child(
                Button::new("ssh-reconnect")
                    .label(crate::ui::i18n::t(crate::ui::i18n::L10nKey::Reconnect))
                    .primary()
                    .small()
                    .on_click(
                        cx.listener(|this, _, window, cx| this.restart_ssh_session(window, cx)),
                    ),
            )
            .children(profile.map(|id| {
                Button::new("ssh-edit-profile")
                    .label(crate::ui::i18n::t(crate::ui::i18n::L10nKey::SshEditProfile))
                    .ghost()
                    .small()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_ssh_profile_in_settings(id, window, cx)
                    }))
            }));
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

    pub(crate) fn forwards_section(
        &self,
        pane_id: Option<u64>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let pane_id = pane_id?;
        let open = self.loopback_panel.form_pane_id == Some(pane_id);
        // The section's own affordance, and the same 24px chrome tile the Info
        // tab's cwd actions use. It used to be built by hand — a 32px tile
        // forced down to 24 and then set `.xsmall()`, which quietly overrode
        // the 13px the icon asked for with the button size's own 12, so the
        // glyph never was the size the code claimed. `chrome_tile_sized`
        // derives it instead: `TILE_GLYPH_SM / BUTTON_ICON_SCALE` of the button
        // size, the same pair every other 24px tile in the panel is on.
        let add = crate::ui::tab_strip::chrome_tile_sized(
            Button::new(("ssh-forward-add-toggle", pane_id))
                .icon(Icon::empty().path("icons/plus.svg")),
            TILE_SIZE_SM,
            TILE_GLYPH_SM,
            open,
            cx,
        )
        .rounded_md()
        .tooltip(if open {
            t(L10nKey::Cancel)
        } else {
            t(L10nKey::ForwardTooltipAdd)
        })
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
        let mut list = v_flex().px(px(CONTENT_INSET - 4.)).py(px(2.)).gap(px(1.));
        for forward in &managed {
            list = list.child(self.forward_row(forward, &mono, cx));
        }

        Some(
            v_flex()
                .child(self.panel_subtitle(t(L10nKey::ForwardPanelTitle), true, Some(add), cx))
                .when(managed.is_empty() && !open, |this| {
                    this.child(
                        div()
                            .px(px(CONTENT_INSET))
                            .py(px(2.))
                            .text_size(rems(TEXT))
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::ui::i18n::t(crate::ui::i18n::L10nKey::None)),
                    )
                })
                .when(!managed.is_empty(), |this| this.child(list))
                .when(open, |this| this.child(self.forward_form(pane_id, cx)))
                .into_any_element(),
        )
    }

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
        let bind = if matches!(forward.bind_host.as_str(), "127.0.0.1" | "localhost" | "") {
            forward.bind_port.to_string()
        } else {
            format!("{}:{}", forward.bind_host, forward.bind_port)
        };
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
                                    .text_size(rems(TEXT_MONO))
                                    .font_family(mono.clone())
                                    .text_color(if errored { theme.danger } else { muted })
                                    .child(tail),
                            ),
                    )
                    .when_some(forward.description.clone(), |this, desc| {
                        this.child(
                            div()
                                .truncate()
                                .text_size(rems(META))
                                .text_color(muted)
                                .child(desc),
                        )
                    }),
            )
            .child(
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
                        .w(px(crate::ui::tab_strip::MIN_TARGET))
                        .h(px(crate::ui::tab_strip::MIN_TARGET))
                        .rounded(px(4.))
                        .tooltip(t(L10nKey::ForwardTooltipRemove))
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                this.remove_managed_forward(pane_id, forward_id, cx)
                            },
                        )),
                    ),
            )
    }

    fn forward_form(&self, pane_id: u64, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let danger = theme.danger;
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let kind = self.loopback_panel.mf_kind;
        let editing = self.loopback_panel.mf_editing.is_some();
        let selected = match kind {
            SshForwardKind::Local => 0,
            SshForwardKind::Remote => 1,
            SshForwardKind::Dynamic => 2,
        };
        let needs_target = kind != SshForwardKind::Dynamic;

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
                        .text_size(rems(META))
                        .text_color(muted)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().child(Input::new(host).xsmall()))
                .child(div().text_size(rems(META)).text_color(muted).child(":"))
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
                &[
                    t(L10nKey::ForwardLocal),
                    t(L10nKey::ForwardRemote),
                    t(L10nKey::ForwardDynamic),
                ],
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
                t(L10nKey::ForwardBindLabel),
                &self.loopback_panel.mf_bind_host,
                &self.loopback_panel.mf_bind_port,
            ))
            .child(
                div()
                    .opacity(if needs_target { 1.0 } else { NO_TARGET_FADE })
                    .child(pair(
                        if needs_target {
                            t(L10nKey::ForwardToLabel)
                        } else {
                            t(L10nKey::ForwardSocksLabel)
                        },
                        &self.loopback_panel.mf_target_host,
                        &self.loopback_panel.mf_target_port,
                    )),
            )
            .child(Input::new(&self.loopback_panel.mf_description).xsmall())
            .when_some(self.loopback_panel.mf_error.clone(), |this, error| {
                this.child(div().text_size(rems(META)).text_color(danger).child(error))
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap(px(4.))
                    .pt(px(1.))
                    .child(
                        Button::new(("ssh-managed-forward-cancel", pane_id))
                            .label(t(L10nKey::Cancel))
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_managed_forward_form(window, cx)
                            })),
                    )
                    .child(
                        Button::new(("ssh-managed-forward-add", pane_id))
                            .label(if editing {
                                t(L10nKey::Save)
                            } else {
                                t(L10nKey::ForwardAdd)
                            })
                            .primary()
                            .xsmall()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.add_managed_forward(pane_id, window, cx)
                            })),
                    ),
            )
    }
}

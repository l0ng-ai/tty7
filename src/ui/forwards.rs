use gpui::{AnyElement, Context, Div, Entity, FontWeight, Stateful, div, prelude::*, px, rems};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::{ActiveTheme as _, Disableable as _, IconName, Sizable as _, h_flex, v_flex};

use crate::core::ssh_profile::ForwardRule;
use crate::daemon::protocol::{ForwardStatus, ManagedForward, SshForwardKind, SshForwardRule};
use crate::terminal::view::TerminalView;
use crate::ui::app::{CONTENT_INSET, Tty7App};
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::right_panel::{META, TEXT_MONO};

/// How far a forward's target endpoint fades when the rule is Dynamic and has
/// no target to name. The rules editor in Settings and the live Forwards panel
/// draw the same row, so they fade it by the same amount.
pub(crate) const NO_TARGET_FADE: f32 = 0.4;

/// The managed-forward form's five text fields, read out of their inputs.
///
/// Split out so the question "do these make a rule?" can be asked without a
/// `Window` and answered the same way twice: `add_managed_forward` needs the
/// rule, and `forward_form` needs to know whether there is one yet — that is
/// what decides whether Add is live and whether the form says what is missing.
pub(crate) struct ForwardFields {
    /// Whether the whole `ssh -L` grammar is on screen. With it off the form
    /// is one number and the rule is derived from it.
    pub(crate) advanced: bool,
    pub(crate) kind: SshForwardKind,
    pub(crate) bind_host: String,
    pub(crate) bind_port: String,
    pub(crate) target_host: String,
    pub(crate) target_port: String,
    pub(crate) description: String,
}

impl ForwardFields {
    /// The rule these fields describe, or `None` while they do not describe
    /// one yet.
    ///
    /// The same conditions the settings sheet's `ForwardRuleForm::collect`
    /// applies, so a rule typed here and a rule typed there are accepted or
    /// refused alike — including port 0, which parses as a `u16` but asks the
    /// OS to pick the port, and there is nowhere in either form to say which
    /// one it picked.
    pub(crate) fn collect(&self) -> Option<SshForwardRule> {
        if !self.advanced {
            return self.simple_rule();
        }
        let bind_port: u16 = self.bind_port.trim().parse().ok().filter(|p| *p > 0)?;
        let (target_host, target_port) = if self.kind == SshForwardKind::Dynamic {
            (String::new(), 0)
        } else {
            let port: u16 = self.target_port.trim().parse().ok().filter(|p| *p > 0)?;
            let host = self.target_host.trim();
            if host.is_empty() {
                return None;
            }
            (host.to_string(), port)
        };
        let bind_host = match self.bind_host.trim() {
            // The panel's own default, and the one the strip's tooltip
            // promises: an empty bind host is loopback, not every interface.
            "" => "127.0.0.1".to_string(),
            host => host.to_string(),
        };
        let description = self.description.trim();
        Some(SshForwardRule {
            kind: self.kind,
            bind_host,
            bind_port,
            target_host,
            target_port,
            description: (!description.is_empty()).then(|| description.to_string()),
            enabled: true,
        })
    }

    /// The rule the short form's single number describes: bring the far
    /// side's own `:port` over to the same number here.
    ///
    /// The bind port is that same number rather than 0 so the address is one
    /// anybody can predict — remote :3000 is localhost:3000. When it is taken
    /// on this machine `add_managed_forward` retries with 0 and the OS picks,
    /// which is a better answer than handing the collision back to be solved
    /// by hand.
    fn simple_rule(&self) -> Option<SshForwardRule> {
        let port: u16 = self.target_port.trim().parse().ok().filter(|p| *p > 0)?;
        Some(SshForwardRule {
            kind: SshForwardKind::Local,
            bind_host: "127.0.0.1".to_string(),
            bind_port: port,
            target_host: "localhost".to_string(),
            target_port: port,
            // No field for it on screen, so nothing to carry: a description
            // left over from a trip through the advanced form is not something
            // this rule was given.
            description: None,
            enabled: true,
        })
    }

    /// Whether the form is still empty enough that saying what is missing
    /// would be nagging rather than helping — the same restraint the settings
    /// sheet shows through `ForwardRuleForm::is_blank`.
    pub(crate) fn is_blank(&self) -> bool {
        if !self.advanced {
            return self.target_port.trim().is_empty();
        }
        [
            &self.bind_host,
            &self.bind_port,
            &self.target_host,
            &self.target_port,
            &self.description,
        ]
        .iter()
        .all(|v| v.trim().is_empty())
    }
}

/// The entry a forward request just appended: the one the panel did not have
/// before it asked.
///
/// Ids come from a counter that only goes up, so "none of the ids from before"
/// names the new entry exactly — and it is the new entry that says whether the
/// rule is listening or why it is not.
pub(crate) fn added_forward<'a>(
    before: &[u64],
    list: &'a [ManagedForward],
) -> Option<&'a ManagedForward> {
    list.iter().find(|m| !before.contains(&m.id))
}

/// The rule a live forward was made from.
///
/// An edit removes the old forward before adding the new one, so when the new
/// one will not come up this is what puts the old one back.
pub(crate) fn rule_of(forward: &ManagedForward) -> SshForwardRule {
    SshForwardRule {
        kind: forward.kind,
        bind_host: forward.bind_host.clone(),
        bind_port: forward.bind_port,
        target_host: forward.target_host.clone(),
        target_port: forward.target_port,
        description: forward.description.clone(),
        enabled: forward.enabled,
    }
}

/// Which of a saved host's rules a live forward was opened from, among
/// those not already switched to `enabled` — the one a switch in the panel
/// has to be written back to.
///
/// Matched on the whole mapping, since a live forward carries no reference to
/// the rule it came from. A saved rule always names its port, so the port the
/// forward bound is the one the rule asked for. A forward added in the panel
/// matches nothing and stays as temporary as it always was.
pub(crate) fn saved_rule_index(
    rules: &[ForwardRule],
    forward: &ManagedForward,
    enabled: bool,
) -> Option<usize> {
    rules.iter().position(|r| {
        let saved = crate::ui::ssh_connect::map_forward(r);
        saved.enabled != enabled
            && saved.kind == forward.kind
            && saved.bind_host == forward.bind_host
            && saved.bind_port == forward.bind_port
            && saved.target_host == forward.target_host
            && saved.target_port == forward.target_port
    })
}

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
        let (danger, foreground) = (theme.danger, theme.foreground);

        let bar = crate::ui::notice::pill(danger, cx)
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(foreground)
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
                    .text_color(danger)
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
        // The bar only. `body_area` anchors it, together with whatever else is
        // floating down there — this used to place itself at `bottom_4` and so
        // did the remote input notice, on the same container.
        Some(bar.into_any_element())
    }

    pub(crate) fn forward_row(
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
        // A switched-off forward has an error status only because it binds
        // nothing; it is drawn faded, not red.
        let off = !forward.enabled;
        let errored = !off && matches!(forward.status, ForwardStatus::Error(_));
        let bind = if matches!(forward.bind_host.as_str(), "127.0.0.1" | "localhost" | "") {
            forward.bind_port.to_string()
        } else {
            format!("{}:{}", forward.bind_host, forward.bind_port)
        };
        let mapping = match forward.kind {
            SshForwardKind::Dynamic => "SOCKS".to_string(),
            _ => format!("→ {}:{}", forward.target_host, forward.target_port),
        };
        let tail = match &forward.status {
            _ if off => mapping,
            ForwardStatus::Error(msg) => msg.clone(),
            ForwardStatus::Listening => mapping,
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
            .py(px(5.))
            .rounded(crate::ui::rounding::ROW_RADIUS)
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
                    .when(off, |col| col.opacity(0.5))
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
            // Always shown, unlike Remove beside it: whether a forward is on is
            // state to read at a glance, not an action to go looking for.
            .child(
                div()
                    .flex_shrink_0()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        crate::ui::theme::switch(
                            ("panel-forward-enabled", forward_id as usize),
                            cx,
                        )
                        .checked(forward.enabled)
                        .xsmall()
                        .tooltip(if off {
                            t(L10nKey::ForwardTooltipTurnOn)
                        } else {
                            t(L10nKey::ForwardTooltipTurnOff)
                        })
                        .on_click(cx.listener(
                            move |this, on: &bool, window, cx| {
                                this.set_managed_forward_enabled(
                                    pane_id, forward_id, *on, window, cx,
                                )
                            },
                        )),
                    ),
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

    /// The form that adds a forward.
    ///
    /// Short by default — one number, because "bring the remote's :3000 over
    /// here" is what nearly every hand-built forward is, and collecting one
    /// number through five fields is what made this read like paperwork. The
    /// full `ssh -L` grammar is one disclosure away for the rest.
    pub(crate) fn forward_form(&self, pane_id: u64, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let danger = theme.danger;
        let sf = cx.global::<crate::ui::presets::Surfaces>().sidebar;
        let kind = self.loopback_panel.mf_kind;
        let editing = self.loopback_panel.mf_editing.is_some();
        let advanced = self.loopback_panel.mf_advanced;
        let fields = self.managed_forward_fields(cx);
        // The form used to accept a click on Add and then do nothing at all
        // when the fields did not make a rule. Now Add is only live when there
        // is something to add, and the line below the form says what is still
        // missing — but not while the form has barely been touched.
        let rule = fields.collect();
        let complete = rule.is_some();
        let incomplete = !complete && !fields.is_blank();
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
            // Escape backs out of the form, the way it backs out of the sftp
            // edit box and every sheet the app puts up. Return is answered by
            // the boxes themselves — see `arm_managed_forward_form` — because
            // an Input takes Return before it can bubble to here.
            .on_key_down(
                cx.listener(move |this, ev: &gpui::KeyDownEvent, window, cx| {
                    if ev.keystroke.key == "escape" {
                        this.close_managed_forward_form(window, cx);
                    }
                }),
            )
            .when(!advanced, |form| {
                form.child(
                    h_flex()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            div()
                                .flex_none()
                                .text_size(rems(META))
                                .text_color(muted)
                                .child(t(L10nKey::ForwardPortLabel)),
                        )
                        .child(
                            div()
                                .w(px(64.))
                                .child(Input::new(&self.loopback_panel.mf_target_port).xsmall()),
                        )
                        // Where the port will come out, said before the click
                        // rather than after it. The number is the same one on
                        // both ends unless it is taken here, which is the case
                        // the row itself reports once the forward exists.
                        .children(rule.as_ref().map(|r| {
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(rems(META))
                                .text_color(muted)
                                .child(t_fmt(
                                    L10nKey::ForwardPortHere,
                                    &[("port", &r.bind_port.to_string())],
                                ))
                        })),
                )
            })
            .when(advanced, |form| {
                form.child(self.segmented_on(
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
            })
            .when(incomplete, |form| {
                form.child(div().text_size(rems(META)).text_color(danger).child(
                    match (advanced, needs_target) {
                        (false, _) => t(L10nKey::ForwardNeedsPort),
                        (true, true) => t(L10nKey::SettingsFwdNeedsBoth),
                        (true, false) => t(L10nKey::SettingsFwdNeedsListen),
                    },
                ))
            })
            .when_some(self.loopback_panel.mf_error.clone(), |form, msg| {
                form.child(div().text_size(rems(META)).text_color(danger).child(msg))
            })
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap(px(4.))
                    .pt(px(1.))
                    // An edit is always shown whole — the short form cannot
                    // spell what an existing rule may contain — so there is
                    // nothing to disclose and no toggle to offer.
                    .child(div().when(!editing, |slot| {
                        slot.child(
                            Button::new(("ssh-managed-forward-advanced", pane_id))
                                .label(if advanced {
                                    t(L10nKey::ForwardSimpleToggle)
                                } else {
                                    t(L10nKey::ForwardAdvancedToggle)
                                })
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.toggle_managed_forward_advanced(cx)
                                })),
                        )
                    }))
                    .child(
                        h_flex()
                            .gap(px(4.))
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
                                    .disabled(!complete)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.add_managed_forward(pane_id, window, cx)
                                    })),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(kind: SshForwardKind, bind_port: &str, host: &str, port: &str) -> ForwardFields {
        ForwardFields {
            advanced: true,
            kind,
            bind_host: "127.0.0.1".to_string(),
            bind_port: bind_port.to_string(),
            target_host: host.to_string(),
            target_port: port.to_string(),
            description: String::new(),
        }
    }

    fn managed(id: u64, bind_port: u16) -> ManagedForward {
        ManagedForward {
            id,
            pane_id: 7,
            kind: SshForwardKind::Local,
            bind_host: "127.0.0.1".to_string(),
            bind_port,
            target_host: "10.0.0.5".to_string(),
            target_port: 80,
            description: Some("the staging box".to_string()),
            status: ForwardStatus::Listening,
            enabled: true,
        }
    }

    #[test]
    fn a_complete_local_rule_is_collected() {
        let rule = fields(SshForwardKind::Local, "8080", "10.0.0.5", "80")
            .collect()
            .expect("a bind port and a target make a rule");
        assert_eq!(rule.bind_port, 8080);
        assert_eq!(rule.target_host, "10.0.0.5");
        assert_eq!(rule.target_port, 80);
        assert_eq!(rule.description, None);
    }

    #[test]
    fn a_half_typed_rule_is_not_a_rule() {
        assert!(
            fields(SshForwardKind::Local, "", "10.0.0.5", "80")
                .collect()
                .is_none()
        );
        assert!(
            fields(SshForwardKind::Local, "8080", "", "80")
                .collect()
                .is_none()
        );
        assert!(
            fields(SshForwardKind::Local, "8080", "10.0.0.5", "")
                .collect()
                .is_none()
        );
        assert!(
            fields(SshForwardKind::Local, "http", "10.0.0.5", "80")
                .collect()
                .is_none(),
            "a service name is not a port"
        );
    }

    #[test]
    fn port_zero_is_refused_rather_than_quietly_ephemeral() {
        assert!(
            fields(SshForwardKind::Local, "0", "10.0.0.5", "80")
                .collect()
                .is_none(),
            "there is nowhere in this form to say which port the OS picked"
        );
        assert!(
            fields(SshForwardKind::Local, "8080", "10.0.0.5", "0")
                .collect()
                .is_none()
        );
    }

    /// The short form: one number, and the rule it makes reaches the far
    /// side's own loopback and comes out here under the same number.
    #[test]
    fn one_number_is_a_whole_rule_in_the_short_form() {
        let mut form = fields(SshForwardKind::Local, "", "", "3000");
        form.advanced = false;
        // Whatever a trip through the advanced form left behind is not part of
        // what the short form was asked for.
        form.bind_host = "0.0.0.0".to_string();
        form.description = "left over".to_string();
        let rule = form.collect().expect("a port is enough");
        assert_eq!(rule.kind, SshForwardKind::Local);
        assert_eq!(rule.bind_host, "127.0.0.1");
        assert_eq!(rule.bind_port, 3000, "the address has to be predictable");
        assert_eq!(rule.target_host, "localhost");
        assert_eq!(rule.target_port, 3000);
        assert_eq!(rule.description, None);
    }

    #[test]
    fn the_short_form_is_blank_until_the_port_is_typed() {
        let mut form = fields(SshForwardKind::Local, "8080", "10.0.0.5", "");
        form.advanced = false;
        assert!(
            form.is_blank(),
            "fields the short form does not show cannot make it dirty"
        );
        assert!(form.collect().is_none());
        form.target_port = "http".to_string();
        assert!(
            !form.is_blank(),
            "a typed port that is not one still counts"
        );
        assert!(form.collect().is_none());
    }

    #[test]
    fn a_socks_proxy_needs_nothing_but_a_port_to_listen_on() {
        let rule = fields(SshForwardKind::Dynamic, "1080", "", "")
            .collect()
            .expect("a dynamic forward has no target");
        assert_eq!(rule.bind_port, 1080);
        assert_eq!(rule.target_host, "");
        assert_eq!(rule.target_port, 0);
    }

    #[test]
    fn an_untouched_form_is_blank_and_a_touched_one_is_not() {
        let mut form = fields(SshForwardKind::Local, "", "", "");
        form.bind_host = String::new();
        assert!(form.is_blank());
        form.description = "  ".to_string();
        assert!(form.is_blank(), "whitespace is not typing");
        form.bind_port = "8".to_string();
        assert!(!form.is_blank());
    }

    #[test]
    fn a_rule_survives_the_round_trip_through_a_live_forward() {
        let rule = rule_of(&managed(3, 8080));
        assert_eq!(rule.kind, SshForwardKind::Local);
        assert_eq!(rule.bind_host, "127.0.0.1");
        assert_eq!(rule.bind_port, 8080);
        assert_eq!(rule.target_host, "10.0.0.5");
        assert_eq!(rule.target_port, 80);
        assert_eq!(rule.description.as_deref(), Some("the staging box"));
    }

    #[test]
    fn the_entry_an_add_appended_is_the_one_that_was_not_there_before() {
        let list = vec![managed(1, 8080), managed(4, 9090)];
        let added = added_forward(&[1], &list).expect("the new entry");
        assert_eq!(added.id, 4);
        assert!(
            added_forward(&[1, 4], &list).is_none(),
            "nothing was added, so there is nothing to point at"
        );
    }

    #[test]
    fn a_switched_off_forward_keeps_its_switch_through_an_edit() {
        let mut off = managed(3, 8080);
        off.enabled = false;
        assert!(!rule_of(&off).enabled);
        assert!(rule_of(&managed(3, 8080)).enabled);
    }

    /// The panel's switch is written back to the saved host only for the rule
    /// the forward was opened from, and a rule added in the panel — which no
    /// saved rule describes — is left as temporary as it always was.
    #[test]
    fn a_switch_is_written_back_to_the_saved_rule_it_came_from() {
        use crate::core::ssh_profile::{ForwardKind, HostPort};
        let saved = |target: &str, enabled: bool| ForwardRule {
            kind: ForwardKind::Local,
            bind: HostPort::new("127.0.0.1", 8080),
            target: HostPort::new(target, 80),
            description: "the staging box".to_string(),
            enabled,
        };
        let rules = vec![saved("10.0.0.9", true), saved("10.0.0.5", true)];
        let live = managed(3, 8080);
        assert_eq!(saved_rule_index(&rules, &live, false), Some(1));
        assert_eq!(
            saved_rule_index(&rules, &live, true),
            None,
            "already on: nothing to write"
        );

        let mut elsewhere = managed(3, 8081);
        elsewhere.description = None;
        assert_eq!(saved_rule_index(&rules, &elsewhere, false), None);

        let mut remote = managed(3, 8080);
        remote.kind = SshForwardKind::Remote;
        assert_eq!(saved_rule_index(&rules, &remote, false), None);
    }
}

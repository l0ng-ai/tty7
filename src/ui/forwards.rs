use gpui::{AnyElement, Context, Div, Entity, FontWeight, Stateful, div, prelude::*, px, rems};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::Input;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};

use crate::daemon::protocol::{ForwardStatus, ManagedForward, SshForwardKind, SshForwardRule};
use crate::terminal::view::TerminalView;
use crate::ui::app::{CONTENT_INSET, TILE_GLYPH_SM, TILE_SIZE_SM, Tty7App};
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::right_panel::{META, TEXT, TEXT_MONO};

/// How far a forward's target endpoint fades when the rule is Dynamic and has
/// no target to name. The rules editor in Settings and the live Forwards panel
/// draw the same row, so they fade it by the same amount.
pub(crate) const NO_TARGET_FADE: f32 = 0.4;

/// What a forward listens on when nobody says otherwise, and what the panel
/// puts in the bind-host field when it opens the form. Loopback, not every
/// interface: a rule typed in a hurry should not expose the far side's service
/// to the network this laptop is on.
pub(crate) const DEFAULT_BIND_HOST: &str = "127.0.0.1";

/// The managed-forward form's five text fields, read out of their inputs.
///
/// Split out so the question "do these make a rule?" can be asked without a
/// `Window` and answered the same way twice: `add_managed_forward` needs the
/// rule, and `forward_form` needs to know whether there is one yet — that is
/// what decides whether Add is live and whether the form says what is missing.
pub(crate) struct ForwardFields {
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
            "" => DEFAULT_BIND_HOST.to_string(),
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
        })
    }

    /// Whether the form is still empty enough that saying what is missing
    /// would be nagging rather than helping — the same restraint the settings
    /// sheet shows through `ForwardRuleForm::is_blank`.
    ///
    /// The bind host is compared against the default rather than against empty:
    /// the panel opens the form with `127.0.0.1` already in that field, so a
    /// field-by-field emptiness check called a form nobody had touched
    /// "started", and every Add opened under a red line telling the user what
    /// was missing from a form they had not begun to fill in.
    pub(crate) fn is_blank(&self) -> bool {
        let untouched_bind_host = matches!(self.bind_host.trim(), "" | DEFAULT_BIND_HOST);
        untouched_bind_host
            && [
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
    }
}

/// The one-letter badge a rule wears, the same letter `ssh -L/-R/-D` uses.
fn kind_letter(kind: SshForwardKind) -> &'static str {
    match kind {
        SshForwardKind::Local => "L",
        SshForwardKind::Remote => "R",
        SshForwardKind::Dynamic => "D",
    }
}

/// What a rule listens on, with the loopback host left off — `8080` rather than
/// `127.0.0.1:8080`, which is the same address spelled three times a row.
fn bind_label(host: &str, port: u16) -> String {
    match host {
        DEFAULT_BIND_HOST | "localhost" | "" => port.to_string(),
        host => format!("{host}:{port}"),
    }
}

/// The square action tile a forward row's buttons are cut from — the panel's
/// existing hover-strip size, in one place so the switch and the delete are the
/// same target.
fn row_tile(id: impl Into<gpui::ElementId>, icon: IconName, cx: &mut Context<Tty7App>) -> Button {
    crate::ui::tab_strip::chrome_tile(Button::new(id).icon(icon).xsmall(), false, cx)
        .w(px(crate::ui::tab_strip::MIN_TARGET))
        .h(px(crate::ui::tab_strip::MIN_TARGET))
        .rounded(px(4.))
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
        // offering to edit one opened Settings on "Nothing selected" — this
        // comment already said that was the thing to avoid, and the code under
        // it only checked that the spec's uuid *parsed*. Every connection has
        // one. Ask the same question `unsaved_ssh_session` asks.
        let profiles = &cx.global::<crate::core::config::Config>().ssh_profiles;
        let profile = view
            .ssh_spec()
            .and_then(|spec| crate::ui::ssh_connect::saved_profile_of(&spec, profiles));

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
            .child(match profile {
                // A saved host: the form that opens is the one this connection
                // was dialled from, so a wrong hostname or password is fixed
                // where it lives.
                Some(id) => Button::new("ssh-edit-profile")
                    .label(t(L10nKey::SshEditProfile))
                    .ghost()
                    .small()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_ssh_profile_in_settings(id, window, cx)
                    })),
                // A one-off: there is no host to open, but the address that
                // failed is worth keeping — the form opens prefilled from the
                // live spec, which is where the typo gets corrected (#438).
                None => Button::new("ssh-save-as-host")
                    .label(t(L10nKey::SshSaveAsHost))
                    .ghost()
                    .small()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.save_ssh_session_as_host(window, cx)
                    })),
            });
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
        // Switched-off rules sit under the running ones, in the order they
        // were switched off. Their index into the panel's own list is what the
        // buttons act on, so it is carried alongside rather than derived twice.
        let paused: Vec<(usize, SshForwardRule)> = self
            .loopback_panel
            .paused
            .iter()
            .enumerate()
            .filter(|(_, p)| p.pane_id == pane_id)
            .map(|(ix, p)| (ix, p.rule.clone()))
            .collect();
        let empty = managed.is_empty() && paused.is_empty();

        let mono = cx.theme().mono_font_family.clone();
        let mut list = v_flex().px(px(CONTENT_INSET - 4.)).py(px(2.)).gap(px(1.));
        for forward in &managed {
            list = list.child(self.forward_row(forward, &mono, cx));
        }
        for (index, rule) in &paused {
            list = list.child(self.paused_forward_row(*index, rule, &mono, cx));
        }

        Some(
            v_flex()
                .child(self.panel_subtitle(t(L10nKey::ForwardPanelTitle), true, Some(add), cx))
                .when(empty && !open, |this| {
                    this.child(
                        div()
                            .px(px(CONTENT_INSET))
                            .py(px(2.))
                            .text_size(rems(TEXT))
                            .text_color(cx.theme().muted_foreground)
                            .child(crate::ui::i18n::t(crate::ui::i18n::L10nKey::None)),
                    )
                })
                .when(!empty, |this| this.child(list))
                .when(open, |this| this.child(self.forward_form(pane_id, cx)))
                .into_any_element(),
        )
    }

    /// A rule that is switched off: the same row, faded, with the switch that
    /// puts it back always visible — a row nobody can see a way out of reads as
    /// broken rather than as off.
    fn paused_forward_row(
        &self,
        index: usize,
        rule: &SshForwardRule,
        mono: &gpui::SharedString,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let letter = kind_letter(rule.kind);
        let bind = bind_label(&rule.bind_host, rule.bind_port);
        let tail = match rule.kind {
            SshForwardKind::Dynamic => "SOCKS".to_string(),
            _ => format!("→ {}:{}", rule.target_host, rule.target_port),
        };
        let group = gpui::SharedString::from(format!("panel-forward-off-{index}"));

        h_flex()
            .id(("panel-forward-off", index))
            .group(group.clone())
            .items_center()
            .gap(px(8.))
            .px(px(4.))
            .py(px(3.))
            .rounded(px(5.))
            .opacity(NO_TARGET_FADE)
            .child(crate::ui::right_panel::git_badge(letter, muted, mono))
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
                                    .text_color(muted)
                                    .child(tail),
                            ),
                    )
                    .when_some(rule.description.clone(), |this, desc| {
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
                h_flex()
                    .flex_shrink_0()
                    .gap(px(1.))
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        row_tile(("panel-forward-on", index), IconName::Play, cx)
                            .tooltip(t(L10nKey::ForwardTooltipTurnOn))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.resume_paused_forward(index, window, cx)
                            })),
                    )
                    .child(
                        div()
                            .opacity(0.)
                            .group_hover(group, |s| s.opacity(1.))
                            .child(
                                row_tile(("panel-forward-off-del", index), IconName::Close, cx)
                                    .tooltip(t(L10nKey::ForwardTooltipForget))
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        this.remove_paused_forward(index, cx)
                                    })),
                            ),
                    ),
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
        let letter = kind_letter(forward.kind);
        let errored = matches!(forward.status, ForwardStatus::Error(_));
        let bind = bind_label(&forward.bind_host, forward.bind_port);
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
                h_flex()
                    .flex_shrink_0()
                    .gap(px(1.))
                    .opacity(0.)
                    .group_hover(group, |s| s.opacity(1.))
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    // Off, not gone: the rule is kept and can be switched back
                    // on without being typed again (#437).
                    .child(
                        row_tile(
                            ("panel-forward-pause", forward_id as usize),
                            IconName::Pause,
                            cx,
                        )
                        .tooltip(t(L10nKey::ForwardTooltipTurnOff))
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                this.pause_managed_forward(pane_id, forward_id, cx)
                            },
                        )),
                    )
                    .child(
                        row_tile(
                            ("panel-forward-del", forward_id as usize),
                            IconName::Close,
                            cx,
                        )
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
        let fields = self.managed_forward_fields(cx);
        // The form used to accept a click on Add and then do nothing at all
        // when the fields did not make a rule. Now Add is only live when there
        // is something to add, and the line below the form says what is still
        // missing — but not while the form has barely been touched.
        let complete = fields.collect().is_some();
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
            .when(incomplete, |form| {
                form.child(div().text_size(rems(META)).text_color(danger).child(
                    match needs_target {
                        true => t(L10nKey::SettingsFwdNeedsBoth),
                        false => t(L10nKey::SettingsFwdNeedsListen),
                    },
                ))
            })
            .when_some(self.loopback_panel.mf_error.clone(), |form, msg| {
                form.child(div().text_size(rems(META)).text_color(danger).child(msg))
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
                            .disabled(!complete)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.add_managed_forward(pane_id, window, cx)
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(kind: SshForwardKind, bind_port: &str, host: &str, port: &str) -> ForwardFields {
        ForwardFields {
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

    /// The panel opens the form with the default bind host already in the
    /// field. Reading that as "the user has started filling this in" is what
    /// put a red line under every freshly opened Add, telling the user what was
    /// missing from a form they had not touched.
    #[test]
    fn the_bind_host_the_form_opens_with_is_not_typing() {
        let mut form = fields(SshForwardKind::Local, "", "", "");
        form.bind_host = DEFAULT_BIND_HOST.to_string();
        assert!(form.is_blank(), "the form opens on this; nobody typed it");

        form.bind_host = "0.0.0.0".to_string();
        assert!(
            !form.is_blank(),
            "a bind host that is not the default was typed, and the form may say what is missing"
        );
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
}

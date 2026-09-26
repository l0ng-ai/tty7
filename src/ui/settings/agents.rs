//! Integrations: the agent hooks that put each agent's live status in the
//! sidebar, per machine, and the `tty7` command on PATH.

use crate::core::agent_hooks::{HookAgent, HooksState};

use super::kit::{self, BtnKind, Tk, fs};
use super::shell::MenuEntry;
use super::*;

/// The agent the tab bar draws for this hook, so the two show the same mark.
fn cli_agent(agent: HookAgent) -> crate::core::cli_agent::CLIAgent {
    use crate::core::cli_agent::CLIAgent as C;
    match agent {
        HookAgent::Claude => C::Claude,
        HookAgent::Codex => C::Codex,
        HookAgent::TraeCode => C::TraeCode,
        HookAgent::Copilot => C::Copilot,
        HookAgent::OpenCode => C::OpenCode,
        HookAgent::Pi => C::Pi,
        HookAgent::Grok => C::Grok,
        HookAgent::OhMyPi => C::OhMyPi,
        HookAgent::Gemini => C::Gemini,
        HookAgent::Droid => C::Droid,
        HookAgent::Qwen => C::Qwen,
        HookAgent::Goose => C::Goose,
        HookAgent::Kimi => C::Kimi,
        HookAgent::QoderCLI => C::QoderCLI,
        HookAgent::Crush => C::Crush,
        HookAgent::CodeBuddy => C::CodeBuddy,
        HookAgent::Cursor => C::Cursor,
    }
}

impl Tty7App {
    pub(crate) fn render_settings_agents(&self, cx: &mut Context<Self>) -> AnyElement {
        Self::settings_page([
            self.render_agent_hooks_group(cx),
            self.render_command_line_rows(cx),
        ])
    }

    pub(crate) fn render_command_line_rows(&self, cx: &mut Context<Self>) -> AnyElement {
        let on = cx.global::<Config>().install_cli_on_path;
        let control = self.settings_switch("install-cli-on-path", on, cx, |this, on, _, cx| {
            this.set_install_cli_on_path(on, cx)
        });
        self.settings_group(
            Some(t(L10nKey::SettingsCommandLine)),
            None,
            [self
                .settings_row(
                    t(L10nKey::SettingsInstallCliOnPath),
                    t(L10nKey::SettingsCommandLineDesc),
                    control,
                    cx,
                )
                .into_any_element()],
            cx,
        )
    }

    fn render_agent_hooks_group(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = Tk::of(cx);
        let Some(s) = self.active_settings() else {
            return div().into_any_element();
        };
        let view = s.agent_hooks_states.clone();
        let note = s.agent_hooks_note.clone();
        let selected = s.agent_hooks_host;
        let query_input = s.agent_query.clone();
        let query = query_input.read(cx).value().trim().to_lowercase();
        let show_all = s.agent_show_all;
        let touched = s.agent_touched.clone();
        let busy = s.agent_busy.clone();
        let focused = s.focused_setting;
        let menu_open = s.menu.clone();

        // The machine the hooks below belong to.
        let machines = self.agent_hooks_machines(cx);
        let offline = self.agent_hooks_offline_count(cx);
        let current = machines
            .iter()
            .find(|m| m.host == selected)
            .map(|m| m.label.clone())
            .unwrap_or_default();
        let rows: Vec<AgentHookRow> = match &view {
            AgentHooksView::Ready(rows) => rows.clone(),
            _ => Vec::new(),
        };
        let installed = rows
            .iter()
            .filter(|r| r.state != HooksState::NotInstalled)
            .count();
        let mut entries = vec![MenuEntry::Heading(
            t(L10nKey::SettingsConnectedMachines).into(),
        )];
        for m in &machines {
            let host = m.host;
            entries.push(MenuEntry::Item {
                label: m.label.clone().into(),
                checked: host == selected,
                font: None,
                danger: false,
                trailing: (host == selected && matches!(view, AgentHooksView::Ready(_))).then(
                    || {
                        t_fmt(
                            L10nKey::SettingsInstalledCount,
                            &[("count", &installed.to_string())],
                        )
                        .into()
                    },
                ),
                on_pick: std::rc::Rc::new(move |this, _w, cx| {
                    if let Some(s) = this.active_settings_mut() {
                        s.agent_touched.clear();
                    }
                    this.select_agent_hooks_host(host, cx)
                }),
            });
        }
        if offline > 0 {
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Note(
                t_plural(L10nKey::SettingsOfflineMachines, offline, &[]).into(),
            ));
        }
        let machine_icon = Icon::empty()
            .path(if selected.is_local() {
                "icons/settings/machine-local.svg"
            } else {
                "icons/settings/machine-remote.svg"
            })
            .size(px(12.))
            .text_color(tk.k5)
            .into_any_element();
        let machine = self.settings_dropdown(
            "agent-hooks-machine",
            current.clone(),
            Some(machine_icon),
            entries,
            cx,
        );
        let machine_desc = if selected.is_local() {
            t(L10nKey::SettingsMachineLocalDesc).to_string()
        } else {
            t_fmt(L10nKey::SettingsMachineRemoteDesc, &[("name", &current)])
        };
        let machine_row = h_flex()
            .min_h(px(52.))
            .py(px(12.))
            .gap(px(32.))
            .items_center()
            .justify_between()
            .child(
                v_flex()
                    .min_w_0()
                    .gap(px(2.))
                    .child(div().text_size(fs(13.)).child(t(L10nKey::SettingsMachine)))
                    .child(
                        div()
                            .text_size(fs(12.))
                            .line_height(px(17.))
                            .text_color(tk.k5)
                            .child(machine_desc),
                    ),
            )
            .child(machine)
            .into_any_element();

        let total = HookAgent::ALL.len();
        let toolbar = h_flex()
            .gap(px(12.))
            .items_center()
            .pt(px(10.))
            .pb(px(8.))
            .child(
                kit::search_field(&query_input, &tk)
                    .w(px(240.))
                    .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                        if ev.keystroke.key.as_str() == "escape" {
                            let input = this.active_settings().map(|s| s.agent_query.clone());
                            if let Some(input) = input
                                && !input.read(cx).value().is_empty()
                            {
                                input.update(cx, |i, cx| i.set_value("", window, cx));
                                cx.stop_propagation();
                            }
                        }
                    })),
            )
            .child(div().flex_1())
            .when(matches!(view, AgentHooksView::Ready(_)), |r| {
                r.child(div().text_size(fs(12.)).text_color(tk.k45).child(t_fmt(
                    L10nKey::SettingsAgentsInstalledSummary,
                    &[
                        ("count", &installed.to_string()),
                        ("total", &total.to_string()),
                    ],
                )))
            })
            .into_any_element();

        let mut body: Vec<AnyElement> = vec![machine_row, toolbar];
        match &view {
            AgentHooksView::Loading => body.push(
                div()
                    .h(px(44.))
                    .flex()
                    .items_center()
                    .text_color(tk.k35)
                    .child(t(L10nKey::SettingsReadingAgentConfig))
                    .into_any_element(),
            ),
            AgentHooksView::Unavailable(reason) => body.push(
                div()
                    .py(px(12.))
                    .text_size(fs(12.5))
                    .text_color(tk.warn_text)
                    .child(reason.clone())
                    .into_any_element(),
            ),
            AgentHooksView::Ready(rows) => {
                let has = |r: &AgentHookRow| r.state != HooksState::NotInstalled;
                let is_focused =
                    |r: &AgentHookRow| focused.is_some_and(|k| t(k) == r.agent.display_name());
                let mut list: Vec<&AgentHookRow> = rows
                    .iter()
                    .filter(|r| {
                        if !query.is_empty() {
                            return format!("{} {}", r.agent.display_name(), r.target)
                                .to_lowercase()
                                .contains(&query);
                        }
                        show_all || has(r) || touched.contains(&r.agent) || is_focused(r)
                    })
                    .collect();
                let rank = |r: &AgentHookRow| match r.state {
                    HooksState::Outdated => 0,
                    HooksState::Installed => 1,
                    HooksState::NotInstalled => 2,
                };
                list.sort_by_key(|r| {
                    (
                        rank(r),
                        HookAgent::ALL
                            .iter()
                            .position(|a| *a == r.agent)
                            .unwrap_or(0),
                    )
                });
                let none = list.is_empty();
                let mut col = v_flex()
                    .id("agent-hook-list")
                    .when(!query.is_empty() || show_all, |c| {
                        c.max_h(px(416.)).overflow_y_scroll()
                    });
                for (i, row) in list.into_iter().enumerate() {
                    let row_note = note
                        .as_ref()
                        .filter(|(a, _)| *a == row.agent)
                        .map(|(_, text)| text.clone());
                    col = col.child(self.render_agent_row(
                        i,
                        row,
                        busy.contains(&row.agent),
                        row_note,
                        menu_open.as_deref(),
                        cx,
                    ));
                }
                body.push(col.into_any_element());
                if none {
                    body.push(
                        div()
                            .h(px(44.))
                            .flex()
                            .items_center()
                            .text_color(tk.k35)
                            .child(if query.is_empty() {
                                t(L10nKey::SettingsNoAgentsInstalled).to_string()
                            } else {
                                t_fmt(L10nKey::SettingsNoAgentsMatch, &[("query", &query)])
                            })
                            .into_any_element(),
                    );
                }
                let rest = total - installed;
                if query.is_empty() && rest > 0 {
                    body.push(
                        h_flex()
                            .h(px(40.))
                            .gap(px(8.))
                            .items_center()
                            .text_size(fs(12.))
                            .text_color(tk.k45)
                            .when(!show_all, |r| {
                                r.child(t_plural(L10nKey::SettingsMoreAgents, rest, &[]))
                            })
                            .child(
                                kit::button(
                                    "agent-show-all",
                                    if show_all {
                                        t(L10nKey::SettingsShowInstalledOnly)
                                    } else {
                                        t(L10nKey::SettingsShowAll)
                                    },
                                    BtnKind::Link,
                                )
                                .on_click(cx.listener(
                                    |this, _, _w, cx| {
                                        if let Some(s) = this.active_settings_mut() {
                                            s.agent_show_all = !s.agent_show_all;
                                            s.agent_touched.clear();
                                        }
                                        cx.notify();
                                    },
                                )),
                            )
                            .into_any_element(),
                    );
                }
            }
        }

        self.settings_group(
            Some(t(L10nKey::SettingsAgentsIntro)),
            Some(t(L10nKey::SettingsAgentsIntroDesc).to_string()),
            body,
            cx,
        )
    }

    fn render_agent_row(
        &self,
        i: usize,
        row: &AgentHookRow,
        busy: bool,
        note: Option<String>,
        menu_open: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tk = Tk::of(cx);
        let agent = row.agent;
        let mark = cli_agent(agent);
        let (status, dot) = if busy {
            (Some(t(L10nKey::SettingsWorking)), tk.k3)
        } else {
            match row.state {
                HooksState::Installed => (Some(t(L10nKey::SettingsStatusInstalled)), tk.ok),
                HooksState::Outdated => (Some(t(L10nKey::SettingsUpdateAvailable)), tk.warn),
                HooksState::NotInstalled => (None, tk.k3),
            }
        };
        let action = (!busy)
            .then(|| match row.state {
                HooksState::NotInstalled => Some(t(L10nKey::SettingsInstall)),
                HooksState::Outdated => Some(t(L10nKey::SettingsUpdate)),
                HooksState::Installed => None,
            })
            .flatten();
        let has_menu = row.state != HooksState::NotInstalled && !busy;
        let menu_id: SharedString = format!("agent-menu-{i}").into();
        let open = menu_open == Some(menu_id.as_ref());
        let local = self
            .active_settings()
            .is_some_and(|s| s.agent_hooks_host.is_local());
        let target = row.target.clone();
        let menu = has_menu.then(|| {
            let mut entries = vec![MenuEntry::item(
                t(L10nKey::SettingsReinstall),
                false,
                move |this, _w, cx| this.settings_install_agent_hooks(agent, cx),
            )];
            if local {
                entries.push(MenuEntry::item(
                    t(L10nKey::SettingsRevealHookFile),
                    false,
                    move |_this, _w, cx| {
                        let path = crate::core::ssh_profile::expand_tilde(&target);
                        cx.reveal_path(std::path::Path::new(&path));
                    },
                ));
            }
            entries.push(MenuEntry::Item {
                label: t(L10nKey::SettingsUninstall).into(),
                checked: false,
                font: None,
                danger: true,
                trailing: None,
                on_pick: std::rc::Rc::new(move |this, _w, cx| {
                    this.settings_uninstall_agent_hooks(agent, cx)
                }),
            });
            let toggle = menu_id.clone();
            div()
                .relative()
                .child(
                    div()
                        .id(ElementId::Name(SharedString::from(format!(
                            "{menu_id}-trigger"
                        ))))
                        .size(px(26.))
                        .rounded(px(6.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .when(open, |d| d.bg(tk.k05))
                        .hover(move |s| s.bg(tk.k05))
                        .child(
                            Icon::empty()
                                .path("icons/settings/more.svg")
                                .size(px(12.))
                                .text_color(tk.k5),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.toggle_settings_menu(toggle.clone(), 0, false, window, cx)
                        })),
                )
                .when(open, |d| {
                    // Built only while open: the entries hold callbacks, and
                    // the panel is where they are wired to the rows.
                    d.child(kit::popover_below(
                        true,
                        4.,
                        self.settings_menu_panel(&menu_id, entries, cx)
                            .min_w(px(170.)),
                    ))
                })
        });

        v_flex()
            .id(SharedString::from(format!("agent-row-{i}")))
            .anchor_scroll(self.first_hit_anchor(agent.display_name(), cx))
            .child(
                h_flex()
                    .min_h(px(52.))
                    .py(px(8.))
                    .gap(px(12.))
                    .items_center()
                    .child(
                        // The tab bar's agent disc: the brand field, the mark
                        // on it, and a hairline where the field is the page.
                        div()
                            .size(px(20.))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(rgb(mark.accent_rgb()))
                            .when(presets::needs_edge(mark.accent_rgb(), tk.page), |d| {
                                d.shadow(vec![kit::ring(tk.k15, 0.5, false)])
                            })
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                gpui::svg()
                                    .path(mark.icon_path())
                                    .size(px(11.))
                                    .text_color(rgb(mark.icon_rgb())),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(agent.display_name())
                            .child(
                                div()
                                    .font_family(Tk::mono(cx))
                                    .text_size(fs(11.5))
                                    .text_color(tk.k45)
                                    .truncate()
                                    .child(row.target.clone()),
                            ),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .gap(px(10.))
                            .items_center()
                            .when_some(status, |r, text| {
                                r.child(
                                    h_flex()
                                        .gap(px(6.))
                                        .items_center()
                                        .text_size(fs(12.))
                                        .text_color(tk.k5)
                                        .whitespace_nowrap()
                                        .child(div().size(px(6.)).rounded_full().bg(dot))
                                        .child(text),
                                )
                            })
                            .when_some(action, |r, label| {
                                r.child(
                                    kit::button(
                                        ("agent-hooks-install", i),
                                        label,
                                        BtnKind::Secondary,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _w, cx| {
                                            this.settings_install_agent_hooks(agent, cx)
                                        },
                                    )),
                                )
                            })
                            .children(menu),
                    ),
            )
            .when_some(note, |col, text| {
                col.child(
                    div()
                        .pl(px(32.))
                        .pb(px(8.))
                        .mt(px(-4.))
                        .text_size(fs(12.))
                        .line_height(px(17.))
                        .text_color(tk.k45)
                        .child(text),
                )
            })
            .into_any_element()
    }
}

//! Integrations: the agent hooks that put each agent's live status in the
//! sidebar, per machine, and the `tty7` command on PATH.

use crate::core::agent_hooks::{HookAgent, HooksState};
use crate::core::cli_agent::{launch_program, program_on_path};
use crate::ui::host_ops::HostId;

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
        HookAgent::QoderCLICn => C::QoderCLICn,
        HookAgent::Crush => C::Crush,
        HookAgent::CodeBuddy => C::CodeBuddy,
        HookAgent::Cursor => C::Cursor,
        HookAgent::PrimeAgent => C::PrimeAgent,
        HookAgent::Antigravity => C::Antigravity,
        HookAgent::Empryo => C::Empryo,
        HookAgent::Jcode => C::Jcode,
        HookAgent::Muse => C::Muse,
    }
}

/// What a `worktree_setup_trust` key is shown as: the repository's path, with
/// the machine left off when it is this one. Another machine's key leads with
/// that connection's name from `hosts`, or with "Remote" once the connection
/// is gone, since its id is a hash nobody would recognise.
fn trust_key_label(key: &str, hosts: &[(HostId, String)]) -> String {
    let Some((id, path)) = key
        .split_once(':')
        .and_then(|(id, path)| Some((HostId(id.parse().ok()?), path)))
    else {
        return key.to_string();
    };
    if id.is_local() {
        return path.to_string();
    }
    let host = hosts
        .iter()
        .find(|(known, _)| *known == id)
        .map_or(t(L10nKey::SettingsWorktreeSetupRemote), |(_, name)| {
            name.as_str()
        });
    format!("{host} · {path}")
}

impl Tty7App {
    pub(crate) fn render_settings_agents(&self, cx: &mut Context<Self>) -> AnyElement {
        Self::settings_page([
            self.render_agent_hooks_group(cx),
            self.render_agent_sessions_rows(cx),
            self.render_launch_commands_group(cx),
            self.render_worktree_setup_group(cx),
            self.render_command_line_rows(cx),
        ])
    }

    /// Restoring agent conversations, and the sessions hidden from search.
    /// Plain rows, so search builds this group on its own as well.
    pub(crate) fn render_agent_sessions_rows(&self, cx: &mut Context<Self>) -> AnyElement {
        let cfg = cx.global::<Config>();
        let restore_on = cfg.restore_agent_sessions;
        let hidden = cfg.hidden_agent_sessions.len();
        let restore = self.settings_switch(
            "restore-agent-sessions",
            restore_on,
            cx,
            |this, on, _, cx| this.update_config(cx, |cfg| cfg.restore_agent_sessions = on),
        );
        let unhide = kit::button(
            "unhide-agent-sessions",
            t(L10nKey::SettingsUnhideAll),
            BtnKind::Secondary,
        )
        .disabled(hidden == 0)
        .on_click(cx.listener(|this, _, _w, cx| {
            this.update_config(cx, |cfg| cfg.hidden_agent_sessions.clear());
            cx.notify();
        }))
        .into_any_element();
        let hidden_desc = format!(
            "{} · {}",
            t(L10nKey::SettingsHiddenSessionsDesc),
            t_fmt(
                L10nKey::SettingsHiddenSessionsCount,
                &[("count", &hidden.to_string())]
            ),
        );
        self.settings_group(
            Some(t(L10nKey::SettingsAgentSessions)),
            None,
            [
                self.settings_row(
                    t(L10nKey::SettingsRestoreAgentSessions),
                    t(L10nKey::SettingsRestoreAgentSessionsDesc),
                    restore,
                    cx,
                ),
                self.settings_row(t(L10nKey::SettingsHiddenSessions), hidden_desc, unhide, cx),
            ]
            .map(IntoElement::into_any_element),
            cx,
        )
    }

    /// The command line quick launch types for each agent on this computer,
    /// one field per agent, each with its own Reset.
    fn render_launch_commands_group(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = Tk::of(cx);
        let inputs = self
            .active_settings()
            .map(|s| s.launch_inputs.clone())
            .unwrap_or_default();
        let overrides = &cx.global::<Config>().agent_launch;
        // Only a line of the user's own can name a program that is not there:
        // a row without one is listed because its binary was on `PATH`.
        let path = std::env::var_os("PATH").unwrap_or_default();
        let checked: Vec<(bool, Option<String>)> = inputs
            .iter()
            .map(|(agent, _)| {
                let line = agent.launch_command(overrides);
                let modified = line != agent.binary();
                let missing = modified
                    .then(|| launch_program(&line).unwrap_or(line))
                    .filter(|program| !program_on_path(program, &path))
                    .map(|program| {
                        t_fmt(
                            L10nKey::SettingsLaunchProgramMissing,
                            &[("program", &program)],
                        )
                    });
                (modified, missing)
            })
            .collect();
        let mut rows: Vec<AnyElement> = Vec::with_capacity(inputs.len().max(1));
        for ((agent, input), (modified, missing)) in inputs.iter().zip(checked) {
            let agent = *agent;
            let field = self.settings_checked_input(input, PATH_FIELD_W, missing, cx);
            rows.push(
                self.settings_row_resettable(
                    agent.display_name(),
                    t_fmt(
                        L10nKey::SettingsLaunchCommandDefault,
                        &[("binary", agent.binary())],
                    ),
                    field,
                    modified,
                    move |this, window, cx| this.reset_launch_command(agent, window, cx),
                    cx,
                )
                .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .h(px(44.))
                    .flex()
                    .items_center()
                    .text_color(tk.k35)
                    .child(t(L10nKey::SettingsNoLaunchableAgents))
                    .into_any_element(),
            );
        }
        self.settings_group(
            Some(t(L10nKey::SettingsLaunchCommands)),
            Some(t(L10nKey::SettingsLaunchCommandsDesc).to_string()),
            rows,
            cx,
        )
    }

    /// The `.tty7/setup` scripts approved to run in a new worktree, each with
    /// a Revoke that makes the next worktree of that repository ask again.
    fn render_worktree_setup_group(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = Tk::of(cx);
        let mut approved: Vec<(String, String)> = cx
            .global::<Config>()
            .worktree_setup_trust
            .iter()
            .map(|(key, digest)| (key.clone(), digest.clone()))
            .collect();
        approved.sort();
        // Listing the connections reads `~/.ssh/config`, so only when a
        // remote key is there to name.
        let hosts: Vec<(HostId, String)> = if approved
            .iter()
            .any(|(key, _)| !key.starts_with(&format!("{}:", HostId::LOCAL.0)))
        {
            crate::ui::remote_connect::available_hosts(cx)
                .into_iter()
                .map(|h| (h.target.host_id(), h.label))
                .collect()
        } else {
            Vec::new()
        };
        let mut rows: Vec<AnyElement> = Vec::with_capacity(approved.len().max(1));
        for (i, (key, digest)) in approved.into_iter().enumerate() {
            let short: String = digest.chars().take(12).collect();
            let revoke = kit::button(
                SharedString::from(format!("revoke-setup-{i}")),
                t(L10nKey::SettingsRevoke),
                BtnKind::Secondary,
            )
            .on_click(cx.listener({
                let key = key.clone();
                move |this, _, _w, cx| {
                    this.update_config(cx, |cfg| {
                        cfg.worktree_setup_trust.remove(&key);
                    });
                    cx.notify();
                }
            }))
            .into_any_element();
            rows.push(
                self.settings_row(
                    trust_key_label(&key, &hosts),
                    t_fmt(L10nKey::SettingsWorktreeSetupHash, &[("hash", &short)]),
                    revoke,
                    cx,
                )
                .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .h(px(44.))
                    .flex()
                    .items_center()
                    .text_color(tk.k35)
                    .child(t(L10nKey::SettingsNoWorktreeSetupScripts))
                    .into_any_element(),
            );
        }
        self.settings_group(
            Some(t(L10nKey::SettingsWorktreeSetupScripts)),
            Some(t(L10nKey::SettingsWorktreeSetupScriptsDesc).to_string()),
            rows,
            cx,
        )
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
                let bounded = !query.is_empty() || show_all;
                let scroll = s.agent_list_scroll.clone();
                let mut col = v_flex().id("agent-hook-list-rows").when(bounded, |c| {
                    // Match the viewport's negative margin so controls keep
                    // their alignment, with room for the raised button shadow.
                    c.px(px(4.))
                        .on_scroll_wheel(nested_list_wheel(scroll.clone()))
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
                body.push(
                    div()
                        .id("agent-hook-list")
                        .when(bounded, |c| {
                            c.mx(px(-4.))
                                .max_h(px(416.))
                                .overflow_y_scroll()
                                .track_scroll(&scroll)
                        })
                        .child(col)
                        .into_any_element(),
                );
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
        let manual_muse = agent == HookAgent::Muse
            && self
                .active_settings()
                .is_some_and(|s| !s.agent_hooks_host.is_local());
        let (status, dot) = if busy {
            (Some(t(L10nKey::SettingsWorking)), tk.k3)
        } else if manual_muse {
            (Some(t(L10nKey::SettingsMuseManualInstall)), tk.warn)
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

#[cfg(test)]
mod tests {
    use super::trust_key_label;
    use crate::ui::host_ops::HostId;

    #[test]
    fn a_trust_key_shows_as_its_path_and_the_machine_by_name() {
        let remote = HostId::from_connection_key("ssh-direct:me@box:22");
        let hosts = [(remote, "build-box".to_string())];
        assert_eq!(
            trust_key_label("0:/Users/me/src/app", &hosts),
            "/Users/me/src/app"
        );
        assert_eq!(trust_key_label("0:C:\\src\\app", &hosts), "C:\\src\\app");
        assert_eq!(
            trust_key_label(&format!("{}:/srv/app", remote.0), &hosts),
            "build-box · /srv/app"
        );
        // A connection since removed: the hash would mean nothing to anyone.
        assert_eq!(
            trust_key_label("1234567890123456789:/srv/app", &hosts),
            "Remote · /srv/app"
        );
        assert_eq!(trust_key_label("not-a-key", &hosts), "not-a-key");
    }
}

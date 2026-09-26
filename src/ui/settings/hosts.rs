//! The SSH page: one column of hosts — the few used most recently, or every
//! one grouped by where it came from, or whatever a search finds — each of
//! which opens in place to show its details or to be edited.

use super::kit::{self, BtnKind, Tk, fs};
use super::*;

/// How many hosts the page shows before "Show all".
const RECENT_HOSTS: usize = 5;

/// A list line: a group's heading, or a host.
enum HostLine {
    Head {
        key: String,
        count: usize,
        collapsed: bool,
    },
    Host(SshProfile),
}

impl Tty7App {
    pub(crate) fn render_settings_ssh(&self, cx: &mut Context<Self>) -> AnyElement {
        Self::settings_page([
            self.render_hosts_group(cx),
            self.render_ssh_connection_rows(cx),
        ])
    }

    /// The settings that are rows: they are what search finds on this page.
    pub(crate) fn render_ssh_connection_rows(&self, cx: &mut Context<Self>) -> AnyElement {
        let cfg = cx.global::<Config>();
        let (verify, warn) = (cfg.verify_host_keys, cfg.ssh_warn_on_close);
        let verify = self.settings_switch("ssh-verify-host-keys", verify, cx, |this, on, _, cx| {
            this.set_verify_host_keys(on, cx)
        });
        let warn = self.settings_switch("ssh-warn-on-close", warn, cx, |this, on, _, cx| {
            this.set_ssh_warn_on_close(on, cx)
        });
        self.settings_group(
            Some(t(L10nKey::SettingsSecurity)),
            Some(t(L10nKey::SettingsSecurityIntro).to_string()),
            [
                self.settings_row(
                    t(L10nKey::SettingsVerifyHostKeys),
                    t(L10nKey::SettingsVerifyHostKeysDesc),
                    verify,
                    cx,
                ),
                self.settings_row(
                    t(L10nKey::WarnBeforeClosing),
                    t(L10nKey::SettingsWarnBeforeClosingDesc),
                    warn,
                    cx,
                ),
            ]
            .map(IntoElement::into_any_element),
            cx,
        )
    }

    fn render_hosts_group(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = Tk::of(cx);
        let Some(s) = self.active_settings() else {
            return div().into_any_element();
        };
        let filter = s.ssh_filter.clone();
        let query = filter.read(cx).value().trim().to_lowercase();
        let show_all = s.ssh_show_all;
        let collapsed = s.ssh_collapsed_groups.clone();
        let editing = s.ssh_form.as_ref().map(|f| f.editing);
        let cfg = cx.global::<Config>();
        let profiles = cfg.ssh_profiles.clone();
        let adding = editing.filter(|id| !profiles.iter().any(|p| p.id == *id));
        let by_name = |a: &SshProfile, b: &SshProfile| {
            host_title(a)
                .to_lowercase()
                .cmp(&host_title(b).to_lowercase())
        };

        // The recent few: the ones actually connected to, most-used first,
        // topped up alphabetically on a fresh install.
        let recent: Vec<SshProfile> = crate::ui::ssh_connect::ssh_profiles_by_frecency(cx)
            .into_iter()
            .take(RECENT_HOSTS)
            .collect();
        let mut lines: Vec<HostLine> = Vec::new();
        if !query.is_empty() {
            let mut hits: Vec<SshProfile> = profiles
                .iter()
                .filter(|p| ssh_row_matches(p, &query))
                .cloned()
                .collect();
            hits.sort_by(by_name);
            lines.extend(hits.into_iter().map(HostLine::Host));
        } else if show_all {
            let mut groups: Vec<(String, Vec<SshProfile>)> = Vec::new();
            for p in &profiles {
                let key = ssh_group_key(p).to_string();
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, bucket)) => bucket.push(p.clone()),
                    None => groups.push((key, vec![p.clone()])),
                }
            }
            groups.sort_by(|a, b| {
                ssh_group_rank(&a.0)
                    .cmp(&ssh_group_rank(&b.0))
                    .then_with(|| a.0.cmp(&b.0))
            });
            for (key, mut bucket) in groups {
                bucket.sort_by(by_name);
                let is_collapsed = collapsed.contains(&key);
                lines.push(HostLine::Head {
                    key,
                    count: bucket.len(),
                    collapsed: is_collapsed,
                });
                if !is_collapsed {
                    lines.extend(bucket.into_iter().map(HostLine::Host));
                }
            }
        } else {
            lines.extend(recent.iter().cloned().map(HostLine::Host));
        }
        let visible: Vec<Uuid> = lines
            .iter()
            .filter_map(|l| match l {
                HostLine::Host(p) => Some(p.id),
                _ => None,
            })
            .collect();

        let total = profiles.len();
        let files = profiles
            .iter()
            .map(|p| ssh_group_key(p).to_string())
            .collect::<std::collections::HashSet<_>>()
            .len();
        let rest = total.saturating_sub(recent.len());

        let toolbar = h_flex()
            .gap(px(12.))
            .items_center()
            .pb(px(8.))
            .child(
                kit::search_field(&filter, &tk)
                    .w(px(240.))
                    .capture_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                        let key = ev.keystroke.key.as_str();
                        match key {
                            "down" | "up" if !visible.is_empty() => {
                                let current = this.active_settings().and_then(|s| s.ssh_open);
                                let at =
                                    current.and_then(|id| visible.iter().position(|v| *v == id));
                                let next = match (at, key) {
                                    (None, _) => 0,
                                    (Some(i), "down") => (i + 1).min(visible.len() - 1),
                                    (Some(i), _) => i.saturating_sub(1),
                                };
                                if this.active_settings().is_some_and(|s| s.ssh_form.is_none()) {
                                    if let Some(s) = this.active_settings_mut() {
                                        s.ssh_open = Some(visible[next]);
                                    }
                                }
                                cx.stop_propagation();
                                cx.notify();
                            }
                            "escape" => {
                                let filter = this.active_settings().map(|s| s.ssh_filter.clone());
                                if let Some(filter) = filter
                                    && !filter.read(cx).value().is_empty()
                                {
                                    filter.update(cx, |f, cx| f.set_value("", window, cx));
                                    cx.stop_propagation();
                                }
                            }
                            _ => {}
                        }
                    })),
            )
            .child(div().flex_1())
            .child(
                kit::button(
                    "ssh-import-config",
                    t(L10nKey::SettingsImportFromSshConfig),
                    BtnKind::Link,
                )
                .text_size(fs(12.5))
                .on_click(
                    cx.listener(|this, _, window, cx| this.import_ssh_config_profiles(window, cx)),
                ),
            )
            .child(
                kit::button(
                    "ssh-add-host",
                    t(L10nKey::SettingsAddHost),
                    BtnKind::Secondary,
                )
                .icon("icons/settings/plus.svg")
                .dimmed(adding.is_some())
                .on_click(cx.listener(|this, _, window, cx| {
                    if this
                        .active_settings()
                        .and_then(|s| s.ssh_form.as_ref())
                        .is_some_and(|f| {
                            !cx.global::<Config>()
                                .ssh_profiles
                                .iter()
                                .any(|p| p.id == f.editing)
                        })
                    {
                        return;
                    }
                    let filter = this.active_settings().map(|s| s.ssh_filter.clone());
                    if let Some(filter) = filter {
                        filter.update(cx, |f, cx| f.set_value("", window, cx));
                    }
                    this.add_new_profile(window, cx);
                })),
            );

        let mut list = v_flex()
            .id("ssh-host-list")
            .mx(px(-10.))
            .px(px(10.))
            .when(!query.is_empty() || show_all, |l| {
                l.max_h(px(360.)).overflow_y_scroll()
            });
        if let Some(id) = adding {
            list = list.child(self.render_host_row(None, id, cx));
        }
        for line in lines {
            list = match line {
                HostLine::Head {
                    key,
                    count,
                    collapsed,
                } => list.child(self.render_host_group_head(&key, count, collapsed, cx)),
                HostLine::Host(p) => list.child(self.render_host_row(Some(&p), p.id, cx)),
            };
        }

        let empty = visible_is_empty(&profiles, &query) && adding.is_none();
        let footer = if !query.is_empty() {
            empty.then(|| {
                div()
                    .h(px(40.))
                    .flex()
                    .items_center()
                    .text_color(tk.k35)
                    .child(t_fmt(L10nKey::SettingsNoHostsMatch, &[("query", &query)]))
                    .into_any_element()
            })
        } else if total == 0 && adding.is_none() {
            Some(
                div()
                    .h(px(40.))
                    .flex()
                    .items_center()
                    .text_color(tk.k35)
                    .child(t(L10nKey::SettingsNoHostsYet))
                    .into_any_element(),
            )
        } else if rest > 0 || show_all {
            Some(
                h_flex()
                    .h(px(36.))
                    .mt(px(6.))
                    .gap(px(8.))
                    .items_center()
                    .text_size(fs(12.))
                    .text_color(tk.k45)
                    .child(if show_all {
                        t_fmt(
                            L10nKey::SettingsHostsFromFiles,
                            &[("count", &total.to_string()), ("files", &files.to_string())],
                        )
                    } else {
                        t_plural(L10nKey::SettingsMoreHosts, rest, &[])
                    })
                    .child(
                        kit::button(
                            "ssh-show-all",
                            if show_all {
                                t(L10nKey::SettingsShowRecentOnly)
                            } else {
                                t(L10nKey::SettingsShowAll)
                            },
                            BtnKind::Link,
                        )
                        .on_click(cx.listener(|this, _, _w, cx| {
                            if let Some(s) = this.active_settings_mut() {
                                s.ssh_show_all = !s.ssh_show_all;
                            }
                            cx.notify();
                        })),
                    )
                    .into_any_element(),
            )
        } else {
            None
        };

        self.settings_group(
            Some(t(L10nKey::SettingsHosts)),
            Some(t(L10nKey::SettingsHostsDesc).to_string()),
            [v_flex()
                .pt(px(2.))
                .child(toolbar)
                .child(list)
                .children(footer)
                .into_any_element()],
            cx,
        )
    }

    fn render_host_group_head(
        &self,
        key: &str,
        count: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tk = Tk::of(cx);
        let owned = key.to_string();
        h_flex()
            .id(SharedString::from(format!("ssh-group-{key}")))
            .h(px(34.))
            .flex_shrink_0()
            .items_end()
            .pb(px(7.))
            .gap(px(6.))
            .cursor_pointer()
            .text_size(fs(11.5))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(tk.heading)
            .hover(move |s| s.text_color(tk.fg))
            .child(
                div().mb(px(1.)).child(
                    Icon::empty()
                        .path(if collapsed {
                            "icons/settings/chevron-right.svg"
                        } else {
                            "icons/settings/chevron-down.svg"
                        })
                        .size(px(9.)),
                ),
            )
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .child(ssh_group_label(key).to_string()),
            )
            .child(
                div()
                    .font_weight(FontWeight::NORMAL)
                    .text_color(tk.k35)
                    .child(count.to_string()),
            )
            .on_click(cx.listener(move |this, _, _w, cx| this.toggle_ssh_group(owned.clone(), cx)))
            .into_any_element()
    }

    /// One host: the row, and under it either the host's details or, while it
    /// is being edited, the editor. `p` is `None` for a host not saved yet.
    fn render_host_row(
        &self,
        p: Option<&SshProfile>,
        id: Uuid,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tk = Tk::of(cx);
        let Some(s) = self.active_settings() else {
            return div().into_any_element();
        };
        let editing = s.ssh_form.as_ref().is_some_and(|f| f.editing == id);
        let open = editing || s.ssh_open == Some(id);
        let live = self.live_ssh_profiles(cx).contains(&id);
        let dirty = editing && self.ssh_form_dirty(cx);

        // While editing, the row shows what the form would save.
        let (title, address) = match (editing, p) {
            (true, _) => {
                let form = s.ssh_form.as_ref().unwrap();
                let name = form.name.read(cx).value().trim().to_string();
                let host = form.host.read(cx).value().trim().to_string();
                let user = form.user.read(cx).value().trim().to_string();
                let port = form.port.read(cx).value().trim().to_string();
                let jump = form.jump.read(cx).value().trim().to_string();
                let mut addr = String::new();
                if !user.is_empty() {
                    addr.push_str(&user);
                    addr.push('@');
                }
                addr.push_str(if host.is_empty() { "…" } else { &host });
                if !port.is_empty() && port != "22" {
                    addr.push(':');
                    addr.push_str(&port);
                }
                if !jump.is_empty() {
                    addr.push_str(&format!("  via {jump}"));
                }
                let host_empty = host.is_empty();
                let title = match (name.is_empty(), host_empty) {
                    (false, _) => name,
                    (true, false) => host,
                    (true, true) => t(L10nKey::SettingsNewHost).to_string(),
                };
                (
                    title,
                    if host_empty {
                        "user@hostname".to_string()
                    } else {
                        addr
                    },
                )
            }
            (false, Some(p)) => (host_title(p), host_address(p, cx)),
            (false, None) => (t(L10nKey::SettingsNewHost).to_string(), String::new()),
        };
        let blank_title = editing && p.is_none() && title == t(L10nKey::SettingsNewHost);
        let (meta, meta_color) = if dirty {
            (t(L10nKey::SettingsUnsaved).to_string(), tk.warn_text)
        } else if live {
            (t(L10nKey::SettingsLive).to_string(), tk.ok)
        } else {
            let used = cx
                .global::<Config>()
                .ssh_profile_frecency
                .get(&id)
                .map(|u| u.last_used)
                .filter(|&t| t > 0);
            let text = match (used, p) {
                (_, None) => String::new(),
                (Some(then), _) => {
                    crate::ui::home::relative_time(crate::core::config::unix_now(), then)
                }
                (None, _) => t(L10nKey::SettingsNever).to_string(),
            };
            (text, tk.k4)
        };

        let head = div()
            .id(SharedString::from(format!("ssh-host-{}", id.as_u128())))
            .h(px(40.))
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(12.))
            .rounded(px(8.))
            .cursor_pointer()
            .when(!open, |r| r.hover(move |s| s.bg(tk.k04)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(1.))
                    .child(
                        div()
                            .line_height(px(17.))
                            .truncate()
                            .when(open, |d| d.font_weight(FontWeight::MEDIUM))
                            .text_color(if blank_title { tk.k4 } else { tk.fg })
                            .child(title),
                    )
                    .child(
                        div()
                            .font_family(Tk::mono(cx))
                            .text_size(fs(11.))
                            .line_height(px(15.))
                            .text_color(tk.k45)
                            .truncate()
                            .child(address),
                    ),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .text_size(fs(12.))
                    .text_color(meta_color)
                    .child(meta),
            )
            .child(
                div().size(px(10.)).flex_shrink_0().child(
                    Icon::empty()
                        .path(if open {
                            "icons/settings/chevron-down.svg"
                        } else {
                            "icons/settings/chevron-right.svg"
                        })
                        .size(px(10.))
                        .text_color(tk.k35),
                ),
            )
            .on_click(cx.listener(move |this, _, _w, cx| {
                if this
                    .active_settings()
                    .and_then(|s| s.ssh_form.as_ref())
                    .is_some_and(|f| f.editing == id)
                {
                    return;
                }
                if let Some(s) = this.active_settings_mut() {
                    s.ssh_open = if s.ssh_open == Some(id) {
                        None
                    } else {
                        Some(id)
                    };
                }
                cx.notify();
            }));

        let body = if editing {
            Some(self.render_host_editor(p.is_some(), cx))
        } else if open {
            p.map(|p| self.render_host_details(p, cx))
        } else {
            None
        };
        v_flex()
            .flex_shrink_0()
            .mx(px(-10.))
            .rounded(px(8.))
            .when(open, |d| d.bg(tk.k04))
            .child(head)
            .children(body)
            .into_any_element()
    }

    fn render_host_details(&self, p: &SshProfile, cx: &mut Context<Self>) -> AnyElement {
        let tk = Tk::of(cx);
        let id = p.id;
        let cfg = cx.global::<Config>();
        let jump = p.jump_host.and_then(|j| {
            cfg.ssh_profiles
                .iter()
                .find(|q| q.id == j)
                .map(|q| host_title(q))
        });
        let fields: Vec<(&str, String)> = [
            ("HostName", Some(p.host.clone())),
            (
                "User",
                Some(if p.user.is_empty() {
                    "$USER".to_string()
                } else {
                    p.user.clone()
                }),
            ),
            ("Port", Some(p.port.to_string())),
            ("IdentityFile", p.identity_files.first().cloned()),
            ("ProxyJump", jump),
            (
                t(L10nKey::SettingsDefinedIn),
                Some(ssh_group_label(ssh_group_key(p)).to_string()),
            ),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.filter(|v| !v.is_empty()).map(|v| (k, v)))
        .collect();
        let copied = self.active_settings().and_then(|s| s.ssh_copied) == Some(id);
        let command = ssh_command(p);
        let mono = Tk::mono(cx);
        v_flex()
            .gap(px(14.))
            .px(px(10.))
            .pt(px(4.))
            .pb(px(14.))
            .child(
                v_flex()
                    .gap(px(6.))
                    .text_size(fs(12.))
                    .children(fields.into_iter().map(|(k, v)| {
                        h_flex()
                            .line_height(px(17.))
                            .child(
                                div()
                                    .w(px(96.))
                                    .flex_shrink_0()
                                    .text_color(tk.k45)
                                    .child(k.to_string()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(mono.clone())
                                    .text_size(fs(11.5))
                                    .text_color(tk.fg)
                                    .child(v),
                            )
                    })),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(
                        kit::button(
                            "ssh-host-connect",
                            t(L10nKey::SettingsConnectInNewTab),
                            BtnKind::Primary,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.close_settings_then(window, cx, move |this, window, cx| {
                                    this.connect_ssh_profile(id, window, cx)
                                });
                            },
                        )),
                    )
                    .child(
                        kit::button(
                            "ssh-host-edit",
                            t(L10nKey::SettingsEditHost),
                            BtnKind::Secondary,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                if let Some(profile) = cx
                                    .global::<Config>()
                                    .ssh_profiles
                                    .iter()
                                    .find(|p| p.id == id)
                                    .cloned()
                                {
                                    this.ssh_form_load(&profile, window, cx);
                                }
                            },
                        )),
                    )
                    .child(div().flex_1())
                    .child(
                        kit::button(
                            "ssh-host-copy",
                            if copied {
                                t(L10nKey::SettingsCopied)
                            } else {
                                t(L10nKey::SettingsCopySshCommand)
                            },
                            BtnKind::Link,
                        )
                        .on_click(cx.listener(move |this, _, _w, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(command.clone()));
                            if let Some(s) = this.active_settings_mut() {
                                s.ssh_copied = Some(id);
                            }
                            cx.notify();
                            cx.spawn(async move |this, cx| {
                                smol::Timer::after(std::time::Duration::from_millis(1500)).await;
                                let _ = this.update(cx, |this, cx| {
                                    if let Some(s) = this.active_settings_mut()
                                        && s.ssh_copied == Some(id)
                                    {
                                        s.ssh_copied = None;
                                        cx.notify();
                                    }
                                });
                            })
                            .detach();
                        })),
                    ),
            )
            .into_any_element()
    }

    fn render_host_editor(&self, saved: bool, cx: &mut Context<Self>) -> AnyElement {
        let tk = Tk::of(cx);
        let Some(form) = self.active_settings().and_then(|s| s.ssh_form.as_ref()) else {
            return div().into_any_element();
        };
        let id = form.editing;
        let confirm_remove = self.active_settings().is_some_and(|s| s.ssh_confirm_remove);
        let (_, errors) = self.ssh_form_collect(cx).unzip();
        let errors = errors.unwrap_or_default();
        let dirty = self.ssh_form_dirty(cx);
        // A brand-new host is not told it needs a host before anyone had the
        // chance to type one; a malformed value says so at once.
        let core_blank = form.core_is_blank(cx);
        let host_error = errors.host.as_ref().filter(|_| !core_blank);
        let error = host_error
            .or(errors.port.as_ref())
            .or(errors.jump.as_ref())
            .or(errors.socks.as_ref())
            .or(errors.http.as_ref())
            .map(|e| e.message());
        let can_save = errors.is_empty() && (dirty || !saved);

        let field = |label: &'static str, input: &Entity<InputState>, bad: bool| {
            let focused = self.settings_input_focused(input, cx);
            h_flex()
                .items_center()
                .child(
                    div()
                        .w(px(SSH_LABEL_W))
                        .flex_shrink_0()
                        .text_size(fs(12.))
                        .text_color(tk.k45)
                        .child(label),
                )
                .child(
                    kit::text_field(input, focused, bad, &tk, cx)
                        .bg(tk.page)
                        .text_size(fs(11.5))
                        .flex_1()
                        .max_w(px(FORM_FIELD_W)),
                )
        };
        let fields = v_flex()
            .gap(px(6.))
            .child(field("Alias", &form.name, false))
            .child(field("HostName", &form.host, host_error.is_some()))
            .child(field("User", &form.user, false))
            .child(field("Port", &form.port, errors.port.is_some()))
            .child(field("IdentityFile", &form.identity_files, false))
            .child(field("ProxyJump", &form.jump, errors.jump.is_some()));

        v_flex()
            .gap(px(14.))
            .px(px(10.))
            .pt(px(4.))
            .pb(px(14.))
            .capture_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "enter" => {
                        this.save_host_form(window, cx);
                        cx.stop_propagation();
                    }
                    "escape" => {
                        this.cancel_ssh_form(cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .child(fields)
            .when_some(error, |v, e| {
                v.child(div().text_size(fs(12.)).text_color(tk.danger).child(e))
            })
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(
                        kit::button(
                            "ssh-form-save",
                            if saved {
                                t(L10nKey::Save)
                            } else {
                                t(L10nKey::SettingsAddHost)
                            },
                            BtnKind::Primary,
                        )
                        .dimmed(!can_save)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.save_host_form(window, cx)),
                        ),
                    )
                    .child(
                        kit::button("ssh-form-cancel", t(L10nKey::Cancel), BtnKind::Secondary)
                            .on_click(cx.listener(|this, _, _w, cx| this.cancel_ssh_form(cx))),
                    )
                    .child(
                        div()
                            .pl(px(4.))
                            .text_size(fs(11.5))
                            .text_color(tk.k4)
                            .child(t(L10nKey::SettingsStoredInTty7)),
                    )
                    .child(div().flex_1())
                    .when(saved, |r| {
                        r.child(
                            kit::button(
                                "ssh-form-remove",
                                if confirm_remove {
                                    t(L10nKey::SettingsClickAgainToRemove)
                                } else {
                                    t(L10nKey::SettingsRemoveHost)
                                },
                                if confirm_remove {
                                    BtnKind::Danger
                                } else {
                                    BtnKind::Link
                                },
                            )
                            .on_click(cx.listener(
                                move |this, _, _w, cx| {
                                    let confirmed = this
                                        .active_settings()
                                        .is_some_and(|s| s.ssh_confirm_remove);
                                    if confirmed {
                                        this.delete_profile_confirmed(id, cx);
                                    } else if let Some(s) = this.active_settings_mut() {
                                        s.ssh_confirm_remove = true;
                                        cx.notify();
                                    }
                                },
                            )),
                        )
                    }),
            )
            .into_any_element()
    }

    fn save_host_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.save_editing_profile(window, cx) {
            if let Some(s) = self.active_settings_mut() {
                s.ssh_form = None;
                s.ssh_open = Some(id);
                s.ssh_confirm_remove = false;
            }
            cx.notify();
        } else {
            cx.notify();
        }
    }
}

fn host_title(p: &SshProfile) -> String {
    if p.name.is_empty() {
        to_connect_string(p)
    } else {
        p.name.clone()
    }
}

/// `user@host:port  via jump`, the way the row's second line reads.
fn host_address(p: &SshProfile, cx: &App) -> String {
    let mut addr = String::new();
    if !p.user.is_empty() {
        addr.push_str(&p.user);
        addr.push('@');
    }
    addr.push_str(&p.host);
    if p.port != DEFAULT_SSH_PORT {
        addr.push_str(&format!(":{}", p.port));
    }
    if let Some(jump) = p.jump_host.and_then(|j| {
        cx.global::<Config>()
            .ssh_profiles
            .iter()
            .find(|q| q.id == j)
            .map(host_title)
    }) {
        addr.push_str(&format!("  via {jump}"));
    }
    addr
}

/// The command a terminal elsewhere would reach this host with.
fn ssh_command(p: &SshProfile) -> String {
    let mut cmd = String::from("ssh");
    if p.port != DEFAULT_SSH_PORT {
        cmd.push_str(&format!(" -p {}", p.port));
    }
    if let Some(key) = p.identity_files.first() {
        cmd.push_str(&format!(" -i {key}"));
    }
    cmd.push(' ');
    if !p.user.is_empty() {
        cmd.push_str(&p.user);
        cmd.push('@');
    }
    cmd.push_str(&p.host);
    cmd
}

fn visible_is_empty(profiles: &[SshProfile], query: &str) -> bool {
    !profiles.iter().any(|p| ssh_row_matches(p, query))
}

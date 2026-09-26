//! Keyboard shortcuts: every action in the palette's groups, each with its
//! keys as keycaps. Click the keys to record new ones; a chord another action
//! already has asks before it is taken over.

use super::kit::{BtnKind, Tk, fs};
use super::*;

impl Tty7App {
    pub(crate) fn render_settings_keybindings(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = Tk::of(cx);
        let Some(s) = self.active_settings() else {
            return div().into_any_element();
        };
        let search = s.shortcut_search.clone();
        let query = search.read(cx).value().trim().to_lowercase();
        let recording = s
            .recording
            .as_ref()
            .map(|r| (r.action.clone(), r.chords.clone()));
        let conflict = s
            .kb_conflict
            .as_ref()
            .map(|c| (c.action.clone(), c.spec.clone(), c.other.clone()));
        let note = s.rebinding_note.clone();

        let (preset, prefix, overridden) = {
            let cfg = cx.global::<Config>();
            let overridden: std::collections::HashSet<String> =
                cfg.keybindings.keys().cloned().collect();
            (
                cfg.keybinding_preset.clone(),
                cfg.prefix.clone(),
                overridden,
            )
        };
        let tmux = preset == "tmux";
        let changed = overridden.len();
        let effective = crate::ui::keymap::effective_chords(cx);

        // The toolbar: the filter, a way back from every change at once, and
        // the preset the whole table starts from.
        let presets = self.segmented(
            "kb-preset",
            &[t(L10nKey::SettingsDefault), "tmux"],
            usize::from(tmux),
            cx,
            |this, ix, _w, cx| {
                this.set_keybinding_preset(if ix == 0 { "default" } else { "tmux" }, cx)
            },
        );
        let toolbar =
            h_flex()
                .gap(px(12.))
                .items_center()
                .child(
                    kit::search_field(&search, &tk)
                        .h(px(28.))
                        .flex_1()
                        .max_w(px(300.)),
                )
                .child(div().flex_1())
                .when(changed > 0, |r| {
                    r.child(
                        kit::button(
                            "kb-restore-all",
                            t_plural(L10nKey::SettingsRestoreChanged, changed, &[]),
                            BtnKind::Link,
                        )
                        .on_click(cx.listener(|this, _, _w, cx| {
                            this.restore_default_keybindings_confirmed(cx)
                        })),
                    )
                })
                .child(presets);
        let hint = div()
            .text_size(fs(12.))
            .line_height(px(17.))
            .text_color(tk.k45)
            .child(if tmux {
                t(L10nKey::SettingsShortcutsHintTmux)
            } else {
                t(L10nKey::SettingsShortcutsHint)
            });
        let prefix_row = tmux.then(|| {
            h_flex()
                .gap(px(10.))
                .items_center()
                .child(
                    div()
                        .text_size(fs(12.))
                        .text_color(tk.k5)
                        .child(t(L10nKey::SettingsPrefix)),
                )
                .child(self.segmented(
                    "kb-prefix",
                    &["⌃B", "⌃A"],
                    usize::from(prefix == "ctrl-a"),
                    cx,
                    |this, ix, _w, cx| {
                        this.set_keybinding_prefix(if ix == 0 { "ctrl-b" } else { "ctrl-a" }, cx)
                    },
                ))
        });
        let intro = v_flex()
            .mt(px(-12.))
            .gap(px(10.))
            .child(toolbar)
            .child(hint)
            .children(prefix_row)
            .when_some(note, |v, note| {
                v.child(div().text_size(fs(12.)).text_color(tk.fg).child(note))
            })
            .into_any_element();

        // The rows, in the palette's groups.
        let mut grouped: Vec<(
            crate::ui::palette::CommandGroup,
            Vec<(String, Vec<String>, String)>,
        )> = Vec::new();
        for (action, keys) in effective {
            let (group, label) = crate::ui::keymap::action_entry(&action);
            if !query.is_empty()
                && !keybinding_matches_query(&action, &query)
                && !label.to_lowercase().contains(&query)
            {
                continue;
            }
            match grouped.iter_mut().find(|(g, _)| *g == group) {
                Some((_, rows)) => rows.push((action, keys, label)),
                None => grouped.push((group, vec![(action, keys, label)])),
            }
        }
        grouped.sort_by_key(|(g, _)| {
            crate::ui::palette::CommandGroup::ORDER
                .iter()
                .position(|o| o == g)
                .unwrap_or(usize::MAX)
        });

        let mut groups = vec![intro];
        let none = grouped.is_empty();
        for (group, rows) in grouped {
            let mut section = v_flex().child(
                div()
                    .pb(px(6.))
                    .text_size(fs(11.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(tk.heading)
                    .child(group.title()),
            );
            for (action, keys, label) in rows {
                let is_recording = recording.as_ref().is_some_and(|(a, _)| *a == action);
                let conflict_here = conflict.as_ref().filter(|(a, _, _)| *a == action);
                let shown: Vec<String> = match (&recording, conflict_here) {
                    (Some((a, chords)), _) if *a == action => vec![chords.join(" ")]
                        .into_iter()
                        .filter(|c| !c.is_empty())
                        .collect(),
                    (_, Some((_, spec, _))) => vec![spec.clone()],
                    _ => keys,
                };
                let modified = overridden.contains(&action) && !is_recording;
                let ring = if is_recording {
                    vec![kit::ring(tk.fg, 1., true)]
                } else if conflict_here.is_some() {
                    vec![kit::ring(tk.warn, 1., true)]
                } else {
                    Vec::new()
                };
                let hover_ring = if is_recording || conflict_here.is_some() {
                    vec![kit::ring(tk.fg, 1., true)]
                } else {
                    vec![kit::ring(tk.k15, 0.5, true)]
                };
                let record_action = action.clone();
                let keys_button = h_flex()
                    .id(SharedString::from(format!("kb-{action}")))
                    .h(px(26.))
                    .min_w(px(72.))
                    .pl(px(8.))
                    .pr(px(4.))
                    .gap(px(5.))
                    .items_center()
                    .justify_end()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(is_recording, |b| b.bg(tk.page))
                    .shadow(ring)
                    .hover(move |b| b.shadow(hover_ring.clone()))
                    .when(is_recording && shown.is_empty(), |b| {
                        b.child(
                            div()
                                .pr(px(2.))
                                .text_size(fs(12.))
                                .text_color(tk.k45)
                                .child(t(L10nKey::SettingsPressKeysShort)),
                        )
                    })
                    .children(shown.iter().enumerate().map(|(n, spec)| {
                        h_flex()
                            .gap(px(5.))
                            .items_center()
                            .when(n > 0, |d| {
                                d.child(div().text_size(fs(11.)).text_color(tk.k4).child("/"))
                            })
                            .child(Self::keycap_row(spec, &tk))
                    }))
                    .when(!is_recording && shown.is_empty(), |b| {
                        b.child(
                            div()
                                .pr(px(4.))
                                .text_size(fs(12.))
                                .text_color(tk.k3)
                                .child("—"),
                        )
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let already = this
                            .active_settings()
                            .and_then(|s| s.recording.as_ref())
                            .is_some_and(|r| r.action == record_action);
                        if already {
                            if let Some(s) = this.active_settings_mut() {
                                s.recording = None;
                            }
                            cx.notify();
                        } else {
                            this.start_recording_key(record_action.clone(), window, cx)
                        }
                    }));
                let reset_action = action.clone();
                let row = h_flex()
                    .h(px(34.))
                    .px(px(8.))
                    .gap(px(16.))
                    .items_center()
                    .justify_between()
                    .rounded(px(7.))
                    .when(conflict_here.is_none(), |r| r.hover(move |s| s.bg(tk.k04)))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(8.))
                            .items_center()
                            .child(div().min_w_0().truncate().child(label))
                            .when(modified, |r| {
                                r.child(
                                    div()
                                        .id(SharedString::from(format!("reset-{action}")))
                                        .flex_shrink_0()
                                        .text_size(fs(11.5))
                                        .text_color(tk.k4)
                                        .cursor_pointer()
                                        .hover(move |s| s.text_color(tk.fg))
                                        .child(t(L10nKey::Reset))
                                        .on_click(cx.listener(move |this, _, _w, cx| {
                                            this.reset_keybinding(reset_action.clone(), cx)
                                        })),
                                )
                            }),
                    )
                    .child(keys_button);
                let conflict_line = conflict_here.map(|(_, spec, other)| {
                    let keys = crate::ui::keymap::key_chords(spec)
                        .into_iter()
                        .map(|chord| chord.concat())
                        .collect::<Vec<_>>()
                        .join(&format!(" {} ", t(L10nKey::SettingsKeyThen)));
                    let other = crate::ui::keymap::action_entry(other)
                        .1
                        .trim_end_matches('…')
                        .to_string();
                    h_flex()
                        .px(px(8.))
                        .pt(px(2.))
                        .pb(px(10.))
                        .gap(px(8.))
                        .items_center()
                        .text_size(fs(12.))
                        .text_color(tk.k6)
                        .child(
                            div()
                                .size(px(6.))
                                .flex_shrink_0()
                                .rounded_full()
                                .bg(tk.warn),
                        )
                        .child(div().flex_1().min_w_0().child(t_fmt(
                            L10nKey::SettingsShortcutConflict,
                            &[("keys", &keys), ("action", &other)],
                        )))
                        .child(
                            kit::button(
                                "kb-conflict-replace",
                                t(L10nKey::SettingsReplace),
                                BtnKind::Primary,
                            )
                            .text_size(fs(12.))
                            .on_click(cx.listener(
                                |this, _, _w, cx| this.resolve_keybinding_conflict(true, cx),
                            )),
                        )
                        .child(
                            kit::button(
                                "kb-conflict-cancel",
                                t(L10nKey::Cancel),
                                BtnKind::Secondary,
                            )
                            .text_size(fs(12.))
                            .on_click(cx.listener(
                                |this, _, _w, cx| this.resolve_keybinding_conflict(false, cx),
                            )),
                        )
                });
                section = section.child(
                    v_flex()
                        .mx(px(-8.))
                        .rounded(px(7.))
                        .when(conflict_line.is_some(), |d| d.bg(tk.k04))
                        .child(row)
                        .children(conflict_line),
                );
            }
            groups.push(section.into_any_element());
        }
        if none {
            groups.push(
                div()
                    .text_color(tk.k35)
                    .child(t_fmt(L10nKey::SettingsNoActionsMatch, &[("query", &query)]))
                    .into_any_element(),
            );
        }
        Self::settings_page(groups)
    }
}

use super::kit;

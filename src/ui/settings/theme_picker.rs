//! Appearance's Theme group: the Light / Dark / System cards, one dropdown
//! per theme slot with a live preview of the theme under the pointer, and the
//! editor a duplicated theme opens.

use std::rc::Rc;

use super::kit::{self, BtnKind, Tk, fs};
use super::*;

fn rgb_of((r, g, b): (u8, u8, u8)) -> gpui::Rgba {
    rgb((r as u32) << 16 | (g as u32) << 8 | b as u32)
}

/// The colors a theme is recognised by at a glance.
struct Swatch {
    bg: gpui::Rgba,
    fg: gpui::Rgba,
    dim: gpui::Rgba,
    ansi: [gpui::Rgba; 16],
}

impl Swatch {
    fn of(p: &presets::Theme) -> Self {
        let ansi = std::array::from_fn(|i| rgb_of(p.ansi16[i]));
        Self {
            bg: rgb(p.background_color()),
            fg: rgb(p.foreground),
            dim: rgb_of(p.ansi16[8]),
            ansi,
        }
    }
}

impl Tty7App {
    fn theme_mode(cx: &App) -> ThemeMode {
        let cfg = cx.global::<Config>();
        if cfg.theme_follow_system {
            ThemeMode::System
        } else if presets::by_id(cx, &cfg.theme_preset).dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        }
    }

    pub(crate) fn render_theme_group(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = Tk::of(cx);
        let mode = Self::theme_mode(cx);
        let cfg = cx.global::<Config>();
        let (light_id, dark_id) = (
            cfg.theme_preset_light.clone(),
            cfg.theme_preset_dark.clone(),
        );
        let legible = cfg.theme_legible_palette;
        let light = Swatch::of(&presets::by_id(cx, &light_id));
        let dark = Swatch::of(&presets::by_id(cx, &dark_id));

        let modes = [
            (ThemeMode::Light, t(L10nKey::SettingsLight), vec![&light]),
            (ThemeMode::Dark, t(L10nKey::SettingsDark), vec![&dark]),
            (
                ThemeMode::System,
                t(L10nKey::SettingsThemeModeSystem),
                vec![&light, &dark],
            ),
        ];
        let mode_cards = h_flex()
            .gap(px(14.))
            .children(modes.into_iter().map(|(m, label, halves)| {
                let on = m == mode;
                let bar = |w: f32, c: gpui::Rgba| div().w(px(w)).h(px(3.)).rounded(px(1.)).bg(c);
                v_flex()
                    .id(SharedString::from(format!("theme-mode-{m:?}")))
                    .items_center()
                    .gap(px(7.))
                    .cursor_pointer()
                    .child(
                        h_flex()
                            .w(px(76.))
                            .h(px(50.))
                            .rounded(px(6.))
                            .overflow_hidden()
                            .shadow(if on {
                                vec![kit::ring(tk.page, 1., false), kit::ring(tk.fg, 2.5, false)]
                            } else {
                                vec![kit::ring(tk.k15, 0.5, false)]
                            })
                            .children(halves.into_iter().map(|s| {
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .bg(s.bg)
                                    .justify_center()
                                    .gap(px(5.))
                                    .px(px(9.))
                                    .overflow_hidden()
                                    .child(
                                        h_flex()
                                            .gap(px(3.))
                                            .child(bar(4., s.ansi[2]))
                                            .child(bar(26., s.fg)),
                                    )
                                    .child(
                                        h_flex()
                                            .gap(px(3.))
                                            .child(bar(10., s.ansi[4]))
                                            .child(bar(16., s.dim)),
                                    )
                                    .child(
                                        h_flex()
                                            .gap(px(3.))
                                            .child(bar(6., s.ansi[3]))
                                            .child(bar(20., s.dim)),
                                    )
                            })),
                    )
                    .child(
                        div()
                            .text_size(fs(12.))
                            .text_color(if on { tk.fg } else { tk.k5 })
                            .when(on, |d| d.font_weight(FontWeight::MEDIUM))
                            .child(label),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.set_theme_mode(m, window, cx)),
                    )
            }))
            .into_any_element();
        let mode_desc = match mode {
            ThemeMode::System => t(L10nKey::SettingsThemeModeSystemDesc),
            ThemeMode::Light => t(L10nKey::SettingsThemeModeLightDesc),
            ThemeMode::Dark => t(L10nKey::SettingsThemeModeDarkDesc),
        };

        let mut rows = vec![
            self.settings_row(
                t(L10nKey::SettingsSyncWithSystem),
                mode_desc,
                mode_cards,
                cx,
            )
            .into_any_element(),
        ];
        let slots: Vec<bool> = match mode {
            ThemeMode::System => vec![false, true],
            ThemeMode::Light => vec![false],
            ThemeMode::Dark => vec![true],
        };
        for dark_slot in slots {
            let desc = match (mode, dark_slot) {
                (ThemeMode::System, false) => t(L10nKey::SettingsThemeSlotLightDesc),
                (ThemeMode::System, true) => t(L10nKey::SettingsThemeSlotDarkDesc),
                _ => t(L10nKey::SettingsThemeSlotDesc),
            };
            let label = if dark_slot {
                t(L10nKey::SettingsDarkThemeLabel)
            } else {
                t(L10nKey::SettingsLightThemeLabel)
            };
            let control = self.theme_slot_dropdown(dark_slot, cx);
            rows.push(
                h_flex()
                    .min_h(px(52.))
                    .py(px(12.))
                    .gap(px(32.))
                    .items_center()
                    .justify_between()
                    .child(
                        v_flex()
                            .min_w_0()
                            .gap(px(2.))
                            .child(div().text_size(fs(13.)).child(label))
                            .child(
                                div()
                                    .text_size(fs(12.))
                                    .line_height(px(17.))
                                    .text_color(tk.k5)
                                    .child(desc),
                            ),
                    )
                    .child(control)
                    .into_any_element(),
            );
        }
        let legible = self.settings_switch(
            "theme-legible-palette",
            legible,
            cx,
            |this, on, window, cx| this.set_theme_legible_palette(on, window, cx),
        );
        rows.push(
            self.settings_row(
                t(L10nKey::SettingsLegiblePalette),
                t(L10nKey::SettingsLegiblePaletteDesc),
                legible,
                cx,
            )
            .into_any_element(),
        );
        self.settings_group(Some(t(L10nKey::SettingsThemeIntroTitle)), None, rows, cx)
    }

    fn theme_slot_dropdown(&self, dark_slot: bool, cx: &mut Context<Self>) -> AnyElement {
        let tk = Tk::of(cx);
        let cfg = cx.global::<Config>();
        let current_id = match (cfg.theme_follow_system, dark_slot) {
            (false, _) => cfg.theme_preset.clone(),
            (true, true) => cfg.theme_preset_dark.clone(),
            (true, false) => cfg.theme_preset_light.clone(),
        };
        let current = presets::by_id(cx, &current_id);
        let id: SharedString = if dark_slot {
            "theme-slot-dark"
        } else {
            "theme-slot-light"
        }
        .into();
        let open = self.settings_menu_open(&id);

        let mut themes: Vec<presets::Theme> = presets::all(cx)
            .into_iter()
            .filter(|p| p.dark == dark_slot || p.id == current_id)
            .collect();
        themes.sort_by_key(|p| p.path.is_some());
        let start = themes.iter().position(|p| p.id == current_id).unwrap_or(0);

        let sw = Swatch::of(&current);
        let leading = div()
            .w(px(16.))
            .h(px(12.))
            .flex_shrink_0()
            .rounded(px(3.))
            .bg(sw.bg)
            .shadow(vec![kit::ring(tk.k15, 0.5, true)])
            .flex()
            .items_center()
            .px(px(3.))
            .child(div().w(px(6.)).h(px(2.)).rounded(px(1.)).bg(sw.fg))
            .into_any_element();
        let toggle = id.clone();
        let trigger = kit::select_trigger(
            ElementId::Name(SharedString::from(format!("{id}-trigger"))),
            current.name.clone(),
            Some(leading),
            open,
            &tk,
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            this.toggle_settings_menu(toggle.clone(), start, true, window, cx)
        }));

        let popover = open.then(|| self.theme_popover(&id, dark_slot, &current_id, themes, cx));
        div()
            .relative()
            .flex_shrink_0()
            .child(trigger)
            .when_some(popover, |d, p| d.child(kit::popover_below(true, 6., p)))
            .into_any_element()
    }

    fn theme_popover(
        &self,
        id: &SharedString,
        dark_slot: bool,
        current_id: &str,
        themes: Vec<presets::Theme>,
        cx: &mut Context<Self>,
    ) -> Div {
        let tk = Tk::of(cx);
        let Some(s) = self.active_settings() else {
            return div();
        };
        let query = s.menu_query.read(cx).value().trim().to_lowercase();
        let visible: Vec<presets::Theme> = themes
            .into_iter()
            .filter(|p| query.is_empty() || p.name.to_lowercase().contains(&query))
            .collect();
        let hi = s.menu_hi.min(visible.len().saturating_sub(1));
        let preview_theme = visible
            .get(hi)
            .cloned()
            .unwrap_or_else(|| presets::by_id(cx, current_id));
        let pv = Swatch::of(&preview_theme);

        // What the highlighted theme does to a shell session, before it is
        // picked.
        let (fg, dim) = (pv.fg, pv.dim);
        let [_, r, g, y, b, m, ..] = pv.ansi;
        let lines: [Vec<(&str, gpui::Rgba)>; 5] = [
            vec![
                ("~/code/orbit ", b),
                ("main ", m),
                ("❯ ", g),
                ("git status -s", fg),
            ],
            vec![(" M ", y), ("src/server.ts", fg)],
            vec![("?? ", r), ("notes.md", fg)],
            vec![
                ("~/code/orbit ", b),
                ("main ", m),
                ("❯ ", g),
                ("npm test", fg),
            ],
            vec![
                ("✓ 42 passed", g),
                ("   ", fg),
                ("✗ 1 failed", r),
                ("   0.8s", dim),
            ],
        ];
        let preview = div().p(px(8.)).child(
            v_flex()
                .rounded(px(6.))
                .bg(pv.bg)
                .shadow(vec![kit::ring(tk.k08, 0.5, true)])
                .px(px(11.))
                .py(px(9.))
                .font_family(Tk::mono(cx))
                .text_size(fs(10.5))
                .line_height(px(16.))
                .children(lines.into_iter().map(|spans| {
                    h_flex().whitespace_nowrap().overflow_hidden().children(
                        spans
                            .into_iter()
                            .map(|(text, c)| div().text_color(c).child(text)),
                    )
                })),
        );

        let picks: Rc<Vec<String>> = Rc::new(visible.iter().map(|p| p.id.clone()).collect());
        let mut list = v_flex()
            .id(ElementId::Name(SharedString::from(format!("{id}-list"))))
            .max_h(px(196.))
            .overflow_y_scroll()
            .track_scroll(&s.menu_scroll)
            .px(px(6.))
            .py(px(4.))
            .gap(px(1.));
        for (row, p) in visible.iter().enumerate() {
            let sw = Swatch::of(p);
            let pick = p.id.clone();
            list = list.child(
                h_flex()
                    .id(ElementId::Name(SharedString::from(format!(
                        "theme-pick-{}",
                        p.id
                    ))))
                    .h(px(28.))
                    .flex_shrink_0()
                    .px(px(8.))
                    .gap(px(9.))
                    .items_center()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(row == hi, |r| r.bg(tk.k06))
                    .child(kit::check_mark(p.id == current_id, &tk))
                    .child(div().flex_1().min_w_0().truncate().child(p.name.clone()))
                    .when(p.path.is_some(), |r| {
                        r.child(
                            div()
                                .text_size(fs(11.))
                                .text_color(tk.k4)
                                .child(t(L10nKey::SettingsCustom)),
                        )
                    })
                    .child(
                        h_flex()
                            .gap(px(2.))
                            .px(px(4.))
                            .py(px(3.))
                            .rounded(px(3.))
                            .bg(sw.bg)
                            .shadow(vec![kit::ring(tk.k08, 0.5, true)])
                            .children(
                                (1..=6).map(|i| div().size(px(5.)).rounded_full().bg(sw.ansi[i])),
                            ),
                    )
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if let Some(s) = this.active_settings_mut()
                            && s.menu_hi != row
                        {
                            s.menu_hi = row;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.close_settings_menu(window, cx);
                        this.pick_slot_theme(dark_slot, &pick, window, cx);
                    })),
            );
        }
        if visible.is_empty() {
            list = list.child(
                div()
                    .h(px(28.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .text_color(tk.k35)
                    .child(t(L10nKey::SettingsNoThemesMatch)),
            );
        }
        // A theme file that fails to parse used to just not be in the list.
        // This is a folder the user drops files into; "it isn't there" needs a
        // reason attached.
        let rejected = presets::rejected(cx);
        let rejected_note = (!rejected.is_empty() && query.is_empty()).then(|| {
            v_flex()
                .px(px(14.))
                .py(px(8.))
                .gap(px(4.))
                .border_t_1()
                .border_color(tk.k08)
                .text_size(fs(11.5))
                .child(
                    div()
                        .text_color(tk.fg)
                        .child(t(L10nKey::SettingsThemesRejected)),
                )
                .children(rejected.into_iter().map(|(name, why)| {
                    v_flex()
                        .child(div().text_color(tk.danger).child(name))
                        .child(div().text_color(tk.k45).child(why))
                }))
        });
        let fork_from = current_id.to_string();
        let key_picks = picks.clone();

        kit::menu_panel(&tk)
            .w(px(300.))
            .p_0()
            .gap_0()
            .overflow_hidden()
            .capture_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                let key = ev.keystroke.key.as_str();
                let len = key_picks.len();
                match key {
                    "down" | "up" => {
                        if let Some(s) = this.active_settings_mut() {
                            let hi = s.menu_hi.min(len.saturating_sub(1));
                            s.menu_hi = if key == "down" {
                                (hi + 1).min(len.saturating_sub(1))
                            } else {
                                hi.saturating_sub(1)
                            };
                            s.menu_scroll.scroll_to_item(s.menu_hi);
                        }
                        cx.stop_propagation();
                        cx.notify();
                    }
                    "enter" => {
                        let hi = this.active_settings().map(|s| s.menu_hi).unwrap_or(0);
                        if let Some(pick) = key_picks.get(hi.min(len.saturating_sub(1))).cloned() {
                            this.close_settings_menu(window, cx);
                            this.pick_slot_theme(dark_slot, &pick, window, cx);
                        }
                        cx.stop_propagation();
                    }
                    "escape" => {
                        this.close_settings_menu(window, cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .child(preview)
            .child(
                h_flex()
                    .h(px(32.))
                    .px(px(14.))
                    .gap(px(7.))
                    .items_center()
                    .border_t_1()
                    .border_b_1()
                    .border_color(tk.k08)
                    .child(kit::search_glass(11., &tk))
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&s.menu_query)
                                .appearance(false)
                                .px_0()
                                .py_0()
                                .h(px(28.)),
                        ),
                    )
                    .child(div().text_size(fs(11.)).text_color(tk.k35).child("↑↓ ↵")),
            )
            .child(list)
            .children(rejected_note)
            .child(
                h_flex()
                    .h(px(34.))
                    .px(px(14.))
                    .gap(px(14.))
                    .items_center()
                    .border_t_1()
                    .border_color(tk.k08)
                    .child(
                        kit::button(
                            "theme-duplicate",
                            t(L10nKey::SettingsDuplicateToEdit),
                            BtnKind::Link,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.close_settings_menu(window, cx);
                                this.fork_slot_theme(dark_slot, &fork_from, window, cx);
                            },
                        )),
                    )
                    .child(
                        kit::button(
                            "theme-open-folder",
                            t(L10nKey::SettingsOpenThemesFolder),
                            BtnKind::Link,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.close_settings_menu(window, cx);
                            this.open_themes_folder(window, cx);
                        })),
                    ),
            )
    }

    /// Copies a slot's theme to a file of its own and puts the copy in the
    /// slot, where the editor below picks it up.
    fn fork_slot_theme(
        &mut self,
        dark_slot: bool,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let theme = presets::by_id(cx, id);
        match presets::fork_to_file(&theme) {
            Ok(new_id) => {
                presets::load_registry(cx);
                self.pick_slot_theme(dark_slot, &new_id, window, cx);
            }
            Err(e) => {
                log::warn!("failed to duplicate theme: {e}");
                crate::ui::host_ops::HostOps::notify_err(
                    window,
                    cx,
                    t(L10nKey::ThemeDuplicateFailed),
                    &e,
                );
            }
        }
    }

    /// The editor a custom theme opens: its colors, its background image, and
    /// the sixteen ANSI colors.
    pub(crate) fn render_theme_editor_group(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tk = Tk::of(cx);
        let editor = self
            .active_settings()
            .and_then(|s| s.theme_editor.as_ref())?;
        let seed: Vec<(String, Entity<ColorPickerState>)> = editor
            .seed
            .iter()
            .map(|(edit, state)| {
                (
                    crate::ui::app::theme_edit_label(*edit).to_string(),
                    state.clone(),
                )
            })
            .collect();
        let ansi: Vec<Entity<ColorPickerState>> =
            editor.ansi.iter().map(|(_, s)| s.clone()).collect();

        let theme = presets::by_id(cx, &crate::ui::theme::effective_preset_id(cx));
        let image = theme.image.clone();
        let image_name = image.as_ref().map(|i| {
            i.path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| i.path.display().to_string())
        });

        let mut rows: Vec<AnyElement> = seed
            .into_iter()
            .map(|(label, state)| {
                self.settings_row(
                    label,
                    "",
                    ColorPicker::new(&state).small().into_any_element(),
                    cx,
                )
                .into_any_element()
            })
            .collect();

        let image_control = h_flex()
            .gap(px(12.))
            .items_center()
            .when_some(image_name.clone(), |r, name| {
                r.child(
                    div()
                        .max_w(px(160.))
                        .truncate()
                        .text_size(fs(12.))
                        .text_color(tk.k5)
                        .child(name),
                )
                .child(
                    kit::button(
                        "remove-theme-image",
                        t(L10nKey::SettingsRemoveThemeImage),
                        BtnKind::Link,
                    )
                    .on_click(
                        cx.listener(|this, _, window, cx| this.remove_theme_image(window, cx)),
                    ),
                )
            })
            .child(
                kit::button(
                    "pick-theme-image",
                    if image.is_some() {
                        t(L10nKey::SettingsChangeThemeImage)
                    } else {
                        t(L10nKey::SettingsChooseThemeImage)
                    },
                    BtnKind::Secondary,
                )
                .on_click(cx.listener(|this, _, _w, cx| this.pick_theme_image(cx))),
            )
            .into_any_element();
        rows.push(
            self.settings_row(
                t(L10nKey::SettingsBackgroundImage),
                t(L10nKey::SettingsBackgroundImageDesc),
                image_control,
                cx,
            )
            .into_any_element(),
        );
        if let Some(img) = image {
            let app = cx.entity().downgrade();
            let control = kit::slider(
                "theme-image-opacity",
                img.opacity,
                format!("{:.0}%", img.opacity * 100.),
                &tk,
                Rc::new(move |f, window, cx| {
                    let v = (f * 100.).round() / 100.;
                    let _ = app.update(cx, |this, cx| this.set_theme_image_opacity(v, window, cx));
                }),
                Rc::new(|_, _| {}),
            );
            rows.push(
                self.settings_row(
                    t(L10nKey::SettingsImageOpacity),
                    t(L10nKey::SettingsImageOpacityDesc),
                    control,
                    cx,
                )
                .into_any_element(),
            );
        }
        let ansi_grid = v_flex()
            .gap(px(6.))
            .children(ansi.chunks(8).map(|chunk| {
                h_flex()
                    .gap(px(6.))
                    .children(chunk.iter().map(|state| ColorPicker::new(state).small()))
            }))
            .into_any_element();
        rows.push(
            self.settings_row(t(L10nKey::SettingsAnsiColors), "", ansi_grid, cx)
                .into_any_element(),
        );
        rows.push(
            h_flex()
                .pt(px(4.))
                .child(
                    kit::button(
                        "open-themes-folder",
                        t(L10nKey::SettingsOpenThemesFolder),
                        BtnKind::Link,
                    )
                    .on_click(cx.listener(|this, _, w, cx| this.open_themes_folder(w, cx))),
                )
                .into_any_element(),
        );
        Some(self.settings_group(
            Some(t(L10nKey::SettingsEditTheme)),
            Some(t(L10nKey::SettingsEditThemeIntro).to_string()),
            rows,
            cx,
        ))
    }
}

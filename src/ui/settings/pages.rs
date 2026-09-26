//! The pages that are rows and nothing else: General, the text and window
//! halves of Appearance, Terminal, Keyboard & Mouse, Window & Tabs, and About.

use std::rc::Rc;

use super::kit::{self, BtnKind, Stepper, Tk, fs};
use super::shell::{MenuEntry, SearchOption};
use super::*;

impl Tty7App {
    pub(crate) fn render_settings_general(&self, cx: &mut Context<Self>) -> AnyElement {
        let cfg = cx.global::<Config>();
        let current_lang = Self::normalize_gui_language(&cfg.gui_language);
        let language = self.settings_dropdown(
            "settings-language",
            crate::ui::i18n::SUPPORTED_LANGUAGES
                .iter()
                .find(|lang| lang.code == current_lang)
                .map(|lang| t(lang.label_key))
                .unwrap_or_default(),
            None,
            crate::ui::i18n::SUPPORTED_LANGUAGES
                .iter()
                .map(|lang| {
                    let code = lang.code;
                    MenuEntry::item(
                        t(lang.label_key),
                        code == current_lang,
                        move |this, window, cx| this.set_gui_language(code, window, cx),
                    )
                })
                .collect(),
            cx,
        );
        let mut first = vec![
            self.settings_row(
                t(L10nKey::SettingsLanguage),
                t(L10nKey::SettingsLanguageDesc),
                language,
                cx,
            )
            .into_any_element(),
        ];
        if cfg!(target_os = "macos") {
            let set_default = self
                .settings_button(
                    "set-default-terminal",
                    t(L10nKey::SettingsDefaultTerminalSet),
                    cx,
                    |_, window, cx| {
                        let message = match crate::core::default_terminal::set_as_default_terminal()
                        {
                            Ok(()) => t(L10nKey::SettingsDefaultTerminalSetSuccess).to_string(),
                            Err(error) => t_fmt(
                                L10nKey::SettingsDefaultTerminalSetFailed,
                                &[("error", &error)],
                            ),
                        };
                        window.push_notification(message, cx);
                    },
                )
                .into_any_element();
            first.push(
                self.settings_row(
                    t(L10nKey::SettingsDefaultTerminal),
                    t(L10nKey::SettingsDefaultTerminalDesc),
                    set_default,
                    cx,
                )
                .into_any_element(),
            );
        }

        let cfg = cx.global::<Config>();
        let startup_idx = match cfg.startup_mode {
            crate::core::config::StartupMode::Normal => 0,
            crate::core::config::StartupMode::Maximized => 1,
            crate::core::config::StartupMode::Fullscreen => 2,
        };
        let restore_session = cfg.restore_session;
        let remember_window_size = cfg.remember_window_size;
        let show_tray_icon = cfg.show_tray_icon;
        let notify_idx = match cfg.notify_on_command_finish {
            NotifyMode::Never => 0,
            NotifyMode::Unfocused => 1,
            NotifyMode::Always => 2,
        };
        // Exact-match highlight with a "Custom (Ns)" fallback: a hand-set 20s
        // used to light up "30s" (#550).
        let (threshold_sel, threshold_custom) = preset_choice(
            &NOTIFY_THRESHOLD_BUCKETS,
            cfg.notify_threshold_secs,
            |secs| format!("{secs}s"),
        );

        let startup = self.segmented(
            "wt-startup",
            &[
                t(L10nKey::SettingsStartupNormal),
                t(L10nKey::SettingsStartupMaximized),
                t(L10nKey::SettingsStartupFullscreen),
            ],
            startup_idx,
            cx,
            |this, ix, _w, cx| {
                let mode = match ix {
                    0 => crate::core::config::StartupMode::Normal,
                    1 => crate::core::config::StartupMode::Maximized,
                    _ => crate::core::config::StartupMode::Fullscreen,
                };
                this.set_startup_mode(mode, cx);
            },
        );
        let remember = self.settings_switch(
            "wt-remember-window",
            remember_window_size,
            cx,
            |this, on, _, cx| this.set_remember_window_size(on, cx),
        );
        let restore = self.settings_switch(
            "wt-restore-session",
            restore_session,
            cx,
            |this, on, _, cx| this.set_restore_session(on, cx),
        );
        let tray = self.settings_switch("wt-tray-icon", show_tray_icon, cx, |this, on, _, cx| {
            this.set_show_tray_icon(on, cx)
        });
        let notify = self.segmented(
            "wt-notify",
            &[
                t(L10nKey::NotifyModeNever),
                t(L10nKey::NotifyModeUnfocused),
                t(L10nKey::NotifyModeAlways),
            ],
            notify_idx,
            cx,
            |this, ix, _w, cx| {
                let mode = match ix {
                    0 => NotifyMode::Never,
                    1 => NotifyMode::Unfocused,
                    _ => NotifyMode::Always,
                };
                this.set_notify_mode(mode, cx);
            },
        );
        let threshold = self.segmented_valued(
            "wt-notify-threshold",
            &NOTIFY_THRESHOLD_LABELS,
            threshold_sel,
            threshold_custom,
            cx,
            |this, ix, _w, cx| {
                let secs = NOTIFY_THRESHOLD_BUCKETS
                    .get(ix)
                    .copied()
                    .unwrap_or(Config::default().notify_threshold_secs);
                this.set_notify_threshold(secs, cx);
            },
        );

        Self::settings_page([
            self.settings_group(None, None, first, cx),
            self.settings_group(
                Some(t(L10nKey::SettingsStartupRestore)),
                None,
                [
                    self.settings_row(
                        t(L10nKey::SettingsStartupWindow),
                        t(L10nKey::SettingsStartupWindowDesc),
                        startup,
                        cx,
                    ),
                    self.settings_row(
                        t(L10nKey::SettingsRememberWindowSize),
                        t(L10nKey::SettingsRememberWindowSizeDesc),
                        remember,
                        cx,
                    ),
                    self.settings_row(
                        t(L10nKey::SettingsRestoreLastLayout),
                        t(L10nKey::SettingsRestoreLastLayoutDesc),
                        restore,
                        cx,
                    ),
                    self.settings_row(
                        t(L10nKey::SettingsShowTrayIcon),
                        t(L10nKey::SettingsShowTrayIconDesc),
                        tray,
                        cx,
                    ),
                ]
                .map(IntoElement::into_any_element),
                cx,
            ),
            self.settings_group(
                Some(t(L10nKey::SettingsNotifications)),
                None,
                [
                    self.settings_row(
                        t(L10nKey::SettingsNotifyOnCommandFinish),
                        t(L10nKey::SettingsNotifyOnCommandFinishDesc),
                        notify,
                        cx,
                    ),
                    self.settings_row(
                        t(L10nKey::SettingsNotifyThreshold),
                        t(L10nKey::SettingsNotifyThresholdDesc),
                        threshold,
                        cx,
                    ),
                ]
                .map(IntoElement::into_any_element),
                cx,
            ),
        ])
    }

    /// A font menu over every installed family. `default_label` names the
    /// first row for the menus whose empty value means "follow something
    /// else".
    fn font_dropdown(
        &self,
        id: &'static str,
        current: Option<&str>,
        default_label: Option<&'static str>,
        cx: &mut Context<Self>,
        commit: impl Fn(&mut Self, String, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let names = self
            .active_settings()
            .map(|s| s.font_names.clone())
            .unwrap_or_default();
        let mut options: Vec<SearchOption> = Vec::with_capacity(names.len() + 1);
        let mut values: Vec<String> = Vec::with_capacity(names.len() + 1);
        if let Some(label) = default_label {
            options.push(SearchOption {
                label: label.into(),
                font: None,
            });
            values.push(label.to_string());
        }
        for name in names.iter() {
            options.push(SearchOption {
                label: name.clone().into(),
                font: Some(name.clone().into()),
            });
            values.push(name.clone());
        }
        let selected = match current {
            Some(name) => values.iter().position(|v| v == name),
            None => default_label.map(|_| 0),
        };
        let label = current
            .map(str::to_string)
            .or(default_label.map(str::to_string))
            .unwrap_or_default();
        let values = Rc::new(values);
        self.settings_search_dropdown(
            id,
            label,
            Rc::new(options),
            selected,
            Rc::new(move |this, ix, window, cx| {
                if let Some(value) = values.get(ix) {
                    commit(this, value.clone(), window, cx);
                }
            }),
            cx,
        )
    }

    fn stepper_control(
        &self,
        id: &'static str,
        value: String,
        unit: &str,
        (can_dec, can_inc): (bool, bool),
        cx: &mut Context<Self>,
        step: impl Fn(&mut Self, f32, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let tk = Tk::of(cx);
        let step = Rc::new(step);
        let (dec, inc) = (step.clone(), step);
        kit::stepper(
            id,
            Stepper {
                value,
                unit: unit.to_string(),
                can_dec,
                can_inc,
            },
            &tk,
            cx.listener(move |this, _, _w, cx| dec(this, -1., cx)),
            cx.listener(move |this, _, _w, cx| inc(this, 1., cx)),
        )
    }

    pub(crate) fn render_settings_appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::core::config::{
            FONT_SIZE_MAX, FONT_SIZE_MIN, LINE_HEIGHT_MAX, LINE_HEIGHT_MIN, UI_FONT_SIZE_MAX,
            UI_FONT_SIZE_MIN,
        };
        let cfg = cx.global::<Config>();
        let cursor_style = cfg.cursor_style;
        let cursor_blink = cfg.cursor_blink;
        let font_thicken = cfg.font_thicken;
        let font_family = cfg.font_family.clone();
        let font_bold = cfg.font_family_bold.clone();
        let font_italic = cfg.font_family_italic.clone();
        let ui_font = cfg.ui_font_family.clone();
        let font_ligatures = cfg.font_features.as_ref().is_some_and(|features| {
            features.is_calt_enabled() == Some(true)
                || features
                    .tag_value_list()
                    .iter()
                    .any(|(tag, value)| tag == "liga" && *value != 0)
        });
        let font_size = self.font_size;
        let line_height = self.line_height;
        let ui_font_size = self.ui_font_size(cx);

        let font_family_control = self.font_dropdown(
            "font-family",
            Some(&font_family),
            None,
            cx,
            |this, name, _w, cx| this.commit_font_family(name, cx),
        );
        let font_bold_control = self.font_dropdown(
            "font-family-bold",
            font_bold.as_deref(),
            Some(font_default_label()),
            cx,
            |this, name, _w, cx| this.commit_font_family_emphasis(true, name, cx),
        );
        let font_italic_control = self.font_dropdown(
            "font-family-italic",
            font_italic.as_deref(),
            Some(font_default_label()),
            cx,
            |this, name, _w, cx| this.commit_font_family_emphasis(false, name, cx),
        );
        let ui_font_control = self.font_dropdown(
            "ui-font-family",
            ui_font.as_deref(),
            Some(ui_font_default_label()),
            cx,
            |this, name, window, cx| this.commit_ui_font_family(name, window, cx),
        );
        let font_size_control = self.stepper_control(
            "font-size",
            format!("{font_size:.0}"),
            " pt",
            (font_size > FONT_SIZE_MIN, font_size < FONT_SIZE_MAX),
            cx,
            |this, dir, cx| this.change_font_size(dir * FONT_SIZE_STEP, cx),
        );
        let line_height_control = self.stepper_control(
            "line-height",
            format!("{line_height:.2}"),
            "",
            (line_height > LINE_HEIGHT_MIN, line_height < LINE_HEIGHT_MAX),
            cx,
            |this, dir, cx| this.change_line_height(dir * LINE_HEIGHT_STEP, cx),
        );
        let ui_font_size_control = self.stepper_control(
            "ui-font-size",
            format!("{ui_font_size:.0}"),
            " pt",
            (
                ui_font_size > UI_FONT_SIZE_MIN,
                ui_font_size < UI_FONT_SIZE_MAX,
            ),
            cx,
            |this, dir, cx| this.change_ui_font_size(dir * UI_FONT_SIZE_STEP, cx),
        );
        let ligatures =
            self.settings_switch("font-ligatures", font_ligatures, cx, |this, on, _, cx| {
                this.set_font_ligatures(on, cx)
            });
        // macOS alone dilates glyph strokes, so elsewhere there is no row.
        let thicken_row = cfg!(target_os = "macos").then(|| {
            let control =
                self.settings_switch("font-thicken", font_thicken, cx, |this, on, _, cx| {
                    this.set_font_thicken(on, cx)
                });
            self.settings_row(
                t(L10nKey::SettingsFontThicken),
                t(L10nKey::SettingsFontThickenDesc),
                control,
                cx,
            )
            .into_any_element()
        });

        let cursor_idx = match cursor_style {
            CursorStyle::Block => 0,
            CursorStyle::Bar => 1,
            CursorStyle::Underline => 2,
        };
        let cursor_style_control = self.segmented(
            "cursor-style",
            &[
                t(L10nKey::CursorShapeBlock),
                t(L10nKey::CursorShapeBar),
                t(L10nKey::CursorShapeUnderline),
            ],
            cursor_idx,
            cx,
            |this, ix, _w, cx| {
                let style = match ix {
                    0 => CursorStyle::Block,
                    1 => CursorStyle::Bar,
                    _ => CursorStyle::Underline,
                };
                this.set_cursor_style(style, cx);
            },
        );
        let blink = self.settings_switch("cursor-blink", cursor_blink, cx, |this, on, _, cx| {
            this.set_cursor_blink(on, cx)
        });

        let mut groups = vec![self.render_theme_group(cx)];
        groups.extend(self.render_theme_editor_group(cx));
        groups.push(
            self.settings_group(
                Some(t(L10nKey::SettingsTerminalFontGroup)),
                None,
                [
                    self.settings_row(
                        t(L10nKey::SettingsFontFamily),
                        t(L10nKey::SettingsFontFamilyDesc),
                        font_family_control,
                        cx,
                    )
                    .into_any_element(),
                    self.settings_row(
                        t(L10nKey::SettingsFontSize),
                        t(L10nKey::SettingsFontSizeDesc),
                        font_size_control,
                        cx,
                    )
                    .into_any_element(),
                    self.settings_row(
                        t(L10nKey::SettingsLineHeight),
                        t(L10nKey::SettingsLineHeightDesc),
                        line_height_control,
                        cx,
                    )
                    .into_any_element(),
                    self.settings_row(
                        t(L10nKey::SettingsBoldFont),
                        t(L10nKey::SettingsBoldFontDesc),
                        font_bold_control,
                        cx,
                    )
                    .into_any_element(),
                    self.settings_row(
                        t(L10nKey::SettingsItalicFont),
                        t(L10nKey::SettingsItalicFontDesc),
                        font_italic_control,
                        cx,
                    )
                    .into_any_element(),
                    self.settings_row(
                        t(L10nKey::SettingsFontLigatures),
                        t(L10nKey::SettingsFontLigaturesDesc),
                        ligatures,
                        cx,
                    )
                    .into_any_element(),
                ]
                .into_iter()
                .chain(thicken_row),
                cx,
            ),
        );
        groups.push(
            self.settings_group(
                Some(t(L10nKey::SettingsInterfaceFontGroup)),
                None,
                [
                    self.settings_row(
                        t(L10nKey::SettingsUiFontFamily),
                        t(L10nKey::SettingsUiFontFamilyDesc),
                        ui_font_control,
                        cx,
                    ),
                    self.settings_row(
                        t(L10nKey::SettingsUiFontSize),
                        t(L10nKey::SettingsUiFontSizeDesc),
                        ui_font_size_control,
                        cx,
                    ),
                ]
                .map(IntoElement::into_any_element),
                cx,
            ),
        );
        groups.push(
            self.settings_group(
                Some(t(L10nKey::SettingsCursor)),
                None,
                [
                    self.settings_row(
                        t(L10nKey::SettingsCursorShape),
                        t(L10nKey::SettingsCursorShapeDesc),
                        cursor_style_control,
                        cx,
                    ),
                    self.settings_row(
                        t(L10nKey::SettingsCursorBlink),
                        t(L10nKey::SettingsCursorBlinkDesc),
                        blink,
                        cx,
                    ),
                ]
                .map(IntoElement::into_any_element),
                cx,
            ),
        );
        groups.push(self.render_window_section(cx));
        Self::settings_page(groups)
    }

    fn render_window_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = Tk::of(cx);
        let config = cx.global::<Config>();
        let overridden = window_overrides_active(config, cfg!(target_os = "windows"));
        let dim_inactive_panes = config.dim_inactive_panes;
        let opacity = Tty7App::effective_window_opacity(cx);

        const MIN: f32 = 0.2;
        const MAX: f32 = 1.0;
        let app = cx.entity().downgrade();
        let release_app = app.clone();
        let opacity_control = kit::slider(
            "window-opacity",
            (opacity - MIN) / (MAX - MIN),
            format!("{:.0}%", opacity * 100.),
            &tk,
            Rc::new(move |f, window, cx| {
                let v = ((MIN + f * (MAX - MIN)) * 100.).round() / 100.;
                cx.global_mut::<Config>().window_opacity = Some(v.clamp(MIN, MAX));
                crate::ui::theme::apply_theme(Some(window), cx);
                let _ = app.update(cx, |_, cx| cx.notify());
            }),
            Rc::new(move |_window, cx| {
                let _ = release_app.update(cx, |this, cx| this.persist_settings_config(cx));
            }),
        );

        // Windows exposes the native backdrop materials directly; macOS keeps
        // the simple blur toggle, which drives its vibrancy.
        #[cfg(target_os = "windows")]
        let blur_control = {
            let current = config.window_backdrop;
            let entries = crate::ui::theme::backdrop_options(current)
                .into_iter()
                .map(|backdrop| {
                    MenuEntry::item(
                        t(crate::ui::app::window_backdrop_label_key(backdrop)),
                        backdrop == current,
                        move |this, window, cx| this.set_window_backdrop(backdrop, window, cx),
                    )
                })
                .collect();
            self.settings_dropdown(
                "window-backdrop",
                t(crate::ui::app::window_backdrop_label_key(current)),
                None,
                entries,
                cx,
            )
        };
        #[cfg(not(target_os = "windows"))]
        let blur_control = {
            let theme = presets::by_id(cx, &crate::ui::theme::effective_preset_id(cx));
            let blur = cx.global::<Config>().window_blur.unwrap_or(theme.blur);
            self.settings_switch("window-blur", blur, cx, |this, on, window, cx| {
                this.set_window_blur(on, window, cx)
            })
        };
        // `Auto` is the one backdrop that still defers to the legacy blur
        // flag. Offer that switch exactly when it has an effect — otherwise a
        // stored `window_blur: true` would blur the window with no control to
        // clear it.
        #[cfg(target_os = "windows")]
        let auto_blur_row =
            (cx.global::<Config>().window_backdrop == WindowBackdrop::Auto).then(|| {
                let theme = presets::by_id(cx, &crate::ui::theme::effective_preset_id(cx));
                let blur = cx.global::<Config>().window_blur.unwrap_or(theme.blur);
                let control =
                    self.settings_switch("window-blur", blur, cx, |this, on, window, cx| {
                        this.set_window_blur(on, window, cx)
                    });
                self.settings_row(
                    t(L10nKey::SettingsBlur),
                    t(L10nKey::SettingsBlurAutoDesc),
                    control,
                    cx,
                )
                .into_any_element()
            });
        #[cfg(not(target_os = "windows"))]
        let auto_blur_row: Option<AnyElement> = None;
        let dim = self.settings_switch(
            "dim-inactive-panes",
            dim_inactive_panes,
            cx,
            |this, on, _, cx| this.set_dim_inactive_panes(on, cx),
        );
        let follow_theme = overridden.then(|| {
            h_flex()
                .pt(px(4.))
                .pb(px(8.))
                .child(
                    kit::button(
                        "follow-theme-window",
                        t(L10nKey::FollowTheme),
                        BtnKind::Link,
                    )
                    .on_click(
                        cx.listener(|this, _, window, cx| this.reset_window_overrides(window, cx)),
                    ),
                )
                .into_any_element()
        });

        let mut rows = vec![
            self.settings_row(
                t(L10nKey::SettingsOpacity),
                t(L10nKey::SettingsOpacityDesc),
                opacity_control,
                cx,
            )
            .into_any_element(),
            self.settings_row(
                t(if cfg!(target_os = "windows") {
                    L10nKey::SettingsBackdrop
                } else {
                    L10nKey::SettingsBlur
                }),
                t(if cfg!(target_os = "windows") {
                    L10nKey::SettingsBackdropDesc
                } else {
                    L10nKey::SettingsBlurDesc
                }),
                blur_control,
                cx,
            )
            .into_any_element(),
        ];
        rows.extend(auto_blur_row);
        rows.extend(follow_theme);
        rows.push(
            self.settings_row(
                t(L10nKey::SettingsDimInactivePanes),
                t(L10nKey::SettingsDimInactivePanesDesc),
                dim,
                cx,
            )
            .into_any_element(),
        );
        self.settings_group(Some(t(L10nKey::SettingsTransparency)), None, rows, cx)
    }

    fn render_shell_group(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = Tk::of(cx);
        let (program_input, args_input, wd_path_input) = match self.active_settings() {
            Some(s) => (
                s.shell_program_input.clone(),
                s.shell_args_input.clone(),
                s.wd_path_input.clone(),
            ),
            None => return div().into_any_element(),
        };
        let wd_strategy = cx.global::<Config>().working_directory.strategy;
        let platform_default = if cfg!(windows) {
            "PowerShell"
        } else {
            t(L10nKey::SettingsShellDefaultLoginShell)
        };

        // tty7 already knows which shells are installed — it lists them on the
        // new-tab button — so the field carries a menu of them. The field
        // stays: a shell tty7 did not find still has to be reachable by path.
        // Detected shells only: a `custom_shells` row carries arguments this
        // picker would silently drop.
        let shells: Vec<_> = self
            .shells
            .shells
            .iter()
            .filter(|shell| !shell.user_authored)
            .cloned()
            .collect();
        let current_program = program_input.read(cx).value().trim().to_string();
        let platform_default_item: SharedString = if cfg!(windows) {
            "PowerShell".into()
        } else {
            t(L10nKey::AppPlaceholderLoginShell).into()
        };
        let pick = |program: String, input: Entity<InputState>| {
            move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                input.update(cx, |state, cx| state.set_value(program.clone(), window, cx));
                this.commit_shell_from_picker(cx);
            }
        };
        let mut entries = vec![MenuEntry::item(
            platform_default_item,
            current_program.is_empty(),
            pick(String::new(), program_input.clone()),
        )];
        if !shells.is_empty() {
            entries.push(MenuEntry::Separator);
        }
        for shell in &shells {
            entries.push(MenuEntry::item(
                shell.label.clone(),
                current_program == shell.program,
                pick(shell.program.clone(), program_input.clone()),
            ));
        }
        let menu_id: SharedString = "shell-program-detected".into();
        let open = self.settings_menu_open(&menu_id);
        let picker = (!shells.is_empty()).then(|| {
            let toggle = menu_id.clone();
            div()
                .id("shell-program-detected-trigger")
                .flex_shrink_0()
                .ml(px(4.))
                .mr(px(-4.))
                .size(px(18.))
                .rounded(px(4.))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(move |s| s.bg(tk.k05))
                .when(open, |d| d.bg(tk.k05))
                .child(kit::chevron_down(&tk))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.toggle_settings_menu(toggle.clone(), 0, false, window, cx)
                }))
        });
        let program_control = div()
            .relative()
            .child(
                self.settings_text_input(&program_input, FIELD_W, false, cx)
                    .children(picker),
            )
            .when(open, |d| {
                d.child(kit::popover_below(
                    true,
                    4.,
                    self.settings_menu_panel(&menu_id, entries, cx)
                        .min_w(px(FIELD_W)),
                ))
            })
            .into_any_element();
        // Args become argv verbatim, so a quote that never closes is a value
        // that cannot be saved at all — `commit_shell` refuses it, and this
        // line is the explanation (#551). The input commits on Enter/blur, so
        // a half-typed quote is never marked wrong mid-keystroke.
        let args_value = args_input.read(cx).value();
        let args_error = crate::ui::app::split_shell_args(&args_value)
            .is_err()
            .then(|| t(L10nKey::SettingsArgumentsInvalid).to_string());
        let args_control = self.settings_checked_input(&args_input, FIELD_W, args_error, cx);

        use crate::core::config::WdStrategy;
        let wd_idx = match wd_strategy {
            WdStrategy::Inherit => 0,
            WdStrategy::Home => 1,
            WdStrategy::Custom => 2,
        };
        let wd_radio = self.segmented(
            "wd-strategy",
            &[
                t(L10nKey::SettingsWdInherit),
                t(L10nKey::SettingsWdHome),
                t(L10nKey::SettingsWdCustom),
            ],
            wd_idx,
            cx,
            |this, ix, _w, cx| {
                let s = match ix {
                    0 => WdStrategy::Inherit,
                    1 => WdStrategy::Home,
                    _ => WdStrategy::Custom,
                };
                this.set_working_directory_strategy(s, cx);
            },
        );
        let mut rows = vec![
            self.settings_row(
                t(L10nKey::SettingsProgram),
                t(L10nKey::SettingsProgramDesc),
                program_control,
                cx,
            )
            .into_any_element(),
            self.settings_row(
                t(L10nKey::SettingsArguments),
                t(L10nKey::SettingsArgumentsDesc),
                args_control,
                cx,
            )
            .into_any_element(),
            self.settings_row(
                t(L10nKey::SettingsStartIn),
                t(L10nKey::SettingsStartInDesc),
                wd_radio,
                cx,
            )
            .into_any_element(),
        ];
        if wd_strategy == WdStrategy::Custom {
            // `commit_working_directory_path` refuses the same value through
            // the same predicate, so the red line and the not-saved config
            // always agree (#601).
            let value = wd_path_input.read(cx).value();
            let error = (!crate::ui::app::wd_path_saveable(&value))
                .then(|| t(L10nKey::SettingsWdPathInvalid).to_string());
            let control = self.settings_checked_input(&wd_path_input, 240., error, cx);
            rows.push(
                self.settings_row(
                    t(L10nKey::SettingsCustomPath),
                    t(L10nKey::SettingsCustomPathDesc),
                    control,
                    cx,
                )
                .into_any_element(),
            );
        }
        rows.push(
            div()
                .pt(px(4.))
                .text_size(fs(12.))
                .line_height(px(17.))
                .text_color(tk.k6)
                .child(t(L10nKey::SettingsShellFooter))
                .into_any_element(),
        );
        self.settings_group(
            Some(t(L10nKey::SettingsShell)),
            Some(t_fmt(
                L10nKey::SettingsShellIntro,
                &[("default", platform_default)],
            )),
            rows,
            cx,
        )
    }

    pub(crate) fn render_settings_terminal(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = Tk::of(cx);
        let cfg = cx.global::<Config>();
        let link_url = cfg.link_url;
        let ssh_loopback_forward = cfg.ssh_loopback_forward;
        let scroll_mult = cfg.mouse_scroll_multiplier;
        let smooth_scroll = cfg.smooth_scroll;
        let bell = cfg.bell;
        let link_file_open = cfg.file_open_mode();
        // A bucket highlights only on an exact match; any other value gets a
        // "Custom (N)" cell so the highlight never claims a number the config
        // does not have (#550).
        let (scrollback_sel, scrollback_custom) =
            preset_choice(&SCROLLBACK_BUCKETS, cfg.scrollback_limit, group_thousands);
        let Some(link_file_command_input) = self
            .active_settings()
            .map(|s| s.link_file_command_input.clone())
        else {
            return div().into_any_element();
        };

        let link_switch = self.settings_switch("term-link-url", link_url, cx, |this, on, _, cx| {
            this.set_link_url(on, cx)
        });
        let loopback_switch = self.settings_switch(
            "term-ssh-loopback-forward",
            ssh_loopback_forward,
            cx,
            |this, on, _, cx| this.set_ssh_loopback_forward(on, cx),
        );
        let link_file_open_radio = self.segmented(
            "term-link-file-open",
            &[
                t(L10nKey::SettingsOpenFilesInternal),
                t(L10nKey::SettingsOpenFilesSystem),
                t(L10nKey::SettingsOpenFilesCommand),
            ],
            match link_file_open {
                LinkFileOpen::Internal => 0,
                LinkFileOpen::System => 1,
                LinkFileOpen::Command => 2,
            },
            cx,
            |this, ix, _w, cx| {
                let mode = match ix {
                    0 => LinkFileOpen::Internal,
                    1 => LinkFileOpen::System,
                    _ => LinkFileOpen::Command,
                };
                this.set_link_file_open(mode, cx);
            },
        );
        // Only shown under `Command`: an empty box next to two working modes
        // reads as "this is what file links do".
        let link_file_command_row = (link_file_open == LinkFileOpen::Command).then(|| {
            let control = self
                .settings_text_input(&link_file_command_input, 240., false, cx)
                .into_any_element();
            self.settings_row(
                t(L10nKey::SettingsOpenFilesCommand),
                t_fmt(
                    L10nKey::SettingsOpenFilesWithDesc,
                    &[
                        ("modifier", LINK_MODIFIER_LABEL),
                        ("path", "{path}"),
                        ("line", "{line}"),
                        ("column", "{column}"),
                    ],
                ),
                control,
                cx,
            )
            .into_any_element()
        });
        let scrollback = self.segmented_valued(
            "term-scrollback",
            &SCROLLBACK_LABELS,
            scrollback_sel,
            scrollback_custom,
            cx,
            |this, ix, _w, cx| {
                let lines = SCROLLBACK_BUCKETS
                    .get(ix)
                    .copied()
                    .unwrap_or(Config::default().scrollback_limit);
                this.set_scrollback_limit(lines, cx);
            },
        );
        let bell_idx = match bell {
            BellMode::None => 0,
            BellMode::Visual => 1,
            BellMode::Audible => 2,
            BellMode::Both => 3,
        };
        let bell_control = self.segmented(
            "term-bell",
            &[
                t(L10nKey::SettingsBellModeOff),
                t(L10nKey::SettingsBellModeVisual),
                t(L10nKey::SettingsBellModeAudible),
                t(L10nKey::SettingsBellModeBoth),
            ],
            bell_idx,
            cx,
            |this, ix, _w, cx| {
                let mode = match ix {
                    0 => BellMode::None,
                    1 => BellMode::Visual,
                    2 => BellMode::Audible,
                    3 => BellMode::Both,
                    _ => BellMode::default(),
                };
                this.set_bell_mode(mode, cx);
            },
        );
        const MIN: f32 = 0.5;
        const MAX: f32 = 5.0;
        const STEP: f32 = 0.25;
        let app = cx.entity().downgrade();
        let release_app = app.clone();
        let scroll_control = kit::slider(
            "term-scroll-speed",
            (scroll_mult - MIN) / (MAX - MIN),
            format!("{scroll_mult:.2}×"),
            &tk,
            Rc::new(move |f, _window, cx| {
                let v = ((MIN + f * (MAX - MIN)) / STEP).round() * STEP;
                cx.global_mut::<Config>().mouse_scroll_multiplier = v.clamp(MIN, MAX);
                let _ = app.update(cx, |_, cx| cx.notify());
            }),
            Rc::new(move |_window, cx| {
                let _ = release_app.update(cx, |this, cx| this.persist_settings_config(cx));
            }),
        );
        let smooth = self.settings_switch(
            "term-smooth-scroll",
            smooth_scroll,
            cx,
            |this, on, _, cx| this.set_smooth_scroll(on, cx),
        );

        Self::settings_page([
            self.render_shell_group(cx),
            self.render_prompt_group(cx),
            self.settings_group(
                Some(t(L10nKey::SettingsScrolling)),
                None,
                [
                    self.settings_row(
                        t(L10nKey::SettingsScrollback),
                        t(L10nKey::SettingsScrollbackDesc),
                        scrollback,
                        cx,
                    ),
                    self.settings_row(
                        t(L10nKey::SettingsScrollSpeed),
                        t(L10nKey::SettingsScrollSpeedDesc),
                        scroll_control,
                        cx,
                    ),
                    self.settings_row(
                        t(L10nKey::SettingsSmoothScroll),
                        t(L10nKey::SettingsSmoothScrollDesc),
                        smooth,
                        cx,
                    ),
                ]
                .map(IntoElement::into_any_element),
                cx,
            ),
            self.settings_group(
                Some(t(L10nKey::SettingsBell)),
                None,
                [self
                    .settings_row(
                        t(L10nKey::SettingsTerminalBell),
                        t(L10nKey::SettingsTerminalBellDesc),
                        bell_control,
                        cx,
                    )
                    .into_any_element()],
                cx,
            ),
            self.settings_group(
                Some(t(L10nKey::SettingsLinks)),
                None,
                [
                    self.settings_row(
                        t(L10nKey::DetectUrls),
                        t_fmt(
                            L10nKey::SettingsDetectUrlsDesc,
                            &[("modifier", LINK_MODIFIER_LABEL)],
                        ),
                        link_switch,
                        cx,
                    )
                    .into_any_element(),
                    self.settings_row(
                        t(L10nKey::ForwardSshLoopbackLinks),
                        t(L10nKey::SettingsForwardSshLoopbackLinksDesc),
                        loopback_switch,
                        cx,
                    )
                    .into_any_element(),
                    self.settings_row(
                        t(L10nKey::OpenFilesWith),
                        t_fmt(
                            L10nKey::SettingsOpenFilesModeDesc,
                            &[("modifier", LINK_MODIFIER_LABEL)],
                        ),
                        link_file_open_radio,
                        cx,
                    )
                    .into_any_element(),
                ]
                .into_iter()
                .chain(link_file_command_row),
                cx,
            ),
        ])
    }

    fn render_prompt_group(&self, cx: &mut Context<Self>) -> AnyElement {
        let cfg = cx.global::<Config>();
        let prompt_editor = cfg.prompt_editor;
        let tab_completion = cfg.tab_completion;
        let history_search = cfg.history_search;
        let per_pane_history = cfg.per_pane_history;
        // Tab completion and history search are menus tty7 opens *inside* its
        // own prompt editor. With the editor off both keys belong to the
        // shell, so the switches grey out and say why; their stored values
        // are untouched and come back with it.
        let gated = |desc: L10nKey| match prompt_editor {
            true => t(desc).to_string(),
            false => format!("{} {}", t(desc), t(L10nKey::SettingsNeedsPromptEditor)),
        };
        let editor = self.settings_switch(
            "term-prompt-editor",
            prompt_editor,
            cx,
            |this, on, _, cx| this.set_prompt_editor(on, cx),
        );
        let completion = kit::switch("term-tab-completion")
            .checked(tab_completion)
            .disabled(!prompt_editor)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_tab_completion(*on, cx)))
            .into_any_element();
        let history = kit::switch("term-history-search")
            .checked(history_search)
            .disabled(!prompt_editor)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_history_search(*on, cx)))
            .into_any_element();
        let per_pane = self.settings_switch(
            "term-per-pane-history",
            per_pane_history,
            cx,
            |this, on, _, cx| this.set_per_pane_history(on, cx),
        );
        self.settings_group(
            Some(t(L10nKey::SettingsPrompt)),
            Some(t(L10nKey::SettingsPromptIntro).to_string()),
            [
                self.settings_row(
                    t(L10nKey::SettingsPromptEditor),
                    t(L10nKey::SettingsPromptEditorDesc),
                    editor,
                    cx,
                ),
                self.settings_row_gated_when(
                    t(L10nKey::SettingsTabCompletion),
                    gated(L10nKey::SettingsTabCompletionDesc),
                    completion,
                    !prompt_editor,
                    cx,
                ),
                self.settings_row_gated_when(
                    t(L10nKey::SettingsHistorySearch),
                    gated(L10nKey::SettingsHistorySearchDesc),
                    history,
                    !prompt_editor,
                    cx,
                ),
                self.settings_row(
                    t(L10nKey::SettingsPerPaneHistory),
                    t(L10nKey::SettingsPerPaneHistoryDescription),
                    per_pane,
                    cx,
                ),
            ]
            .map(IntoElement::into_any_element),
            cx,
        )
    }

    pub(crate) fn render_settings_input(&self, cx: &mut Context<Self>) -> AnyElement {
        let cfg = cx.global::<Config>();
        let mouse_hide = cfg.mouse_hide_while_typing;
        let focus_follows = cfg.focus_follows_mouse;
        let mouse_reporting = cfg.mouse_reporting;
        let mouse_zoom = cfg.mouse_zoom_modifier;
        let option_as_alt = cfg.macos_option_as_alt;
        let smart_select = cfg.smart_select;
        let copy_on_select = cfg.copy_on_select;
        let clip_trim = cfg.clipboard_trim_trailing_spaces;

        let focus = self.settings_switch(
            "term-focus-follows",
            focus_follows,
            cx,
            |this, on, _, cx| this.set_focus_follows_mouse(on, cx),
        );
        let hide = self.settings_switch("term-mouse-hide", mouse_hide, cx, |this, on, _, cx| {
            this.set_mouse_hide_while_typing(on, cx)
        });
        let report = self.settings_switch(
            "term-mouse-report",
            mouse_reporting,
            cx,
            |this, on, _, cx| this.set_mouse_reporting(on, cx),
        );
        let smart =
            self.settings_switch("term-smart-select", smart_select, cx, |this, on, _, cx| {
                this.set_smart_select(on, cx)
            });
        let copy = self.settings_switch(
            "term-copy-on-select",
            copy_on_select,
            cx,
            |this, on, _, cx| this.set_copy_on_select(on, cx),
        );
        let trim = self.settings_switch("term-clip-trim", clip_trim, cx, |this, on, _, cx| {
            this.set_clipboard_trim(on, cx)
        });
        // Ctrl only earns a cell where it is a different key from the platform
        // modifier: off macOS the two are the same key. A config that names
        // `ctrl` there still highlights it, in the one cell that means it.
        let mac = cfg!(target_os = "macos");
        let zoom_labels: Vec<&str> = if mac {
            vec!["⌘", "⌃", "⌥", t(L10nKey::SettingsMouseZoomOff)]
        } else {
            vec!["Ctrl", "Alt", t(L10nKey::SettingsMouseZoomOff)]
        };
        let zoom_idx = match (mouse_zoom, mac) {
            (MouseZoomModifier::Platform, _) => 0,
            (MouseZoomModifier::Ctrl, true) => 1,
            (MouseZoomModifier::Ctrl, false) => 0,
            (MouseZoomModifier::Alt, true) => 2,
            (MouseZoomModifier::Alt, false) => 1,
            (MouseZoomModifier::None, true) => 3,
            (MouseZoomModifier::None, false) => 2,
        };
        let zoom = self.segmented(
            "term-mouse-zoom",
            &zoom_labels,
            zoom_idx,
            cx,
            move |this, ix, _w, cx| {
                let modifier = match (ix, mac) {
                    (0, _) => MouseZoomModifier::Platform,
                    (1, true) => MouseZoomModifier::Ctrl,
                    (1, false) => MouseZoomModifier::Alt,
                    (2, true) => MouseZoomModifier::Alt,
                    _ => MouseZoomModifier::None,
                };
                this.set_mouse_zoom_modifier(modifier, cx);
            },
        );
        let shortcuts = self
            .settings_button(
                "open-keybindings",
                t(L10nKey::SettingsEditShortcuts),
                cx,
                |this, window, cx| {
                    this.navigate_settings(SettingsSection::Keybindings, None, window, cx)
                },
            )
            .into_any_element();
        let option_group = mac.then(|| {
            let control = self.settings_switch(
                "term-option-as-alt",
                option_as_alt,
                cx,
                |this, on, _, cx| this.set_macos_option_as_alt(on, cx),
            );
            self.settings_group(
                Some(t(L10nKey::SettingsKeyboard)),
                None,
                [self
                    .settings_row(
                        t(L10nKey::SettingsOptionAsMeta),
                        t(L10nKey::SettingsOptionAsMetaDesc),
                        control,
                        cx,
                    )
                    .into_any_element()],
                cx,
            )
        });

        let mut groups = vec![
            self.settings_group(
                None,
                None,
                [self
                    .settings_row(
                        t(L10nKey::SettingsSearchKeybindingsTitle),
                        t(L10nKey::SettingsKeybindingsIntroDesc),
                        shortcuts,
                        cx,
                    )
                    .into_any_element()],
                cx,
            ),
            self.settings_group(
                Some(t(L10nKey::SettingsSelectionClipboard)),
                None,
                [
                    self.settings_row(
                        t(L10nKey::SettingsSmartSelection),
                        t(L10nKey::SettingsSmartSelectionDesc),
                        smart,
                        cx,
                    ),
                    self.settings_row(
                        t(L10nKey::SettingsCopyOnSelect),
                        t(L10nKey::SettingsCopyOnSelectDesc),
                        copy,
                        cx,
                    ),
                    self.settings_row(
                        t(L10nKey::SettingsTrimTrailingSpaces),
                        t(L10nKey::SettingsTrimTrailingSpacesDesc),
                        trim,
                        cx,
                    ),
                ]
                .map(IntoElement::into_any_element),
                cx,
            ),
        ];
        groups.extend(option_group);
        groups.push(
            self.settings_group(
                Some(t(L10nKey::SettingsMouse)),
                None,
                [
                    self.settings_row(
                        t(L10nKey::SettingsFocusFollowsMouse),
                        t(L10nKey::SettingsFocusFollowsMouseDesc),
                        focus,
                        cx,
                    ),
                    self.settings_row(
                        t(L10nKey::SettingsHideMouseWhileTyping),
                        t(L10nKey::SettingsHideMouseWhileTypingDesc),
                        hide,
                        cx,
                    ),
                    self.settings_row(
                        t(L10nKey::SettingsReportMouseToApps),
                        t(L10nKey::SettingsReportMouseToAppsDesc),
                        report,
                        cx,
                    ),
                    self.settings_row(
                        t(L10nKey::SettingsMouseZoom),
                        t(L10nKey::SettingsMouseZoomDesc),
                        zoom,
                        cx,
                    ),
                ]
                .map(IntoElement::into_any_element),
                cx,
            ),
        );
        Self::settings_page(groups)
    }

    pub(crate) fn render_window_preferences(&self, cx: &mut Context<Self>) -> AnyElement {
        let cfg = cx.global::<Config>();
        let new_tab_idx = match cfg.new_tab_position {
            NewTabPosition::AfterCurrent => 0,
            NewTabPosition::End => 1,
        };
        let tab_bar_idx = match cfg.tab_bar_position {
            TabBarPosition::Top => 0,
            TabBarPosition::Left => 1,
        };
        let sidebar_diff_preview = cfg.sidebar_diff_preview;
        let grouping_idx = match cfg.sidebar_grouping {
            crate::core::config::SidebarGrouping::Repo => 0,
            crate::core::config::SidebarGrouping::RepoOrDirectory => 1,
            crate::core::config::SidebarGrouping::None => 2,
        };
        let new_tab = self.segmented(
            "wt-new-tab-pos",
            &[t(L10nKey::SettingsAfterCurrent), t(L10nKey::SettingsAtEnd)],
            new_tab_idx,
            cx,
            |this, ix, _w, cx| {
                this.set_new_tab_position(
                    if ix == 0 {
                        NewTabPosition::AfterCurrent
                    } else {
                        NewTabPosition::End
                    },
                    cx,
                );
            },
        );
        let tab_bar = self.segmented(
            "wt-tab-bar-pos",
            &[t(L10nKey::SettingsTop), t(L10nKey::SettingsLeft)],
            tab_bar_idx,
            cx,
            |this, ix, _w, cx| {
                this.set_tab_bar_position(
                    if ix == 0 {
                        TabBarPosition::Top
                    } else {
                        TabBarPosition::Left
                    },
                    cx,
                );
            },
        );
        let grouping = self.segmented(
            "wt-sidebar-grouping",
            &[
                t(L10nKey::SettingsByRepo),
                t(L10nKey::SettingsByRepoOrFolder),
                t(L10nKey::SettingsFlat),
            ],
            grouping_idx,
            cx,
            |this, ix, _w, cx| {
                let grouping = match ix {
                    0 => crate::core::config::SidebarGrouping::Repo,
                    1 => crate::core::config::SidebarGrouping::RepoOrDirectory,
                    _ => crate::core::config::SidebarGrouping::None,
                };
                this.set_sidebar_grouping(grouping, cx);
            },
        );
        let diff = self.settings_switch(
            "wt-sidebar-diff-preview",
            sidebar_diff_preview,
            cx,
            |this, on, _, cx| this.set_sidebar_diff_preview(on, cx),
        );
        Self::settings_page([self.settings_group(
            Some(t(L10nKey::SettingsTabs)),
            None,
            [
                self.settings_row(
                    t(L10nKey::SettingsNewTabPosition),
                    t(L10nKey::SettingsNewTabPositionDesc),
                    new_tab,
                    cx,
                ),
                self.settings_row(
                    t(L10nKey::SettingsTabBarPosition),
                    t(L10nKey::SettingsTabBarPositionDesc),
                    tab_bar,
                    cx,
                ),
                self.settings_row(
                    t(L10nKey::SettingsSidebarGrouping),
                    t(L10nKey::SettingsSidebarGroupingDesc),
                    grouping,
                    cx,
                ),
                self.settings_row(
                    t(L10nKey::SettingsDiffPreviewFromCounts),
                    t(L10nKey::SettingsDiffPreviewFromCountsDesc),
                    diff,
                    cx,
                ),
            ]
            .map(IntoElement::into_any_element),
            cx,
        )])
    }

    pub(crate) fn render_settings_about(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::core::update::UpdatePhase;
        let tk = Tk::of(cx);
        let status = cx
            .try_global::<crate::core::update::UpdateStatus>()
            .cloned()
            .unwrap_or_default();
        let busy = matches!(
            status.phase,
            UpdatePhase::Checking
                | UpdatePhase::Downloading { .. }
                | UpdatePhase::Verifying
                | UpdatePhase::Installing
        );
        let transferring = matches!(
            status.phase,
            UpdatePhase::Downloading { .. } | UpdatePhase::Verifying
        );
        // A staged package whose directory has since been swept away is not an
        // offer worth making.
        let ready = status
            .ready
            .clone()
            .filter(crate::core::update::PendingUpdate::is_usable);
        // "You're running the latest version" above "27.0.0 is ready to
        // install" is a contradiction, and a reachable one; the staged package
        // is the more useful claim.
        let phase_text = localized_update_phase(&status.phase)
            .filter(|_| ready.is_none() || !matches!(status.phase, UpdatePhase::UpToDate));
        let version_desc = match &phase_text {
            Some(text) => format!("{} · {text}", env!("CARGO_PKG_VERSION")),
            None => env!("CARGO_PKG_VERSION").to_string(),
        };

        let check = h_flex()
            .gap(px(8.))
            .when(transferring, |r| {
                r.child(
                    kit::button(
                        "cancel-update-download",
                        t(L10nKey::SettingsUpdateCancel),
                        BtnKind::Link,
                    )
                    .on_click(cx.listener(|_, _, _w, cx| crate::core::update::cancel_download(cx))),
                )
            })
            .child(
                kit::button(
                    "check-update-now",
                    if matches!(status.phase, UpdatePhase::Checking) {
                        t(L10nKey::SettingsUpdateChecking)
                    } else {
                        t(L10nKey::SettingsUpdateCheckNow)
                    },
                    BtnKind::Secondary,
                )
                .disabled(busy)
                .on_click(cx.listener(|_, _, _w, cx| crate::core::update::spawn_check_forced(cx))),
            )
            .into_any_element();
        let mut rows = vec![
            self.settings_row(t(L10nKey::SettingsVersion), version_desc, check, cx)
                .into_any_element(),
        ];
        // A failure the user can act on. Persisted, so it is still here
        // tomorrow.
        if let Some(failure) = status.failure.clone() {
            let control = h_flex()
                .gap(px(12.))
                .child(
                    kit::button(
                        "update-dismiss",
                        t(L10nKey::SettingsUpdateDismiss),
                        BtnKind::Link,
                    )
                    .on_click(cx.listener(|_, _, _w, cx| crate::core::update::dismiss_failure(cx))),
                )
                .child(
                    kit::button(
                        "update-manual",
                        t(L10nKey::SettingsUpdateDownloadManually),
                        BtnKind::Link,
                    )
                    .on_click(
                        cx.listener(|_, _, _w, _cx| crate::core::update::open_releases_page()),
                    ),
                )
                .child(
                    kit::button(
                        "update-retry",
                        t(L10nKey::SettingsUpdateRetry),
                        BtnKind::Secondary,
                    )
                    .disabled(busy)
                    .on_click(cx.listener(|_, _, _w, cx| {
                        crate::core::update::dismiss_failure(cx);
                        crate::core::update::install_available(cx);
                    })),
                )
                .into_any_element();
            rows.push(
                self.settings_row(
                    t_fmt(
                        L10nKey::SettingsUpdateFailedTitle,
                        &[("version", &failure.version)],
                    ),
                    failure.detail.clone(),
                    control,
                    cx,
                )
                .into_any_element(),
            );
        }
        // Downloaded and verified: the decision left is "may I restart".
        if let Some(pending) = ready.clone() {
            let control = h_flex()
                .gap(px(12.))
                .child(
                    kit::button(
                        "discard-ready",
                        t(L10nKey::SettingsUpdateDiscard),
                        BtnKind::Link,
                    )
                    .disabled(busy)
                    .on_click(cx.listener(|_, _, _w, cx| crate::core::update::discard_pending(cx))),
                )
                .child(
                    kit::button(
                        "install-ready",
                        t(L10nKey::SettingsUpdateInstallNow),
                        BtnKind::Primary,
                    )
                    .disabled(busy)
                    .on_click(
                        cx.listener(|_, _, _w, cx| crate::core::update::install_available(cx)),
                    ),
                )
                .into_any_element();
            rows.push(
                self.settings_row(
                    t_fmt(
                        L10nKey::SettingsUpdateReady,
                        &[("version", &pending.version)],
                    ),
                    if pending.apply_on_launch {
                        t(L10nKey::SettingsUpdateReadyNextLaunch).to_string()
                    } else {
                        String::new()
                    },
                    control,
                    cx,
                )
                .into_any_element(),
            );
        }
        // Where the package cannot be installed for the user, the release page
        // is the whole update path; that cannot live only in a dialog that has
        // already been dismissed.
        if let Some(upd) = status.available.clone().filter(|_| ready.is_none()) {
            let action = if upd.installable {
                t(L10nKey::SettingsUpdateAndRelaunch)
            } else {
                t(L10nKey::SettingsUpdateViewRelease)
            };
            let control = kit::button("install-update", action, BtnKind::Primary)
                .disabled(busy)
                .on_click(cx.listener(|_, _, _w, cx| crate::core::update::install_available(cx)))
                .into_any_element();
            rows.push(
                self.settings_row(
                    t_fmt(
                        L10nKey::SettingsVersionAvailable,
                        &[("version", &upd.version)],
                    ),
                    upd.install_hint
                        .map(|hint| localized_update_install_hint(&hint))
                        .unwrap_or_default(),
                    control,
                    cx,
                )
                .into_any_element(),
            );
        }
        let settings_file = crate::core::config::config_path("config.json");
        if let Some(path) = settings_file {
            let shown = tildify(&path.to_string_lossy());
            let reveal = self
                .settings_button(
                    "reveal-settings-file",
                    t(L10nKey::SettingsReveal),
                    cx,
                    move |_, _, cx| {
                        if !path.exists() {
                            cx.global::<Config>().save();
                        }
                        cx.reveal_path(&path);
                    },
                )
                .into_any_element();
            rows.push(
                self.settings_row(t(L10nKey::SettingsSettingsFile), shown, reveal, cx)
                    .into_any_element(),
            );
        }

        let mut groups = vec![self.settings_group(None, None, rows, cx)];
        groups.push(self.render_settings_maintenance(cx));
        groups.push(
            v_flex()
                .gap(px(6.))
                .text_size(fs(12.))
                .line_height(px(17.))
                .text_color(tk.k45)
                .child(t(L10nKey::SettingsAboutDesc1))
                .child(
                    Link::new("about-github")
                        .href("https://github.com/l0ng-ai/tty7")
                        .text_color(tk.k6)
                        .child("github.com/l0ng-ai/tty7"),
                )
                .into_any_element(),
        );
        Self::settings_page(groups)
    }

    fn render_settings_maintenance(&self, cx: &mut Context<Self>) -> AnyElement {
        let stale_daemon = crate::daemon::spawn::local_daemon_stale_build();
        // Whether picking up the new build costs the user their running panes
        // decides what this offer is, so it decides what it says.
        let stale_daemon_note = if crate::daemon::spawn::local_daemon_supports(
            crate::daemon::protocol::FEATURE_HANDOFF,
        ) {
            L10nKey::SettingsDaemonStaleDescInPlace
        } else {
            L10nKey::SettingsDaemonStaleDesc
        };
        let cfg = cx.global::<Config>();
        let check_for_updates = cfg.check_for_updates;
        let auto_download = cfg.auto_download_updates;
        let channel_idx = match cfg.update_channel {
            UpdateChannel::Stable => 0,
            UpdateChannel::Nightly => 1,
        };
        let channel = self.segmented(
            "wt-update-channel",
            &[
                t(L10nKey::SettingsUpdateChannelStable),
                t(L10nKey::SettingsUpdateChannelNightly),
            ],
            channel_idx,
            cx,
            |this, ix, _w, cx| {
                this.set_update_channel(
                    match ix {
                        0 => UpdateChannel::Stable,
                        _ => UpdateChannel::Nightly,
                    },
                    cx,
                );
            },
        );
        let Some(http_proxy_input) = self.active_settings().map(|s| s.http_proxy_input.clone())
        else {
            return div().into_any_element();
        };
        // Only flags a committed value: the input commits on Enter/blur.
        let http_proxy_value = http_proxy_input.read(cx).value().trim().to_string();
        let http_proxy_error = (!http_proxy_value.is_empty()
            && !tty7_core::daemon::install::proxy::is_valid_manual(&http_proxy_value))
        .then(|| t(L10nKey::SettingsAppHttpProxyInvalid).to_string());
        let http_proxy =
            self.settings_checked_input(&http_proxy_input, FIELD_W, http_proxy_error, cx);
        let check =
            self.settings_switch("check-updates", check_for_updates, cx, |this, on, _, cx| {
                this.set_check_for_updates(on, cx)
            });
        let auto = self.settings_switch(
            "auto-download-updates",
            auto_download,
            cx,
            |this, on, _, cx| this.set_auto_download_updates(on, cx),
        );
        let restart = self
            .settings_button(
                "restart-daemon",
                t(L10nKey::SettingsRestartServer),
                cx,
                |this, window, cx| this.restart_daemon(window, cx),
            )
            .into_any_element();
        // A stale server has a more specific thing to say than the row's
        // standing description, and it ends with the same button.
        let server_desc = match stale_daemon.as_deref() {
            Some(build) => format!(
                "{} {}",
                t_fmt(L10nKey::SettingsDaemonStale, &[("build", build)]),
                t(stale_daemon_note)
            ),
            None => t(L10nKey::SettingsServerDesc).to_string(),
        };
        self.settings_group(
            Some(t(L10nKey::SettingsUpdates)),
            None,
            [
                self.settings_row(
                    t(L10nKey::SettingsAutoDownload),
                    t(L10nKey::SettingsAutoDownloadDesc),
                    auto,
                    cx,
                ),
                self.settings_row(
                    t(L10nKey::SettingsCheckUpdatesOnLaunch),
                    t(L10nKey::SettingsCheckUpdatesDesc),
                    check,
                    cx,
                ),
                self.settings_row(
                    t(L10nKey::SettingsUpdateChannel),
                    t(L10nKey::SettingsUpdateChannelDesc),
                    channel,
                    cx,
                ),
                self.settings_row(
                    t(L10nKey::SettingsAppHttpProxy),
                    t(L10nKey::SettingsAppHttpProxyDesc),
                    http_proxy,
                    cx,
                ),
                self.settings_row(t(L10nKey::SettingsServer), server_desc, restart, cx),
            ]
            .map(IntoElement::into_any_element),
            cx,
        )
    }
}

//! The Settings tab UI (Cmd+,): a sidebar of sections beside a scrollable
//! content pane. This module owns the panel's *state types* and its *rendering*
//! only; the lifecycle (opening/closing the tab, committing the font family,
//! applying theme/font changes) lives in `app.rs`, where it can touch the
//! shell's tabs and panes. The render methods extend `Tty7App` from here so the
//! window shell stays focused on tab/pane orchestration.

use gpui::{
    AnyElement, Context, Div, Entity, FontWeight, Image, ImageFormat, KeyDownEvent, SharedString,
    Stateful, Subscription, Window, div, img, prelude::*, px, rgb,
};
use gpui_component::Selectable as _;
use gpui_component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_component::color_picker::{ColorPicker, ColorPickerState};
use gpui_component::input::{Input, InputState};
use gpui_component::select::{SearchableVec, Select, SelectState};
use gpui_component::sidebar::{Sidebar, SidebarCollapsible, SidebarMenu, SidebarMenuItem};
use gpui_component::slider::{Slider, SliderState};
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use std::sync::Arc;

use crate::core::config::{Config, CursorStyle, NewTabPosition, NotifyMode};
use crate::ui::app::{FONT_SIZE_STEP, LINE_HEIGHT_STEP, ThemeEdit, Tty7App};
use crate::ui::keymap::default_bindings;
use crate::ui::presets;

/// Which section of the settings panel is currently selected in the sidebar.
/// Sections are named for the *object* being configured (the appearance, the
/// terminal, the shell, the window) — never for a property class like
/// "Behavior", which reads fine but predicts nothing about what's inside.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    Appearance,
    Terminal,
    Shell,
    WindowTabs,
    Keybindings,
    About,
}

impl SettingsSection {
    /// A `&'static` label for `TTY7_PROFILE` aggregation, so each section's build
    /// cost and rebuild rate report under their own line.
    fn profile_label(self) -> &'static str {
        match self {
            SettingsSection::Appearance => "settings:appearance",
            SettingsSection::Terminal => "settings:terminal",
            SettingsSection::Shell => "settings:shell",
            SettingsSection::WindowTabs => "settings:window-tabs",
            SettingsSection::Keybindings => "settings:keybindings",
            SettingsSection::About => "settings:about",
        }
    }
}

/// The in-app color editor for the active *editable* theme: one color picker per
/// seed color (background/foreground/accent/cursor/selection) and per ANSI slot,
/// each wired to write its change straight back to the theme's YAML file. Rebuilt
/// by `Tty7App::rebuild_theme_editor` whenever the active theme changes, so it
/// always targets (and reflects) the theme on screen.
pub(crate) struct ThemeEditor {
    /// The id the pickers were built for (which theme they edit).
    #[allow(dead_code)]
    pub(crate) for_id: String,
    /// Seed-color pickers: `(edit target, row label, picker state)`.
    pub(crate) seed: Vec<(ThemeEdit, String, Entity<ColorPickerState>)>,
    /// One picker per ANSI slot 0–15.
    pub(crate) ansi: Vec<(ThemeEdit, String, Entity<ColorPickerState>)>,
    pub(crate) _subs: Vec<Subscription>,
}

/// Live state for the settings panel (Cmd+,). Holds the panel's focus owner
/// (so Esc closes it), the currently selected sidebar section, and the
/// font-family text input plus its commit subscriptions.
pub(crate) struct SettingsState {
    pub(crate) focus_handle: gpui::FocusHandle,
    pub(crate) section: SettingsSection,
    pub(crate) font_select: Entity<SelectState<SearchableVec<String>>>,
    /// Bold / italic face pickers. Their first row is the `FONT_DEFAULT_LABEL`
    /// sentinel, meaning "reuse the primary face with synthesized emphasis".
    pub(crate) font_bold_select: Entity<SelectState<SearchableVec<String>>>,
    pub(crate) font_italic_select: Entity<SelectState<SearchableVec<String>>>,
    /// Shell program override (empty = the platform default shell).
    pub(crate) shell_program_input: Entity<InputState>,
    /// Shell launch arguments, space-separated (e.g. `-l`).
    pub(crate) shell_args_input: Entity<InputState>,
    /// Custom working-directory path (used when the strategy is `Custom`).
    pub(crate) wd_path_input: Entity<InputState>,
    /// Mouse-scroll multiplier slider (Terminal section).
    pub(crate) scroll_slider: Entity<SliderState>,
    /// The color editor for the active editable theme, or `None` when the active
    /// theme is read-only (a built-in / import) or the system is being followed.
    pub(crate) theme_editor: Option<ThemeEditor>,
    pub(crate) _subs: Vec<Subscription>,
}

/// Sentinel first row in the bold/italic font pickers meaning "no distinct face
/// — reuse the primary family with synthesized emphasis". Chosen to be an
/// unlikely real font name.
pub(crate) const FONT_DEFAULT_LABEL: &str = "Default (match primary)";

/// Humanize a CamelCase action name for display: "CloseActiveTab" → "Close
/// Active Tab".
fn humanize_action(action: &str) -> String {
    let mut out = String::new();
    for (i, ch) in action.chars().enumerate() {
        if i > 0 && ch.is_uppercase() {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

impl Tty7App {
    /// Build the settings tab body: a fixed left sidebar (section nav) beside a
    /// scrollable content area for the selected section. Esc closes the tab.
    pub(crate) fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (background, foreground) = (theme.background, theme.foreground);

        let (focus_handle, section) = match self.active_settings() {
            Some(s) => (s.focus_handle.clone(), s.section),
            None => return div(), // not a settings tab; nothing to render
        };

        // `TTY7_PROFILE`: time this section's whole element build and, via the
        // aggregated call rate, expose whether the panel is rebuilding once (on a
        // real change) or in a tight `notify` loop. Labelled per section so
        // Appearance's cost stands apart from the lighter pages.
        let prof = crate::ui::perf::enabled()
            .then(|| (std::time::Instant::now(), section.profile_label()));

        // Sidebar nav item that activates a section on click.
        let nav_item = |label: &'static str, target: SettingsSection, icon: IconName| {
            let view = cx.entity();
            SidebarMenuItem::new(label)
                .icon(Icon::new(icon))
                .active(section == target)
                .on_click(move |_, _window, cx| {
                    view.update(cx, |this, cx| this.select_settings_section(target, cx));
                })
        };

        let sidebar = Sidebar::new("settings-sidebar")
            .collapsible(SidebarCollapsible::None)
            // Narrower than the stock 255px — three short items don't need that
            // much column, and a tighter rail reads more native/less hollow.
            .w(px(212.))
            .header(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.muted_foreground)
                    .child("SETTINGS"),
            )
            .child(
                SidebarMenu::new()
                    .child(nav_item(
                        "Appearance",
                        SettingsSection::Appearance,
                        IconName::Palette,
                    ))
                    // Sliders for Terminal (it's the tuning page), the `>_`
                    // prompt glyph for Shell (it configures the prompt's
                    // program) — the two would otherwise both claim `>_`.
                    .child(nav_item(
                        "Terminal",
                        SettingsSection::Terminal,
                        IconName::Settings2,
                    ))
                    .child(nav_item(
                        "Shell",
                        SettingsSection::Shell,
                        IconName::SquareTerminal,
                    ))
                    .child(nav_item(
                        "Window & Tabs",
                        SettingsSection::WindowTabs,
                        IconName::WindowRestore,
                    ))
                    // The icon set ships no keyboard glyph; CaseSensitive ("Aa")
                    // is the closest key-ish cue available.
                    .child(nav_item(
                        "Keybindings",
                        SettingsSection::Keybindings,
                        IconName::CaseSensitive,
                    ))
                    .child(nav_item("About", SettingsSection::About, IconName::Info)),
            );

        let content = match section {
            SettingsSection::Appearance => self.render_settings_appearance(cx),
            SettingsSection::Terminal => self.render_settings_terminal(cx),
            SettingsSection::Shell => self.render_settings_shell(cx),
            SettingsSection::WindowTabs => self.render_settings_window_tabs(cx),
            SettingsSection::Keybindings => self.render_settings_keybindings(cx),
            SettingsSection::About => self.render_settings_about(cx),
        };

        // One continuous, flat sheet (no cards) — one document: bold section
        // headers and full-width rules carry the structure, so settings read as a
        // unified document rather than a widget floating in empty space.
        let content_pane = v_flex()
            .id("settings-content")
            .flex_1()
            .h_full()
            .bg(background)
            .overflow_y_scroll()
            .child(
                div()
                    .px_10()
                    .py_8()
                    // Fill the pane edge-to-edge; cap only on very wide windows so
                    // rows never stretch to an unreadable width.
                    .child(div().w_full().max_w(px(860.)).child(content)),
            );

        let root = div()
            .size_full()
            .flex()
            .flex_row()
            .bg(background)
            .text_color(foreground)
            .track_focus(&focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key.as_str() == "escape" {
                    this.close_settings(window, cx);
                }
            }))
            // The Sidebar draws its own right border; no wrapper border here, or
            // the two stack into one thick rule.
            .child(sidebar)
            .child(content_pane);

        if let Some((start, label)) = prof {
            crate::ui::perf::record(label, start.elapsed());
        }
        root
    }

    /// Just the styled section title (no margin). Shared by `section_header` and
    /// `section_intro` so the two can never drift in size, weight, or color.
    fn header_text(&self, title: &str, cx: &Context<Self>) -> Div {
        div()
            .text_base()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().foreground)
            .child(title.to_string())
    }

    /// A bold section header that introduces a group of settings.
    /// With no cards, the header *is* the unit of grouping — it tells the eye
    /// where one set of related controls begins.
    fn section_header(&self, title: &str, cx: &Context<Self>) -> Div {
        self.header_text(title, cx).mb_4()
    }

    /// A section header paired with its one-line intro as a single unit: the
    /// subtitle sits tight under the title (`gap_1`) and the block leaves a
    /// consistent gap before the first control (`mb_4`). Replaces the ad-hoc
    /// "header, then a loose paragraph" pattern that stranded the subtitle 16px
    /// below its own title (glued instead to the controls) and used a different
    /// bottom margin — `mb_1` here, `mb_2` there — in every section.
    fn section_intro(&self, title: &str, desc: impl Into<String>, cx: &Context<Self>) -> Div {
        v_flex()
            .mb_4()
            .gap_1()
            .child(self.header_text(title, cx))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(desc.into()),
            )
    }

    /// A full-width hairline between sections, so the page reads as one
    /// continuous sheet rather than stacked boxes.
    fn section_rule(&self, cx: &Context<Self>) -> Div {
        div().h(px(1.)).my_7().bg(cx.theme().border)
    }

    /// One labelled settings row, shared by every section: title + description
    /// in a fixed-width left column, control immediately beside it. A fixed
    /// column (not space-between) keeps label and control visually paired
    /// regardless of window width — space-between on a wide pane stretched the
    /// two apart into a dead gap.
    fn settings_row(
        &self,
        label: impl Into<String>,
        desc: impl Into<String>,
        control: AnyElement,
        cx: &Context<Self>,
    ) -> Div {
        let theme = cx.theme();
        h_flex()
            .items_center()
            .gap_8()
            .py_2()
            .child(
                v_flex()
                    .gap_0p5()
                    .w(px(260.))
                    .flex_shrink_0()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.foreground)
                            .child(label.into()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(desc.into()),
                    ),
            )
            .child(control)
    }

    /// A segmented control (gpui-component's `ButtonGroup`, outline) for a small
    /// set of mutually-exclusive options — the refined stand-in for a raw row of
    /// radio circles, which read as an unstyled form beside the sheet's tuned
    /// steppers and chips. Joined outline segments with a soft-filled active one
    /// speak the same segmented language as the −│value│+ stepper; `small` pins
    /// every option control to the same 24px height as the selects beside them.
    /// `selected` is the active index; `on_pick` fires with the newly chosen one.
    fn segmented(
        &self,
        id: &'static str,
        options: &'static [&'static str],
        selected: usize,
        cx: &mut Context<Self>,
        on_pick: impl Fn(&mut Self, usize, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        ButtonGroup::new(id)
            .outline()
            .small()
            .children(options.iter().enumerate().map(|(i, label)| {
                // `(id, i)` keeps each segment's element id unique across the
                // several segmented controls on the page.
                Button::new((id, i)).label(*label).selected(i == selected)
            }))
            .on_click(cx.listener(move |this, clicks: &Vec<usize>, window, cx| {
                // Single-select: `clicks` carries just the newly chosen index.
                if let Some(&ix) = clicks.first() {
                    on_pick(this, ix, window, cx);
                }
            }))
            .into_any_element()
    }

    /// Appearance section: theme, font size, font family.
    fn render_settings_appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let foreground = theme.foreground;
        let border = theme.border;
        let hover_bg = theme.secondary.opacity(0.6);
        let stepper_bg = theme.secondary.opacity(0.35);
        let font_size = self.font_size;
        let (font_select, font_bold_select, font_italic_select) = match self.active_settings() {
            Some(s) => (
                s.font_select.clone(),
                s.font_bold_select.clone(),
                s.font_italic_select.clone(),
            ),
            None => return div().into_any_element(),
        };
        let cfg = cx.global::<Config>();
        let cursor_style = cfg.cursor_style;
        let cursor_blink = cfg.cursor_blink;
        let font_ligatures = cfg.font_features.as_ref().is_some_and(|features| {
            features.is_calt_enabled() == Some(true)
                || features
                    .tag_value_list()
                    .iter()
                    .any(|(tag, value)| tag == "liga" && *value != 0)
        });

        // Unified −/value/+ stepper plus a quiet Reset.
        let step = move |id: &'static str, glyph: &'static str, divider: bool| {
            div()
                .id(id)
                .px_2p5()
                .py_1()
                .text_sm()
                .cursor_pointer()
                .text_color(foreground)
                .when(divider, |s| s.border_l_1().border_color(border))
                .hover(|h| h.bg(hover_bg))
                .child(glyph)
        };
        // One shared height for every small control in this section (matches
        // gpui-component's own Size::Small button height) so the stepper pill
        // and the font-family select sit at the same visual weight instead of
        // each defaulting to its own padding.
        let control_h = px(24.);
        // The −│value│+ pill plus its quiet Reset — one shape shared by the
        // font-size and line-height rows; callers hand in the wired buttons.
        let stepper_row =
            move |dec: Stateful<Div>, value: String, inc: Stateful<Div>, reset: Button| {
                h_flex()
                    .items_center()
                    .justify_start()
                    .w(px(240.))
                    .gap_3()
                    .child(
                        h_flex()
                            .items_center()
                            .h(control_h)
                            .rounded_lg()
                            .bg(stepper_bg)
                            .border_1()
                            .border_color(border)
                            .overflow_hidden()
                            .child(dec)
                            .child(
                                div()
                                    .min_w(px(40.))
                                    // Hairline on the value's left edge so both internal
                                    // seams read (−│value│+); the `+` supplies the right one.
                                    .border_l_1()
                                    .border_color(border)
                                    .py_1()
                                    .text_center()
                                    .text_sm()
                                    .text_color(foreground)
                                    .child(value),
                            )
                            .child(inc),
                    )
                    .child(reset)
                    .into_any_element()
            };
        let font_size_control = stepper_row(
            step("font-dec", "−", false).on_click(
                cx.listener(|this, _, _w, cx| this.change_font_size(-FONT_SIZE_STEP, cx)),
            ),
            format!("{:.0}", font_size),
            step("font-inc", "+", true)
                .on_click(cx.listener(|this, _, _w, cx| this.change_font_size(FONT_SIZE_STEP, cx))),
            Button::new("font-reset")
                .label("Reset")
                .ghost()
                .small()
                .on_click(cx.listener(|this, _, _w, cx| this.reset_font_size(cx))),
        );

        let line_height = self.line_height;
        let line_height_control = stepper_row(
            step("lh-dec", "−", false).on_click(
                cx.listener(|this, _, _w, cx| this.change_line_height(-LINE_HEIGHT_STEP, cx)),
            ),
            format!("{:.2}", line_height),
            step("lh-inc", "+", true).on_click(
                cx.listener(|this, _, _w, cx| this.change_line_height(LINE_HEIGHT_STEP, cx)),
            ),
            Button::new("lh-reset")
                .label("Reset")
                .ghost()
                .small()
                .on_click(cx.listener(|this, _, _w, cx| this.reset_line_height(cx))),
        );

        // One font dropdown, shared shape for primary / bold / italic pickers.
        let font_dropdown = |state: &Entity<SelectState<SearchableVec<String>>>| {
            h_flex()
                .justify_start()
                .w(px(240.))
                .child(
                    Select::new(state)
                        .small()
                        .w(px(180.))
                        .h(control_h)
                        .search_placeholder("Search fonts…")
                        // Cap the popup's own height so browsing doesn't dump the
                        // OS's entire font catalog on screen at once — it just
                        // scrolls from here. Every font is still in the list and
                        // reachable by typing; this only trims what's shown.
                        .menu_max_h(px(224.)),
                )
                .into_any_element()
        };
        let font_family_control = font_dropdown(&font_select);
        let font_bold_control = font_dropdown(&font_bold_select);
        let font_italic_control = font_dropdown(&font_italic_select);
        let ligature_switch = Switch::new("font-ligatures")
            .checked(font_ligatures)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_font_ligatures(*on, cx)))
            .into_any_element();

        let cursor_idx = match cursor_style {
            CursorStyle::Block => 0,
            CursorStyle::Bar => 1,
            CursorStyle::Underline => 2,
        };
        let cursor_style_control = self.segmented(
            "cursor-style",
            &["Block", "Bar", "Underline"],
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
        // Blink lives here beside the shape — one Cursor home, not "shape is
        // appearance, blink is behavior" split across two pages.
        let blink_switch = Switch::new("cursor-blink")
            .checked(cursor_blink)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_cursor_blink(*on, cx)))
            .into_any_element();

        v_flex()
            .child(self.section_intro(
                "Theme",
                "Pick a color theme. Each one sets its own light or dark look.",
                cx,
            ))
            .child(self.render_theme_picker(cx))
            .child(self.section_rule(cx))
            .child(self.section_header("Typography", cx))
            .child(self.settings_row(
                "Font Size",
                "Terminal text size in pixels.",
                font_size_control,
                cx,
            ))
            .child(self.settings_row(
                "Line Height",
                "Row spacing as a multiple of the font size.",
                line_height_control,
                cx,
            ))
            .child(self.settings_row(
                "Font Family",
                "Pick from fonts installed on your system.",
                font_family_control,
                cx,
            ))
            .child(self.settings_row(
                "Bold Font",
                "Face for bold text; Default synthesizes it from the primary.",
                font_bold_control,
                cx,
            ))
            .child(self.settings_row(
                "Italic Font",
                "Face for italic text; Default synthesizes it from the primary.",
                font_italic_control,
                cx,
            ))
            .child(self.settings_row(
                "Font ligatures",
                "Enable common programming ligature features for terminal text.",
                ligature_switch,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header("Cursor", cx))
            .child(self.settings_row(
                "Cursor shape",
                "How the terminal cursor is drawn.",
                cursor_style_control,
                cx,
            ))
            .child(self.settings_row(
                "Blink cursor",
                "Pulse the cursor while the terminal is focused.",
                blink_switch,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.render_custom_themes(cx))
            .into_any_element()
    }

    /// Custom themes section. On an editable theme, the color editor; on a
    /// read-only built-in / import, a "Duplicate to edit" button that forks it
    /// into an editable file. The folder button is always available.
    fn render_custom_themes(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor = self.active_settings().and_then(|s| s.theme_editor.as_ref());

        let folder_button = Button::new("open-themes-folder")
            .label("Open Themes Folder")
            .small()
            .on_click(cx.listener(|this, _, _w, cx| this.open_themes_folder(cx)));

        if let Some(editor) = editor {
            // Snapshot the picker handles so the render borrow of `self` ends.
            let seed: Vec<_> = editor
                .seed
                .iter()
                .map(|(_, label, state)| (label.clone(), state.clone()))
                .collect();
            let ansi: Vec<_> = editor
                .ansi
                .iter()
                .map(|(_, label, state)| (label.clone(), state.clone()))
                .collect();
            return v_flex()
                .child(self.section_intro(
                    "Edit theme",
                    "You're editing a copy. Changes save to its file in the themes \
                     folder and apply live.",
                    cx,
                ))
                .children(
                    seed.into_iter()
                        .map(|(label, state)| self.render_theme_color_row(label, state, cx)),
                )
                .child(self.section_header("ANSI colors", cx))
                .children(
                    ansi.into_iter()
                        .map(|(label, state)| self.render_theme_color_row(label, state, cx)),
                )
                .child(h_flex().mt_4().child(folder_button))
                .into_any_element();
        }

        // Read-only theme (built-in or import): offer to duplicate it into an
        // editable copy, plus the folder affordance.
        v_flex()
            .child(self.section_intro(
                "Custom themes",
                "Duplicate a theme to edit its colors here, or drop your own in the \
                 themes folder: a tty7 YAML theme or an iTerm2 .itermcolors scheme.",
                cx,
            ))
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        Button::new("duplicate-theme")
                            .label("Duplicate to Edit")
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.fork_active_theme(window, cx)
                            })),
                    )
                    .child(folder_button),
            )
            .into_any_element()
    }

    /// One color-editor row: a label paired with its picker. The picker's own
    /// `Change` event (wired in `rebuild_theme_editor`) writes the edit to the
    /// theme file, so the row itself is purely presentational.
    fn render_theme_color_row(
        &self,
        label: String,
        state: Entity<ColorPickerState>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let control = h_flex()
            .items_center()
            .w(px(240.))
            .child(ColorPicker::new(&state).small())
            .into_any_element();
        self.settings_row(label, "", control, cx)
    }

    /// Shell section: the program tty7 launches in each new terminal, plus its
    /// launch arguments. Both apply to *newly spawned* panes/tabs — existing
    /// shells keep running until closed. An empty program falls back to the
    /// platform default (the login shell on Unix; PowerShell 7 when installed,
    /// else Windows PowerShell, on Windows).
    fn render_settings_shell(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted_fg = cx.theme().muted_foreground;
        let (program_input, args_input, wd_path_input) = match self.active_settings() {
            Some(s) => (
                s.shell_program_input.clone(),
                s.shell_args_input.clone(),
                s.wd_path_input.clone(),
            ),
            None => return div().into_any_element(),
        };
        let wd_strategy = cx.global::<Config>().working_directory.strategy;

        // Name what an empty Program field falls back to, so the default
        // behaviour is legible without the user having to know it.
        let platform_default = if cfg!(windows) {
            "PowerShell"
        } else {
            "your login shell"
        };

        let program_control = div()
            .w(px(260.))
            .child(Input::new(&program_input).small())
            .into_any_element();
        let args_control = div()
            .w(px(260.))
            .child(Input::new(&args_input).small())
            .into_any_element();

        use crate::core::config::WdStrategy;
        let wd_idx = match wd_strategy {
            WdStrategy::Inherit => 0,
            WdStrategy::Home => 1,
            WdStrategy::Custom => 2,
        };
        let wd_radio = self.segmented(
            "wd-strategy",
            &["Inherit", "Home", "Custom"],
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
        // The custom path input only matters for `Custom`; show it there.
        let wd_path_control = if wd_strategy == WdStrategy::Custom {
            div()
                .w(px(260.))
                .child(Input::new(&wd_path_input).small())
                .into_any_element()
        } else {
            div().into_any_element()
        };

        v_flex()
            .child(self.section_intro(
                "Shell",
                format!(
                    "The program each new terminal launches. Leave Program empty to use the platform default ({platform_default})."
                ),
                cx,
            ))
            .child(self.settings_row(
                "Program",
                "Executable name on PATH or an absolute path. e.g. zsh, fish, pwsh.",
                program_control,
                cx,
            ))
            .child(self.settings_row(
                "Arguments",
                "Space-separated launch flags. e.g. -l for a login shell.",
                args_control,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header("Working directory", cx))
            .child(self.settings_row(
                "Start in",
                "Inherit the launch dir, your home, or a fixed path.",
                wd_radio,
                cx,
            ))
            .when(wd_strategy == crate::core::config::WdStrategy::Custom, |v| {
                v.child(self.settings_row(
                    "Custom path",
                    "The directory new shells start in.",
                    wd_path_control,
                    cx,
                ))
            })
            .child(
                div()
                    .mt_3()
                    .text_xs()
                    .text_color(muted_fg)
                    .child("Applies to new tabs and panes; shells already open keep running. A new tab still inherits the active pane's directory."),
            )
            .into_any_element()
    }

    /// Terminal section: how the terminal surface itself behaves — scrolling,
    /// mouse, links, clipboard, notifications. Plain switches and segmented
    /// controls driven straight off the `Config` global (each control's handler
    /// mutates + saves it). Small groups on purpose: each header names exactly
    /// what it contains, so it doubles as the landmark you scan for.
    fn render_settings_terminal(&self, cx: &mut Context<Self>) -> AnyElement {
        let foreground = cx.theme().foreground;
        let cfg = cx.global::<Config>();
        let link_url = cfg.link_url;
        let mouse_hide = cfg.mouse_hide_while_typing;
        let focus_follows = cfg.focus_follows_mouse;
        let option_as_alt = cfg.macos_option_as_alt;
        let scroll_mult = cfg.mouse_scroll_multiplier;
        let clip_trim = cfg.clipboard_trim_trailing_spaces;
        let copy_on_select = cfg.copy_on_select;
        // Map the persisted scrollback depth onto its preset radio index (default
        // to 10k's slot for any off-preset value a hand-edit might leave).
        let scrollback_idx = match cfg.scrollback_limit {
            n if n <= 1_000 => 0,
            n if n <= 10_000 => 1,
            _ => 2,
        };
        let notify_idx = match cfg.notify_on_command_finish {
            NotifyMode::Never => 0,
            NotifyMode::Unfocused => 1,
            NotifyMode::Always => 2,
        };
        let scroll_slider = match self.active_settings() {
            Some(s) => s.scroll_slider.clone(),
            None => return div().into_any_element(),
        };

        let link_switch = Switch::new("term-link-url")
            .checked(link_url)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_link_url(*on, cx)))
            .into_any_element();
        let scrollback_radio = self.segmented(
            "term-scrollback",
            &["1,000", "10,000", "100,000"],
            scrollback_idx,
            cx,
            |this, ix, _w, cx| {
                let lines = match ix {
                    0 => 1_000,
                    1 => 10_000,
                    _ => 100_000,
                };
                this.set_scrollback_limit(lines, cx);
            },
        );
        let notify_radio = self.segmented(
            "term-notify",
            &["Never", "When unfocused", "Always"],
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

        let focus_switch = Switch::new("term-focus-follows")
            .checked(focus_follows)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_focus_follows_mouse(*on, cx)))
            .into_any_element();
        let mouse_hide_switch = Switch::new("term-mouse-hide")
            .checked(mouse_hide)
            .on_click(
                cx.listener(|this, on: &bool, _w, cx| this.set_mouse_hide_while_typing(*on, cx)),
            )
            .into_any_element();
        let trim_switch = Switch::new("term-clip-trim")
            .checked(clip_trim)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_clipboard_trim(*on, cx)))
            .into_any_element();
        let copy_on_select_switch = Switch::new("term-copy-on-select")
            .checked(copy_on_select)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_copy_on_select(*on, cx)))
            .into_any_element();
        // macOS only: the Option/special-character split this toggle resolves
        // doesn't exist on other platforms, where Alt always carries Meta.
        let option_alt_row = cfg!(target_os = "macos").then(|| {
            let switch = Switch::new("term-option-as-alt")
                .checked(option_as_alt)
                .on_click(
                    cx.listener(|this, on: &bool, _w, cx| this.set_macos_option_as_alt(*on, cx)),
                )
                .into_any_element();
            self.settings_row(
                "Option (⌥) acts as Meta",
                "⌥+key sends the escape chord shells expect (⌥B = back one word) \
                 instead of typing a special character (∫).",
                switch,
                cx,
            )
        });
        // Slider + a live readout of the current multiplier beside it.
        let scroll_control = h_flex()
            .items_center()
            .gap_3()
            .w(px(240.))
            .child(div().flex_1().child(Slider::new(&scroll_slider)))
            .child(
                div()
                    .w(px(36.))
                    .text_sm()
                    .text_color(foreground)
                    .child(format!("{scroll_mult:.2}×")),
            )
            .into_any_element();

        v_flex()
            .child(self.section_header("Scrolling", cx))
            .child(self.settings_row(
                "Scrollback",
                "Lines of history kept per pane. Applies to new panes.",
                scrollback_radio,
                cx,
            ))
            .child(self.settings_row(
                "Scroll speed",
                "Multiplier applied to mouse-wheel scrolling.",
                scroll_control,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header("Mouse", cx))
            .child(self.settings_row(
                "Focus follows mouse",
                "Hovering a pane focuses it without a click.",
                focus_switch,
                cx,
            ))
            .child(self.settings_row(
                "Hide mouse while typing",
                "Hide the pointer as you type; it returns on the next move.",
                mouse_hide_switch,
                cx,
            ))
            .when_some(option_alt_row, |v, row| {
                v.child(self.section_rule(cx))
                    .child(self.section_header("Keyboard", cx))
                    .child(row)
            })
            .child(self.section_rule(cx))
            .child(self.section_header("Links", cx))
            .child(self.settings_row(
                "Detect URLs",
                "Underline links on hover and open them on ⌘-click.",
                link_switch,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header("Clipboard", cx))
            .child(self.settings_row(
                "Copy on select",
                "Selecting text with the mouse copies it to the clipboard right away, no ⌘C needed.",
                copy_on_select_switch,
                cx,
            ))
            .child(self.settings_row(
                "Trim trailing spaces on copy",
                "Strip trailing whitespace from each copied line.",
                trim_switch,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header("Notifications", cx))
            .child(self.settings_row(
                "Notify on command finish",
                "Desktop alert after a long (≥10s) command completes.",
                notify_radio,
                cx,
            ))
            .into_any_element()
    }

    /// Window & Tabs section: the app window's lifecycle and tab placement.
    fn render_settings_window_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let cfg = cx.global::<Config>();
        let startup_idx = match cfg.startup_mode {
            crate::core::config::StartupMode::Normal => 0,
            crate::core::config::StartupMode::Maximized => 1,
            crate::core::config::StartupMode::Fullscreen => 2,
        };
        let new_tab_idx = match cfg.new_tab_position {
            NewTabPosition::AfterCurrent => 0,
            NewTabPosition::End => 1,
        };

        let startup_radio = self.segmented(
            "wt-startup",
            &["Normal", "Maximized", "Fullscreen"],
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
        let new_tab_radio = self.segmented(
            "wt-new-tab-pos",
            &["After current", "At end"],
            new_tab_idx,
            cx,
            |this, ix, _w, cx| {
                let pos = if ix == 0 {
                    NewTabPosition::AfterCurrent
                } else {
                    NewTabPosition::End
                };
                this.set_new_tab_position(pos, cx);
            },
        );

        v_flex()
            .child(self.section_header("Window", cx))
            .child(self.settings_row(
                "Startup window",
                "Window state when tty7 launches.",
                startup_radio,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header("Tabs", cx))
            .child(self.settings_row(
                "New tab position",
                "Where a freshly opened tab is inserted.",
                new_tab_radio,
                cx,
            ))
            .into_any_element()
    }

    /// Theme gallery: one clickable card per theme (built-ins + user files), each
    /// a mini-terminal preview painted in its own colors. The selected card gets a
    /// soft ring + a check; clicking switches the active theme live via
    /// `set_preset`.
    fn render_theme_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let border = theme.border;
        let foreground = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let active_id = cx.global::<Config>().theme_preset.clone();

        // Selection chrome is monochrome — the ring and check track the theme's
        // own `foreground`, exactly like the active tab in the title bar.
        let sel_ring = foreground;

        let to_u32 = |(r, g, b): (u8, u8, u8)| (r as u32) << 16 | (g as u32) << 8 | b as u32;

        // Cap the gallery at four cards wide but keep it flex-wrapping, so narrow
        // panels fall back to three, two, or one per row.
        let mut gallery = h_flex().flex_wrap().gap_5().mt_2().mb_2().max_w(px(864.));
        for p in presets::all(cx) {
            let id = p.id.clone();
            let is_active = active_id == id;
            let accent = rgb(p.accent);
            let ansi = |i: usize| rgb(to_u32(p.ansi16[i]));
            let fg = rgb(p.foreground);
            // A "line of code": thin rounded bars, sized like words and tightly
            // spaced so the preview reads as real terminal text, not fat pills.
            let bar =
                |w: f32, color: gpui::Rgba| div().h(px(4.)).w(px(w)).rounded(px(1.5)).bg(color);

            let preview = v_flex()
                .h(px(120.))
                .bg(rgb(p.background_color()))
                .rounded(px(10.))
                .px_3()
                .py_3()
                .gap(px(10.))
                .child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(div().text_size(px(11.)).text_color(accent).child("❯"))
                        .child(bar(60., fg)),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(bar(26., ansi(2)))
                        .child(bar(46., ansi(4)))
                        .child(bar(16., ansi(3))),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(bar(18., ansi(1)))
                        .child(bar(52., fg)),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(bar(14., ansi(6)))
                        .child(bar(38., accent)),
                );

            let label = h_flex()
                .items_center()
                .gap_1p5()
                .px_1()
                .child(
                    div()
                        .text_sm()
                        .font_weight(if is_active {
                            FontWeight::SEMIBOLD
                        } else {
                            FontWeight::MEDIUM
                        })
                        .text_color(if is_active { foreground } else { muted_fg })
                        .child(p.name.clone()),
                )
                .when(is_active, |s| {
                    s.child(Icon::new(IconName::Check).small().text_color(foreground))
                });

            let card = div()
                .rounded_xl()
                .overflow_hidden()
                .border_1()
                .border_color(if is_active {
                    sel_ring.opacity(0.35)
                } else {
                    border
                })
                .when(is_active, |s| s.shadow_md())
                .when(!is_active, |s| {
                    s.shadow_sm()
                        .hover(|h| h.border_color(sel_ring.opacity(0.18)))
                })
                .child(preview);

            let click_id = id.clone();
            gallery = gallery.child(
                v_flex()
                    .id(SharedString::from(format!("theme-{id}")))
                    .w(px(196.))
                    .gap_2()
                    .cursor_pointer()
                    .child(card)
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_preset(&click_id, window, cx)
                    })),
            );
        }

        gallery.into_any_element()
    }

    /// Keybindings section: the effective shortcut list (defaults + overrides).
    fn render_settings_keybindings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let foreground = theme.foreground;
        let border = theme.border;
        let kbd_bg = theme.secondary.opacity(0.6);

        let cfg = cx.global::<Config>();
        let keybindings: Vec<(String, String)> = default_bindings()
            .into_iter()
            .map(|(action, key)| {
                let key = cfg
                    .keybindings
                    .get(action)
                    .cloned()
                    .unwrap_or_else(|| key.to_string());
                (action.to_string(), key)
            })
            .collect();

        // A single key glyph rendered as a small keycap, so a shortcut reads like
        // keys on a keyboard rather than a run of slashed-together text.
        let keycap = move |tok: String| {
            div()
                .flex()
                .items_center()
                .justify_center()
                .min_w(px(22.))
                .h(px(22.))
                .px_1p5()
                .rounded_md()
                .bg(kbd_bg)
                .border_1()
                .border_color(border)
                .text_xs()
                .text_color(foreground)
                .child(tok)
        };

        let count = keybindings.len();
        let mut list = v_flex();
        for (i, (action, key)) in keybindings.into_iter().enumerate() {
            list = list.child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .py_2p5()
                    .when(i + 1 < count, |s| s.border_b_1().border_color(border))
                    .child(
                        div()
                            .text_sm()
                            .text_color(foreground)
                            .child(humanize_action(&action)),
                    )
                    .child(
                        h_flex().gap_1().children(
                            crate::ui::keymap::key_tokens(&key)
                                .into_iter()
                                .map(|t| keycap(t)),
                        ),
                    ),
            );
        }

        v_flex()
            .child(self.section_intro(
                "Keyboard Shortcuts",
                "Remap keys by editing config.json (restart to apply).",
                cx,
            ))
            .child(list)
            .into_any_element()
    }

    /// About section: app identity and stack.
    fn render_settings_about(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (foreground, muted_fg) = (theme.foreground, theme.muted_foreground);

        // Startup update check (see `core::update`): a newer release, if one was
        // found, plus the toggle that controls whether we look at all.
        let update = cx
            .try_global::<crate::core::update::UpdateStatus>()
            .and_then(|s| s.available.clone());
        let check_for_updates = cx.global::<Config>().check_for_updates;

        let logo = Arc::new(Image::from_bytes(
            ImageFormat::Png,
            include_bytes!("../../assets/logo@256.png").to_vec(),
        ));

        v_flex()
            .child(self.section_header("About", cx))
            .child(
                h_flex()
                    .gap_4()
                    .items_center()
                    .child(img(logo).size_12().rounded_lg())
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(foreground)
                                    .child("tty7"),
                            )
                            .child(div().text_sm().text_color(muted_fg).child(format!(
                                "Version {}",
                                env!("CARGO_PKG_VERSION")
                            ))),
                    ),
            )
            .child(
                v_flex()
                    .mt_5()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(foreground)
                            .child("The window closes. Your session doesn't."),
                    )
                    .child(div().text_sm().text_color(muted_fg).child(
                        "The window is just a view — your shells run in a persistent daemon, so closing it never kills a session. GPU-rendered through gpui on Zed's alacritty_terminal core.",
                    ))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted_fg)
                            .child("GPU-rendered · daemon-backed · shell-aware"),
                    ),
            )
            // Updates: the startup check drops a newer version here if it found
            // one. We never self-update — "Download" just opens the Releases
            // page; the toggle turns the check off (see `core::update`).
            .child(
                v_flex()
                    .mt_6()
                    .gap_2()
                    .child(self.section_rule(cx))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(foreground)
                            .child("Updates"),
                    )
                    .when_some(update, |this, upd| {
                        this.child(
                            h_flex()
                                .gap_3()
                                .items_center()
                                .child(div().text_sm().text_color(foreground).child(
                                    format!("Version {} is available.", upd.version),
                                ))
                                .child(
                                    // Match the sibling "Restart Background
                                    // Service…" button (default style, not the
                                    // dark `.primary()` fill) so About reads as
                                    // one panel.
                                    Button::new("download-update")
                                        .label("Download")
                                        .small()
                                        .on_click(cx.listener(|this, _, _w, _cx| {
                                            this.open_releases_page()
                                        })),
                                ),
                        )
                    })
                    .child(div().text_sm().text_color(muted_fg).child(
                        "Check GitHub for a newer release on launch and show a download link here. tty7 never updates itself; the button opens the Releases page.",
                    ))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Switch::new("check-updates")
                                    .checked(check_for_updates)
                                    .on_click(cx.listener(|this, on: &bool, _w, cx| {
                                        this.set_check_for_updates(*on, cx)
                                    })),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(foreground)
                                    .child("Check for updates on launch"),
                            ),
                    ),
            )
            // Manage that daemon. A fresh process is the only way to pick up a
            // macOS permission granted after it started (e.g. Full Disk Access),
            // to recover if it wedges, or to start clean — quitting/reopening the
            // window alone never restarts it. Ends every running session, so the
            // action confirms first.
            .child(
                v_flex()
                    .mt_6()
                    .gap_2()
                    .child(self.section_rule(cx))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(foreground)
                            .child("Daemon"),
                    )
                    .child(div().text_sm().text_color(muted_fg).child(
                        "Restart the daemon to pick up a newly granted macOS permission, recover if it stops responding, or start from a clean slate. This ends all running sessions; your tabs and layout reopen with fresh shells.",
                    ))
                    .child(
                        h_flex().child(
                            Button::new("restart-daemon")
                                .label("Restart Daemon…")
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.restart_daemon(window, cx)
                                })),
                        ),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_action_splits_on_capitals() {
        assert_eq!(humanize_action("NewTab"), "New Tab");
        assert_eq!(
            humanize_action("ToggleMaximizePane"),
            "Toggle Maximize Pane"
        );
        assert_eq!(humanize_action("Quit"), "Quit");
    }
}

//! The settings window's frame: the navigation, the page header, the rows and
//! groups every page is built from, the popovers controls open, and the
//! search and "modified" views that stand in for a page.

use std::rc::Rc;

use super::kit::{self, BtnKind, Tk, fs};
use super::*;

/// What a menu row does when picked.
pub(crate) type PickFn = Rc<dyn Fn(&mut Tty7App, &mut Window, &mut Context<Tty7App>)>;

/// One line of a dropdown's menu.
pub(crate) enum MenuEntry {
    Item {
        label: SharedString,
        checked: bool,
        /// Draw the label in this family — a font menu shows each font.
        font: Option<SharedString>,
        danger: bool,
        trailing: Option<SharedString>,
        on_pick: PickFn,
    },
    Heading(SharedString),
    Separator,
    Note(SharedString),
}

impl MenuEntry {
    pub(crate) fn item(
        label: impl Into<SharedString>,
        checked: bool,
        on_pick: impl Fn(&mut Tty7App, &mut Window, &mut Context<Tty7App>) + 'static,
    ) -> Self {
        MenuEntry::Item {
            label: label.into(),
            checked,
            font: None,
            danger: false,
            trailing: None,
            on_pick: Rc::new(on_pick),
        }
    }
}

/// A searchable menu's rows: what each says, and the family to draw it in.
pub(crate) struct SearchOption {
    pub label: SharedString,
    pub font: Option<SharedString>,
}

impl Tty7App {
    // ── Popovers ─────────────────────────────────────────────────────────

    pub(crate) fn settings_menu_open(&self, id: &str) -> bool {
        self.active_settings()
            .and_then(|s| s.menu.as_ref())
            .is_some_and(|m| m.as_ref() == id)
    }

    /// Opens the popover `id` — closing whichever one was open — or closes it
    /// when it is the one already open. `highlight` is the row the arrow keys
    /// start from; a popover with a query box gets the caret in it.
    pub(crate) fn toggle_settings_menu(
        &mut self,
        id: SharedString,
        highlight: usize,
        with_query: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(s) = self.active_settings_mut() else {
            return;
        };
        if s.menu.as_ref() == Some(&id) {
            s.menu = None;
            let handle = s.focus_handle.clone();
            window.focus(&handle, cx);
            cx.notify();
            return;
        }
        s.menu = Some(id);
        s.menu_hi = highlight;
        let query = s.menu_query.clone();
        let scroll = s.menu_scroll.clone();
        query.update(cx, |q, cx| q.set_value("", window, cx));
        if let Some(s) = self.active_settings_mut() {
            // Clearing the query resets the highlight; put it back.
            s.menu_hi = highlight;
        }
        scroll.scroll_to_item(highlight);
        if with_query {
            let handle = query.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    pub(crate) fn close_settings_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.active_settings_mut() else {
            return;
        };
        if s.menu.take().is_some() {
            let handle = s.focus_handle.clone();
            window.focus(&handle, cx);
            cx.notify();
        }
    }

    /// A dropdown: the trigger showing the current value, and while open, its
    /// menu hanging from the trigger's right edge.
    pub(crate) fn settings_dropdown(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        leading: Option<AnyElement>,
        entries: Vec<MenuEntry>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tk = Tk::of(cx);
        let id: SharedString = id.into();
        let open = self.settings_menu_open(&id);
        let checked_at = entries
            .iter()
            .position(|e| matches!(e, MenuEntry::Item { checked: true, .. }))
            .unwrap_or(0);
        let toggle_id = id.clone();
        let trigger = kit::select_trigger(
            ElementId::Name(SharedString::from(format!("{id}-trigger"))),
            label,
            leading,
            open,
            &tk,
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            this.toggle_settings_menu(toggle_id.clone(), checked_at, false, window, cx)
        }));
        div()
            .relative()
            .flex_shrink_0()
            .child(trigger)
            .when(open, |d| {
                d.child(kit::popover_below(
                    true,
                    4.,
                    self.settings_menu_panel(&id, entries, cx).min_w(px(148.)),
                ))
            })
            .into_any_element()
    }

    pub(crate) fn settings_menu_panel(
        &self,
        id: &str,
        entries: Vec<MenuEntry>,
        cx: &mut Context<Self>,
    ) -> Div {
        let tk = Tk::of(cx);
        let mut panel = kit::menu_panel(&tk);
        for (i, entry) in entries.into_iter().enumerate() {
            panel = match entry {
                MenuEntry::Item {
                    label,
                    checked,
                    font,
                    danger,
                    trailing,
                    on_pick,
                } => panel.child(
                    kit::menu_row(
                        ElementId::Name(SharedString::from(format!("{id}-item-{i}"))),
                        false,
                        &tk,
                    )
                    .child(kit::check_mark(checked, &tk))
                    .child(
                        div()
                            .flex_1()
                            .when_some(font, |d, f| d.font_family(f))
                            .when(danger, |d| d.text_color(tk.danger))
                            .child(label),
                    )
                    .when_some(trailing, |r, text| {
                        r.child(
                            div()
                                .pl(px(12.))
                                .text_size(fs(11.5))
                                .text_color(tk.k4)
                                .child(text),
                        )
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.close_settings_menu(window, cx);
                        on_pick(this, window, cx);
                    })),
                ),
                MenuEntry::Heading(text) => panel.child(kit::menu_heading(text, &tk)),
                MenuEntry::Separator => panel.child(kit::menu_separator(&tk)),
                MenuEntry::Note(text) => panel.child(
                    div()
                        .px(px(8.))
                        .pt(px(4.))
                        .pb(px(6.))
                        .text_size(fs(11.5))
                        .line_height(px(16.))
                        .text_color(tk.k45)
                        .child(text),
                ),
            };
        }
        panel
    }

    /// A dropdown over a long list — the font menus — with a query box at the
    /// top and the arrow keys to walk what it leaves.
    pub(crate) fn settings_search_dropdown(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        options: Rc<Vec<SearchOption>>,
        current: Option<usize>,
        on_pick: Rc<dyn Fn(&mut Tty7App, usize, &mut Window, &mut Context<Tty7App>)>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tk = Tk::of(cx);
        let id: SharedString = id.into();
        let open = self.settings_menu_open(&id);
        let toggle_id = id.clone();
        let start = current.unwrap_or(0);
        let trigger = kit::select_trigger(
            ElementId::Name(SharedString::from(format!("{id}-trigger"))),
            label,
            None,
            open,
            &tk,
        )
        .on_click(cx.listener(move |this, _, window, cx| {
            this.toggle_settings_menu(toggle_id.clone(), start, true, window, cx)
        }));
        let popover = open.then(|| {
            let Some(s) = self.active_settings() else {
                return div();
            };
            let query = s.menu_query.read(cx).value().trim().to_lowercase();
            let visible: Rc<Vec<usize>> = Rc::new(
                options
                    .iter()
                    .enumerate()
                    .filter(|(_, o)| query.is_empty() || o.label.to_lowercase().contains(&query))
                    .map(|(i, _)| i)
                    .collect(),
            );
            let hi = s.menu_hi.min(visible.len().saturating_sub(1));
            let keys_visible = visible.clone();
            let keys_pick = on_pick.clone();
            let mut list = v_flex()
                .id(ElementId::Name(SharedString::from(format!("{id}-list"))))
                .max_h(px(240.))
                .overflow_y_scroll()
                .track_scroll(&s.menu_scroll)
                .p(px(4.))
                .gap(px(1.));
            for (row, &ix) in visible.iter().enumerate() {
                let option = &options[ix];
                let pick = on_pick.clone();
                list = list.child(
                    kit::menu_row(
                        ElementId::NamedInteger(id.clone(), ix as u64),
                        row == hi,
                        &tk,
                    )
                    .h(px(28.))
                    .child(kit::check_mark(Some(ix) == current, &tk))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .when_some(option.font.clone(), |d, f| d.font_family(f))
                            .child(option.label.clone()),
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
                        pick(this, ix, window, cx);
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
                        .child(t(L10nKey::SettingsNoMatchesShort)),
                );
            }
            kit::menu_panel(&tk)
                .w(px(260.))
                .p_0()
                .gap_0()
                .overflow_hidden()
                .capture_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                    let key = ev.keystroke.key.as_str();
                    let len = keys_visible.len();
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
                            if let Some(&ix) = keys_visible.get(hi.min(len.saturating_sub(1))) {
                                this.close_settings_menu(window, cx);
                                keys_pick(this, ix, window, cx);
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
                .child(
                    h_flex()
                        .h(px(32.))
                        .px(px(12.))
                        .gap(px(7.))
                        .items_center()
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
        });
        div()
            .relative()
            .flex_shrink_0()
            .child(trigger)
            .when_some(popover, |d, p| d.child(kit::popover_below(true, 4., p)))
            .into_any_element()
    }

    // ── Rows and groups ──────────────────────────────────────────────────

    /// A group of rows under a quiet heading, with the sentence that explains
    /// the group when it needs one.
    pub(crate) fn settings_group(
        &self,
        title: Option<&str>,
        desc: Option<String>,
        body: impl IntoIterator<Item = AnyElement>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let tk = Tk::of(cx);
        v_flex()
            .when_some(title, |col, title| {
                col.child(
                    kit::section_head(title, desc, &tk)
                        .id(settings_header_id(title))
                        .anchor_scroll(self.first_hit_anchor(title, cx)),
                )
            })
            .children(body)
            .into_any_element()
    }

    /// A page: its groups, forty points apart.
    pub(crate) fn settings_page(groups: impl IntoIterator<Item = AnyElement>) -> AnyElement {
        v_flex().gap(px(40.)).children(groups).into_any_element()
    }

    pub(crate) fn settings_row(
        &self,
        label: impl Into<String>,
        desc: impl Into<String>,
        control: AnyElement,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        self.settings_row_gated_when(label, desc, control, false, cx)
    }

    /// The same row, greyed out when `gated` — for a control another setting
    /// has switched off, whose own value is still there for when it comes back.
    ///
    /// Only the text dims. The control draws its own disabled state.
    pub(crate) fn settings_row_gated_when(
        &self,
        label: impl Into<String>,
        desc: impl Into<String>,
        control: AnyElement,
        gated: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let tk = Tk::of(cx);
        let label = label.into();
        let desc = desc.into();
        let entry = settings_search_entries()
            .iter()
            .find(|entry| t(entry.title) == label);
        let modified = entry.is_some_and(|entry| entry.modified(cx.global::<Config>()));
        let capture = self
            .active_settings()
            .is_some_and(|s| s.search_rows.borrow().is_some());
        // Descriptions can carry live status, so they must not take part in
        // the identity that preserves GPUI's hover state.
        let element_id = settings_row_id(&label, &desc);
        let (hit, miss) = match self
            .active_settings()
            .filter(|s| !s.search_active && !capture)
        {
            Some(s) => {
                let query = s.search.read(cx).value().trim().to_lowercase();
                let focused = s.focused_setting.is_some_and(|k| t(k) == label);
                match query.is_empty() || section_match_count(s.section, &query) == 0 {
                    true => (focused, false),
                    false => {
                        let hit = row_matches_query(s.section, &label, &query);
                        (hit, !hit)
                    }
                }
            }
            None => (false, false),
        };
        let first_hit_anchor = self.first_hit_anchor(&label, cx);
        // The control never shrinks, so on a narrow pane it would squeeze the
        // label column to a letter per line. Below the width where both still
        // fit, the control goes on a line of its own instead.
        let stacked = self.settings_row_under(STACK_ROW_BELOW, cx);
        // The reset sits on the label's own line, beside the name it resets,
        // so a changed row keeps the height of an unchanged one. Rows the
        // catalog cannot reset in place still say they differ.
        let resettable = entry.filter(|e| {
            e.title != L10nKey::SettingsSearchKeybindingsTitle
                && e.title != L10nKey::SettingsThemeIntroTitle
        });
        let title_line = h_flex()
            .gap(px(8.))
            .items_center()
            .min_w_0()
            .child(div().text_size(fs(13.)).text_color(tk.fg).child(label))
            .when(modified, |line| match resettable {
                Some(entry) => {
                    let key = entry.title;
                    line.child(
                        div()
                            .id(SharedString::from(format!("reset-setting-{key:?}")))
                            .flex_shrink_0()
                            .text_size(fs(11.5))
                            .text_color(tk.k4)
                            .cursor_pointer()
                            .hover(move |s| s.text_color(tk.fg))
                            .child(t(L10nKey::Reset))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.reset_settings_value(key, window, cx)
                            })),
                    )
                }
                None => line.child(
                    div()
                        .flex_shrink_0()
                        .text_size(fs(11.5))
                        .text_color(tk.k4)
                        .child(t(L10nKey::SettingsModified)),
                ),
            });
        let labels = v_flex()
            .gap(px(2.))
            .min_w_0()
            .when(gated, |col| col.opacity(0.45))
            .child(title_line)
            .when(!desc.is_empty(), |col| {
                col.child(
                    div()
                        .text_size(fs(12.))
                        .line_height(px(17.))
                        .text_color(tk.k5)
                        .child(desc),
                )
            });
        let row = div()
            .id(element_id)
            .flex()
            .when(stacked, |row| row.flex_col().items_start().gap(px(8.)))
            .when(!stacked, |row| {
                row.flex_row().items_center().justify_between().gap(px(32.))
            })
            .min_h(px(52.))
            .py(px(12.))
            .when(hit, |row| {
                row.bg(tk.k04).rounded(px(7.)).mx(px(-8.)).px(px(8.))
            })
            // Only the first hit on the page carries the anchor: it is the one
            // the page scrolls to.
            .anchor_scroll(first_hit_anchor)
            .when(miss, |row| row.opacity(0.45))
            .child(labels)
            .child(
                h_flex()
                    .when(stacked, |c| c.w_full())
                    .when(!stacked, |c| c.flex_shrink_0().justify_end())
                    .gap(px(8.))
                    .child(control),
            );
        if capture {
            if let Some(entry) = entry {
                self.active_settings()
                    .unwrap()
                    .search_rows
                    .borrow_mut()
                    .as_mut()
                    .unwrap()
                    .push((entry.title, row.into_any_element()));
                return div().id("captured-setting");
            }
        }
        row
    }

    /// A text field for the right-hand column, with its focus ring.
    pub(crate) fn settings_text_input(
        &self,
        input: &Entity<InputState>,
        width: f32,
        invalid: bool,
        cx: &App,
    ) -> Div {
        let tk = Tk::of(cx);
        let focused = self.settings_input_focused(input, cx);
        kit::text_field(input, focused, invalid, &tk, cx)
            .w(px(width))
            .max_w_full()
    }

    pub(crate) fn settings_input_focused(&self, input: &Entity<InputState>, cx: &App) -> bool {
        let handle = input.read(cx).focus_handle(cx);
        self.active_settings()
            .and_then(|s| s.focused.borrow().clone())
            .is_some_and(|f| f == handle)
    }

    /// A text field with the line that says what is wrong with it.
    pub(crate) fn settings_checked_input(
        &self,
        input: &Entity<InputState>,
        width: f32,
        error: Option<String>,
        cx: &App,
    ) -> AnyElement {
        let tk = Tk::of(cx);
        let invalid = error.is_some();
        v_flex()
            .items_end()
            .gap(px(4.))
            .child(self.settings_text_input(input, width, invalid, cx))
            .when_some(error, |col, e| {
                col.child(
                    div()
                        .max_w(px(width.max(220.)))
                        .text_size(fs(12.))
                        .text_color(tk.danger)
                        .child(e),
                )
            })
            .into_any_element()
    }

    pub(crate) fn segmented(
        &self,
        id: impl Into<SharedString>,
        options: &[&str],
        selected: usize,
        cx: &mut Context<Self>,
        on_pick: impl Fn(&mut Self, usize, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        self.segmented_full(id, options, Some(selected), None, cx, on_pick)
    }

    /// A segmented control over a fixed set of values, used where the config
    /// accepts anything in a range. When the live value matches a bucket
    /// exactly that bucket is highlighted; when it does not, a trailing
    /// "Custom (N)" cell carries the highlight instead of the nearest bucket
    /// getting a label it does not have (#550).
    ///
    /// The custom cell is not a button — there is no bucket value behind it to
    /// write — so it takes neither a click handler nor a pointer cursor.
    pub(crate) fn segmented_valued(
        &self,
        id: impl Into<SharedString>,
        options: &[&str],
        selected: Option<usize>,
        custom_label: Option<String>,
        cx: &mut Context<Self>,
        on_pick: impl Fn(&mut Self, usize, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        self.segmented_full(id, options, selected, custom_label, cx, on_pick)
    }

    fn segmented_full(
        &self,
        id: impl Into<SharedString>,
        options: &[&str],
        selected: Option<usize>,
        custom_label: Option<String>,
        cx: &mut Context<Self>,
        on_pick: impl Fn(&mut Self, usize, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let tk = Tk::of(cx);
        let id: SharedString = id.into();
        let on_pick = Rc::new(on_pick);
        let cells: Vec<(String, Option<usize>)> = options
            .iter()
            .enumerate()
            .map(|(i, l)| (l.to_string(), Some(i)))
            .chain(custom_label.map(|l| (l, None)))
            .collect();
        h_flex()
            .id(ElementId::Name(id.clone()))
            .flex_shrink_0()
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(7.))
            .bg(tk.k05)
            .children(cells.into_iter().enumerate().map(|(i, (label, bucket))| {
                // A bucket is highlighted only on an exact match, and the
                // custom cell (`bucket == None`) exactly when no bucket was.
                let active = bucket == selected;
                let on_pick = on_pick.clone();
                let cell = h_flex()
                    .id(ElementId::NamedInteger(id.clone(), i as u64))
                    .items_center()
                    .justify_center()
                    .h(px(22.))
                    .px(px(10.))
                    .rounded(px(5.))
                    .whitespace_nowrap()
                    .text_size(fs(12.))
                    .when(active, |s| {
                        s.bg(tk.chip).shadow(tk.raised()).text_color(tk.fg)
                    })
                    .when(!active, |s| {
                        s.text_color(tk.k5).hover(move |h| h.text_color(tk.fg))
                    })
                    .child(label);
                match bucket {
                    Some(ix) => {
                        cell.cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                on_pick(this, ix, window, cx);
                            }))
                    }
                    None => cell,
                }
            }))
            .into_any_element()
    }

    /// A switch bound to a setter.
    pub(crate) fn settings_switch(
        &self,
        id: &'static str,
        on: bool,
        cx: &mut Context<Self>,
        set: impl Fn(&mut Self, bool, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        kit::switch(id)
            .checked(on)
            .on_click(cx.listener(move |this, on: &bool, window, cx| set(this, *on, window, cx)))
            .into_any_element()
    }

    pub(crate) fn settings_button(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
        act: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> kit::Btn {
        kit::button(id, label, BtnKind::Secondary)
            .on_click(cx.listener(move |this, _, window, cx| act(this, window, cx)))
    }

    // ── The frame ────────────────────────────────────────────────────────

    pub(crate) fn render_settings(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let tk = Tk::of(cx);
        // Opaque, so workspace translucency never shows through the page, but
        // with the preset's gradient rather than a flat color.
        let background: Background = crate::ui::theme::overlay_background(cx);
        // That fill also covers the theme's background image, so the page
        // carries its own copy, dimmed the way the workspace dims it.
        let background_layers = crate::ui::app::overlay_surface_layers(cx);

        let (focus_handle, section, search) = match self.active_settings() {
            Some(s) => (s.focus_handle.clone(), s.section, s.search.clone()),
            None => return div(),
        };
        if let Some(s) = self.active_settings() {
            s.focused.replace(window.focused(cx));
        }
        let query = search.read(cx).value().trim().to_lowercase();
        let searching = self.active_settings().is_some_and(|s| s.search_active);
        let menu_open = self.active_settings().is_some_and(|s| s.menu.is_some());

        let viewport_w = window.viewport_size().width.as_f32();
        let scale = ui_scale(cx);
        self.settings_viewport_w.set(viewport_w);
        self.settings_row_width
            .set(settings_row_width(viewport_w, scale));
        self.settings_hit_anchored.set(false);

        // A query that matched here has to be reachable, not just counted: the
        // anchor lands on the first matching row, and `scroll_to` reads where
        // it ended up on the frame after this one.
        if searching
            && let Some(s) = self.active_settings()
            && s.reveal_first_hit.replace(false)
        {
            s.search_anchor.scroll_to(window, cx);
        }
        if let Some(s) = self.active_settings()
            && !searching
            && s.reveal_first_hit.get()
        {
            s.reveal_first_hit.set(false);
            let matched_here =
                section_match_count(section, &query) > 0 || s.focused_setting.is_some();
            if s.focused_setting.is_some() || (!query.is_empty() && matched_here) {
                s.search_anchor.scroll_to(window, cx);
            }
        }

        let prof = crate::ui::perf::enabled()
            .then(|| (std::time::Instant::now(), section.profile_label()));

        let nav = self.render_settings_nav(nav_width(viewport_w, scale), searching, cx);

        let (title, subtitle, content) = if searching {
            self.render_settings_results(cx)
        } else {
            let content = match section {
                SettingsSection::General => self.render_settings_general(cx),
                SettingsSection::Appearance => self.render_settings_appearance(cx),
                SettingsSection::Terminal => self.render_settings_terminal(cx),
                SettingsSection::KeyboardMouse => self.render_settings_input(cx),
                SettingsSection::Ssh => self.render_settings_ssh(cx),
                SettingsSection::Agents => self.render_settings_agents(cx),
                SettingsSection::WindowTabs => self.render_window_preferences(cx),
                SettingsSection::Keybindings => self.render_settings_keybindings(cx),
                SettingsSection::About => self.render_settings_about(cx),
            };
            (t(section.title()).to_string(), None, content)
        };

        let back = (!searching && section == SettingsSection::Keybindings).then(|| {
            h_flex()
                .id("settings-back")
                .mt(px(-20.))
                .mb(px(-24.))
                .ml(px(-2.))
                .gap(px(4.))
                .items_center()
                .text_size(fs(12.))
                .text_color(tk.k5)
                .cursor_pointer()
                .hover(move |s| s.text_color(tk.fg))
                .child(Icon::empty().path("icons/settings/back.svg").size(px(10.)))
                .child(t(L10nKey::SettingsNavInput))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.navigate_settings(SettingsSection::KeyboardMouse, None, window, cx)
                }))
        });

        let header = h_flex()
            .items_baseline()
            .gap(px(10.))
            .child(
                div()
                    .text_size(fs(20.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(tk.fg)
                    .child(title),
            )
            .when_some(subtitle, |row, sub| {
                row.child(div().text_size(fs(12.)).text_color(tk.k4).child(sub))
            });

        let notices = self.render_settings_notices(cx);

        let body = v_flex()
            .id("settings-content")
            .size_full()
            .overflow_y_scroll()
            .when_some(self.active_settings(), |pane, s| {
                pane.track_scroll(&s.content_scroll)
            })
            .child(
                // A block, not a flex box: as a flex item it would negotiate a
                // height with the pane and leave most of the page outside the
                // scroll range.
                div().w_full().px(px(56.)).pb(px(48.)).child(
                    v_flex()
                        .w_full()
                        .max_w(px(READING_COLUMN * scale))
                        .gap(px(40.))
                        .children(back)
                        .child(header)
                        .children(notices)
                        .child(content),
                ),
            );
        let main = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(div().h(px(TITLE_BAR_HEIGHT)).flex_shrink_0())
            .when_some(self.active_settings(), |pane, s| {
                pane.child(crate::ui::scrollbar::with_inset_vertical_scrollbar(
                    "settings-content-scrollbar",
                    body,
                    &s.content_scroll,
                    px(SCROLLBAR_WINDOW_INSET),
                ))
            });

        let root = div()
            .size_full()
            .relative()
            .flex()
            .flex_row()
            .bg(background)
            .text_color(tk.fg)
            .text_size(fs(13.))
            .track_focus(&focus_handle)
            .capture_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                this.settings_capture_key(searching, ev, window, cx)
            }))
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key.as_str() != "escape" {
                    return;
                }
                // Escape peels one layer at a time: an open popover, a
                // pending shortcut conflict, the search, then the window.
                if menu_open {
                    this.close_settings_menu(window, cx);
                    cx.stop_propagation();
                    return;
                }
                if let Some(s) = this.active_settings_mut()
                    && s.kb_conflict.take().is_some()
                {
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                if searching {
                    if let Some(s) = this.active_settings_mut() {
                        s.modified_only = false;
                    }
                    if let Some(s) = this.active_settings() {
                        s.search
                            .clone()
                            .update(cx, |s, cx| s.set_value("", window, cx));
                    }
                    this.autoselect_settings_search(cx);
                    cx.stop_propagation();
                    return;
                }
                this.close_settings_checked(window, cx);
            }))
            .children(background_layers)
            .child(nav)
            .child(main)
            .child(
                crate::ui::app::window_move_gesture(
                    div()
                        .id("settings-titlebar-drag")
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(TITLE_BAR_HEIGHT)),
                    "settings-titlebar-drag",
                    window,
                    cx,
                )
                .on_double_click(|_, window, _| window.titlebar_double_click()),
            )
            .child(
                div()
                    .id("settings-close")
                    .absolute()
                    .top(px((TITLE_BAR_HEIGHT - 26.) / 2.))
                    .right(px(12.))
                    .size(px(26.))
                    .rounded(px(6.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .occlude()
                    .cursor_pointer()
                    .hover(move |s| s.bg(tk.k05))
                    .child(
                        Icon::empty()
                            .path("icons/settings/close.svg")
                            .size(px(10.))
                            .text_color(tk.k45),
                    )
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new(t(L10nKey::Close)).build(window, cx)
                    })
                    .on_click(
                        cx.listener(|this, _, window, cx| this.close_settings_checked(window, cx)),
                    ),
            )
            // While a popover is open the page under it only takes one click:
            // the one that closes it.
            .when(menu_open, |r| {
                r.child(
                    div()
                        .id("settings-popover-scrim")
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| this.close_settings_menu(window, cx)),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(|this, _, window, cx| this.close_settings_menu(window, cx)),
                        ),
                )
            });

        if let Some((start, label)) = prof {
            crate::ui::perf::record(label, start.elapsed());
        }
        root
    }

    /// The save failures and the unsaved theme draft, said at the top of the
    /// page with what can be done about them.
    fn render_settings_notices(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let tk = Tk::of(cx);
        let mut out = Vec::new();
        let notice = |text: String, tk: &Tk| {
            div()
                .text_size(fs(12.5))
                .line_height(px(18.))
                .text_color(tk.fg)
                .child(text)
        };
        if self.theme_draft_dirty() {
            let error = self
                .active_settings()
                .and_then(|s| s.theme_draft_error.clone());
            out.push(
                v_flex()
                    .gap(px(10.))
                    .child(notice(t(L10nKey::SettingsThemeDraft).to_string(), &tk))
                    .when_some(error, |v, error| {
                        v.child(
                            div()
                                .text_size(fs(12.))
                                .text_color(tk.danger)
                                .child(t_fmt(L10nKey::SettingsSaveError, &[("error", &error)])),
                        )
                    })
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .child(
                                kit::button(
                                    "save-theme-draft",
                                    t(L10nKey::SettingsSaveChanges),
                                    BtnKind::Primary,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.save_theme_draft(window, cx);
                                    },
                                )),
                            )
                            .child(
                                kit::button(
                                    "cancel-theme-draft",
                                    t(L10nKey::Cancel),
                                    BtnKind::Secondary,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.cancel_theme_draft(window, cx),
                                )),
                            ),
                    )
                    .into_any_element(),
            );
        }
        if let Some(error) = self.active_settings().and_then(|s| s.save_error.clone()) {
            out.push(
                v_flex()
                    .gap(px(10.))
                    .child(
                        div()
                            .text_size(fs(12.5))
                            .text_color(tk.danger)
                            .child(t_fmt(L10nKey::SettingsSaveError, &[("error", &error)])),
                    )
                    .child(
                        h_flex().child(
                            kit::button(
                                "retry-settings-save",
                                t(L10nKey::SettingsRetrySave),
                                BtnKind::Secondary,
                            )
                            .on_click(
                                cx.listener(|this, _, _window, cx| {
                                    this.persist_settings_config(cx)
                                }),
                            ),
                        ),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    fn render_settings_nav(
        &self,
        width: f32,
        searching: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tk = Tk::of(cx);
        let Some(s) = self.active_settings() else {
            return div().into_any_element();
        };
        let section = s.section.navigation_section();
        let search = s.search.clone();
        let modified_only = s.modified_only;
        let cfg = cx.global::<Config>();
        let modified_in = |target: SettingsSection| {
            settings_search_entries()
                .iter()
                .filter(|e| e.section == target && e.modified(cfg))
                .count()
        };
        let counts: Vec<(SettingsSection, usize)> = SettingsSection::ALL
            .iter()
            .map(|&target| (target, modified_in(target)))
            .collect();
        let total: usize = counts.iter().map(|(_, n)| n).sum();
        let search_focused = self.settings_input_focused(&search, cx);

        let items = counts.into_iter().map(|(target, count)| {
            let active = !searching && section == target;
            h_flex()
                .id(SharedString::from(format!(
                    "settings-nav-{}",
                    target.profile_label()
                )))
                .h(px(28.))
                .px(px(8.))
                .gap(px(10.))
                .items_center()
                .rounded(px(7.))
                .cursor_pointer()
                .when(active, |r| r.bg(tk.k07))
                .when(!active, |r| r.hover(move |s| s.bg(tk.k04)))
                .child(
                    Icon::empty()
                        .path(target.icon_path())
                        .size(px(14.))
                        .text_color(if active { tk.fg } else { tk.nav }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(if active { tk.fg } else { tk.nav })
                        .when(active, |d| d.font_weight(FontWeight::MEDIUM))
                        .child(t(target.title())),
                )
                .when(count > 0, |r| {
                    r.child(
                        div()
                            .text_size(fs(11.5))
                            .text_color(tk.k4)
                            .child(count.to_string()),
                    )
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.navigate_settings(target, None, window, cx)
                }))
        });

        v_flex()
            .w(px(width))
            .flex_shrink_0()
            .h_full()
            // The main window's tab rail fill, so the two sidebars read as the
            // same surface. Opaque: the rail's translucency rule would let the
            // page behind show through.
            .bg(gpui::rgb(cx.global::<presets::Surfaces>().rail.base))
            .border_r_1()
            .border_color(tk.k08)
            .child(div().h(px(TITLE_BAR_HEIGHT)).flex_shrink_0())
            .child(
                div()
                    .px(px(12.))
                    .pt(px(4.))
                    .pb(px(16.))
                    .flex_shrink_0()
                    .child(
                        h_flex()
                            .h(px(28.))
                            .px(px(8.))
                            .gap(px(7.))
                            .items_center()
                            .rounded(px(7.))
                            .bg(tk.k04)
                            .child(kit::search_glass(11., &tk))
                            .child(
                                div().flex_1().min_w_0().child(
                                    Input::new(&search)
                                        .appearance(false)
                                        .px_0()
                                        .py_0()
                                        .h(px(26.)),
                                ),
                            )
                            .when(!search_focused, |r| {
                                r.child(div().text_size(fs(11.)).text_color(tk.k35).child(
                                    if cfg!(target_os = "macos") {
                                        "⌘F"
                                    } else {
                                        "Ctrl F"
                                    },
                                ))
                            }),
                    ),
            )
            .child(
                v_flex()
                    .id("settings-nav")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(12.))
                    .gap(px(1.))
                    .children(items),
            )
            .child(
                div().p(px(12.)).border_t_1().border_color(tk.k08).child(
                    h_flex()
                        .id("settings-modified-filter")
                        .h(px(28.))
                        .px(px(8.))
                        .gap(px(10.))
                        .items_center()
                        .rounded(px(7.))
                        .cursor_pointer()
                        .hover(move |s| s.bg(tk.k05))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(tk.k6)
                                .child(t(L10nKey::SettingsModifiedOnly)),
                        )
                        .when(total > 0, |r| {
                            r.child(
                                div()
                                    .text_size(fs(11.5))
                                    .text_color(tk.k4)
                                    .child(total.to_string()),
                            )
                        })
                        .child(
                            kit::switch("settings-modified-switch")
                                .small()
                                .checked(modified_only),
                        )
                        .on_click(cx.listener(|this, _, _window, cx| {
                            if let Some(s) = this.active_settings_mut() {
                                s.modified_only = !s.modified_only;
                            }
                            this.autoselect_settings_search(cx);
                        })),
                ),
            )
            .into_any_element()
    }

    /// Keys the page handles before any control sees them: ⌘F to the search,
    /// and the arrows and Enter walking the search results.
    fn settings_capture_key(
        &mut self,
        searching: bool,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = ev.keystroke.key.as_str();
        if key == "f" && ev.keystroke.modifiers.secondary() && !ev.keystroke.modifiers.shift {
            if let Some(s) = self.active_settings() {
                let handle = s.search.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
                if let Some(s) = self.active_settings_mut() {
                    s.menu = None;
                }
                cx.stop_propagation();
                cx.notify();
            }
            return;
        }
        if !searching
            || !self
                .active_settings()
                .is_some_and(|s| s.search.read(cx).focus_handle(cx).is_focused(window))
            || !matches!(key, "up" | "down" | "enter")
        {
            return;
        }
        let Some(s) = self.active_settings() else {
            return;
        };
        let query = s.search.read(cx).value().trim().to_lowercase();
        let entries = search_result_order(&query, s.modified_only, cx.global::<Config>());
        let actions = search_result_actions(&query, s.modified_only);
        let count = entries.len() + actions.len();
        if count > 0 {
            let index = s.search_selection.min(count - 1);
            if key == "enter" && index >= entries.len() {
                let action = actions[index - entries.len()].clone();
                self.open_shortcut_search(action, window, cx);
            } else if key == "enter" {
                let entry = entries[index];
                let target = if entry.title == L10nKey::SettingsSearchKeybindingsTitle {
                    SettingsSection::Keybindings
                } else {
                    entry.section
                };
                self.navigate_settings(target, Some(entry.title), window, cx);
            } else if let Some(s) = self.active_settings_mut() {
                s.search_selection = if key == "down" {
                    (index + 1).min(count - 1)
                } else {
                    index.saturating_sub(1)
                };
                s.reveal_first_hit.set(true);
                cx.notify();
            }
        }
        cx.stop_propagation();
    }

    /// Opens the shortcut editor filtered to one action.
    pub(crate) fn open_shortcut_search(
        &mut self,
        action: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_settings_edits_resolved(window, cx, move |this, window, cx| {
            this.navigate_settings(SettingsSection::Keybindings, None, window, cx);
            if let Some(s) = this.active_settings() {
                s.shortcut_search
                    .clone()
                    .update(cx, |s, cx| s.set_value(action, window, cx));
            }
        });
    }

    /// Search results and the "modified" filter: the matching rows, live,
    /// grouped under the page each lives on.
    fn render_settings_results(
        &self,
        cx: &mut Context<Self>,
    ) -> (String, Option<String>, AnyElement) {
        let tk = Tk::of(cx);
        let Some(state) = self.active_settings() else {
            return (String::new(), None, div().into_any_element());
        };
        let query = state.search.read(cx).value().trim().to_lowercase();
        let modified_only = state.modified_only;
        let selection = state.search_selection;
        *state.search_rows.borrow_mut() = Some(Vec::new());
        // Build each page that has a match once, and keep only the matching
        // rows. Discarded page chrome never enters the element tree, so the
        // controls keep their usual ids.
        for section in SettingsSection::ALL {
            if !settings_search_entries().iter().any(|entry| {
                entry.section == section
                    && entry_matches(entry, &query)
                    && (!modified_only || entry.modified(cx.global::<Config>()))
            }) {
                continue;
            }
            match section {
                SettingsSection::General => {
                    self.render_settings_general(cx);
                }
                SettingsSection::Appearance => {
                    self.render_settings_appearance(cx);
                }
                SettingsSection::Terminal => {
                    self.render_settings_terminal(cx);
                }
                SettingsSection::KeyboardMouse => {
                    self.render_settings_input(cx);
                }
                SettingsSection::WindowTabs => {
                    self.render_window_preferences(cx);
                }
                SettingsSection::Ssh => {
                    self.render_ssh_connection_rows(cx);
                }
                SettingsSection::Agents => {
                    self.render_command_line_rows(cx);
                }
                SettingsSection::About => {
                    self.render_settings_about(cx);
                }
                SettingsSection::Keybindings => {}
            }
        }
        let mut controls = self
            .active_settings()
            .unwrap()
            .search_rows
            .borrow_mut()
            .take()
            .unwrap_or_default();
        let matches = search_result_order(&query, modified_only, cx.global::<Config>());
        let actions = search_result_actions(&query, modified_only);

        let mut groups: Vec<AnyElement> = Vec::new();
        let mut index = 0usize;
        for section in SettingsSection::ALL {
            let here: Vec<&&SearchEntry> =
                matches.iter().filter(|e| e.section == section).collect();
            if here.is_empty() {
                continue;
            }
            let mut rows = Vec::new();
            for entry in here {
                let title = entry.title;
                let target = if title == L10nKey::SettingsSearchKeybindingsTitle {
                    SettingsSection::Keybindings
                } else {
                    entry.section
                };
                let control = controls
                    .iter()
                    .position(|(key, _)| *key == title)
                    .map(|i| controls.remove(i).1);
                let selected = index == selection;
                let row = control.unwrap_or_else(|| {
                    self.settings_row(
                        t(title),
                        entry.description(),
                        kit::button(
                            SharedString::from(format!("search-open-{title:?}")),
                            t(L10nKey::SettingsOpenSetting),
                            BtnKind::Secondary,
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.navigate_settings(target, Some(title), window, cx)
                        }))
                        .into_any_element(),
                        cx,
                    )
                    .into_any_element()
                });
                rows.push(
                    div()
                        .id(SharedString::from(format!("search-result-{title:?}")))
                        .when(selected, |d| {
                            d.bg(tk.k04).rounded(px(7.)).mx(px(-8.)).px(px(8.))
                        })
                        .when_some(
                            self.active_settings()
                                .filter(|_| selected)
                                .map(|s| s.search_anchor.clone()),
                            |d, anchor| d.anchor_scroll(Some(anchor)),
                        )
                        .child(row)
                        .into_any_element(),
                );
                index += 1;
            }
            groups.push(
                v_flex()
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "search-page-{}",
                                section.profile_label()
                            )))
                            .pb(px(8.))
                            .text_size(fs(11.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(tk.heading)
                            .cursor_pointer()
                            .hover(move |s| s.text_color(tk.fg))
                            .child(t(section.title()))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.navigate_settings(section, None, window, cx)
                            })),
                    )
                    .children(rows)
                    .into_any_element(),
            );
        }

        // Action names stay searchable without a whole shortcut editor in the
        // results: each one is a row that opens the editor on it.
        if !actions.is_empty() {
            let effective = crate::ui::keymap::effective_chords(cx);
            let mut rows = Vec::new();
            for action in actions.iter() {
                let (_, label) = crate::ui::keymap::action_entry(action);
                let chords = effective
                    .iter()
                    .find(|(a, _)| a == action)
                    .and_then(|(_, keys)| keys.first().cloned())
                    .unwrap_or_default();
                let selected = index == selection;
                let action = action.clone();
                rows.push(
                    h_flex()
                        .id(SharedString::from(format!("search-shortcut-{index}")))
                        .h(px(34.))
                        .mx(px(-8.))
                        .px(px(8.))
                        .gap(px(16.))
                        .items_center()
                        .rounded(px(7.))
                        .cursor_pointer()
                        .when(selected, |d| d.bg(tk.k04))
                        .hover(move |s| s.bg(tk.k04))
                        .when_some(
                            self.active_settings()
                                .filter(|_| selected)
                                .map(|s| s.search_anchor.clone()),
                            |d, anchor| d.anchor_scroll(Some(anchor)),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(label))
                        .child(Self::keycap_row(&chords, &tk))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_shortcut_search(action.clone(), window, cx)
                        }))
                        .into_any_element(),
                );
                index += 1;
            }
            groups.push(
                v_flex()
                    .child(kit::section_head(
                        t(L10nKey::SettingsSearchKeybindingsTitle),
                        None,
                        &tk,
                    ))
                    .children(rows)
                    .into_any_element(),
            );
        }

        let total = index;
        let title = if modified_only && query.is_empty() {
            t(L10nKey::SettingsModifiedTitle).to_string()
        } else {
            t(L10nKey::SettingsSearchResults).to_string()
        };
        let subtitle = (!(modified_only && query.is_empty()))
            .then(|| t_plural(L10nKey::SettingsMatchCount, total, &[]));
        if groups.is_empty() {
            let text = if modified_only && query.is_empty() {
                t(L10nKey::SettingsNothingModified).to_string()
            } else if modified_only {
                t(L10nKey::SettingsNoModified).to_string()
            } else {
                t_fmt(L10nKey::SettingsNothingMatches, &[("query", &query)])
            };
            groups.push(
                div()
                    .py(px(24.))
                    .text_color(tk.k35)
                    .child(text)
                    .into_any_element(),
            );
        }
        (title, subtitle, Self::settings_page(groups))
    }

    /// A shortcut as keycaps: `⌘ T`, `⌃B then X`.
    pub(crate) fn keycap_row(spec: &str, tk: &Tk) -> Div {
        let chords = crate::ui::keymap::key_chords(spec);
        h_flex()
            .flex_shrink_0()
            .gap(px(5.))
            .items_center()
            .children(chords.into_iter().enumerate().map(|(i, chord)| {
                h_flex()
                    .gap(px(5.))
                    .items_center()
                    .when(i > 0, |d| {
                        d.child(
                            div()
                                .text_size(fs(11.))
                                .text_color(tk.k4)
                                .child(t(L10nKey::SettingsKeyThen)),
                        )
                    })
                    .child(
                        h_flex()
                            .gap(px(2.))
                            .children(chord.into_iter().map(|key| kit::keycap(key, tk))),
                    )
            }))
    }
}

/// The search results in the order the page draws them: grouped by page in
/// nav order, best match first within each.
pub(crate) fn search_result_order<'a>(
    query: &str,
    modified_only: bool,
    cfg: &Config,
) -> Vec<&'a SearchEntry> {
    let mut matches: Vec<&SearchEntry> = settings_search_entries()
        .iter()
        .filter(|entry| entry_matches(entry, query) && (!modified_only || entry.modified(cfg)))
        .collect();
    matches.sort_by_key(|entry| {
        (
            SettingsSection::ALL
                .iter()
                .position(|&s| s == entry.section)
                .unwrap_or(0),
            entry.rank(query),
        )
    });
    matches
}

/// The shortcut actions a query names, after the settings it matched.
pub(crate) fn search_result_actions(query: &str, modified_only: bool) -> Vec<String> {
    if query.is_empty() || modified_only {
        return Vec::new();
    }
    crate::ui::keymap::default_bindings()
        .into_iter()
        .filter(|(action, _)| keybinding_matches_query(action, query))
        .map(|(action, _)| action.to_string())
        .collect()
}

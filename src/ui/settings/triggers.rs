//! Triggers: text typed into a pane when the line before its cursor matches a
//! regular expression. A list of rows, each of which opens into a small form.

use crate::core::config::Trigger;
use crate::terminal::trigger::{escape, unescape};

use super::kit::{self, BtnKind, Tk, fs};
use super::*;

const LABEL_W: f32 = 150.;
const FIELD_W: f32 = 420.;

/// The trigger being edited. `editing` is its place in the config's list, or
/// `None` for one that has not been saved yet.
pub(crate) struct TriggerForm {
    pub(crate) editing: Option<usize>,
    pub(crate) name: Entity<InputState>,
    pub(crate) regex: Entity<InputState>,
    pub(crate) text: Entity<InputState>,
    pub(crate) enabled: bool,
    pub(crate) _subs: Vec<Subscription>,
}

/// What is wrong with a form, if anything: the field and the line to show.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FormError {
    Regex,
    Text,
}

impl TriggerForm {
    /// The trigger the form describes, or why it can't be saved. An empty
    /// regex is not an error to show, but it is nothing to save either.
    fn collect(&self, cx: &App) -> Result<Option<Trigger>, FormError> {
        let regex = self.regex.read(cx).value().to_string();
        if regex::Regex::new(&regex).is_err() {
            return Err(FormError::Regex);
        }
        let text = unescape(&self.text.read(cx).value()).ok_or(FormError::Text)?;
        Ok((!regex.is_empty()).then(|| Trigger {
            name: self.name.read(cx).value().trim().to_string(),
            regex,
            text,
            enabled: self.enabled,
        }))
    }
}

impl Tty7App {
    pub(crate) fn render_settings_triggers(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = Tk::of(cx);
        let triggers = cx.global::<Config>().triggers.clone();
        let editing = self
            .active_settings()
            .and_then(|s| s.trigger_form.as_ref())
            .map(|f| f.editing);

        // A row like any other setting's, so that search finds the page by it.
        let add = kit::button(
            "trigger-add",
            t(L10nKey::SettingsTriggerAdd),
            BtnKind::Secondary,
        )
        .on_click(cx.listener(|this, _, window, cx| {
            // From a search result this is also the way onto the page.
            this.navigate_settings(SettingsSection::Triggers, None, window, cx);
            this.trigger_form_load(None, window, cx)
        }))
        .into_any_element();
        let mut body: Vec<AnyElement> = vec![
            self.settings_row(
                t(L10nKey::SettingsNavTriggers),
                t(L10nKey::SettingsTriggersDesc),
                add,
                cx,
            )
            .into_any_element(),
        ];
        body.extend(triggers.iter().enumerate().map(|(i, trigger)| {
            if editing == Some(Some(i)) {
                self.render_trigger_editor(cx)
            } else {
                self.render_trigger_row(i, trigger, cx)
            }
        }));
        if editing == Some(None) {
            body.push(self.render_trigger_editor(cx));
        } else if triggers.is_empty() {
            body.push(
                div()
                    .py(px(10.))
                    .text_size(fs(12.))
                    .text_color(tk.muted)
                    .child(t(L10nKey::SettingsTriggersEmpty))
                    .into_any_element(),
            );
        }

        Self::settings_page([self.settings_group(None, None, body, cx)])
    }

    fn render_trigger_row(
        &self,
        i: usize,
        trigger: &Trigger,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tk = Tk::of(cx);
        let mono = Tk::mono(cx);
        let title = if trigger.name.is_empty() {
            trigger.regex.clone()
        } else {
            trigger.name.clone()
        };
        let valid = regex::Regex::new(&trigger.regex).is_ok();
        let line = |text: String, color| {
            div()
                .truncate()
                .font_family(mono.clone())
                .text_size(fs(11.5))
                .text_color(color)
                .child(text)
        };
        h_flex()
            .id(SharedString::from(format!("trigger-row-{i}")))
            .min_h(px(52.))
            .py(px(8.))
            .gap(px(12.))
            .items_center()
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .when(!trigger.enabled, |v| v.opacity(0.5))
                    .child(
                        div()
                            .truncate()
                            .text_size(fs(13.))
                            .text_color(tk.fg)
                            .child(title),
                    )
                    .child(line(
                        format!("/{}/", trigger.regex),
                        if valid { tk.muted } else { tk.danger },
                    ))
                    .child(line(
                        format!(
                            "{} {}",
                            t(L10nKey::SettingsTriggerText),
                            escape(&trigger.text)
                        ),
                        tk.muted,
                    )),
            )
            .child(
                kit::switch(SharedString::from(format!("trigger-enabled-{i}")))
                    .checked(trigger.enabled)
                    .on_click(cx.listener(move |this, on: &bool, _w, cx| {
                        let on = *on;
                        this.update_triggers(cx, |triggers| {
                            if let Some(t) = triggers.get_mut(i) {
                                t.enabled = on;
                            }
                        });
                    })),
            )
            .child(
                kit::button(
                    SharedString::from(format!("trigger-edit-{i}")),
                    t(L10nKey::SettingsEditHost),
                    BtnKind::Secondary,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.trigger_form_load(Some(i), window, cx)
                })),
            )
            .into_any_element()
    }

    fn render_trigger_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        let tk = Tk::of(cx);
        let Some(form) = self.active_settings().and_then(|s| s.trigger_form.as_ref()) else {
            return div().into_any_element();
        };
        let saved = form.editing.is_some();
        let confirm_remove = self
            .active_settings()
            .is_some_and(|s| s.trigger_confirm_remove);
        let state = form.collect(cx);
        let error = state.as_ref().err().copied();
        let can_save = matches!(state, Ok(Some(_)));

        let field = |label: &'static str, input: &Entity<InputState>, bad: bool| {
            let focused = self.settings_input_focused(input, cx);
            h_flex()
                .items_center()
                .child(
                    div()
                        .w(px(LABEL_W))
                        .flex_shrink_0()
                        .text_size(fs(12.))
                        .text_color(tk.muted)
                        .child(label),
                )
                .child(
                    kit::text_field(input, focused, bad, &tk, cx)
                        .bg(tk.page)
                        .text_size(fs(11.5))
                        .flex_1()
                        .max_w(px(FIELD_W)),
                )
        };

        v_flex()
            .gap(px(12.))
            .py(px(12.))
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                match ev.keystroke.key.as_str() {
                    "enter" => {
                        this.save_trigger_form(cx);
                        cx.stop_propagation();
                    }
                    "escape" => {
                        this.cancel_trigger_form(cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(field(t(L10nKey::SettingsTriggerName), &form.name, false))
                    .child(field(
                        t(L10nKey::SettingsTriggerPattern),
                        &form.regex,
                        error == Some(FormError::Regex),
                    ))
                    .child(field(
                        t(L10nKey::SettingsTriggerText),
                        &form.text,
                        error == Some(FormError::Text),
                    )),
            )
            .child(
                div()
                    .text_size(fs(11.5))
                    .text_color(tk.muted)
                    .child(t(L10nKey::SettingsTriggerHint)),
            )
            .when_some(error, |v, e| {
                v.child(
                    div()
                        .text_size(fs(12.))
                        .text_color(tk.danger)
                        .child(t(match e {
                            FormError::Regex => L10nKey::SettingsTriggerBadPattern,
                            FormError::Text => L10nKey::SettingsTriggerBadText,
                        })),
                )
            })
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(
                        kit::button("trigger-form-save", t(L10nKey::Save), BtnKind::Primary)
                            .dimmed(!can_save)
                            .on_click(cx.listener(|this, _, _w, cx| this.save_trigger_form(cx))),
                    )
                    .child(
                        kit::button(
                            "trigger-form-cancel",
                            t(L10nKey::Cancel),
                            BtnKind::Secondary,
                        )
                        .on_click(cx.listener(|this, _, _w, cx| this.cancel_trigger_form(cx))),
                    )
                    .child(div().flex_1())
                    .when(saved, |r| {
                        r.child(
                            kit::button(
                                "trigger-form-remove",
                                if confirm_remove {
                                    t(L10nKey::SettingsClickAgainToRemove)
                                } else {
                                    t(L10nKey::SettingsTriggerRemove)
                                },
                                if confirm_remove {
                                    BtnKind::Danger
                                } else {
                                    BtnKind::Link
                                },
                            )
                            .on_click(cx.listener(|this, _, _w, cx| this.remove_trigger_form(cx))),
                        )
                    }),
            )
            .into_any_element()
    }

    /// Opens the form on the trigger at `index`, or on a blank one.
    fn trigger_form_load(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let trigger = index
            .and_then(|i| cx.global::<Config>().triggers.get(i).cloned())
            .unwrap_or_default();
        let seed =
            |value: &str, hint: &'static str, window: &mut Window, cx: &mut Context<Self>| {
                let value = value.to_string();
                cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(hint)
                        .default_value(value)
                })
            };
        let name = seed(&trigger.name, "", window, cx);
        let regex = seed(&trigger.regex, r"(?i)password:\s*$", window, cx);
        let text = seed(&escape(&trigger.text), r"hunter2\r", window, cx);
        // The Save button and the error line follow what is typed.
        let subs = [&name, &regex, &text]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(input, window, |_this, _i, _ev: &InputEvent, _w, cx| {
                    cx.notify()
                })
            })
            .collect();
        if let Some(s) = self.active_settings_mut() {
            s.trigger_form = Some(TriggerForm {
                editing: index,
                name,
                regex,
                text,
                enabled: trigger.enabled,
                _subs: subs,
            });
            s.trigger_confirm_remove = false;
        }
        cx.notify();
    }

    fn save_trigger_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.active_settings().and_then(|s| s.trigger_form.as_ref()) else {
            return;
        };
        let Ok(Some(trigger)) = form.collect(cx) else {
            return;
        };
        let editing = form.editing;
        self.update_triggers(cx, |triggers| {
            match editing.and_then(|i| triggers.get_mut(i)) {
                Some(slot) => *slot = trigger,
                None => triggers.push(trigger),
            }
        });
        self.cancel_trigger_form(cx);
    }

    fn remove_trigger_form(&mut self, cx: &mut Context<Self>) {
        let Some(s) = self.active_settings_mut() else {
            return;
        };
        if !s.trigger_confirm_remove {
            s.trigger_confirm_remove = true;
            cx.notify();
            return;
        }
        let editing = s.trigger_form.as_ref().and_then(|f| f.editing);
        if let Some(i) = editing {
            self.update_triggers(cx, |triggers| {
                if i < triggers.len() {
                    triggers.remove(i);
                }
            });
        }
        self.cancel_trigger_form(cx);
    }

    fn cancel_trigger_form(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.active_settings_mut() {
            s.trigger_form = None;
            s.trigger_confirm_remove = false;
        }
        cx.notify();
    }
}

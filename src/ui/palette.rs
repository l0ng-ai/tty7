//! Search Everywhere's chord opens the palette from wherever focus is, and an
//! open palette keeps every workspace key for itself.
//!
//! Both run in a keystroke interceptor, which gpui calls before it matches a
//! single binding. Bound the ordinary way the chord missed focus in the
//! Settings window (its own window, whose tree has no palette listeners), and
//! while the palette was open any app binding it did not handle — ⌘D, ⌘W,
//! ⌘1 — fell through to the workspace behind it.

use gpui::{
    Action, App, Entity, Global, KeyBinding, Keystroke, KeystrokeEvent, Subscription, Window,
};

use crate::core::actions::{
    EditorGoToSymbol, HideApp, MinimizeWindow, QuickOpenFile, Quit, TogglePalette,
};
use crate::ui::app::Tty59App;
use crate::ui::search::{KEY_CONTEXT, SearchTab};
use crate::ui::settings_window::SettingsWindow;

struct Interceptor {
    _sub: Subscription,
}
impl Global for Interceptor {}

pub(crate) fn init(cx: &mut App) {
    if cx.has_global::<Interceptor>() {
        return;
    }
    let sub = cx.intercept_keystrokes(intercept);
    cx.set_global(Interceptor { _sub: sub });
}

/// Whether `ks` is `TogglePalette`'s chord as the keymap has it now, so a
/// rebinding moves it and an unbound one opens nothing.
fn is_palette_chord(ks: &Keystroke, cx: &App) -> bool {
    cx.key_bindings()
        .borrow()
        .bindings_for_action(&TogglePalette)
        .any(|b| matches!(b.keystrokes(), [only] if ks.should_match(only)))
}

/// The query field's own editing chords, which the modal rule leaves alone.
/// Shift only with Z (redo): ⌘⇧A is New Agent Tab on macOS, and any other
/// shifted letter is the workspace's to bind.
fn edits_text(ks: &Keystroke) -> bool {
    let m = &ks.modifiers;
    m.secondary()
        && !m.alt
        && match ks.key.as_str() {
            "a" | "c" | "v" | "x" => !m.shift,
            "z" => true,
            _ => false,
        }
}

fn recording_a_shortcut(app: &Entity<Tty59App>, cx: &App) -> bool {
    app.read(cx)
        .active_settings()
        .is_some_and(|s| s.recording.is_some())
}

fn intercept(ev: &KeystrokeEvent, window: &mut Window, cx: &mut App) {
    let Some(view) = window
        .root::<gpui_component::Root>()
        .flatten()
        .map(|root| root.read(cx).view().clone())
    else {
        return;
    };
    let ks = &ev.keystroke;
    if let Ok(settings) = view.clone().downcast::<SettingsWindow>() {
        from_settings(&settings, ks, cx);
        return;
    }
    let Ok(app) = view.downcast::<Tty59App>() else {
        return;
    };
    // Recording a keybinding takes every key, the palette's included.
    if recording_a_shortcut(&app, cx) {
        return;
    }
    if is_palette_chord(ks, cx) {
        cx.stop_propagation();
        app.update(cx, |app, cx| app.toggle_search(window, cx));
        return;
    }
    let Some(search) = app.read(cx).search.clone() else {
        return;
    };
    // Focus got out from under the palette: the key would land behind it.
    if !ev.context_stack.iter().any(|c| c.contains(KEY_CONTEXT)) {
        cx.stop_propagation();
        search.update(cx, |search, cx| search.focus(window, cx));
        return;
    }
    if edits_text(ks) {
        return;
    }
    let (bindings, _) = cx
        .key_bindings()
        .borrow()
        .bindings_for_input(std::slice::from_ref(ks), &ev.context_stack);
    // The palette's own chords: Go to File's, and Go to Symbol's while it shows.
    let tab = search.read(cx).tab();
    let owned = |b: &KeyBinding| {
        b.action().partial_eq(&QuickOpenFile)
            || (tab == SearchTab::Symbols && b.action().partial_eq(&EditorGoToSymbol))
    };
    if bindings.first().is_some_and(owned) {
        return;
    }
    // A binding with no context is the workspace's (split, close tab, …).
    let workspace = bindings.iter().any(|b| {
        b.predicate().is_none()
            && ![&Quit as &dyn Action, &HideApp, &MinimizeWindow]
                .iter()
                .any(|keep| b.action().partial_eq(*keep))
    });
    if workspace {
        cx.stop_propagation();
    }
}

/// Settings has a window of its own: the palette opens over the workspace
/// that owns it, brought forward.
fn from_settings(settings: &Entity<SettingsWindow>, ks: &Keystroke, cx: &mut App) {
    let Some(app) = settings.read(cx).app.upgrade() else {
        return;
    };
    if recording_a_shortcut(&app, cx) || !is_palette_chord(ks, cx) {
        return;
    }
    let Some((_, owner)) = app.read(cx).settings_window else {
        return;
    };
    cx.stop_propagation();
    cx.defer(move |cx| {
        let _ = owner.update(cx, |_, window, cx| {
            window.activate_window();
            app.update(cx, |app, cx| {
                if app.search.is_none() {
                    app.toggle_search(window, cx);
                }
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;

    use gpui::{Focusable as _, TestAppContext, VisualTestContext};

    use super::*;
    use crate::core::config::RightPanelTab;
    use crate::daemon::protocol::ClientMsg;
    use crate::daemon::transport::Stream;
    use crate::ui::app::test_window::harness_with_tabs;
    use crate::ui::keymap::effective_key;
    use crate::ui::settings::SettingsSection;

    fn key(action: &str, vcx: &mut VisualTestContext) -> String {
        vcx.update(|_, cx| effective_key(action, cx))
            .unwrap_or_else(|| panic!("{action} is bound"))
    }

    fn showing(app: &Entity<Tty59App>, vcx: &mut VisualTestContext) -> Option<SearchTab> {
        app.read_with(vcx, |app, cx| app.search.as_ref().map(|s| s.read(cx).tab()))
    }

    /// The first thing the pane sent its daemon, if it sent anything.
    fn pane_input(daemon: &mut Stream) -> Option<Vec<u8>> {
        daemon
            .set_read_timeout(Some(std::time::Duration::from_millis(250)))
            .unwrap();
        loop {
            match ClientMsg::read(daemon) {
                Ok(ClientMsg::Input(bytes)) => return Some(bytes),
                Ok(_) => continue,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return None;
                }
                Err(e) => panic!("pane socket failed: {e}"),
            }
        }
    }

    #[gpui::test]
    fn the_chord_opens_the_palette_from_the_side_panel_and_settings(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);
        app.update(&mut vcx, |app, cx| {
            app.right_panel_visible = true;
            app.right_panel_tab = RightPanelTab::Files;
            cx.notify();
        });
        vcx.run_until_parked();
        let chord = key("TogglePalette", &mut vcx);
        type Input = fn(&Tty59App) -> Entity<gpui_component::input::InputState>;
        let side_panel: Input = |app| app.file_search.clone();
        let settings: Input = |app| app.active_settings().unwrap().search.clone();
        for (place, input) in [("side panel", side_panel), ("settings", settings)] {
            if place == "settings" {
                app.update_in(&mut vcx, |app, window, cx| {
                    app.open_settings_section(SettingsSection::General, window, cx)
                });
                vcx.run_until_parked();
            }
            app.update_in(&mut vcx, |app, window, cx| {
                input(app).update(cx, |input, cx| input.focus(window, cx));
            });
            vcx.run_until_parked();
            let focused = app.read_with(&vcx, |app, cx| input(app).read(cx).focus_handle(cx));
            assert!(
                vcx.update(|window, _| focused.is_focused(window)),
                "the {place} field has focus"
            );
            vcx.simulate_keystrokes(&chord);
            vcx.run_until_parked();
            assert_eq!(
                showing(&app, &mut vcx),
                Some(SearchTab::All),
                "{chord} from the {place}"
            );
            vcx.simulate_keystrokes(&chord);
            vcx.run_until_parked();
            assert_eq!(showing(&app, &mut vcx), None, "{chord} again closes it");
        }
    }

    #[gpui::test]
    fn keys_while_the_palette_is_open_do_not_reach_the_workspace(cx: &mut TestAppContext) {
        let (app, mut vcx, mut streams) = harness_with_tabs(cx, 2);
        let daemon = &mut streams[0];
        // Typed at the pane, a key reaches its shell.
        app.update_in(&mut vcx, |app, window, cx| app.focus_active(window, cx));
        vcx.run_until_parked();
        vcx.simulate_keystrokes("q");
        vcx.run_until_parked();
        assert_eq!(pane_input(daemon).as_deref(), Some(&b"q"[..]));

        let palette = key("TogglePalette", &mut vcx);
        vcx.simulate_keystrokes(&palette);
        vcx.run_until_parked();
        // A workspace chord the query field does not handle stays in the palette.
        let second_tab = key("ActivateTab2", &mut vcx);
        vcx.simulate_keystrokes(&second_tab);
        vcx.run_until_parked();
        assert_eq!(app.read_with(&vcx, |app, _| app.active), 0, "no tab switch");
        // Nor one on a shifted editing letter (⌘⇧A is New Agent Tab on macOS):
        // only the unshifted chords, and redo, are the field's.
        vcx.update(|_, cx| {
            cx.bind_keys([KeyBinding::new(
                "secondary-shift-a",
                crate::core::actions::ActivateTab2,
                None,
            )])
        });
        vcx.simulate_keystrokes("secondary-shift-a");
        vcx.run_until_parked();
        assert_eq!(app.read_with(&vcx, |app, _| app.active), 0, "no tab switch");
        // Focus pulled back onto the pane: the palette takes it back, and the key.
        app.update_in(&mut vcx, |app, window, cx| app.focus_active(window, cx));
        vcx.run_until_parked();
        vcx.simulate_keystrokes("x");
        vcx.run_until_parked();
        assert_eq!(showing(&app, &mut vcx), Some(SearchTab::All));
        assert_eq!(pane_input(daemon), None, "nothing reached the pane");
    }
}

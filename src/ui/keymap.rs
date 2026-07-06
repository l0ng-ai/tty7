//! Keymap and global-action wiring: the default action→keystroke table, merging
//! the user's config overrides on top, and the one-time install of keybindings,
//! the menu bar, and global actions at startup. Kept separate from the window
//! shell so `app.rs` stays focused on tab/pane orchestration.

use gpui::{App, KeyBinding};

use crate::core::actions::*;
use crate::core::config::Config;
use crate::ui::theme::set_menus;

/// Install the application menu bar, keybindings, and global actions.
/// Call once at startup with the app context.
pub fn init(cx: &mut App) {
    // Start from the built-in defaults, then layer the user's `keybindings`
    // overrides on top (remapping an action's key, or adding an entry for an
    // action we'll validate below).
    let overrides = cx.global::<Config>().keybindings.clone();
    let mut effective: Vec<(String, String)> = default_bindings()
        .into_iter()
        .map(|(a, k)| (a.to_string(), k.to_string()))
        .collect();
    for (action, key) in overrides {
        match effective.iter_mut().find(|(a, _)| *a == action) {
            Some(slot) => slot.1 = key,
            None => effective.push((action, key)),
        }
    }

    let mut bindings = Vec::new();
    for (action, key) in &effective {
        if !keystroke_is_valid(key) {
            log::warn!("ignoring keybinding for '{action}': invalid keystroke '{key}'");
            continue;
        }
        match make_binding(action, key) {
            Some(b) => bindings.push(b),
            None => log::warn!("ignoring keybinding: unknown action '{action}'"),
        }
    }
    // `+` arrives as `=`, so keep a fixed `secondary-+` alias for zoom-in
    // alongside whatever IncreaseFontSize is bound to.
    bindings.push(KeyBinding::new("secondary-+", IncreaseFontSize, None));
    // Tab / Shift-Tab must reach the shell (completion, back-tab) — but
    // gpui-component's `Root` binds them to focus navigation in the global "Root"
    // context, which would otherwise swallow the key before it hits the terminal.
    // We rebind them in the deeper "Terminal" context so GPUI's depth-ordered
    // dispatch picks ours first; the handlers in `terminal::view` write to the PTY.
    bindings.push(KeyBinding::new("tab", SendTab, Some("Terminal")));
    bindings.push(KeyBinding::new("shift-tab", SendBackTab, Some("Terminal")));
    cx.bind_keys(bindings);

    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
    set_menus(cx);
}

/// The built-in action → default-keystroke table. The single source of truth for
/// both the default keymap and the names the user can override in config.
pub(crate) fn default_bindings() -> Vec<(&'static str, &'static str)> {
    // `secondary-` is gpui's cross-platform modifier: ⌘ on macOS, Ctrl elsewhere
    // (see `Keystroke::parse`). Using it keeps the same muscle memory on Windows
    // and Linux without binding to the Win/Super key, which the OS reserves.
    vec![
        ("NewTab", "secondary-t"),
        ("CloseActiveTab", "secondary-w"),
        ("SplitRight", "secondary-d"),
        ("SplitDown", "secondary-shift-d"),
        ("FocusNextPane", "secondary-]"),
        ("FocusPrevPane", "secondary-["),
        ("IncreaseFontSize", "secondary-="),
        ("DecreaseFontSize", "secondary--"),
        ("ResetFontSize", "secondary-0"),
        ("TogglePalette", "secondary-p"),
        ("ReopenClosedTab", "secondary-shift-t"),
        ("ToggleMaximizePane", "secondary-enter"),
        ("OpenSettings", "secondary-,"),
        ("Quit", "secondary-q"),
    ]
}

/// The effective keystroke for an action: the user's override if present,
/// otherwise the built-in default. `None` if the action has no binding at all.
/// Used to surface shortcut hints in the UI (command palette, settings).
pub(crate) fn effective_key(action: &str, cx: &App) -> Option<String> {
    if let Some(key) = cx.global::<Config>().keybindings.get(action) {
        return Some(key.clone());
    }
    default_bindings()
        .into_iter()
        .find(|(a, _)| *a == action)
        .map(|(_, k)| k.to_string())
}

/// Split a keybinding spec ("secondary-shift-d", "secondary--") into display
/// tokens, mapping modifiers to per-platform labels (mac glyphs vs. Windows/Linux
/// words). Modifiers always lead; whatever remains is the key itself — which may
/// be "-", so we can't simply split on '-'.
pub(crate) fn key_tokens(spec: &str) -> Vec<String> {
    // `secondary` is gpui's portable modifier (⌘ on mac, Ctrl elsewhere); `cmd`
    // is the literal platform key (⌘ on mac, the Win/Super key elsewhere).
    #[cfg(target_os = "macos")]
    const MODS: [(&str, &str); 6] = [
        ("secondary", "⌘"),
        ("cmd", "⌘"),
        ("ctrl", "⌃"),
        ("alt", "⌥"),
        ("shift", "⇧"),
        ("fn", "fn"),
    ];
    #[cfg(not(target_os = "macos"))]
    const MODS: [(&str, &str); 6] = [
        ("secondary", "Ctrl"),
        ("cmd", "Win"),
        ("ctrl", "Ctrl"),
        ("alt", "Alt"),
        ("shift", "Shift"),
        ("fn", "Fn"),
    ];
    let mut rest = spec;
    let mut tokens = Vec::new();
    'outer: loop {
        for (name, glyph) in MODS {
            let prefix = format!("{name}-");
            // Only consume a modifier if something non-empty follows it, so the
            // trailing key (even "-") is always preserved as the final token.
            if let Some(stripped) = rest.strip_prefix(&prefix) {
                if !stripped.is_empty() {
                    tokens.push(glyph.to_string());
                    rest = stripped;
                    continue 'outer;
                }
            }
        }
        break;
    }
    tokens.push(key_glyph(rest));
    tokens
}

/// Map a bare (non-modifier) key to its display glyph: word keys to symbols,
/// single letters uppercased, punctuation passed through.
fn key_glyph(key: &str) -> String {
    match key {
        "enter" | "return" => "⏎".into(),
        "tab" => "⇥".into(),
        "space" => "Space".into(),
        "escape" | "esc" => "⎋".into(),
        "backspace" => "⌫".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "-" => "−".into(), // typographic minus, not the separator hyphen
        other => other.to_uppercase(),
    }
}

/// True if every whitespace-separated chord in `s` parses as a gpui keystroke.
/// We pre-validate so `KeyBinding::new` (which panics on a parse error) is only
/// ever handed strings we know are good.
fn keystroke_is_valid(s: &str) -> bool {
    let mut any = false;
    for token in s.split_whitespace() {
        any = true;
        if gpui::Keystroke::parse(token).is_err() {
            return false;
        }
    }
    any
}

/// Build a `KeyBinding` for a known action name + (already-validated) keystroke.
/// Returns `None` for an unrecognized action name.
fn make_binding(action: &str, keystroke: &str) -> Option<KeyBinding> {
    Some(match action {
        "NewTab" => KeyBinding::new(keystroke, NewTab, None),
        "CloseActiveTab" => KeyBinding::new(keystroke, CloseActiveTab, None),
        "SplitRight" => KeyBinding::new(keystroke, SplitRight, None),
        "SplitDown" => KeyBinding::new(keystroke, SplitDown, None),
        "FocusNextPane" => KeyBinding::new(keystroke, FocusNextPane, None),
        "FocusPrevPane" => KeyBinding::new(keystroke, FocusPrevPane, None),
        "IncreaseFontSize" => KeyBinding::new(keystroke, IncreaseFontSize, None),
        "DecreaseFontSize" => KeyBinding::new(keystroke, DecreaseFontSize, None),
        "ResetFontSize" => KeyBinding::new(keystroke, ResetFontSize, None),
        "TogglePalette" => KeyBinding::new(keystroke, TogglePalette, None),
        "ReopenClosedTab" => KeyBinding::new(keystroke, ReopenClosedTab, None),
        "ToggleMaximizePane" => KeyBinding::new(keystroke, ToggleMaximizePane, None),
        "OpenSettings" => KeyBinding::new(keystroke, OpenSettings, None),
        "Quit" => KeyBinding::new(keystroke, Quit, None),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The `secondary` modifier renders differently per platform: ⌘ on macOS,
    // "Ctrl" elsewhere. Pick the expected label for the host running the test.
    #[cfg(target_os = "macos")]
    const SECONDARY: &str = "⌘";
    #[cfg(not(target_os = "macos"))]
    const SECONDARY: &str = "Ctrl";
    #[cfg(target_os = "macos")]
    const SHIFT: &str = "⇧";
    #[cfg(not(target_os = "macos"))]
    const SHIFT: &str = "Shift";

    #[test]
    fn key_tokens_maps_modifiers_to_glyphs() {
        assert_eq!(key_tokens("secondary-t"), vec![SECONDARY, "T"]);
        assert_eq!(key_tokens("secondary-shift-d"), vec![SECONDARY, SHIFT, "D"]);
        assert_eq!(key_tokens("secondary-enter"), vec![SECONDARY, "⏎"]);
    }

    #[test]
    fn key_tokens_keeps_the_minus_key_as_the_final_token() {
        // "secondary--" is the secondary key + the "-" key; a naive split on '-'
        // would drop the trailing key.
        assert_eq!(key_tokens("secondary--"), vec![SECONDARY, "−"]);
        assert_eq!(key_tokens("secondary-="), vec![SECONDARY, "="]);
        assert_eq!(key_tokens("secondary-,"), vec![SECONDARY, ","]);
    }
}

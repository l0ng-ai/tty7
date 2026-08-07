use gpui::{App, Global, KeyBinding, Keystroke, NoAction};

use crate::core::actions::*;
use crate::core::config::Config;
use crate::terminal::view::{
    ClearScrollback, CopyText, FindInTerminal, FindNext, FindPrevious, InsertNewline,
    InsertNewlineFallback, PasteText,
};
use crate::ui::theme::set_menus;

#[derive(Default)]
struct BoundKeystrokes(Vec<(String, Option<&'static str>)>);
impl Global for BoundKeystrokes {}

pub fn init(cx: &mut App) {
    let effective = effective_bindings(cx);
    let mut bindings = action_bindings(&effective);
    bindings.push(KeyBinding::new("secondary-+", IncreaseFontSize, None));
    bindings.push(KeyBinding::new("tab", SendTab, Some("Terminal")));
    bindings.push(KeyBinding::new("shift-tab", SendBackTab, Some("Terminal")));
    cx.bind_keys(bindings);
    cx.set_global(BoundKeystrokes(bound_keystrokes(&effective)));

    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
    set_menus(cx);
}

pub fn rebind(cx: &mut App) {
    let previous = cx
        .try_global::<BoundKeystrokes>()
        .map(|b| b.0.clone())
        .unwrap_or_default();
    let effective = effective_bindings(cx);

    let mut bindings: Vec<KeyBinding> = previous
        .iter()
        .filter(|(k, _)| keystroke_is_valid(k))
        .map(|(k, ctx)| KeyBinding::new(k, NoAction {}, *ctx))
        .collect();
    bindings.extend(action_bindings(&effective));
    cx.bind_keys(bindings);
    cx.set_global(BoundKeystrokes(bound_keystrokes(&effective)));

    set_menus(cx);
}

const INSERT_NEWLINE_DEFAULT: &str = "shift-enter";

const INSERT_NEWLINE_ALT_DEFAULT: &str = "alt-enter";

const PASTE_ALT_DEFAULT: &str = "shift-insert";

fn paste_text_default() -> &'static str {
    per_platform("", "ctrl-shift-v")
}

fn extra_defaults() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "InsertNewline",
            INSERT_NEWLINE_DEFAULT,
            INSERT_NEWLINE_ALT_DEFAULT,
        ),
        ("PasteText", paste_text_default(), PASTE_ALT_DEFAULT),
    ]
}

fn extra_keystrokes(effective: &[(String, String)]) -> Vec<(&'static str, &'static str)> {
    extra_defaults()
        .into_iter()
        .filter(|(action, primary, _)| {
            !primary.is_empty() && effective.iter().any(|(a, k)| a == action && k == primary)
        })
        .map(|(action, _, extra)| (action, extra))
        .collect()
}

fn is_default_insert_newline_binding(action: &str, key: &str) -> bool {
    action == "InsertNewline" && key == INSERT_NEWLINE_DEFAULT
}

pub(crate) fn extra_bindings(cx: &App) -> Vec<(String, String)> {
    extra_keystrokes(&effective_bindings(cx))
        .into_iter()
        .map(|(a, k)| (a.to_string(), k.to_string()))
        .collect()
}

fn action_bindings(effective: &[(String, String)]) -> Vec<KeyBinding> {
    let mut bindings = Vec::new();
    for (action, key) in extra_keystrokes(effective) {
        if let Some(b) = make_binding(action, key) {
            bindings.push(b);
        }
    }
    for (action, key) in effective {
        if key.is_empty() {
            continue;
        }
        if !keystroke_is_valid(key) {
            log::warn!("ignoring keybinding for '{action}': invalid keystroke '{key}'");
            continue;
        }
        match make_binding(action, key) {
            Some(b) => {
                bindings.push(b);
                if is_default_insert_newline_binding(action, key) {
                    bindings.push(KeyBinding::new(
                        INSERT_NEWLINE_DEFAULT,
                        InsertNewlineFallback,
                        Some("Terminal"),
                    ));
                }
            }
            None => log::warn!("ignoring keybinding: unknown action '{action}'"),
        }
    }
    bindings
}

fn bound_keystrokes(effective: &[(String, String)]) -> Vec<(String, Option<&'static str>)> {
    let extras = extra_keystrokes(effective);
    effective
        .iter()
        .map(|(a, k)| (a.as_str(), k.as_str()))
        .chain(extras.iter().map(|(a, k)| (*a, *k)))
        .filter(|(_, k)| !k.is_empty() && keystroke_is_valid(k))
        .map(|(a, k)| (k.to_string(), action_context(a)))
        .collect()
}

fn per_platform(mac: &'static str, other: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        mac
    } else {
        other
    }
}

pub(crate) fn default_bindings() -> Vec<(&'static str, &'static str)> {
    vec![
        ("NewTab", per_platform("secondary-t", "secondary-shift-t")),
        ("NewWorkspace", "secondary-shift-n"),
        (
            "CloseActiveTab",
            per_platform("secondary-w", "secondary-shift-w"),
        ),
        ("RenameTab", ""),
        ("NewWorktreeTab", ""),
        ("CloseOtherTabs", ""),
        ("CloseTabsToTheRight", ""),
        ("CopyWorkingDirectory", ""),
        ("MarkTabUnread", ""),
        ("ForkAgentSession", ""),
        ("ForkAgentSessionRight", ""),
        ("ForkAgentSessionLeft", ""),
        ("ForkAgentSessionDown", ""),
        ("ForkAgentSessionUp", ""),
        ("CopyAgentSessionId", ""),
        ("StopWorkspace", ""),
        ("DeleteWorkspace", ""),
        ("RenameWorkspace", ""),
        ("ToggleSwitcher", "secondary-shift-o"),
        (
            "SplitRight",
            per_platform("secondary-d", "secondary-shift-d"),
        ),
        (
            "SplitDown",
            per_platform("secondary-shift-d", "secondary-alt-shift-d"),
        ),
        (
            "FocusNextPane",
            per_platform("secondary-]", "secondary-shift-]"),
        ),
        (
            "FocusPrevPane",
            per_platform("secondary-[", "secondary-shift-["),
        ),
        (
            "FocusPaneLeft",
            per_platform("secondary-alt-left", "alt-left"),
        ),
        (
            "FocusPaneRight",
            per_platform("secondary-alt-right", "alt-right"),
        ),
        ("FocusPaneUp", per_platform("secondary-alt-up", "alt-up")),
        (
            "FocusPaneDown",
            per_platform("secondary-alt-down", "alt-down"),
        ),
        ("ResizePaneLeft", ""),
        ("ResizePaneRight", ""),
        ("ResizePaneUp", ""),
        ("ResizePaneDown", ""),
        ("SwapPaneNext", ""),
        ("SwapPanePrev", ""),
        ("NextTab", "ctrl-tab"),
        ("PrevTab", "ctrl-shift-tab"),
        ("ActivateTab1", per_platform("secondary-1", "alt-1")),
        ("ActivateTab2", per_platform("secondary-2", "alt-2")),
        ("ActivateTab3", per_platform("secondary-3", "alt-3")),
        ("ActivateTab4", per_platform("secondary-4", "alt-4")),
        ("ActivateTab5", per_platform("secondary-5", "alt-5")),
        ("ActivateTab6", per_platform("secondary-6", "alt-6")),
        ("ActivateTab7", per_platform("secondary-7", "alt-7")),
        ("ActivateTab8", per_platform("secondary-8", "alt-8")),
        ("ActivateTab9", per_platform("secondary-9", "alt-9")),
        ("SelectWorkspace1", ""),
        ("SelectWorkspace2", ""),
        ("SelectWorkspace3", ""),
        ("SelectWorkspace4", ""),
        ("SelectWorkspace5", ""),
        ("SelectWorkspace6", ""),
        ("SelectWorkspace7", ""),
        ("SelectWorkspace8", ""),
        ("SelectWorkspace9", ""),
        ("IncreaseFontSize", "secondary-="),
        ("DecreaseFontSize", "secondary--"),
        ("ResetFontSize", "secondary-0"),
        (
            "TogglePalette",
            per_platform("secondary-p", "secondary-shift-p"),
        ),
        (
            "ReopenClosedTab",
            per_platform("secondary-shift-t", "alt-shift-t"),
        ),
        ("ToggleMaximizePane", "secondary-shift-enter"),
        ("ToggleFullscreen", per_platform("secondary-enter", "f11")),
        ("ToggleTabSidebar", ""),
        (
            "ToggleLeftPanel",
            per_platform("secondary-b", "secondary-shift-b"),
        ),
        (
            "ToggleRightPanel",
            per_platform("secondary-j", "secondary-shift-j"),
        ),
        (
            "FindInTerminal",
            if cfg!(target_os = "macos") {
                "secondary-f"
            } else {
                "ctrl-shift-f"
            },
        ),
        (
            "FindNext",
            if cfg!(target_os = "macos") {
                "secondary-g"
            } else {
                "f3"
            },
        ),
        (
            "FindPrevious",
            if cfg!(target_os = "macos") {
                "secondary-shift-g"
            } else {
                "shift-f3"
            },
        ),
        (
            "ClearScrollback",
            per_platform("secondary-k", "secondary-shift-k"),
        ),
        ("InsertNewline", INSERT_NEWLINE_DEFAULT),
        ("CopyText", per_platform("", "ctrl-shift-c")),
        ("PasteText", paste_text_default()),
        ("OpenSettings", "secondary-,"),
        (
            "ShowKeyboardShortcuts",
            if cfg!(target_os = "macos") {
                "secondary-/"
            } else {
                ""
            },
        ),
        ("About", ""),
        ("CheckForUpdates", ""),
        ("OpenDocumentation", ""),
        ("OpenDiscord", ""),
        ("ReportIssue", ""),
        (
            "HideApp",
            if cfg!(target_os = "macos") {
                "secondary-h"
            } else {
                ""
            },
        ),
        (
            "HideOthers",
            if cfg!(target_os = "macos") {
                "secondary-alt-h"
            } else {
                ""
            },
        ),
        ("ShowAll", ""),
        (
            "MinimizeWindow",
            if cfg!(target_os = "macos") {
                "secondary-m"
            } else {
                ""
            },
        ),
        ("ZoomWindow", ""),
        ("ToggleSftp", ""),
        ("ShowSshForwards", ""),
        ("ToggleCodePanel", "secondary-shift-e"),
        ("EditorSave", "secondary-s"),
        ("OpenSshProfiles", ""),
        ("RestartSshSession", "secondary-shift-r"),
        ("Quit", per_platform("secondary-q", "secondary-shift-q")),
    ]
}

pub(crate) fn effective_bindings(cx: &App) -> Vec<(String, String)> {
    let cfg = cx.global::<Config>();
    let mut effective: Vec<(String, String)> = default_bindings()
        .into_iter()
        .map(|(a, k)| (a.to_string(), k.to_string()))
        .collect();
    for (action, key) in preset_bindings(&cfg.keybinding_preset, &cfg.prefix) {
        set_binding(&mut effective, &action, key);
    }
    for (action, key) in &cfg.keybindings {
        set_binding(&mut effective, action, key.clone());
    }
    effective
}

fn set_binding(effective: &mut [(String, String)], action: &str, key: String) {
    if let Some(slot) = effective.iter_mut().find(|(a, _)| a == action) {
        slot.1 = key;
    }
}

fn preset_bindings(preset: &str, prefix: &str) -> Vec<(String, String)> {
    match preset {
        "tmux" => tmux_preset(prefix),
        _ => Vec::new(),
    }
}

fn tmux_preset(prefix: &str) -> Vec<(String, String)> {
    let p = |key: &str| format!("{prefix} {key}");
    [
        ("NewTab", p("c")),
        ("CloseActiveTab", p("x")),
        ("SplitRight", p("%")),
        ("SplitDown", p("\"")),
        ("FocusPaneLeft", p("left")),
        ("FocusPaneRight", p("right")),
        ("FocusPaneUp", p("up")),
        ("FocusPaneDown", p("down")),
        ("ResizePaneLeft", p("ctrl-left")),
        ("ResizePaneRight", p("ctrl-right")),
        ("ResizePaneUp", p("ctrl-up")),
        ("ResizePaneDown", p("ctrl-down")),
        ("SwapPanePrev", p("{")),
        ("SwapPaneNext", p("}")),
        ("ToggleMaximizePane", p("z")),
        ("FocusNextPane", p("o")),
        ("FocusPrevPane", p(";")),
        ("NextTab", p("n")),
        ("PrevTab", p("p")),
        ("ActivateTab1", p("1")),
        ("ActivateTab2", p("2")),
        ("ActivateTab3", p("3")),
        ("ActivateTab4", p("4")),
        ("ActivateTab5", p("5")),
        ("ActivateTab6", p("6")),
        ("ActivateTab7", p("7")),
        ("ActivateTab8", p("8")),
        ("ActivateTab9", p("9")),
    ]
    .into_iter()
    .map(|(a, k)| (a.to_string(), k))
    .collect()
}

pub(crate) fn effective_key(action: &str, cx: &App) -> Option<String> {
    effective_bindings(cx)
        .into_iter()
        .find(|(a, _)| a == action)
        .map(|(_, k)| k)
        .filter(|k| !k.is_empty())
}

pub(crate) fn spec_from_keystroke(ks: &Keystroke) -> Option<String> {
    if matches!(
        ks.key.as_str(),
        "shift" | "control" | "alt" | "platform" | "function" | "cmd" | "ctrl"
    ) {
        return None;
    }
    let m = &ks.modifiers;
    let mut parts: Vec<&str> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        if m.platform {
            parts.push("secondary");
        }
        if m.control {
            parts.push("ctrl");
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        if m.control {
            parts.push("secondary");
        }
        if m.platform {
            parts.push("cmd");
        }
    }
    if m.alt {
        parts.push("alt");
    }
    if m.shift {
        parts.push("shift");
    }
    if m.function {
        parts.push("fn");
    }
    let mut spec = String::new();
    for part in parts {
        spec.push_str(part);
        spec.push('-');
    }
    spec.push_str(&ks.key);
    Some(spec)
}

pub(crate) fn key_chords(spec: &str) -> Vec<Vec<String>> {
    spec.split_whitespace().map(key_tokens).collect()
}

/// How to write the "secondary" modifier for this platform — ⌘ on macOS, Ctrl
/// everywhere else. Anything spelling a shortcut out in the UI needs this
/// rather than a literal ⌘.
pub(crate) fn secondary_glyph() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl"
    }
}

pub(crate) fn key_tokens(spec: &str) -> Vec<String> {
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
        "-" => "−".into(),
        other => other.to_uppercase(),
    }
}

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

fn action_context(action: &str) -> Option<&'static str> {
    match action {
        "FindInTerminal" | "FindNext" | "FindPrevious" | "ClearScrollback" | "InsertNewline"
        | "CopyText" | "PasteText" => Some("Terminal"),
        _ => None,
    }
}

fn make_binding(action: &str, keystroke: &str) -> Option<KeyBinding> {
    Some(match action {
        "NewTab" => KeyBinding::new(keystroke, NewTab, None),
        "NewWorkspace" => KeyBinding::new(keystroke, NewWorkspace, None),
        "StopWorkspace" => KeyBinding::new(keystroke, StopWorkspace, None),
        "DeleteWorkspace" => KeyBinding::new(keystroke, DeleteWorkspace, None),
        "RenameWorkspace" => KeyBinding::new(keystroke, RenameWorkspace, None),
        "ToggleSwitcher" => KeyBinding::new(keystroke, ToggleSwitcher, None),
        "CloseActiveTab" => KeyBinding::new(keystroke, CloseActiveTab, None),
        "RenameTab" => KeyBinding::new(keystroke, RenameTab, None),
        "NewWorktreeTab" => KeyBinding::new(keystroke, NewWorktreeTab, None),
        "CloseOtherTabs" => KeyBinding::new(keystroke, CloseOtherTabs, None),
        "CloseTabsToTheRight" => KeyBinding::new(keystroke, CloseTabsToTheRight, None),
        "CopyWorkingDirectory" => KeyBinding::new(keystroke, CopyWorkingDirectory, None),
        "MarkTabUnread" => KeyBinding::new(keystroke, MarkTabUnread, None),
        "ForkAgentSession" => KeyBinding::new(keystroke, ForkAgentSession, None),
        "ForkAgentSessionRight" => KeyBinding::new(keystroke, ForkAgentSessionRight, None),
        "ForkAgentSessionLeft" => KeyBinding::new(keystroke, ForkAgentSessionLeft, None),
        "ForkAgentSessionDown" => KeyBinding::new(keystroke, ForkAgentSessionDown, None),
        "ForkAgentSessionUp" => KeyBinding::new(keystroke, ForkAgentSessionUp, None),
        "CopyAgentSessionId" => KeyBinding::new(keystroke, CopyAgentSessionId, None),
        "SplitRight" => KeyBinding::new(keystroke, SplitRight, None),
        "SplitDown" => KeyBinding::new(keystroke, SplitDown, None),
        "FocusNextPane" => KeyBinding::new(keystroke, FocusNextPane, None),
        "FocusPrevPane" => KeyBinding::new(keystroke, FocusPrevPane, None),
        "FocusPaneLeft" => KeyBinding::new(keystroke, FocusPaneLeft, None),
        "FocusPaneRight" => KeyBinding::new(keystroke, FocusPaneRight, None),
        "FocusPaneUp" => KeyBinding::new(keystroke, FocusPaneUp, None),
        "FocusPaneDown" => KeyBinding::new(keystroke, FocusPaneDown, None),
        "ResizePaneLeft" => KeyBinding::new(keystroke, ResizePaneLeft, None),
        "ResizePaneRight" => KeyBinding::new(keystroke, ResizePaneRight, None),
        "ResizePaneUp" => KeyBinding::new(keystroke, ResizePaneUp, None),
        "ResizePaneDown" => KeyBinding::new(keystroke, ResizePaneDown, None),
        "SwapPaneNext" => KeyBinding::new(keystroke, SwapPaneNext, None),
        "SwapPanePrev" => KeyBinding::new(keystroke, SwapPanePrev, None),
        "NextTab" => KeyBinding::new(keystroke, NextTab, None),
        "PrevTab" => KeyBinding::new(keystroke, PrevTab, None),
        "ActivateTab1" => KeyBinding::new(keystroke, ActivateTab1, None),
        "ActivateTab2" => KeyBinding::new(keystroke, ActivateTab2, None),
        "ActivateTab3" => KeyBinding::new(keystroke, ActivateTab3, None),
        "ActivateTab4" => KeyBinding::new(keystroke, ActivateTab4, None),
        "ActivateTab5" => KeyBinding::new(keystroke, ActivateTab5, None),
        "ActivateTab6" => KeyBinding::new(keystroke, ActivateTab6, None),
        "ActivateTab7" => KeyBinding::new(keystroke, ActivateTab7, None),
        "ActivateTab8" => KeyBinding::new(keystroke, ActivateTab8, None),
        "ActivateTab9" => KeyBinding::new(keystroke, ActivateTab9, None),
        "SelectWorkspace1" => KeyBinding::new(keystroke, SelectWorkspace1, None),
        "SelectWorkspace2" => KeyBinding::new(keystroke, SelectWorkspace2, None),
        "SelectWorkspace3" => KeyBinding::new(keystroke, SelectWorkspace3, None),
        "SelectWorkspace4" => KeyBinding::new(keystroke, SelectWorkspace4, None),
        "SelectWorkspace5" => KeyBinding::new(keystroke, SelectWorkspace5, None),
        "SelectWorkspace6" => KeyBinding::new(keystroke, SelectWorkspace6, None),
        "SelectWorkspace7" => KeyBinding::new(keystroke, SelectWorkspace7, None),
        "SelectWorkspace8" => KeyBinding::new(keystroke, SelectWorkspace8, None),
        "SelectWorkspace9" => KeyBinding::new(keystroke, SelectWorkspace9, None),
        "IncreaseFontSize" => KeyBinding::new(keystroke, IncreaseFontSize, None),
        "DecreaseFontSize" => KeyBinding::new(keystroke, DecreaseFontSize, None),
        "ResetFontSize" => KeyBinding::new(keystroke, ResetFontSize, None),
        "TogglePalette" => KeyBinding::new(keystroke, TogglePalette, None),
        "ReopenClosedTab" => KeyBinding::new(keystroke, ReopenClosedTab, None),
        "ToggleMaximizePane" => KeyBinding::new(keystroke, ToggleMaximizePane, None),
        "ToggleFullscreen" => KeyBinding::new(keystroke, ToggleFullscreen, None),
        "ToggleTabSidebar" => KeyBinding::new(keystroke, ToggleTabSidebar, None),
        "ToggleLeftPanel" => KeyBinding::new(keystroke, ToggleLeftPanel, None),
        "ToggleRightPanel" => KeyBinding::new(keystroke, ToggleRightPanel, None),
        "ShowRightPanelInfo" => KeyBinding::new(keystroke, ShowRightPanelInfo, None),
        "ShowRightPanelChanges" => KeyBinding::new(keystroke, ShowRightPanelChanges, None),
        "ShowRightPanelFiles" => KeyBinding::new(keystroke, ShowRightPanelFiles, None),
        "FindInTerminal" => KeyBinding::new(keystroke, FindInTerminal, action_context(action)),
        "FindNext" => KeyBinding::new(keystroke, FindNext, action_context(action)),
        "FindPrevious" => KeyBinding::new(keystroke, FindPrevious, action_context(action)),
        "ClearScrollback" => KeyBinding::new(keystroke, ClearScrollback, action_context(action)),
        "InsertNewline" => KeyBinding::new(keystroke, InsertNewline, action_context(action)),
        "CopyText" => KeyBinding::new(keystroke, CopyText, action_context(action)),
        "PasteText" => KeyBinding::new(keystroke, PasteText, action_context(action)),
        "OpenSettings" => KeyBinding::new(keystroke, OpenSettings, None),
        "ShowKeyboardShortcuts" => KeyBinding::new(keystroke, ShowKeyboardShortcuts, None),
        "About" => KeyBinding::new(keystroke, About, None),
        "CheckForUpdates" => KeyBinding::new(keystroke, CheckForUpdates, None),
        "OpenDocumentation" => KeyBinding::new(keystroke, OpenDocumentation, None),
        "OpenDiscord" => KeyBinding::new(keystroke, OpenDiscord, None),
        "ReportIssue" => KeyBinding::new(keystroke, ReportIssue, None),
        "HideApp" => KeyBinding::new(keystroke, HideApp, None),
        "HideOthers" => KeyBinding::new(keystroke, HideOthers, None),
        "ShowAll" => KeyBinding::new(keystroke, ShowAll, None),
        "MinimizeWindow" => KeyBinding::new(keystroke, MinimizeWindow, None),
        "ZoomWindow" => KeyBinding::new(keystroke, ZoomWindow, None),
        "ToggleSftp" => KeyBinding::new(keystroke, ToggleSftp, None),
        "ShowSshForwards" => KeyBinding::new(keystroke, ShowSshForwards, None),
        "ToggleCodePanel" => KeyBinding::new(keystroke, ToggleCodePanel, None),
        "EditorSave" => KeyBinding::new(keystroke, EditorSave, None),
        "OpenSshProfiles" => KeyBinding::new(keystroke, OpenSshProfiles, None),
        "RestartSshSession" => KeyBinding::new(keystroke, RestartSshSession, None),
        "Quit" => KeyBinding::new(keystroke, Quit, None),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    const SECONDARY: &str = "⌘";
    #[cfg(not(target_os = "macos"))]
    const SECONDARY: &str = "Ctrl";
    #[cfg(target_os = "macos")]
    const SHIFT: &str = "⇧";
    #[cfg(not(target_os = "macos"))]
    const SHIFT: &str = "Shift";
    #[cfg(target_os = "macos")]
    const CTRL: &str = "⌃";
    #[cfg(not(target_os = "macos"))]
    const CTRL: &str = "Ctrl";

    #[test]
    fn key_tokens_maps_modifiers_to_glyphs() {
        assert_eq!(key_tokens("secondary-t"), vec![SECONDARY, "T"]);
        assert_eq!(key_tokens("secondary-shift-d"), vec![SECONDARY, SHIFT, "D"]);
        assert_eq!(key_tokens("secondary-enter"), vec![SECONDARY, "⏎"]);
    }

    #[test]
    fn key_tokens_keeps_the_minus_key_as_the_final_token() {
        assert_eq!(key_tokens("secondary--"), vec![SECONDARY, "−"]);
        assert_eq!(key_tokens("secondary-="), vec![SECONDARY, "="]);
        assert_eq!(key_tokens("secondary-,"), vec![SECONDARY, ","]);
    }

    #[test]
    fn key_chords_splits_a_sequence_into_keycap_groups() {
        assert_eq!(
            key_chords("ctrl-b n"),
            vec![
                vec![CTRL.to_string(), "B".to_string()],
                vec!["N".to_string()]
            ]
        );
        assert_eq!(key_chords("secondary-t"), vec![vec![SECONDARY, "T"]]);
    }

    #[test]
    fn every_default_action_has_a_binding_builder_or_is_unbound() {
        for (action, key) in default_bindings() {
            if !key.is_empty() {
                assert!(
                    keystroke_is_valid(key),
                    "default keystroke for {action} is invalid: {key:?}"
                );
                assert!(
                    make_binding(action, key).is_some(),
                    "no make_binding arm for action {action}"
                );
            }
        }
    }

    #[test]
    fn tmux_preset_keystrokes_all_parse_and_map_to_actions() {
        for (action, key) in tmux_preset("ctrl-b") {
            assert!(
                keystroke_is_valid(&key),
                "tmux preset keystroke for {action} does not parse: {key:?}"
            );
            assert!(
                make_binding(&action, &key).is_some(),
                "tmux preset action {action} has no make_binding arm"
            );
        }
    }

    #[test]
    fn insert_newline_ships_both_default_chords() {
        let effective: Vec<(String, String)> = default_bindings()
            .into_iter()
            .map(|(a, k)| (a.to_string(), k.to_string()))
            .collect();
        assert_eq!(
            effective
                .iter()
                .find(|(a, _)| a == "InsertNewline")
                .map(|(_, k)| k.as_str()),
            Some("shift-enter")
        );
        assert!(extra_keystrokes(&effective).contains(&("InsertNewline", "alt-enter")));
        for key in ["shift-enter", "alt-enter"] {
            assert!(keystroke_is_valid(key), "{key} does not parse");
            assert!(make_binding("InsertNewline", key).is_some());
            assert!(
                bound_keystrokes(&effective).iter().any(|(k, _)| k == key),
                "{key} is not remembered as installed"
            );
        }
        assert_eq!(key_tokens("shift-enter"), vec![SHIFT, "⏎"]);
    }

    #[test]
    fn shift_enter_fallback_retires_with_an_insert_newline_rebind() {
        assert!(is_default_insert_newline_binding(
            "InsertNewline",
            "shift-enter"
        ));
        assert!(!is_default_insert_newline_binding(
            "InsertNewline",
            "ctrl-o"
        ));
        assert!(!is_default_insert_newline_binding("InsertNewline", ""));
    }

    #[test]
    fn shift_enter_prefix_binding_still_waits_for_its_second_key() {
        let mut effective = default_bindings()
            .into_iter()
            .map(|(action, key)| (action.to_string(), key.to_string()))
            .collect::<Vec<_>>();
        effective
            .iter_mut()
            .find(|(action, _)| action == "OpenSettings")
            .unwrap()
            .1 = "shift-enter x".to_string();

        let mut keymap = gpui::Keymap::default();
        keymap.add_bindings(action_bindings(&effective));
        let input = [gpui::Keystroke::parse("shift-enter").unwrap()];
        let context = [gpui::KeyContext::parse("Terminal").unwrap()];
        let (_, pending) = keymap.bindings_for_input(&input, &context);

        assert!(pending, "the keymap must wait for the second key");

        let input = [
            gpui::Keystroke::parse("shift-enter").unwrap(),
            gpui::Keystroke::parse("x").unwrap(),
        ];
        let (matched, pending) = keymap.bindings_for_input(&input, &context);
        assert!(!pending);
        assert!(
            matched
                .first()
                .is_some_and(|binding| binding.action().partial_eq(&OpenSettings))
        );
    }

    #[test]
    fn paste_ships_both_terminal_chords_off_macos_and_retires_together() {
        let effective: Vec<(String, String)> = default_bindings()
            .into_iter()
            .map(|(a, k)| (a.to_string(), k.to_string()))
            .collect();
        if cfg!(target_os = "macos") {
            assert!(
                !extra_keystrokes(&effective)
                    .iter()
                    .any(|(a, _)| *a == "PasteText"),
                "macOS pastes with Cmd+V; no extra chord should install there"
            );
            return;
        }
        assert_eq!(
            effective
                .iter()
                .find(|(a, _)| a == "PasteText")
                .map(|(_, k)| k.as_str()),
            Some("ctrl-shift-v")
        );
        assert_eq!(
            effective
                .iter()
                .find(|(a, _)| a == "CopyText")
                .map(|(_, k)| k.as_str()),
            Some("ctrl-shift-c")
        );
        assert!(extra_keystrokes(&effective).contains(&("PasteText", "shift-insert")));
        for key in ["ctrl-shift-v", "shift-insert"] {
            assert!(
                bound_keystrokes(&effective)
                    .iter()
                    .any(|(k, c)| k == key && *c == Some("Terminal")),
                "{key} must be installed in the Terminal context and remembered for rebind"
            );
        }
        let rebound = vec![("PasteText".to_string(), "ctrl-alt-v".to_string())];
        assert!(
            !extra_keystrokes(&rebound)
                .iter()
                .any(|(a, _)| *a == "PasteText"),
            "moving Paste off its default must retire Shift+Insert with it"
        );
    }

    #[test]
    fn rebinding_insert_newline_retires_both_default_chords() {
        let effective = vec![("InsertNewline".to_string(), "ctrl-o".to_string())];
        assert!(extra_keystrokes(&effective).is_empty());
        assert_eq!(
            bound_keystrokes(&effective),
            vec![("ctrl-o".to_string(), Some("Terminal"))]
        );

        let unbound = vec![("InsertNewline".to_string(), String::new())];
        assert!(extra_keystrokes(&unbound).is_empty());
        assert!(action_bindings(&unbound).is_empty());
    }

    #[test]
    fn bound_keystrokes_remember_the_context_each_binding_was_installed_in() {
        let effective = vec![
            ("InsertNewline".to_string(), "shift-enter".to_string()),
            ("NewTab".to_string(), "secondary-t".to_string()),
        ];
        assert_eq!(
            bound_keystrokes(&effective),
            vec![
                ("shift-enter".to_string(), Some("Terminal")),
                ("secondary-t".to_string(), None),
                ("alt-enter".to_string(), Some("Terminal")),
            ]
        );
    }

    #[test]
    fn action_context_matches_the_scope_make_binding_installs() {
        let extra_actions = extra_keystrokes(
            &default_bindings()
                .into_iter()
                .map(|(a, k)| (a.to_string(), k.to_string()))
                .collect::<Vec<_>>(),
        );
        let actions = default_bindings()
            .into_iter()
            .map(|(a, _)| a)
            .chain(extra_actions.into_iter().map(|(a, _)| a));
        for action in actions {
            let binding =
                make_binding(action, "f13").unwrap_or_else(|| panic!("no arm for {action}"));
            let installed = binding.predicate().map(|p| p.to_string());
            assert_eq!(
                installed.as_deref(),
                action_context(action),
                "{action} is installed in a different context than `action_context` reports"
            );
        }
    }

    #[test]
    fn secondary_enter_chords_are_distinct_from_insert_newline() {
        let defaults = default_bindings();
        let key_of = |action: &str| {
            defaults
                .iter()
                .find(|(a, _)| *a == action)
                .map(|(_, k)| *k)
                .unwrap()
        };
        assert_eq!(
            key_of("ToggleFullscreen"),
            per_platform("secondary-enter", "f11")
        );
        assert_eq!(key_of("ToggleMaximizePane"), "secondary-shift-enter");
        for window_chord in ["secondary-enter", "secondary-shift-enter"] {
            assert_ne!(window_chord, INSERT_NEWLINE_DEFAULT);
            assert_ne!(window_chord, INSERT_NEWLINE_ALT_DEFAULT);
        }
        for chord in [INSERT_NEWLINE_DEFAULT, INSERT_NEWLINE_ALT_DEFAULT] {
            assert_ne!(chord, "shift-alt-enter");
            assert_ne!(chord, "alt-shift-enter");
        }
    }

    #[test]
    fn no_default_binding_sits_on_a_terminal_control_code() {
        // The invariant is "no default may *swallow* a terminal control code".
        // EditorSave deliberately stays on Ctrl+S: its handler in `app.rs` calls
        // `cx.propagate()` whenever the editor does not have focus, so the
        // keystroke falls through to the terminal as XOFF instead of dying at
        // the window. Anything added here must have such a fall-through.
        const FALLS_THROUGH_TO_TERMINAL: [&str; 1] = ["EditorSave"];
        for (action, spec) in default_bindings() {
            if FALLS_THROUGH_TO_TERMINAL.contains(&action) {
                continue;
            }
            for chord in spec.split_whitespace() {
                let ks = Keystroke::parse(chord).expect("default chords parse");
                let m = &ks.modifiers;
                if !m.control || m.alt || m.shift || m.platform || m.function {
                    continue;
                }
                let steals = ks.key.len() == 1
                    && ks.key.chars().next().is_some_and(|c| {
                        c.is_ascii_alphabetic() || "[]\\`".contains(c) || ('2'..='8').contains(&c)
                    })
                    || ks.key == "space";
                assert!(
                    !steals,
                    "{action} is bound to {chord}, which the shell needs as a control code \
                     (Ctrl+[ is ESC, Ctrl+D is EOF, Ctrl+W deletes a word, \
                     Ctrl+2..8 are NUL/ESC/FS/GS/RS/US/DEL). \
                     Window actions belong on ctrl-shift-* off macOS."
                );
            }
        }
    }

    #[test]
    fn every_default_chord_is_claimed_by_exactly_one_action() {
        let mut seen: Vec<(&str, &str)> = Vec::new();
        for (action, spec) in default_bindings() {
            if spec.is_empty() {
                continue;
            }
            if let Some((other, _)) = seen.iter().find(|(_, s)| *s == spec) {
                panic!("{action} and {other} both claim {spec}");
            }
            seen.push((action, spec));
        }
    }

    #[test]
    fn spec_from_keystroke_round_trips_through_parse() {
        for spec in [
            "secondary-t",
            "secondary-shift-t",
            "secondary-alt-left",
            "ctrl-shift-tab",
            "secondary--",
        ] {
            let ks = Keystroke::parse(spec).unwrap();
            let round = spec_from_keystroke(&ks).expect("real key produces a spec");
            let reparsed = Keystroke::parse(&round).unwrap();
            assert_eq!(
                (reparsed.modifiers, reparsed.key),
                (ks.modifiers, ks.key),
                "round trip diverged for {spec}"
            );
        }
    }

    #[test]
    fn spec_from_keystroke_ignores_a_lone_modifier() {
        let ks = Keystroke::parse("secondary").unwrap();
        assert_eq!(spec_from_keystroke(&ks), None);
    }
}

#[cfg(test)]
mod gpui_tests {
    use super::*;
    use crate::core::config::Config;
    use gpui::TestAppContext;

    #[gpui::test]
    fn init_then_rebind_installs_the_merged_table(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
            init(cx);

            {
                let cfg = cx.global_mut::<Config>();
                cfg.keybinding_preset = "tmux".to_string();
                cfg.keybindings
                    .insert("NewTab".to_string(), "secondary-shift-n".to_string());
            }
            rebind(cx);

            let eff = effective_bindings(cx);
            let key_of = |action: &str| {
                eff.iter()
                    .find(|(a, _)| a == action)
                    .map(|(_, k)| k.clone())
                    .unwrap()
            };
            assert_eq!(key_of("NewTab"), "secondary-shift-n");
            assert_eq!(key_of("SplitRight"), "ctrl-b %");
            assert_eq!(
                key_of("TogglePalette"),
                per_platform("secondary-p", "secondary-shift-p")
            );

            cx.global_mut::<Config>().keybinding_preset = "default".to_string();
            rebind(cx);
            let eff = effective_bindings(cx);
            assert_eq!(
                eff.iter()
                    .find(|(a, _)| a == "SplitRight")
                    .map(|(_, k)| k.as_str()),
                Some(per_platform("secondary-d", "secondary-shift-d"))
            );
        });
    }
}

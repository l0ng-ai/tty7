//! Keymap and global-action wiring: the default action→keystroke table, merging
//! the user's config overrides on top, and the one-time install of keybindings,
//! the menu bar, and global actions at startup. Kept separate from the window
//! shell so `app.rs` stays focused on tab/pane orchestration.

use gpui::{App, Global, KeyBinding, Keystroke, NoAction};

use crate::core::actions::*;
use crate::core::config::Config;
use crate::terminal::view::{
    ClearScrollback, FindInTerminal, FindNext, FindPrevious, InsertNewline,
};
use crate::ui::theme::set_menus;

/// The set of keystrokes currently installed for app actions, remembered so a
/// later [`rebind`] can neutralize them with `NoAction` bindings instead of
/// clearing the whole keymap (which would also wipe gpui-component's own input /
/// list / menu bindings). Each entry carries the key context its binding was
/// installed in (see [`action_context`]) so the `NoAction` can be scoped the
/// same way. Stored as a GPUI global.
#[derive(Default)]
struct BoundKeystrokes(Vec<(String, Option<&'static str>)>);
impl Global for BoundKeystrokes {}

/// Install the application menu bar, keybindings, and global actions.
/// Call once at startup with the app context.
pub fn init(cx: &mut App) {
    let effective = effective_bindings(cx);
    let mut bindings = action_bindings(&effective);
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
    cx.set_global(BoundKeystrokes(bound_keystrokes(&effective)));

    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
    set_menus(cx);
}

/// Re-apply keybindings after the effective table changes (an edit in Settings,
/// a preset switch). Appends a `NoAction` binding for every previously-installed
/// keystroke — which suppresses the earlier binding of that keystroke in GPUI's
/// depth-then-index dispatch — then re-adds the current effective bindings, which
/// win because they're added last. The keymap only grows (bounded per process),
/// but we never `clear()` it, so gpui-component's bindings survive untouched.
pub fn rebind(cx: &mut App) {
    let previous = cx
        .try_global::<BoundKeystrokes>()
        .map(|b| b.0.clone())
        .unwrap_or_default();
    let effective = effective_bindings(cx);

    // Neutralize each old keystroke with a `NoAction` in the same context its
    // binding was installed in. The scope matters: GPUI's `binding_enabled`
    // gives a *context-less* binding the maximum match depth (`contexts.len()`),
    // so a global `NoAction` outranks every context-scoped binding on that
    // chord — and a matched `NoAction` discards the rest of the matches — which
    // would kill bindings we never installed (gpui-component's own Input-scoped
    // `shift-enter` while the search field has focus). Scoped the same way, the
    // `NoAction` still suppresses our own old binding: that one sits at the
    // same depth with a lower index, and index breaks the tie.
    let mut bindings: Vec<KeyBinding> = previous
        .iter()
        .filter(|(k, _)| keystroke_is_valid(k))
        .map(|(k, ctx)| KeyBinding::new(k, NoAction {}, *ctx))
        .collect();
    bindings.extend(action_bindings(&effective));
    cx.bind_keys(bindings);
    cx.set_global(BoundKeystrokes(bound_keystrokes(&effective)));

    // Rebuild the menu bar so its macOS key equivalents track the new keymap.
    // AppKit dispatches a menu shortcut (e.g. ⌘W → Close) *before* GPUI's keymap,
    // so a stale equivalent would fire the old action even though we suppressed
    // its keybinding with `NoAction`. `set_menus` re-resolves each item's
    // equivalent from the current keymap (via `bindings_for_action`, which skips
    // the suppressed bindings), so a rebound action loses its old ⌘-shortcut and
    // gains the new one.
    set_menus(cx);
}

/// `InsertNewline`'s primary default: Windows Terminal's chord for a soft
/// newline in the prompt editor, and the one the table in [`default_bindings`]
/// carries.
const INSERT_NEWLINE_DEFAULT: &str = "shift-enter";

/// `InsertNewline`'s second default: iTerm2's chord for the same gesture. Both
/// have inserted a newline since the multi-line editor landed, and both must
/// keep doing so — but a binding spec can't express alternatives (whitespace in
/// a spec means a *sequence*, `ctrl-b n`-style), and the effective table holds
/// exactly one keystroke per action. So this chord is installed alongside the
/// table's, and only while `InsertNewline` still sits on its primary default:
/// rebinding the action in Settings or `config.json` retires both old chords,
/// which is what a user who moves the binding expects.
///
/// Only these two. GPUI matches a binding on exact modifier equality
/// (`Keystroke::should_match`), so Shift+Alt+Enter — which the old hardcoded
/// `(m.shift || m.alt)` test happened to catch — maps to no action and submits
/// like any other Enter. That matches the reference: Warp's key table is a
/// `(ctrl, alt, shift, key)` tuple whose newline arms are exactly Shift+Enter,
/// Alt+Enter and Ctrl+J, so its `(false, true, true, "enter")` falls through
/// too, and its GUI compares whole keystrokes for equality just as gpui does.
const INSERT_NEWLINE_ALT_DEFAULT: &str = "alt-enter";

/// The extra keystroke an effective table installs beyond its one-per-action
/// rows — today only [`INSERT_NEWLINE_ALT_DEFAULT`]. `None` once the action has
/// been rebound off its default.
fn extra_keystrokes(effective: &[(String, String)]) -> Vec<(&'static str, &'static str)> {
    let on_default = effective
        .iter()
        .any(|(a, k)| a == "InsertNewline" && k == INSERT_NEWLINE_DEFAULT);
    if on_default {
        vec![("InsertNewline", INSERT_NEWLINE_ALT_DEFAULT)]
    } else {
        Vec::new()
    }
}

/// The extra `(action, keystroke)` pairs the current effective table installs
/// beyond its one-row-per-action rows. [`effective_bindings`] stays one row per
/// action because Settings renders from it, so anything that has to reason
/// about which chords are actually *live* — conflict detection when the user
/// records a shortcut — must consult this alongside it, or it would miss the
/// extra chord and silently drop it.
pub(crate) fn extra_bindings(cx: &App) -> Vec<(String, String)> {
    extra_keystrokes(&effective_bindings(cx))
        .into_iter()
        .map(|(a, k)| (a.to_string(), k.to_string()))
        .collect()
}

/// Build the `KeyBinding`s for an effective table, skipping unbound rows (empty
/// keystroke) and any that fail validation.
fn action_bindings(effective: &[(String, String)]) -> Vec<KeyBinding> {
    let mut bindings = Vec::new();
    for (action, key) in extra_keystrokes(effective) {
        if let Some(b) = make_binding(action, key) {
            bindings.push(b);
        }
    }
    for (action, key) in effective {
        if key.is_empty() {
            continue; // an action with no assigned key
        }
        if !keystroke_is_valid(key) {
            log::warn!("ignoring keybinding for '{action}': invalid keystroke '{key}'");
            continue;
        }
        match make_binding(action, key) {
            Some(b) => bindings.push(b),
            None => log::warn!("ignoring keybinding: unknown action '{action}'"),
        }
    }
    bindings
}

/// The valid, non-empty keystrokes an effective table actually installs, each
/// paired with the key context it is installed in — the list [`rebind`]
/// remembers so it can suppress them, in that same context, on the next change.
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

/// The built-in action → default-keystroke table. The single source of truth for
/// the default keymap, the names the user can override, and the rows the Settings
/// list renders. An empty keystroke means "no default key" (bind one in Settings
/// or config); it's shown as "—" and never installed.
pub(crate) fn default_bindings() -> Vec<(&'static str, &'static str)> {
    // `secondary-` is gpui's cross-platform modifier: ⌘ on macOS, Ctrl elsewhere
    // (see `Keystroke::parse`). Using it keeps the same muscle memory on Windows
    // and Linux without binding to the Win/Super key, which the OS reserves.
    vec![
        ("NewTab", "secondary-t"),
        ("NewWorkspace", "secondary-shift-n"),
        ("CloseActiveTab", "secondary-w"),
        // Tab operations promoted out of the tab context menu (see
        // `core::actions`). No default chords: the menu bar, the palette and the
        // right-click menu all reach them, and none is frequent enough to earn a
        // reflexive shortcut — but they're bindable here like anything else.
        ("RenameTab", ""),
        ("NewWorktreeTab", ""),
        ("CloseOtherTabs", ""),
        ("CloseTabsToTheRight", ""),
        ("CopyWorkingDirectory", ""),
        ("MarkTabUnread", ""),
        // Fork: the bare action opens a new tab; the four directional ones are
        // the pane right-click menu's placement pick, bindable here for anyone
        // who wants a chord straight to one direction.
        ("ForkAgentSession", ""),
        ("ForkAgentSessionRight", ""),
        ("ForkAgentSessionLeft", ""),
        ("ForkAgentSessionDown", ""),
        ("ForkAgentSessionUp", ""),
        ("CopyAgentSessionId", ""),
        // No default chord on purpose: this is the one action that kills running
        // sessions, and it must not sit one slip away from ⌘W. Reachable from
        // the Shell menu and the palette; bindable in Settings for anyone who
        // wants it.
        ("StopWorkspace", ""),
        ("DeleteWorkspace", ""),
        ("RenameWorkspace", ""),
        ("ToggleSwitcher", "secondary-shift-o"),
        ("SplitRight", "secondary-d"),
        ("SplitDown", "secondary-shift-d"),
        ("FocusNextPane", "secondary-]"),
        ("FocusPrevPane", "secondary-["),
        // Directional pane focus: ⌘⌥ / Ctrl+Alt + arrow.
        ("FocusPaneLeft", "secondary-alt-left"),
        ("FocusPaneRight", "secondary-alt-right"),
        ("FocusPaneUp", "secondary-alt-up"),
        ("FocusPaneDown", "secondary-alt-down"),
        // Resize / swap have no default chord — they're reachable from the
        // command palette and bindable in Settings (and the tmux preset).
        ("ResizePaneLeft", ""),
        ("ResizePaneRight", ""),
        ("ResizePaneUp", ""),
        ("ResizePaneDown", ""),
        ("SwapPaneNext", ""),
        ("SwapPanePrev", ""),
        // Relative tab nav. Ctrl+Tab is free of an OS/terminal meaning on the
        // platforms we ship; rebind if a given setup disagrees.
        ("NextTab", "ctrl-tab"),
        ("PrevTab", "ctrl-shift-tab"),
        ("ActivateTab1", "secondary-1"),
        ("ActivateTab2", "secondary-2"),
        ("ActivateTab3", "secondary-3"),
        ("ActivateTab4", "secondary-4"),
        ("ActivateTab5", "secondary-5"),
        ("ActivateTab6", "secondary-6"),
        ("ActivateTab7", "secondary-7"),
        ("ActivateTab8", "secondary-8"),
        ("ActivateTab9", "secondary-9"),
        // Workspace slots in the Window menu's order. No default chord: ⌘1–9 is
        // already the tab row's, and a workspace switch is a rarer move than a
        // tab switch. Clickable in the Window menu and the title-bar chip, and
        // bindable here for anyone who wants the chord.
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
        ("TogglePalette", "secondary-p"),
        ("ReopenClosedTab", "secondary-shift-t"),
        // ⌘⏎ toggles window fullscreen and ⌘⇧⏎ zooms the focused pane, matching
        // Ghostty's and iTerm2's defaults — pane zoom deliberately does NOT own
        // the bare ⌘⏎, which users expect to affect the whole window.
        ("ToggleMaximizePane", "secondary-shift-enter"),
        ("ToggleFullscreen", "secondary-enter"),
        // No default chord (a layout toggle rarely wants a reflexive shortcut, and
        // this steers clear of collisions) — reachable from the command palette
        // and Settings → Window & Tabs, and bindable there like any other action.
        ("ToggleTabSidebar", ""),
        // Collapse/expand the left rail, on the ⌘B every editor uses for it.
        // Off macOS `secondary-b` is Ctrl+B, which is the tmux preset's default
        // prefix — leave it unbound there rather than fight the prefix.
        (
            "ToggleLeftPanel",
            if cfg!(target_os = "macos") {
                "secondary-b"
            } else {
                ""
            },
        ),
        // The right detail panel, on the ⌘J every editor uses for a dock.
        ("ToggleRightPanel", "secondary-j"),
        // Buffer search. ⌘F on macOS; elsewhere `secondary-f` (Ctrl+F) is
        // readline's forward-char, so follow the GUI-terminal convention and open
        // find on Ctrl+Shift+F, leaving Ctrl+F to the shell. Find-again is ⌘G/⌘⇧G
        // on macOS and F3/Shift+F3 elsewhere (the Windows/Linux norm).
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
        // Like Terminal.app / iTerm2 / Ghostty ⌘K: wipe the screen + scrollback.
        ("ClearScrollback", "secondary-k"),
        // Soft newline in the prompt editor: author a multi-line command without
        // submitting it. Shift+Enter is Windows Terminal's chord and the primary
        // default; Alt+Enter (iTerm2's) is installed alongside it — see
        // `INSERT_NEWLINE_ALT_DEFAULT` for why it can't live in this table.
        ("InsertNewline", INSERT_NEWLINE_DEFAULT),
        ("OpenSettings", "secondary-,"),
        // Help → Keyboard Shortcuts, on the ⌘/ that editors and browsers use for
        // "show me the shortcuts". Off macOS `secondary-/` is Ctrl+/, which some
        // shells bind to undo, so leave it unbound there.
        (
            "ShowKeyboardShortcuts",
            if cfg!(target_os = "macos") {
                "secondary-/"
            } else {
                ""
            },
        ),
        // Menu-bar-only entries: real actions so the palette and Settings can see
        // them, but nothing here wants a chord by default.
        ("About", ""),
        ("CheckForUpdates", ""),
        ("OpenDocumentation", ""),
        ("OpenDiscord", ""),
        ("ReportIssue", ""),
        // macOS supplies these chords itself for a standard App/Window menu; we
        // list them so they show up in Settings → Keybindings rather than looking
        // like undocumented magic, but bind them only where they exist.
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
        // No default chord — reachable from the command palette ("SSH: Remote
        // Files") and bindable in Settings like any other action.
        ("ToggleSftp", ""),
        // No default chord either — the palette ("SSH: Port Forwarding") and the
        // Info tab's own `+` are the primary ways in.
        ("ShowSshForwards", ""),
        // The code panel (file tree + editor overlay), on VS Code's explorer
        // chord. ⌘⇧E is free (no existing binding or preset uses it).
        ("ToggleCodePanel", "secondary-shift-e"),
        // Save the editor's active file. ⌘S is free — the terminal has no save.
        ("EditorSave", "secondary-s"),
        // No default chord — reachable from the command palette ("SSH: Manage
        // Profiles…") and bindable in Settings.
        ("OpenSshProfiles", ""),
        // Reconnect a dropped native-SSH pane (PRD FR-E4). ⌘⇧R is free (no
        // existing binding uses it).
        ("RestartSshSession", "secondary-shift-r"),
        ("Quit", "secondary-q"),
    ]
}

/// The full effective action → keystroke table: the built-in defaults, with the
/// active preset layered on top (tmux prefix sequences), then the user's config
/// overrides last. The single source of truth for installing the keymap and for
/// what the Settings list shows. Empty keystrokes (unbound actions) are kept.
pub(crate) fn effective_bindings(cx: &App) -> Vec<(String, String)> {
    let cfg = cx.global::<Config>();
    let mut effective: Vec<(String, String)> = default_bindings()
        .into_iter()
        .map(|(a, k)| (a.to_string(), k.to_string()))
        .collect();
    // Preset layer: remaps the actions it covers onto prefix-led sequences.
    for (action, key) in preset_bindings(&cfg.keybinding_preset, &cfg.prefix) {
        set_binding(&mut effective, &action, key);
    }
    // User overrides win. Unknown action names (typos, stale keys) are ignored.
    for (action, key) in &cfg.keybindings {
        set_binding(&mut effective, action, key.clone());
    }
    effective
}

/// Update the keystroke of an existing action in the effective table. Unknown
/// action names are ignored so a bad preset/override entry can't inject a row.
fn set_binding(effective: &mut [(String, String)], action: &str, key: String) {
    if let Some(slot) = effective.iter_mut().find(|(a, _)| a == action) {
        slot.1 = key;
    }
}

/// The keybinding overlay a preset contributes: `(action, keystroke)` pairs with
/// the prefix already substituted in. The `default` preset contributes nothing.
fn preset_bindings(preset: &str, prefix: &str) -> Vec<(String, String)> {
    match preset {
        "tmux" => tmux_preset(prefix),
        _ => Vec::new(),
    }
}

/// The tmux-style preset: pane/tab actions mapped onto `prefix key` sequences
/// (e.g. `ctrl-b c` → New Tab). GPUI plays the prefix through to the shell after
/// a 1s timeout if no sequence completes, so a bare prefix still reaches readline.
fn tmux_preset(prefix: &str) -> Vec<(String, String)> {
    // `p("c")` → "<prefix> c". The trailing key can be shifted punctuation
    // (`%`, `"`, `{`, `}`): GPUI matches those via the typed key's `key_char`,
    // so binding the literal glyph works without spelling out `shift-…`.
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

/// The effective keystroke for an action, from the merged table. `None` when the
/// action has no binding at all (unbound). Used to surface shortcut hints in the
/// UI (command palette, settings).
pub(crate) fn effective_key(action: &str, cx: &App) -> Option<String> {
    effective_bindings(cx)
        .into_iter()
        .find(|(a, _)| a == action)
        .map(|(_, k)| k)
        .filter(|k| !k.is_empty())
}

/// Serialize a recorded keystroke into a config spec string (the inverse of
/// `Keystroke::parse`), normalizing the platform's primary modifier to the
/// portable `secondary` so a recorded shortcut stays cross-platform. Returns
/// `None` for a lone modifier press (nothing to bind yet).
pub(crate) fn spec_from_keystroke(ks: &Keystroke) -> Option<String> {
    // A modifier-only keystroke has one of these as its `key`; there's no real
    // key to bind, so keep recording.
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

/// Split a keybinding spec into its whitespace-separated chords, each rendered
/// as its own list of display tokens. A single chord ("secondary-t") yields one
/// group; a tmux-style sequence ("ctrl-b n") yields two, so the UI can draw them
/// as distinct keycap clusters (`⌃B` then `N`).
pub(crate) fn key_chords(spec: &str) -> Vec<Vec<String>> {
    spec.split_whitespace().map(key_tokens).collect()
}

/// Split one keybinding chord ("secondary-shift-d", "secondary--") into display
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

/// The key context an action's binding is installed in, or `None` for a global
/// one. The single source of truth for that decision: [`make_binding`] builds
/// the binding with it and [`bound_keystrokes`] records it, so the `NoAction`
/// [`rebind`] later uses to retire a chord lands in the same scope as the
/// binding it retires.
///
/// Terminal-scoped means the handler lives on the terminal surface, so the
/// "Terminal" context keeps the chord inert on the settings / home pages
/// instead of binding a dead global chord there.
fn action_context(action: &str) -> Option<&'static str> {
    match action {
        "FindInTerminal" | "FindNext" | "FindPrevious" | "ClearScrollback" | "InsertNewline" => {
            Some("Terminal")
        }
        _ => None,
    }
}

/// Build a `KeyBinding` for a known action name + (already-validated) keystroke.
/// Returns `None` for an unrecognized action name.
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
        // Right-panel tab jumps. No entry in the default table above — they ship
        // unbound and exist so a user *can* bind them; the palette reaches them
        // either way.
        "ShowRightPanelInfo" => KeyBinding::new(keystroke, ShowRightPanelInfo, None),
        "ShowRightPanelOutline" => KeyBinding::new(keystroke, ShowRightPanelOutline, None),
        "ShowRightPanelChanges" => KeyBinding::new(keystroke, ShowRightPanelChanges, None),
        "ShowRightPanelFiles" => KeyBinding::new(keystroke, ShowRightPanelFiles, None),
        // Terminal-scoped — see `action_context`, which owns that decision so the
        // context here and the one `rebind` neutralizes with can't drift apart.
        "FindInTerminal" => KeyBinding::new(keystroke, FindInTerminal, action_context(action)),
        "FindNext" => KeyBinding::new(keystroke, FindNext, action_context(action)),
        "FindPrevious" => KeyBinding::new(keystroke, FindPrevious, action_context(action)),
        "ClearScrollback" => KeyBinding::new(keystroke, ClearScrollback, action_context(action)),
        "InsertNewline" => KeyBinding::new(keystroke, InsertNewline, action_context(action)),
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
    // Literal `ctrl` renders as ⌃ on macOS, "Ctrl" elsewhere.
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
        // "secondary--" is the secondary key + the "-" key; a naive split on '-'
        // would drop the trailing key.
        assert_eq!(key_tokens("secondary--"), vec![SECONDARY, "−"]);
        assert_eq!(key_tokens("secondary-="), vec![SECONDARY, "="]);
        assert_eq!(key_tokens("secondary-,"), vec![SECONDARY, ","]);
    }

    #[test]
    fn key_chords_splits_a_sequence_into_keycap_groups() {
        // A tmux-style sequence renders as two distinct clusters.
        assert_eq!(
            key_chords("ctrl-b n"),
            vec![
                vec![CTRL.to_string(), "B".to_string()],
                vec!["N".to_string()]
            ]
        );
        // A single chord is one group.
        assert_eq!(key_chords("secondary-t"), vec![vec![SECONDARY, "T"]]);
    }

    #[test]
    fn every_default_action_has_a_binding_builder_or_is_unbound() {
        // Every action the defaults name must be constructible by `make_binding`
        // (a missing arm would silently drop the binding), and each default key
        // must be empty (unbound) or a valid keystroke.
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
        // Every preset row must produce an installable binding: a parseable
        // sequence (including the shifted punctuation `% " { }`) and a known
        // action. A silent parse failure would leave the preset key dead.
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
        // Shift+Enter and Alt+Enter have both inserted a soft newline since the
        // multi-line editor landed; exposing the gesture as an action must not
        // cost either one. Shift+Enter is the table row, Alt+Enter the extra.
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
        assert_eq!(
            extra_keystrokes(&effective),
            vec![("InsertNewline", "alt-enter")]
        );
        // Both are real, installable bindings, and both are remembered so a
        // later `rebind` can neutralize them.
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
    fn rebinding_insert_newline_retires_both_default_chords() {
        // Move the action off its default and the second chord goes with it —
        // otherwise Alt+Enter would keep inserting newlines behind the user's
        // back after they deliberately moved the binding.
        let effective = vec![("InsertNewline".to_string(), "ctrl-o".to_string())];
        assert!(extra_keystrokes(&effective).is_empty());
        assert_eq!(
            bound_keystrokes(&effective),
            vec![("ctrl-o".to_string(), Some("Terminal"))]
        );

        // Unbinding it entirely (an empty override) installs nothing at all.
        let unbound = vec![("InsertNewline".to_string(), String::new())];
        assert!(extra_keystrokes(&unbound).is_empty());
        assert!(action_bindings(&unbound).is_empty());
    }

    #[test]
    fn bound_keystrokes_remember_the_context_each_binding_was_installed_in() {
        // `rebind` retires an old chord with a `NoAction`, and a context-less
        // one gets GPUI's *maximum* match depth — it would outrank, and discard,
        // deeper bindings on that chord that we never installed (gpui-component
        // binds `shift-enter` in its own "Input" context, which is what steps to
        // the previous match in the terminal's search field). So each remembered
        // keystroke carries the scope its binding actually had.
        let effective = vec![
            ("InsertNewline".to_string(), "shift-enter".to_string()),
            ("NewTab".to_string(), "secondary-t".to_string()),
        ];
        assert_eq!(
            bound_keystrokes(&effective),
            vec![
                ("shift-enter".to_string(), Some("Terminal")),
                ("secondary-t".to_string(), None),
                // The extra chord is scoped by its action, like any other row.
                ("alt-enter".to_string(), Some("Terminal")),
            ]
        );
    }

    #[test]
    fn action_context_matches_the_scope_make_binding_installs() {
        // The two must agree for every action, or `rebind` would neutralize a
        // chord in a scope the binding never had — leaving the old binding live
        // (too narrow) or shadowing unrelated ones (too wide).
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
        // The window bindings the reporter called out (#182) must stay put:
        // ⌘⏎ / ⌘⇧⏎ are different keystrokes from the bare ⇧⏎ and ⌥⏎.
        let defaults = default_bindings();
        let key_of = |action: &str| {
            defaults
                .iter()
                .find(|(a, _)| *a == action)
                .map(|(_, k)| *k)
                .unwrap()
        };
        assert_eq!(key_of("ToggleFullscreen"), "secondary-enter");
        assert_eq!(key_of("ToggleMaximizePane"), "secondary-shift-enter");
        for window_chord in ["secondary-enter", "secondary-shift-enter"] {
            assert_ne!(window_chord, INSERT_NEWLINE_DEFAULT);
            assert_ne!(window_chord, INSERT_NEWLINE_ALT_DEFAULT);
        }
        // Modifier matching is exact, so the three-key chord is nobody's: it
        // inserts nothing and submits, as it does in Warp. Spelled out here so
        // a future reader doesn't "restore" it as a missing default.
        for chord in [INSERT_NEWLINE_DEFAULT, INSERT_NEWLINE_ALT_DEFAULT] {
            assert_ne!(chord, "shift-alt-enter");
            assert_ne!(chord, "alt-shift-enter");
        }
    }

    #[test]
    fn spec_from_keystroke_round_trips_through_parse() {
        // A recorded keystroke → spec string → parsed keystroke must be stable,
        // and the platform primary modifier must normalize to `secondary`.
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
        // Parsing "secondary" yields a keystroke whose *key* is the modifier;
        // there's nothing to bind yet, so recording keeps waiting.
        let ks = Keystroke::parse("secondary").unwrap();
        assert_eq!(spec_from_keystroke(&ks), None);
    }
}

#[cfg(test)]
mod gpui_tests {
    use super::*;
    use crate::core::config::Config;
    use gpui::TestAppContext;

    // Install the keymap for real (init → edit config → rebind) against a live
    // `App`. Every effective keystroke goes through `KeyBinding::new`, which
    // panics on a bad spec — so this catches a preset/default that only *looks*
    // valid, and confirms the three-layer merge (default → tmux preset → user
    // override) resolves as expected.
    #[gpui::test]
    fn init_then_rebind_installs_the_merged_table(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
            init(cx);

            // Turn on the tmux preset and override one action on top of it.
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
            // User override beats the preset's `prefix c` for NewTab.
            assert_eq!(key_of("NewTab"), "secondary-shift-n");
            // A preset-only remap surfaces its prefix sequence.
            assert_eq!(key_of("SplitRight"), "ctrl-b %");
            // An action the preset doesn't touch keeps its default.
            assert_eq!(key_of("TogglePalette"), "secondary-p");

            // Switching back to the default preset drops the sequences.
            cx.global_mut::<Config>().keybinding_preset = "default".to_string();
            rebind(cx);
            let eff = effective_bindings(cx);
            assert_eq!(
                eff.iter()
                    .find(|(a, _)| a == "SplitRight")
                    .map(|(_, k)| k.as_str()),
                Some("secondary-d")
            );
        });
    }
}

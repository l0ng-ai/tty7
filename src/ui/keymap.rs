use gpui::{App, Global, KeyBinding, Keystroke, NoAction};

use crate::core::actions::*;
use crate::core::config::Config;
use crate::terminal::view::{
    ClearScrollback, CopyText, FindInTerminal, FindNext, FindPrevious, InsertNewline,
    InsertNewlineFallback, PasteText,
};
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::palette::CommandGroup;
use crate::ui::settings::humanize_action;
use crate::ui::theme::set_menus;

/// The bindings that were already in the keymap before tty7 put its own
/// there. gpui-component installs a table of its own from `gpui_component::
/// init` — the `Input` context's editing keys, the list and menu navigation,
/// the escape that closes a dialog — and it has no entry point to install
/// them a second time. A rebuild replaces the whole map, so it has to lay
/// these back down first, in the order they were added, or the map that comes
/// out has no backspace in any text field in the app (#548).
struct BaseBindings(Vec<KeyBinding>);
impl Global for BaseBindings {}

pub fn init(cx: &mut App) {
    if cx.try_global::<BaseBindings>().is_none() {
        let base: Vec<KeyBinding> = cx.key_bindings().borrow().bindings().cloned().collect();
        cx.set_global(BaseBindings(base));
    }
    rebuild_keymap(cx);
    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
    set_menus(cx);
}

/// The fixed bindings every keymap carries, whatever the config says. These
/// are the terminal's own keys (Tab/BackTab as input), the font-size step
/// that has no settings row, and the modal-context guards the dispatch path
/// needs — none of them come from `effective_bindings`, so a rebuild that
/// only replayed the config would drop them.
fn fixed_bindings() -> Vec<KeyBinding> {
    let mut bindings = vec![
        KeyBinding::new("secondary-+", IncreaseFontSize, None),
        KeyBinding::new("tab", SendTab, Some("Terminal")),
        KeyBinding::new("shift-tab", SendBackTab, Some("Terminal")),
    ];
    // The switcher's footer tells you Tab is the way across to the tab column
    // once a query is in the box. gpui-component's Root binds `tab` to its
    // focus walker, actions are dispatched before key listeners, and the panel
    // was therefore losing every Tab to the ellipsis button in its own header —
    // ring and all. A binding on the panel's own context is deeper in the
    // dispatch path, so it wins.
    bindings.push(KeyBinding::new("tab", SwitcherAcross, Some("Switcher")));
    bindings.push(KeyBinding::new(
        "shift-tab",
        SwitcherAcrossBack,
        Some("Switcher"),
    ));
    // The palette has nowhere for Tab to go — the arrows walk the list and
    // Enter runs it — but Root's focus walker still had somewhere to send it:
    // out of the modal, onto whichever chrome tile is behind it, ring and all.
    bindings.push(KeyBinding::new("tab", NoAction {}, Some("Palette")));
    bindings.push(KeyBinding::new("shift-tab", NoAction {}, Some("Palette")));
    bindings
}

/// Replaces the keymap with the full set: the bindings tty7 inherited, then
/// the ones the live config asks for, then the fixed ones.
///
/// gpui's `Keymap::add_bindings` only ever pushes — it never dedups and
/// nothing retires a binding — so rebinding by *appending* (the old `rebind`)
/// leaked one full table per call, and every keystroke walks the whole map.
/// Clearing first makes a rebind O(map) instead of O(map × calls), and makes
/// it safe to drive from the config watcher: a hand edit that only reorders
/// or re-comments the file reloads to an identical triple and never reaches
/// here, while a real change rebuilds once (#548). The order is the order
/// `init` bound them in, and gpui reads the map back to front, so tty7's
/// bindings still win over the inherited ones.
fn rebuild_keymap(cx: &mut App) {
    let effective = effective_bindings(cx);
    let mut bindings: Vec<KeyBinding> = cx
        .try_global::<BaseBindings>()
        .map(|base| base.0.clone())
        .unwrap_or_default();
    bindings.extend(action_bindings(&effective));
    bindings.extend(fixed_bindings());
    cx.clear_key_bindings();
    cx.bind_keys(bindings);
}

pub fn rebind(cx: &mut App) {
    rebuild_keymap(cx);
    set_menus(cx);
}

/// The three config fields `effective_bindings` reads, as one comparable
/// value. The config watcher reloads on every write — including the app's
/// own `save()`, which fires on a sidebar drag — so it compares the triple
/// before and after and only rebinds when one of these actually moved;
/// otherwise each save would rebuild the keymap for nothing (#548).
pub(crate) fn keybinding_config(cx: &App) -> (Vec<(String, String)>, String, String) {
    let cfg = cx.global::<Config>();
    let mut overrides: Vec<(String, String)> = cfg
        .keybindings
        .iter()
        .map(|(a, k)| (a.clone(), k.clone()))
        .collect();
    // A HashMap's order is not stable across reloads, and a reordered map is
    // not a changed binding set.
    overrides.sort();
    (overrides, cfg.keybinding_preset.clone(), cfg.prefix.clone())
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
        // `secondary-enter` is `ToggleFullscreen` on macOS. That is not a
        // clash: `ScmCommit` binds inside the `ScmCommit` key context, so it
        // only wins while the caret sits in the commit box.
        ("ScmCommit", "secondary-enter"),
        ("ScmCommitAmend", ""),
        ("ScmStageAll", ""),
        ("ScmUnstageAll", ""),
        ("ScmDiscardAll", ""),
        ("ScmRefresh", ""),
        ("ScmSync", ""),
        ("ScmPush", ""),
        ("ScmPull", ""),
        ("ScmFetch", ""),
        ("ScmCheckoutBranch", ""),
        ("ScmCreateBranch", ""),
        ("ScmToggleGraph", ""),
        ("ToggleDiffViewMode", ""),
        ("ToggleSftp", ""),
        ("ShowSshForwards", ""),
        ("ToggleCodePanel", "secondary-shift-e"),
        // Implemented, dispatchable, and until now unbindable: `set_binding`
        // only fills slots that exist here, so `"ShowRightPanelInfo": "ctrl-1"`
        // in config.json was dropped without a word, and the Keybindings page —
        // which reads this list — never showed them at all.
        ("ShowRightPanelInfo", ""),
        ("ShowRightPanelChanges", ""),
        ("ShowRightPanelFiles", ""),
        ("EditorSave", "secondary-s"),
        ("OpenSshProfiles", ""),
        ("RestartSshSession", "secondary-shift-r"),
        ("Quit", per_platform("secondary-q", "secondary-shift-q")),
    ]
}

/// Where an action sits on the Keybindings page, and what it is called there.
///
/// The page used to render `humanize_action`'s CamelCase split — English in a
/// three-locale app, and a fourth vocabulary on top of the menu bar, the
/// palette and the docs ("Toggle Maximize Pane" for what everything else calls
/// Zoom Pane). This routes both the name and the section through the strings
/// the rest of the app already uses.
pub(crate) fn action_entry(action: &str) -> (CommandGroup, String) {
    authored_entry(action).unwrap_or_else(|| {
        // An action with no entry still renders, under Application and with its
        // name split on capitals — a new action shows up on the page rather
        // than disappearing from it. `every_action_has_an_authored_name` is
        // what keeps that from being how the page looks.
        (CommandGroup::Application, humanize_action(action))
    })
}

fn authored_entry(action: &str) -> Option<(CommandGroup, String)> {
    // The numbered families are one row each in nine copies; a templated label
    // beats nine hand-written strings per locale.
    if let Some(n) = action.strip_prefix("ActivateTab") {
        return Some((
            CommandGroup::TabsPanes,
            t_fmt(L10nKey::KeybindGoToTab, &[("n", n)]),
        ));
    }
    if let Some(n) = action.strip_prefix("SelectWorkspace") {
        return Some((
            CommandGroup::Workspaces,
            t_fmt(L10nKey::KeybindGoToWorkspace, &[("n", n)]),
        ));
    }
    Some(match action {
        "NewTab" => (CommandGroup::TabsPanes, t(L10nKey::CmdNewTab).to_string()),
        "CloseActiveTab" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdClosePaneTab).to_string(),
        ),
        "RenameTab" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdRenameTab).to_string(),
        ),
        "NewWorktreeTab" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdNewWorktreeTab).to_string(),
        ),
        "CloseOtherTabs" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdCloseOtherTabs).to_string(),
        ),
        "CloseTabsToTheRight" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdCloseTabsToTheRight).to_string(),
        ),
        "CopyWorkingDirectory" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdCopyWorkingDirectory).to_string(),
        ),
        "MarkTabUnread" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdMarkTabAsUnread).to_string(),
        ),
        "ReopenClosedTab" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdReopenClosedTab).to_string(),
        ),
        "SplitRight" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdSplitRight).to_string(),
        ),
        "SplitDown" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdSplitDown).to_string(),
        ),
        "ToggleMaximizePane" => (CommandGroup::TabsPanes, t(L10nKey::CmdZoomPane).to_string()),
        "FocusNextPane" => (CommandGroup::TabsPanes, t(L10nKey::CmdNextPane).to_string()),
        "FocusPrevPane" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdPreviousPane).to_string(),
        ),
        "FocusPaneLeft" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdFocusPaneLeft).to_string(),
        ),
        "FocusPaneRight" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdFocusPaneRight).to_string(),
        ),
        "FocusPaneUp" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdFocusPaneUp).to_string(),
        ),
        "FocusPaneDown" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdFocusPaneDown).to_string(),
        ),
        "ResizePaneLeft" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdResizePaneLeft).to_string(),
        ),
        "ResizePaneRight" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdResizePaneRight).to_string(),
        ),
        "ResizePaneUp" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdResizePaneUp).to_string(),
        ),
        "ResizePaneDown" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdResizePaneDown).to_string(),
        ),
        "SwapPaneNext" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdSwapPaneNext).to_string(),
        ),
        "SwapPanePrev" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdSwapPanePrevious).to_string(),
        ),
        "NextTab" => (CommandGroup::TabsPanes, t(L10nKey::CmdNextTab).to_string()),
        "PrevTab" => (
            CommandGroup::TabsPanes,
            t(L10nKey::CmdPreviousTab).to_string(),
        ),
        "NewWorkspace" => (
            CommandGroup::Workspaces,
            t(L10nKey::CmdNewWorkspace).to_string(),
        ),
        "RenameWorkspace" => (
            CommandGroup::Workspaces,
            t(L10nKey::CmdRenameWorkspace).to_string(),
        ),
        "StopWorkspace" => (
            CommandGroup::Workspaces,
            t(L10nKey::CmdStopWorkspace).to_string(),
        ),
        "DeleteWorkspace" => (
            CommandGroup::Workspaces,
            t(L10nKey::CmdDeleteWorkspace).to_string(),
        ),
        "ToggleSwitcher" => (
            CommandGroup::Workspaces,
            t(L10nKey::CmdSwitchWorkspace).to_string(),
        ),
        "IncreaseFontSize" => (
            CommandGroup::View,
            t(L10nKey::AppMenuIncreaseFontSize).to_string(),
        ),
        "DecreaseFontSize" => (
            CommandGroup::View,
            t(L10nKey::AppMenuDecreaseFontSize).to_string(),
        ),
        "ResetFontSize" => (CommandGroup::View, t(L10nKey::CmdResetFontSize).to_string()),
        "ToggleFullscreen" => (
            CommandGroup::View,
            t(L10nKey::CmdEnterFullScreen).to_string(),
        ),
        "ToggleTabSidebar" => (
            CommandGroup::View,
            t(L10nKey::AppMenuTabBarPosition).to_string(),
        ),
        "ToggleLeftPanel" => (
            CommandGroup::View,
            t(L10nKey::AppMenuLeftSidebar).to_string(),
        ),
        "ToggleRightPanel" => (
            CommandGroup::View,
            t(L10nKey::AppMenuRightPanel).to_string(),
        ),
        "ToggleCodePanel" => (CommandGroup::View, t(L10nKey::AppMenuCodePanel).to_string()),
        "ShowRightPanelInfo" => (
            CommandGroup::View,
            t(L10nKey::CmdRightPanelInfo).to_string(),
        ),
        "ShowRightPanelChanges" => (
            CommandGroup::View,
            t(L10nKey::CmdRightPanelChanges).to_string(),
        ),
        "ShowRightPanelFiles" => (
            CommandGroup::View,
            t(L10nKey::CmdRightPanelFiles).to_string(),
        ),
        "FindInTerminal" => (
            CommandGroup::Terminal,
            t(L10nKey::CmdFindInTerminal).to_string(),
        ),
        "FindNext" => (CommandGroup::Terminal, t(L10nKey::CmdFindNext).to_string()),
        "FindPrevious" => (
            CommandGroup::Terminal,
            t(L10nKey::CmdFindPrevious).to_string(),
        ),
        "ClearScrollback" => (
            CommandGroup::Terminal,
            t(L10nKey::CmdClearScrollback).to_string(),
        ),
        "CopyText" => (CommandGroup::Terminal, t(L10nKey::CmdCopy).to_string()),
        "PasteText" => (CommandGroup::Terminal, t(L10nKey::CmdPaste).to_string()),
        "InsertNewline" => (
            CommandGroup::Terminal,
            t(L10nKey::KeybindInsertNewline).to_string(),
        ),
        "EditorSave" => (CommandGroup::Terminal, t(L10nKey::Save).to_string()),
        "OpenSshProfiles" => (
            CommandGroup::Ssh,
            t(L10nKey::CmdSshManageProfiles).to_string(),
        ),
        "RestartSshSession" => (CommandGroup::Ssh, t(L10nKey::CmdSshReconnect).to_string()),
        "ToggleSftp" => (CommandGroup::Ssh, t(L10nKey::CmdSshRemoteFiles).to_string()),
        "ShowSshForwards" => (
            CommandGroup::Ssh,
            t(L10nKey::CmdSshPortForwarding).to_string(),
        ),
        "ForkAgentSession" => (CommandGroup::Agents, t(L10nKey::CmdForkSession).to_string()),
        "ForkAgentSessionRight" => (
            CommandGroup::Agents,
            t(L10nKey::KeybindForkSessionRight).to_string(),
        ),
        "ForkAgentSessionLeft" => (
            CommandGroup::Agents,
            t(L10nKey::KeybindForkSessionLeft).to_string(),
        ),
        "ForkAgentSessionDown" => (
            CommandGroup::Agents,
            t(L10nKey::KeybindForkSessionDown).to_string(),
        ),
        "ForkAgentSessionUp" => (
            CommandGroup::Agents,
            t(L10nKey::KeybindForkSessionUp).to_string(),
        ),
        "CopyAgentSessionId" => (
            CommandGroup::Agents,
            t(L10nKey::CmdCopySessionId).to_string(),
        ),
        "TogglePalette" => (
            CommandGroup::Application,
            t(L10nKey::AppMenuCommandPalette).to_string(),
        ),
        "OpenSettings" => (
            CommandGroup::Application,
            t(L10nKey::CmdSettings).to_string(),
        ),
        "ShowKeyboardShortcuts" => (
            CommandGroup::Application,
            t(L10nKey::CmdKeyboardShortcuts).to_string(),
        ),
        "About" => (
            CommandGroup::Application,
            t(L10nKey::CmdAboutTty7).to_string(),
        ),
        "CheckForUpdates" => (
            CommandGroup::Application,
            t(L10nKey::CmdCheckForUpdates).to_string(),
        ),
        "OpenDocumentation" => (
            CommandGroup::Application,
            t(L10nKey::CmdDocumentation).to_string(),
        ),
        "OpenDiscord" => (
            CommandGroup::Application,
            t(L10nKey::CmdJoinDiscord).to_string(),
        ),
        "ReportIssue" => (
            CommandGroup::Application,
            t(L10nKey::CmdReportIssue).to_string(),
        ),
        "HideApp" => (
            CommandGroup::Application,
            t(L10nKey::AppMenuHideApp).to_string(),
        ),
        "HideOthers" => (
            CommandGroup::Application,
            t(L10nKey::AppMenuHideOthers).to_string(),
        ),
        "ShowAll" => (
            CommandGroup::Application,
            t(L10nKey::AppMenuShowAll).to_string(),
        ),
        "MinimizeWindow" => (
            CommandGroup::Application,
            t(L10nKey::AppMenuMinimize).to_string(),
        ),
        "ZoomWindow" => (
            CommandGroup::Application,
            t(L10nKey::AppMenuZoom).to_string(),
        ),
        "Quit" => (
            CommandGroup::Application,
            t(L10nKey::CmdQuitTty7).to_string(),
        ),
        // The source control verbs wear their palette names, so the
        // Keybindings page and the palette agree on what a chord does.
        "ScmCommit" => (CommandGroup::Git, t(L10nKey::CmdGitCommit).to_string()),
        "ScmCommitAmend" => (
            CommandGroup::Git,
            t(L10nKey::ScmAmendLastCommit).to_string(),
        ),
        "ScmStageAll" => (CommandGroup::Git, t(L10nKey::CmdGitStageAll).to_string()),
        "ScmUnstageAll" => (CommandGroup::Git, t(L10nKey::CmdGitUnstageAll).to_string()),
        "ScmDiscardAll" => (CommandGroup::Git, t(L10nKey::CmdGitDiscardAll).to_string()),
        "ScmRefresh" => (CommandGroup::Git, t(L10nKey::ScmRefresh).to_string()),
        "ScmSync" => (CommandGroup::Git, t(L10nKey::CmdGitSync).to_string()),
        "ScmPush" => (CommandGroup::Git, t(L10nKey::CmdGitPush).to_string()),
        "ScmPull" => (CommandGroup::Git, t(L10nKey::CmdGitPull).to_string()),
        "ScmFetch" => (CommandGroup::Git, t(L10nKey::CmdGitFetch).to_string()),
        "ScmCheckoutBranch" => (CommandGroup::Git, t(L10nKey::CmdGitCheckoutTo).to_string()),
        "ScmCreateBranch" => (
            CommandGroup::Git,
            t(L10nKey::CmdGitCreateBranch).to_string(),
        ),
        "ScmToggleGraph" => (CommandGroup::Git, t(L10nKey::CmdGitToggleGraph).to_string()),
        "ToggleDiffViewMode" => (
            CommandGroup::View,
            t(L10nKey::CmdToggleDiffViewMode).to_string(),
        ),
        _ => return None,
    })
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
    match effective.iter_mut().find(|(a, _)| a == action) {
        Some(slot) => slot.1 = key,
        // A hand-edited config.json with a typo used to vanish into this
        // branch. The Keybindings page lists every name that works.
        None => log::warn!("keybinding for unknown action {action:?} ignored"),
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
            if let Some(stripped) = rest.strip_prefix(&prefix)
                && !stripped.is_empty()
            {
                tokens.push(glyph.to_string());
                rest = stripped;
                continue 'outer;
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
        "ScmCommit" | "ScmCommitAmend" => Some("ScmCommit"),
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
        "ScmCommit" => KeyBinding::new(keystroke, ScmCommit, action_context(action)),
        "ScmCommitAmend" => KeyBinding::new(keystroke, ScmCommitAmend, action_context(action)),
        "ScmStageAll" => KeyBinding::new(keystroke, ScmStageAll, None),
        "ScmUnstageAll" => KeyBinding::new(keystroke, ScmUnstageAll, None),
        "ScmDiscardAll" => KeyBinding::new(keystroke, ScmDiscardAll, None),
        "ScmRefresh" => KeyBinding::new(keystroke, ScmRefresh, None),
        "ScmSync" => KeyBinding::new(keystroke, ScmSync, None),
        "ScmPush" => KeyBinding::new(keystroke, ScmPush, None),
        "ScmPull" => KeyBinding::new(keystroke, ScmPull, None),
        "ScmFetch" => KeyBinding::new(keystroke, ScmFetch, None),
        "ScmCheckoutBranch" => KeyBinding::new(keystroke, ScmCheckoutBranch, None),
        "ScmCreateBranch" => KeyBinding::new(keystroke, ScmCreateBranch, None),
        "ScmToggleGraph" => KeyBinding::new(keystroke, ScmToggleGraph, None),
        "ToggleDiffViewMode" => KeyBinding::new(keystroke, ToggleDiffViewMode, None),
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
    use gpui::Action as _;

    /// The actions a keymap built from `action_bindings` dispatches for `keys`
    /// typed in `context`, in precedence order — the same lookup gpui performs
    /// on a real keypress.
    ///
    /// Bindings are asserted through this rather than through a mirror of the
    /// table, so a chord the app would not really install, or would install in
    /// another context, cannot pass.
    fn dispatched(effective: &[(String, String)], keys: &str, context: &str) -> Vec<&'static str> {
        let mut keymap = gpui::Keymap::default();
        keymap.add_bindings(action_bindings(effective));
        let input: Vec<Keystroke> = keys
            .split(' ')
            .map(|k| Keystroke::parse(k).expect("the typed keystroke parses"))
            .collect();
        let context = [gpui::KeyContext::parse(context).expect("the context parses")];
        keymap
            .bindings_for_input(&input, &context)
            .0
            .iter()
            .map(|b| b.action().name())
            .collect()
    }

    #[test]
    fn every_dispatchable_action_has_a_slot_to_bind_it_in() {
        // `make_binding` is what turns an action name into a real binding, and
        // `default_bindings` is the only list `set_binding` will write into. An
        // action in the first but not the second cannot be bound at all — not
        // from config.json, which drops it silently, and not from the
        // Keybindings page, which reads the second.
        let bindable: std::collections::HashSet<&str> =
            default_bindings().into_iter().map(|(a, _)| a).collect();
        for action in [
            "ShowRightPanelInfo",
            "ShowRightPanelChanges",
            "ShowRightPanelFiles",
        ] {
            assert!(bindable.contains(action), "{action} has nowhere to bind to");
            assert!(
                make_binding(action, "ctrl-1").is_some(),
                "{action} has a slot but nothing to dispatch"
            );
        }
    }

    #[test]
    fn every_action_has_an_authored_name() {
        crate::ui::i18n::set_locale("en");
        let missing: Vec<&str> = default_bindings()
            .into_iter()
            .map(|(a, _)| a)
            .filter(|a| authored_entry(a).is_none())
            .collect();
        assert!(
            missing.is_empty(),
            "these actions would render on the Keybindings page as a CamelCase \
             split of their internal name, in English, in a three-locale app: {missing:?}"
        );
    }

    #[test]
    fn the_page_speaks_the_same_words_as_the_rest_of_the_app() {
        crate::ui::i18n::set_locale("en");
        // The four names the critique found for one action; the menu bar, the
        // palette and the docs all say Zoom Pane.
        assert_eq!(action_entry("ToggleMaximizePane").1, "Zoom Pane");
        assert_eq!(action_entry("CloseActiveTab").1, "Close Pane / Tab");
        assert_eq!(action_entry("ClearScrollback").1, "Clear Scrollback");
        assert_eq!(action_entry("TogglePalette").1, "Command Palette…");
        assert_eq!(action_entry("ToggleSwitcher").1, "Switch Workspace…");
        // The numbered families are templated, not nine strings per locale.
        assert_eq!(action_entry("ActivateTab3").1, "Go to Tab 3");
        assert_eq!(action_entry("SelectWorkspace7").1, "Go to Workspace 7");
        // And they land in the section they belong to.
        assert_eq!(action_entry("ActivateTab3").0, CommandGroup::TabsPanes);
        assert_eq!(action_entry("SelectWorkspace7").0, CommandGroup::Workspaces);
        assert_eq!(action_entry("ToggleSftp").0, CommandGroup::Ssh);
        assert_eq!(action_entry("ForkAgentSessionUp").0, CommandGroup::Agents);
    }

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
    fn every_action_has_a_binding_arm() {
        // The sibling test above only reaches actions that ship with a default
        // keystroke, which leaves the unbound ones — the majority — free to be
        // listed in `default_bindings` with no `make_binding` arm behind them.
        // Nothing surfaces that: the action shows up in Settings, the user
        // assigns a key, and the key silently does nothing.
        for (action, _) in default_bindings() {
            assert!(
                make_binding(action, "ctrl-f12").is_some(),
                "no make_binding arm for action {action}; \
                 anyone who binds a key to it in Settings gets nothing"
            );
        }
    }

    #[test]
    fn the_commit_key_only_fires_inside_the_commit_box() {
        // `secondary-enter` is `ToggleFullscreen` on macOS. The two coexist
        // only because the commit binding is scoped; drop the context and
        // committing steals full screen everywhere.
        assert_eq!(action_context("ScmCommit"), Some("ScmCommit"));
        assert_eq!(action_context("ScmCommitAmend"), Some("ScmCommit"));
        assert_eq!(action_context("ToggleFullscreen"), None);
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
            assert!(
                dispatched(&effective, key, "Terminal").contains(&InsertNewline::name_for_type()),
                "{key} does not reach InsertNewline in a terminal"
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
                dispatched(&effective, key, "Terminal").contains(&PasteText::name_for_type()),
                "{key} must paste in a terminal"
            );
            assert!(
                !dispatched(&effective, key, "Workspace").contains(&PasteText::name_for_type()),
                "{key} is a terminal chord and must not paste outside one"
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
            dispatched(&effective, "ctrl-o", "Terminal"),
            vec![InsertNewline::name_for_type()],
            "the chord the config asks for is the one the keymap dispatches"
        );
        for retired in ["shift-enter", "alt-enter"] {
            assert!(
                dispatched(&effective, retired, "Terminal").is_empty(),
                "{retired} is off the action now and must dispatch nothing"
            );
        }

        let unbound = vec![("InsertNewline".to_string(), String::new())];
        assert!(extra_keystrokes(&unbound).is_empty());
        assert!(action_bindings(&unbound).is_empty());
    }

    #[test]
    fn each_binding_lands_in_the_context_its_action_is_scoped_to() {
        let effective = vec![
            ("InsertNewline".to_string(), "shift-enter".to_string()),
            ("NewTab".to_string(), "secondary-t".to_string()),
        ];
        // The newline is a terminal chord, and its default ships the fallback
        // beside it; both are scoped, so neither reaches the rest of the app.
        let mut newline = dispatched(&effective, "shift-enter", "Terminal");
        newline.sort_unstable();
        assert_eq!(
            newline,
            vec![
                InsertNewline::name_for_type(),
                InsertNewlineFallback::name_for_type(),
            ]
        );
        assert!(dispatched(&effective, "shift-enter", "Workspace").is_empty());
        assert_eq!(
            dispatched(&effective, "alt-enter", "Terminal"),
            vec![InsertNewline::name_for_type()],
            "the extra chord follows its action's scope"
        );
        // A window action has no context: it has to work from a terminal too.
        for context in ["Terminal", "Workspace"] {
            assert_eq!(
                dispatched(&effective, "secondary-t", context),
                vec![NewTab::name_for_type()],
                "NewTab must not be scoped to {context}"
            );
        }
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
        // Per context, not globally: gpui resolves a keystroke by walking the
        // focus chain outwards, so a chord bound inside a narrow context and
        // again with no context is not a clash — the narrow one wins while
        // that element has focus and the global one applies everywhere else.
        // `ScmCommit` and `ToggleFullscreen` both take secondary-enter on that
        // basis. Two bindings sharing a chord *and* a context is still a bug,
        // because then which one fires is arbitrary.
        let mut seen: Vec<(&str, &str, Option<&'static str>)> = Vec::new();
        for (action, spec) in default_bindings() {
            if spec.is_empty() {
                continue;
            }
            let context = action_context(action);
            if let Some((other, _, _)) = seen.iter().find(|(_, s, c)| *s == spec && *c == context) {
                panic!("{action} and {other} both claim {spec} in context {context:?}");
            }
            seen.push((action, spec, context));
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

    #[gpui::test]
    fn the_keybinding_triple_only_moves_when_a_binding_does(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
            init(cx);

            let before = keybinding_config(cx);
            assert_eq!(
                keybinding_config(cx),
                before,
                "reading the config twice changes nothing"
            );

            // An unrelated edit leaves the triple alone — this is the gate the
            // watcher compares, and a sidebar drag must not rebind.
            cx.global_mut::<Config>().dim_inactive_panes = false;
            assert_eq!(keybinding_config(cx), before);

            // A real binding edit moves it.
            cx.global_mut::<Config>()
                .keybindings
                .insert("RenameTab".to_string(), "ctrl-shift-r".to_string());
            assert_ne!(keybinding_config(cx), before);

            // So does the preset, and the prefix.
            let with_override = keybinding_config(cx);
            cx.global_mut::<Config>().keybinding_preset = "tmux".to_string();
            assert_ne!(keybinding_config(cx), with_override);
            let with_preset = keybinding_config(cx);
            cx.global_mut::<Config>().prefix = "ctrl-a".to_string();
            assert_ne!(keybinding_config(cx), with_preset);
        });
    }

    #[gpui::test]
    fn repeated_rebinds_do_not_grow_the_keymap(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
            init(cx);
            let size = cx.key_bindings().borrow().bindings().len();
            // A rebind that clears first stays the same size however many
            // times it runs; the append-only one this replaced grew by a full
            // table each call, and every keystroke walks the whole map (#548).
            for _ in 0..3 {
                rebind(cx);
            }
            assert_eq!(
                cx.key_bindings().borrow().bindings().len(),
                size,
                "rebind clears before it binds, so the map does not grow"
            );
        });
    }

    #[gpui::test]
    fn a_rebuild_keeps_the_bindings_tty7_did_not_install(cx: &mut TestAppContext) {
        cx.update(|cx| {
            // gpui-component binds the whole `Input` editing table — backspace
            // among it — from its own `init`, which runs once, before ours. A
            // rebuild replaces the map, so it has to carry them; without this
            // the first rebind left every text field in the app unable to
            // delete a character (#548).
            gpui_component::init(cx);
            cx.set_global(Config::default());

            let backspace = |cx: &App| {
                let typed = [Keystroke::parse("backspace").unwrap()];
                cx.key_bindings()
                    .borrow()
                    .all_bindings_for_input(&typed)
                    .iter()
                    .map(|b| b.action().name().to_string())
                    .collect::<Vec<_>>()
            };

            let inherited = backspace(cx);
            assert!(
                !inherited.is_empty(),
                "gpui-component binds backspace before tty7 touches the keymap"
            );

            init(cx);
            assert_eq!(backspace(cx), inherited, "init must not drop them");
            rebind(cx);
            assert_eq!(backspace(cx), inherited, "nor may a rebind");
        });
    }
}

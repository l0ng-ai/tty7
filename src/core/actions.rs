//! Menu / keyboard actions, defined in one place so both the application shell
//! (`app.rs`) and the terminal view (`terminal::view`) can reference them
//! without depending on each other. They drive the macOS menu bar and the
//! keymap, so a click and a shortcut go through exactly the same path.

use gpui::actions;

actions!(
    tty7,
    [
        NewTab,
        // Create a workspace and the window that shows it. One workspace is
        // shown by exactly one window and vice versa — there is deliberately no
        // "new window on the same workspace", which would need two clients on
        // one set of daemon panes (the daemon allows only one).
        NewWorkspace,
        // Stop the current workspace: kill its sessions and close its window,
        // keeping its layout on file so it can be started again. The deliberate
        // opposite of a window close, which only detaches — hence the verb.
        StopWorkspace,
        // Stop it *and* forget the layout. The only irreversible one.
        DeleteWorkspace,
        // Rename the current workspace in place, from the title-bar chip.
        // Until now `Workspace.name` could only ever be the derived repo name —
        // there was no way for the user to set one.
        RenameWorkspace,
        // Open the workspace switcher: every workspace on every machine, in one
        // panel. The title-bar chip opens the same thing, so this is the
        // keyboard's half of a control that is otherwise mouse-only.
        ToggleSwitcher,
        // Show the Nth workspace in the Window menu's order (see
        // `ui::windows::menu_order`). Unit actions rather than one
        // parameterized action, matching `ActivateTab1..9` — it keeps them
        // nameable in config/Settings like every other binding.
        SelectWorkspace1,
        SelectWorkspace2,
        SelectWorkspace3,
        SelectWorkspace4,
        SelectWorkspace5,
        SelectWorkspace6,
        SelectWorkspace7,
        SelectWorkspace8,
        SelectWorkspace9,
        CloseActiveTab,
        // Tab operations that until now existed only as tab-context-menu rows,
        // reachable by right-clicking the *right* chip. As actions they also
        // reach the menu bar, the palette, and Settings → Keybindings; each acts
        // on the active tab, which is what "this tab" means with no chip clicked.
        RenameTab,
        NewWorktreeTab,
        CloseOtherTabs,
        CloseTabsToTheRight,
        CopyWorkingDirectory,
        MarkTabUnread,
        // Branch the coding-agent session running in this tab into a second,
        // independent one by shelling the agent's own fork command (issue
        // #211). Placement follows where the user asked from: the bare action —
        // menu bar, palette, a bound key — and the tab context menu open a new
        // tab, while the pane right-click menu offers the four split directions
        // below, since a pane-level ask is a spatial one.
        ForkAgentSession,
        ForkAgentSessionRight,
        ForkAgentSessionLeft,
        ForkAgentSessionDown,
        ForkAgentSessionUp,
        // Put the agent's *native* session id on the clipboard, beside "Copy
        // Working Directory". Codex has no copy/duplicate subcommand, so
        // "copy the session" means copying its id — paste it into `codex
        // resume`, a bug report, or another tool.
        CopyAgentSessionId,
        SplitRight,
        SplitDown,
        FocusNextPane,
        FocusPrevPane,
        // Directional pane focus (tmux `prefix ←/→/↑/↓`): move focus to the
        // adjacent pane in that direction.
        FocusPaneLeft,
        FocusPaneRight,
        FocusPaneUp,
        FocusPaneDown,
        // Grow (Right/Down) or shrink (Left/Up) the focused pane along the
        // matching axis by nudging its nearest enclosing split's ratio.
        ResizePaneLeft,
        ResizePaneRight,
        ResizePaneUp,
        ResizePaneDown,
        // Swap the focused pane with its next / previous sibling in leaf order
        // (tmux `prefix }` / `prefix {`); focus follows the moved pane.
        SwapPaneNext,
        SwapPanePrev,
        // Relative tab navigation (tmux `prefix n` / `prefix p`).
        NextTab,
        PrevTab,
        // Jump straight to tab 1‑9 (⌘/Ctrl+1‑9, tmux `prefix 1‑9`). Unit actions
        // rather than one parameterized action so config/Settings can index them
        // by name like every other binding.
        ActivateTab1,
        ActivateTab2,
        ActivateTab3,
        ActivateTab4,
        ActivateTab5,
        ActivateTab6,
        ActivateTab7,
        ActivateTab8,
        ActivateTab9,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize,
        TogglePalette,
        ReopenClosedTab,
        ToggleMaximizePane,
        ToggleFullscreen,
        // Switch the tab bar between the horizontal title-bar strip and the
        // vertical left-side sidebar (persists `tab_bar_position`).
        ToggleTabSidebar,
        // Collapse/expand the left tab sidebar in place (persists
        // `sidebar_collapsed`). Unlike `ToggleTabSidebar` this does not switch
        // the tab bar to the horizontal strip — the rail just goes away and
        // comes back at the same width.
        ToggleLeftPanel,
        // Show/hide the right detail panel — session info, working-tree changes,
        // and the file tree (persists `right_panel_visible`).
        ToggleRightPanel,
        // Jump straight to one of the right panel's tabs, opening the panel if
        // it was closed. Unit actions rather than one parameterized action so
        // config/Settings can bind them by name; unbound by default, since the
        // panel's own tab row is the primary way in.
        ShowRightPanelInfo,
        ShowRightPanelOutline,
        ShowRightPanelChanges,
        ShowRightPanelFiles,
        OpenSettings,
        // Open Settings straight to its Keybindings section — the Help menu's
        // "Keyboard Shortcuts" and the palette's shortcut entry both land here,
        // rather than making the user open Settings and then find the section.
        ShowKeyboardShortcuts,
        // Open Settings on the About section. The macOS App menu's first item
        // has to exist and has to be called "About tty7"; routing it to the
        // section that already carries version/links keeps one About, not two.
        About,
        // Run the same update check the app does at startup (see `core::update`)
        // on demand, then report the outcome. Previously only the tray offered
        // this, which is not where a Mac user looks for it.
        CheckForUpdates,
        // Standard macOS App-menu items. gpui exposes the platform calls but
        // binds nothing by default, so they need real actions to hang off.
        HideApp,
        HideOthers,
        ShowAll,
        // Standard macOS Window-menu items.
        MinimizeWindow,
        ZoomWindow,
        // Help menu destinations. Each opens a URL in the default browser; kept
        // as separate actions (rather than one parameterized one) so they can be
        // bound and searched by name like everything else.
        OpenDocumentation,
        OpenDiscord,
        ReportIssue,
        RestartDaemon,
        // Show the detail panel's Files tab, which browses the focused pane's
        // remote filesystem over SFTP when that pane is native SSH (WS5).
        ToggleSftp,
        // Open the detail panel's Info tab on the focused native-SSH pane with
        // the add-forward form expanded (WS4). The band itself is always on that
        // tab; this is the way in that doesn't require the panel to be open.
        ShowSshForwards,
        // Toggle the code panel: a full-body overlay of [file tree | editor]
        // covering the terminal (settings-overlay style).
        ToggleCodePanel,
        // Save the editor panel's active file (⌘S).
        EditorSave,
        // Open the SSH profile manager/editor full-window page (WS6, FR-P1).
        OpenSshProfiles,
        // Reconnect a dead native-SSH pane in place (WS6, FR-E4).
        RestartSshSession,
        SendTab,
        SendBackTab,
        Quit
    ]
);

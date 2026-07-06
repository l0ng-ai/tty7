//! Menu / keyboard actions, defined in one place so both the application shell
//! (`app.rs`) and the terminal view (`terminal::view`) can reference them
//! without depending on each other. They drive the macOS menu bar and the
//! keymap, so a click and a shortcut go through exactly the same path.

use gpui::actions;

actions!(
    tty7,
    [
        NewTab,
        CloseActiveTab,
        SplitRight,
        SplitDown,
        FocusNextPane,
        FocusPrevPane,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize,
        TogglePalette,
        ReopenClosedTab,
        ToggleMaximizePane,
        OpenSettings,
        RestartDaemon,
        SendTab,
        SendBackTab,
        Quit
    ]
);

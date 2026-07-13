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
        OpenSettings,
        RestartDaemon,
        SendTab,
        SendBackTab,
        Quit
    ]
);

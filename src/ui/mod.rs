//! The GPUI view layer: the window shell (`app`), the split-pane tree (`pane`),
//! the command palette (`palette`), the settings panel (`settings`), and the
//! menu-bar / keymap / theme wiring (`keymap`, `theme`).
//!
//! Everything here may depend on `core` and `terminal`; nothing in those layers
//! depends back on `ui`.

pub mod app;
pub mod assets;
pub mod code_editor;
pub mod diff_overlay;
pub mod file_tree;
pub mod forwards;
pub mod hints;
pub mod home;
pub mod keymap;
pub mod palette;
pub mod pane;
pub mod perf;
pub mod presets;
pub mod reorder;
pub mod right_panel;
pub mod scrollbar;
pub mod settings;
pub mod sftp;
pub mod ssh_connect;
pub mod ssh_prompt;
pub mod tab_sidebar;
pub mod tab_strip;
// Taskbar overlay badges are a Windows concept (`ITaskbarList3`); macOS's
// Dock badge would be this module's sibling, not its port.
#[cfg(windows)]
pub mod taskbar;
pub mod theme;
pub mod tray;
pub mod windows;
pub mod worktree_prompt;

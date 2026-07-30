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
// The `Host` layer's GUI half. The facade and the registry land ahead of the
// call sites that consume
// them — the six views move over to `HostOps` as a separate change — so they
// read as dead code until that merges.
#[allow(dead_code)]
pub mod host_ops;
#[allow(dead_code)]
pub mod host_registry;
pub mod keymap;
pub mod local_link;
pub mod machine_mirror;
pub mod palette;
pub mod pane;
pub mod pending_pane;
pub mod perf;
pub mod presets;
pub mod remote_connect;
pub mod remote_workspace;
pub mod reorder;
pub mod right_panel;
pub mod rounding;
pub mod scrollbar;
pub mod settings;
pub mod sftp;
pub mod ssh_connect;
pub mod ssh_prompt;
pub mod switcher;
pub mod tab_sidebar;
pub mod tab_strip;
pub mod theme;
pub mod tray;
pub mod tree_sync;
pub mod windows;
pub mod worktree_prompt;

pub mod app;
pub mod assets;
pub mod code_editor;
pub mod diff_overlay;
pub mod file_copy;
pub mod file_tree;
pub mod forwards;
pub mod hints;
pub mod home;
#[allow(dead_code)]
pub mod host_ops;
#[allow(dead_code)]
pub mod host_registry;
pub mod i18n;
pub mod keymap;
pub mod local_link;
pub mod machine_mirror;
pub mod palette;
pub mod pane;
pub mod pane_drag;
pub mod pending_pane;
pub mod perf;
pub mod prefill;
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

/// The two answers a confirmation dialog gets, arranged the way macOS arranges
/// them: the action on the right, where Return lands and where every other app
/// on the machine puts it, and the safe answer on the left, marked as the
/// cancel so it also answers Escape, Space and the initial keyboard focus.
///
/// gpui renders answer 0 rightmost, so the action goes first — `Ok(0)` is
/// "they meant it" and everything else, including a dropped channel, is "leave
/// it alone".
pub(crate) fn confirm_answers(action: &str, keep: &str) -> [gpui::PromptButton; 2] {
    [
        gpui::PromptButton::ok(action),
        gpui::PromptButton::cancel(keep),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_action_answers_return_and_the_safe_one_answers_escape() {
        // gpui hands answer 0 to the platform first, and both NSAlert and
        // TaskDialog draw that one on the right and give it Return. Reversing
        // these two puts Delete under the mouse where Cancel belongs.
        let [action, keep] = super::confirm_answers("Delete", "Cancel");
        assert_eq!(action.label(), "Delete");
        assert!(!action.is_cancel(), "answer 0 has to keep Return");
        assert_eq!(keep.label(), "Cancel");
        assert!(keep.is_cancel(), "only a cancel answer is given Escape");
    }
}

pub mod app;
pub mod assets;
pub mod code_editor;
pub mod diff_overlay;
pub mod diff_rows;
pub mod file_copy;
pub mod file_tree;
pub mod forwards;
pub mod hints;
pub mod home;
pub mod host_ops;
pub mod host_registry;
pub mod i18n;
pub mod keymap;
pub mod local_link;
pub mod machine_mirror;
pub mod palette;
pub mod pane;
pub mod pane_drag;
pub mod path_display;
pub mod pending_pane;
pub mod perf;
pub mod prefill;
pub mod presets;
pub mod remote_connect;
pub mod remote_workspace;
pub mod reorder;
pub mod right_panel;
pub mod rounding;
pub mod scm;
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

    /// Why a dialog has to build its buttons instead of passing strings.
    ///
    /// `window.prompt` takes anything `Into<PromptButton>`, and gpui's
    /// conversion decides the role by matching the English word: "cancel"
    /// becomes a cancel answer and everything else becomes a plain `Other`,
    /// which answers neither Escape nor Return. So handing it `t(Cancel)`
    /// gives a working dialog in English and a Chinese or Japanese one with
    /// nothing on Escape — the failure is invisible to whoever writes it.
    ///
    /// That is the whole reason [`confirm_answers`](super::confirm_answers)
    /// exists. If this test ever fails, gpui started reading roles some other
    /// way and the rule can be relaxed.
    #[test]
    fn a_localized_cancel_label_does_not_become_a_cancel_button() {
        let english: gpui::PromptButton = "Cancel".into();
        assert!(english.is_cancel(), "the English word still works");

        for localized in ["取消", "キャンセル"] {
            let button: gpui::PromptButton = localized.into();
            assert!(
                !button.is_cancel(),
                "{localized:?} became a cancel answer on its own — gpui now \
                 reads roles some other way, so passing t(Cancel) is safe and \
                 this rule can go"
            );
        }

        // The helper says which is which, so the language never comes into it.
        let [action, keep] = super::confirm_answers("丢弃", "取消");
        assert!(!action.is_cancel());
        assert!(keep.is_cancel(), "the cancel is marked, not guessed");
    }
}

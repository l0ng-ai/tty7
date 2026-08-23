pub mod app;
pub mod assets;
pub mod code_editor;
pub mod diff_overlay;
pub mod diff_rows;
pub mod document_column;
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
pub mod sftp_host;
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

    /// Every name a dialog or a toast did not compose is folded onto one
    /// line before it goes in.
    ///
    /// `terminal::view::one_line` says the rule out loud — "anything that
    /// draws a name it did not compose is exposed to it" — and every *row*
    /// surface follows it. No dialog did. `sftp.rs` held both halves fifteen
    /// lines apart: the row folded `entry.name` with a comment about bytes
    /// chosen on a machine this window has no say over, and the delete
    /// confirmation for the same entry interpolated it raw.
    ///
    /// A dialog is the worse place to lose it. gpui breaks text on `\n`
    /// whatever the style says, and NSAlert renders one too, so a file named
    /// `notes.txt\n\nThis one is safe to delete.` writes its own second line
    /// into the question authorising the delete — on a remote listing, or in
    /// any repository you cloned.
    ///
    /// Keys, not expressions, because the keys are what name user data:
    /// `{name}`, `{path}`, `{branch}` and the rest are always someone else's
    /// bytes, while `{verb}` and `{n}` are ours.
    #[test]
    fn a_dialog_or_toast_folds_every_name_it_did_not_compose() {
        /// Substitution keys whose value is never text this app wrote.
        ///
        /// A dialog's `{error}` is checked too. A toast's is not: there the
        /// message *is* the content rather than a fragment inside a sentence
        /// of ours, and git and ssh write genuinely multi-line errors whose
        /// second line is the useful one. A dialog embeds it mid-question.
        const NAMES: [&str; 8] = [
            "name", "path", "branch", "machine", "host", "file", "subject", "author",
        ];
        let surfaces: [(&str, &[&str]); 2] = [
            (
                ".prompt(",
                &[
                    "name", "path", "branch", "machine", "host", "file", "subject", "author",
                    "error",
                ],
            ),
            ("push_notification(", &NAMES),
        ];

        fn walk(dir: &std::path::Path, surfaces: &[(&str, &[&str]); 2], found: &mut Vec<String>) {
            let mut entries: Vec<_> = std::fs::read_dir(dir)
                .expect("the ui sources are readable")
                .filter_map(Result::ok)
                .map(|e| e.path())
                .collect();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    walk(&path, surfaces, found);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let src = std::fs::read_to_string(&path).expect("a source file reads");
                for (call, keys) in *surfaces {
                    for (n, region) in call_regions(&src, call) {
                        for key in keys.iter().copied() {
                            for value in substitutions(&region, key) {
                                if !value.contains("one_line(") {
                                    found.push(format!(
                                        "{}:{} — {{{key}}} is {value}",
                                        path.display(),
                                        n
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        /// Each `call` site in `src`, as (1-based line, argument text).
        fn call_regions(src: &str, call: &str) -> Vec<(usize, String)> {
            let mut out = Vec::new();
            let bytes = src.as_bytes();
            let mut at = 0;
            while let Some(hit) = src[at..].find(call) {
                let open = at + hit + call.len();
                let mut depth = 1usize;
                let mut end = open;
                while end < bytes.len() && depth > 0 {
                    match bytes[end] {
                        b'(' => depth += 1,
                        b')' => depth -= 1,
                        _ => {}
                    }
                    end += 1;
                }
                out.push((src[..open].lines().count(), src[open..end].to_string()));
                at = open;
            }
            out
        }

        /// The value expression of every `("<key>", <value>)` pair in `region`.
        fn substitutions(region: &str, key: &str) -> Vec<String> {
            let needle = format!("(\"{key}\", ");
            let mut out = Vec::new();
            let mut at = 0;
            while let Some(hit) = region[at..].find(&needle) {
                let start = at + hit + needle.len();
                let mut depth = 1usize;
                let mut end = start;
                for (i, c) in region[start..].char_indices() {
                    match c {
                        '(' => depth += 1,
                        ')' if depth == 1 => {
                            end = start + i;
                            break;
                        }
                        ')' => depth -= 1,
                        _ => {}
                    }
                }
                out.push(region[start..end].trim().to_string());
                at = start;
            }
            out
        }

        let ui = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        assert!(ui.is_dir(), "the sources moved: {ui:?}");
        let mut found = Vec::new();
        walk(&ui, &surfaces, &mut found);
        assert!(
            found.is_empty(),
            "these dialogs interpolate a name nobody folded, so the name gets \
             to write its own lines of the question; wrap it in \
             `terminal::view::one_line`:\n{}",
            found.join("\n")
        );
    }
}

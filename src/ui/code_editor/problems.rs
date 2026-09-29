//! The editor's Problems list: every error, warning and note the language
//! servers have published for the open files, grouped by file.
//!
//! It lives at the foot of the code panel rather than as a right-panel tab:
//! problems belong to the files open in the editor, the status bar's error and
//! warning counts are where people look for them, and a click lands in the
//! editor right above it. The status bar counts toggle it, as does the
//! `ToggleEditorProblems` command.
//!
//! Hints are left out, as VS Code leaves them out: servers use them for
//! inlay-ish suggestions that would bury the real problems.
use std::path::{Path, PathBuf};

use gpui::prelude::*;
use gpui::{AnyElement, Context, ScrollHandle, Window, div, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use lsp_types::DiagnosticSeverity;

use crate::ui::app::Tty7App;
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::lsp::LspStore;

/// The pane's height: a handful of rows, leaving the editor the rest.
const PANE_HEIGHT: f32 = 180.;

/// Past this many rows the list stops drawing and says how many are left.
const MAX_ROWS: usize = 1000;

#[derive(Default)]
pub(crate) struct ProblemsPane {
    pub(crate) open: bool,
    scroll: ScrollHandle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Problem {
    /// Zero-based, in editor columns.
    pub(crate) line: u32,
    pub(crate) column: u32,
    pub(crate) severity: Severity,
    pub(crate) message: String,
    pub(crate) source: Option<String>,
}

/// One file's problems, in the order they appear in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileProblems {
    pub(crate) path: PathBuf,
    pub(crate) problems: Vec<Problem>,
}

/// Sort the store's snapshot into what the list draws: files by path, each
/// file's problems by position, hints dropped, files left with nothing gone.
pub(crate) fn collect(snapshot: Vec<(PathBuf, Vec<lsp_types::Diagnostic>)>) -> Vec<FileProblems> {
    let mut files: Vec<FileProblems> = snapshot
        .into_iter()
        .map(|(path, diagnostics)| {
            let mut problems: Vec<Problem> = diagnostics
                .into_iter()
                .filter_map(|d| {
                    let severity = match d.severity {
                        // A server that says nothing means an error (LSP leaves
                        // it to the client; every editor reads it that way).
                        None | Some(DiagnosticSeverity::ERROR) => Severity::Error,
                        Some(DiagnosticSeverity::WARNING) => Severity::Warning,
                        Some(DiagnosticSeverity::INFORMATION) => Severity::Info,
                        _ => return None,
                    };
                    Some(Problem {
                        line: d.range.start.line,
                        column: d.range.start.character,
                        severity,
                        message: d.message,
                        source: d.source,
                    })
                })
                .collect();
            problems.sort_by(|a, b| {
                (a.line, a.column, a.severity).cmp(&(b.line, b.column, b.severity))
            });
            FileProblems { path, problems }
        })
        .filter(|f| !f.problems.is_empty())
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

/// The first line of a message: the list is one row per problem, and the
/// rest is a hover away in the editor.
fn headline(message: &str) -> &str {
    message.lines().next().unwrap_or("").trim_end()
}

impl Tty7App {
    pub(crate) fn toggle_editor_problems(&mut self, cx: &mut Context<Self>) {
        self.editor.problems.open = !self.editor.problems.open;
        cx.notify();
    }

    /// Where a file's name sits in the list: relative to the project the
    /// tab shows, when it is inside it.
    fn problems_label(&self, path: &Path) -> (String, String) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let dir = path.parent().map(|parent| {
            self.tab_code()
                .and_then(|c| {
                    c.roots
                        .iter()
                        .find_map(|r| parent.strip_prefix(r).ok().map(|p| (r, p)))
                })
                .map(|(root, rel)| {
                    let repo = root
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    match rel.as_os_str().is_empty() {
                        true => repo,
                        false => format!("{repo}/{}", rel.display()),
                    }
                })
                .unwrap_or_else(|| parent.display().to_string())
        });
        (name, dir.unwrap_or_default())
    }

    pub(crate) fn render_editor_problems(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.editor.problems.open {
            return None;
        }
        let files = collect(LspStore::diagnostics_snapshot(cx));
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (errors, warnings) =
            files
                .iter()
                .flat_map(|f| &f.problems)
                .fold((0, 0), |(e, w), p| match p.severity {
                    Severity::Error => (e + 1, w),
                    Severity::Warning => (e, w + 1),
                    Severity::Info => (e, w),
                });
        let header = h_flex()
            .flex_none()
            .h(px(28.))
            .px(px(crate::ui::app::CONTENT_INSET))
            .gap_2()
            .items_center()
            .border_t(crate::ui::theme::hairline(window))
            .border_color(theme.sidebar_border)
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(t(L10nKey::EditorProblemsTitle)),
            )
            .child(div().text_xs().text_color(muted).child(t_fmt(
                L10nKey::LspProblemsTooltip,
                &[
                    ("errors", &errors.to_string()),
                    ("warnings", &warnings.to_string()),
                ],
            )))
            .child(div().flex_1())
            .child(
                Button::new("editor-problems-close")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_editor_problems(cx))),
            );

        let mut rows: Vec<AnyElement> = Vec::new();
        let mut left = 0usize;
        for (file_ix, file) in files.iter().enumerate() {
            if rows.len() >= MAX_ROWS {
                left += file.problems.len();
                continue;
            }
            let (name, dir) = self.problems_label(&file.path);
            rows.push(
                h_flex()
                    .id(("editor-problems-file", file_ix))
                    .h(px(24.))
                    .px(px(crate::ui::app::CONTENT_INSET))
                    .gap_2()
                    .items_center()
                    .text_sm()
                    .child(
                        div()
                            .flex_none()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(name),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_ellipsis()
                            .text_xs()
                            .text_color(muted)
                            .child(dir),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(muted)
                            .child(file.problems.len().to_string()),
                    )
                    .into_any_element(),
            );
            for (ix, problem) in file.problems.iter().enumerate() {
                if rows.len() >= MAX_ROWS {
                    left += file.problems.len() - ix;
                    break;
                }
                let (icon, color) = match problem.severity {
                    Severity::Error => (IconName::CircleX, theme.danger),
                    Severity::Warning => (IconName::TriangleAlert, theme.warning),
                    Severity::Info => (IconName::Info, theme.info),
                };
                let at = t_fmt(
                    L10nKey::EditorLnCol,
                    &[
                        ("line", &(problem.line + 1).to_string()),
                        ("column", &(problem.column + 1).to_string()),
                    ],
                );
                let path = file.path.clone();
                let (line, column) = (problem.line + 1, problem.column + 1);
                rows.push(
                    h_flex()
                        .id(("editor-problem", rows.len()))
                        .h(px(22.))
                        .pl(px(crate::ui::app::CONTENT_INSET + 12.))
                        .pr(px(crate::ui::app::CONTENT_INSET))
                        .gap_2()
                        .items_center()
                        .text_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.list_hover))
                        .child(Icon::new(icon).xsmall().text_color(color))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(headline(&problem.message).to_string()),
                        )
                        .children(problem.source.clone().map(|source| {
                            div().flex_none().text_xs().text_color(muted).child(source)
                        }))
                        .child(div().flex_none().text_xs().text_color(muted).child(at))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_file_in_editor_at(
                                &path,
                                Some(line),
                                Some(column),
                                window,
                                cx,
                            );
                        }))
                        .into_any_element(),
                );
            }
        }
        if left > 0 {
            rows.push(
                div()
                    .px(px(crate::ui::app::CONTENT_INSET))
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .child(t_fmt(
                        L10nKey::EditorProblemsMore,
                        &[("n", &left.to_string())],
                    ))
                    .into_any_element(),
            );
        }
        let body: AnyElement = if files.is_empty() {
            div()
                .px(px(crate::ui::app::CONTENT_INSET))
                .py_2()
                .text_sm()
                .text_color(muted)
                .child(t(L10nKey::EditorProblemsNone))
                .into_any_element()
        } else {
            let scroll = self.editor.problems.scroll.clone();
            crate::ui::scrollbar::with_vertical_scrollbar(
                "editor-problems-scrollbar",
                v_flex()
                    .id("editor-problems-list")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .children(rows),
                &scroll,
            )
        };
        Some(
            v_flex()
                .id("editor-problems")
                .flex_none()
                .h(px(PANE_HEIGHT))
                .w_full()
                .child(header)
                .child(div().flex_1().min_h_0().child(body))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Diagnostic, Position, Range};

    fn d(line: u32, character: u32, severity: Option<DiagnosticSeverity>, msg: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(
                Position::new(line, character),
                Position::new(line, character + 1),
            ),
            severity,
            message: msg.into(),
            ..Default::default()
        }
    }

    #[test]
    fn problems_are_sorted_by_file_then_position_without_hints() {
        let files = collect(vec![
            (
                PathBuf::from("/p/src/b.rs"),
                vec![
                    d(9, 0, Some(DiagnosticSeverity::WARNING), "w"),
                    d(2, 4, Some(DiagnosticSeverity::ERROR), "e"),
                    d(2, 1, None, "unspecified is an error"),
                    d(5, 0, Some(DiagnosticSeverity::HINT), "hint"),
                ],
            ),
            (
                PathBuf::from("/p/src/a.rs"),
                vec![d(0, 0, Some(DiagnosticSeverity::INFORMATION), "i")],
            ),
            (
                PathBuf::from("/p/src/c.rs"),
                vec![d(0, 0, Some(DiagnosticSeverity::HINT), "only a hint")],
            ),
        ]);
        assert_eq!(files.len(), 2, "a file with only hints has no problems");
        assert_eq!(files[0].path, PathBuf::from("/p/src/a.rs"));
        assert_eq!(files[0].problems[0].severity, Severity::Info);
        let b: Vec<_> = files[1]
            .problems
            .iter()
            .map(|p| (p.line, p.column, p.severity))
            .collect();
        assert_eq!(
            b,
            vec![
                (2, 1, Severity::Error),
                (2, 4, Severity::Error),
                (9, 0, Severity::Warning)
            ]
        );
    }

    #[test]
    fn a_row_shows_the_first_line_of_the_message() {
        assert_eq!(
            headline("mismatched types\nexpected `u8`"),
            "mismatched types"
        );
        assert_eq!(headline(""), "");
    }

    /// The pane opens from its command and draws with nothing to show.
    #[gpui::test]
    fn the_pane_toggles_and_draws_empty(cx: &mut gpui::TestAppContext) {
        use crate::ui::app::test_window;

        let (app, mut vcx, _pane) = test_window::harness_with_tabs(cx, 1);
        app.update_in(&mut vcx, |app, window, cx| {
            app.editor_new_file(window, cx);
            app.toggle_editor_problems(cx);
        });
        vcx.run_until_parked();
        vcx.update(|window, cx| window.draw(cx));
        app.update_in(&mut vcx, |app, window, cx| {
            assert!(app.editor.problems.open);
            assert!(app.render_editor_problems(window, cx).is_some());
            app.toggle_editor_problems(cx);
            assert!(app.render_editor_problems(window, cx).is_none());
        });
    }
}

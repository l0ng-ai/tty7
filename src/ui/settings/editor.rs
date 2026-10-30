//! The Editor group on the General page: the code editor's switches that
//! used to live only in config.json or in a status-bar toggle.

use super::*;

impl Tty7App {
    pub(super) fn render_editor_group(&self, cx: &mut Context<Self>) -> AnyElement {
        let cfg = cx.global::<Config>();
        let (helix, git_gutter, lsp, soft_wrap, markdown) = (
            cfg.helix_mode,
            cfg.editor_git_gutter,
            cfg.editor_lsp,
            cfg.editor_soft_wrap,
            cfg.editor_markdown_preview,
        );
        let helix = self.settings_switch("wt-editor-helix-mode", helix, cx, |this, on, _, cx| {
            this.set_helix_mode(on, cx)
        });
        let git_gutter =
            self.settings_switch("wt-editor-git-gutter", git_gutter, cx, |this, on, _, cx| {
                this.set_editor_git_gutter(on, cx)
            });
        let lsp = self.settings_switch("wt-editor-lsp", lsp, cx, |this, on, _, cx| {
            this.set_editor_lsp(on, cx)
        });
        let soft_wrap =
            self.settings_switch("wt-editor-soft-wrap", soft_wrap, cx, |this, on, _, cx| {
                this.set_editor_soft_wrap(on, cx)
            });
        let markdown =
            self.settings_switch("wt-editor-markdown", markdown, cx, |this, on, _, cx| {
                this.set_editor_markdown_preview(on, cx)
            });
        self.settings_group(
            Some(t(L10nKey::SettingsEditor)),
            None,
            [
                self.settings_row(
                    t(L10nKey::SettingsEditorHelixMode),
                    t(L10nKey::SettingsEditorHelixModeDesc),
                    helix,
                    cx,
                ),
                self.settings_row(
                    t(L10nKey::SettingsEditorGitGutter),
                    t(L10nKey::SettingsEditorGitGutterDesc),
                    git_gutter,
                    cx,
                ),
                self.settings_row(
                    t(L10nKey::SettingsEditorLsp),
                    t(L10nKey::SettingsEditorLspDesc),
                    lsp,
                    cx,
                ),
                self.settings_row(
                    t(L10nKey::SettingsEditorSoftWrap),
                    t(L10nKey::SettingsEditorSoftWrapDesc),
                    soft_wrap,
                    cx,
                ),
                self.settings_row(
                    t(L10nKey::SettingsEditorMarkdownPreview),
                    t(L10nKey::SettingsEditorMarkdownPreviewDesc),
                    markdown,
                    cx,
                ),
            ]
            .map(IntoElement::into_any_element),
            cx,
        )
    }

    pub(crate) fn toggle_helix_mode(&mut self, cx: &mut Context<Self>) {
        let on = !cx.global::<Config>().helix_mode;
        self.set_helix_mode(on, cx);
    }

    pub(crate) fn set_helix_mode(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.helix_mode = on);
        for tab in &self.tabs {
            for leaf in tab.pane.terminals() {
                leaf.update(cx, |v, cx| v.set_helix_mode(on, cx));
            }
        }
        self.sync_editor_helix_mode(on);
        cx.notify();
    }

    /// `LspStore` watches the config: off closes every document and lets
    /// its server go; on asks every window for the files it has open, which
    /// starts their servers again.
    pub(crate) fn set_editor_lsp(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.editor_lsp = on);
        cx.notify();
    }

    /// What new files open with; files already open keep their own Wrap.
    pub(crate) fn set_editor_soft_wrap(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.editor_soft_wrap = on);
    }

    /// What Markdown files open as; files already open keep their own mode.
    pub(crate) fn set_editor_markdown_preview(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.editor_markdown_preview = on);
    }
}

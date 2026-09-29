//! Where the code editor meets the language servers: the few calls
//! `code_editor.rs` makes when its buffers change, the commands (go to
//! definition, format, rename), and the status bar's word on the server.

use std::path::PathBuf;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{AnyElement, Context, Entity, WeakEntity, Window, div, px};
use gpui_component::input::InputState;
use gpui_component::menu::PopupMenu;
use gpui_component::{
    ActiveTheme as _, Icon, IconName, RopeExt as _, Sizable as _, WindowExt as _, h_flex,
};
use lsp_types::Position;

use super::convert::{self, Encoding};
use super::{Freshen, LspStatus, LspStore};
use crate::ui::app::Tty7App;
use crate::ui::code_editor::{BufferId, LocalFile};
use crate::ui::i18n::{L10nKey, t, t_fmt};

/// One buffer offered to the language servers.
pub(crate) struct LspBuffer {
    pub(crate) input: Entity<InputState>,
    pub(crate) path: PathBuf,
    pub(crate) language: &'static str,
    pub(crate) app: WeakEntity<Tty7App>,
}

impl Tty7App {
    fn lsp_enabled(cx: &gpui::App) -> bool {
        cx.global::<crate::core::config::Config>().editor_lsp
    }

    /// The set of open buffers changed — one opened, closed, or was saved
    /// under a new name. Servers start and documents open or close to match.
    pub(crate) fn lsp_sync_buffers(&self, cx: &mut Context<Self>) {
        let app = cx.entity().downgrade();
        let buffers = if Self::lsp_enabled(cx) {
            self.editor_local_files()
                .into_iter()
                .map(|f| LspBuffer {
                    input: f.input,
                    path: f.path,
                    language: f.language,
                    app: app.clone(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let app = cx.entity().downgrade();
        LspStore::register_window(
            cx.entity_id(),
            Rc::new(move |cx: &mut gpui::App| {
                app.update(cx, |app, cx| app.lsp_sync_buffers(cx)).is_ok()
            }),
            cx,
        );
        LspStore::sync_window(cx.entity_id(), buffers, cx);
    }

    /// The window is closing: every document it owns is closed now, so an
    /// idle server can wind down instead of outliving the window (with a
    /// tray icon the app itself keeps running).
    pub(crate) fn lsp_window_closed(&self, cx: &mut Context<Self>) {
        LspStore::sync_window(cx.entity_id(), Vec::new(), cx);
    }

    /// A buffer's text changed.
    pub(crate) fn lsp_buffer_edited(&self, id: BufferId, cx: &mut Context<Self>) {
        if let Some(f) = self.editor_local_file(id) {
            LspStore::edited(f.input.entity_id(), &f.path, cx);
            self.lsp_signature_help_after_edit(&f, cx);
        }
    }

    /// A buffer was written to disk.
    pub(crate) fn lsp_buffer_saved(&self, id: BufferId, cx: &mut Context<Self>) {
        if let Some(f) = self.editor_local_file(id) {
            LspStore::saved(f.input.entity_id(), &f.path, cx);
        }
    }

    /// The front file, if a server has it and the editor has the focus.
    fn lsp_active(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(LocalFile, super::DocContext)> {
        let f = self.editor_active_local_file()?;
        let text = f.input.read(cx).text().clone();
        let doc = LspStore::context(
            &f.path,
            f.input.entity_id(),
            Freshen::Text(&text),
            Some(window.window_handle()),
            cx,
        )?;
        Some((f, doc))
    }

    /// Opens `path` with the cursor on a server's `position` — through the
    /// same open-at-line path a `file.rs:12:5` link takes, so navigation
    /// history sees a definition jump like any other.
    pub(crate) fn lsp_open_location(
        &mut self,
        path: PathBuf,
        position: Position,
        encoding: Encoding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The open path counts columns in chars, from one.
        let target = path.clone();
        let open = move |app: &mut Tty7App,
                         character: u32,
                         window: &mut Window,
                         cx: &mut Context<Tty7App>| {
            app.open_file_in_editor_at(
                &target,
                Some(position.line + 1),
                Some(character + 1),
                window,
                cx,
            );
        };
        let open_buffer = self
            .editor_local_files()
            .into_iter()
            .find(|f| f.path == path.as_path());
        if let Some(f) = open_buffer {
            let text = f.input.read(cx).text().clone();
            let character = convert::lsp_to_editor(&text, position, encoding).character;
            open(self, character, window, cx);
            return;
        }
        // Not open: the column is converted against the file on disk.
        let read_from = path.clone();
        let line = position.line as usize;
        cx.spawn_in(window, async move |app, cx| {
            let character = cx
                .background_spawn(async move {
                    let text = std::fs::read_to_string(&read_from).ok()?;
                    let line = text.lines().nth(line)?;
                    let byte = convert::byte_of_column(line, position.character, encoding);
                    Some(line[..byte].chars().count() as u32)
                })
                .await
                .unwrap_or(position.character);
            let _ = app.update_in(cx, |app, window, cx| open(app, character, window, cx));
        })
        .detach();
    }

    /// F12: the definition of the symbol under the cursor.
    pub(crate) fn lsp_go_to_definition(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((f, doc)) = self.lsp_active(window, cx) else {
            return;
        };
        let state = f.input.read(cx);
        let request = super::providers::definition_request(&doc, state.text(), state.cursor());
        let (encoding, root) = (doc.encoding, doc.root.clone());
        cx.spawn_in(window, async move |app, cx| {
            let links = match request.await {
                Ok(links) => links,
                Err(e) => {
                    log::debug!("lsp: definition: {e:#}");
                    return;
                }
            };
            let places: Vec<_> = links
                .into_iter()
                .filter_map(|l| {
                    Some((
                        super::uri_to_path(&l.target_uri)?,
                        l.target_selection_range.start,
                    ))
                })
                .collect();
            let _ = app.update_in(cx, |app, window, cx| match places.as_slice() {
                [] => {}
                // One answer is followed straight away, through the same path
                // a secondary-click takes.
                [(path, position)] => {
                    app.lsp_open_location(path.clone(), *position, encoding, window, cx)
                }
                _ => app.lsp_show_places(
                    L10nKey::SearchHeadingDefinitions,
                    places,
                    encoding,
                    root,
                    window,
                    cx,
                ),
            });
        })
        .detach();
    }

    /// ⇧⌥F: the server's formatting of the whole file, applied as edits so
    /// it undoes like one.
    pub(crate) fn lsp_format_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((f, doc)) = self.lsp_active(window, cx) else {
            return;
        };
        if doc.caps.document_formatting_provider.is_none() {
            return;
        }
        let before = f.input.read(cx).text().clone();
        let request = doc.client.request::<lsp_types::request::Formatting>(
            lsp_types::DocumentFormattingParams {
                text_document: lsp_types::TextDocumentIdentifier::new(doc.uri.clone()),
                options: lsp_types::FormattingOptions {
                    tab_size: f.indent.size as u32,
                    insert_spaces: !f.indent.hard_tabs,
                    trim_trailing_whitespace: Some(true),
                    insert_final_newline: Some(true),
                    ..Default::default()
                },
                work_done_progress_params: Default::default(),
            },
        );
        let input = f.input.clone();
        let encoding = doc.encoding;
        cx.spawn_in(window, async move |_, cx| {
            let edits = match request.await {
                Ok(Some(edits)) if !edits.is_empty() => edits,
                Ok(_) => return,
                Err(e) => {
                    log::info!("lsp: formatting: {e:#}");
                    return;
                }
            };
            let _ = cx.update(|window, cx| {
                input.update(cx, |state, cx| {
                    // Typed over while the server worked: its edits describe
                    // a text that is gone.
                    if *state.text() != before {
                        return;
                    }
                    let edits = convert::edits_to_editor(&before, &edits, encoding);
                    state.apply_lsp_edits(&edits, window, cx);
                });
            });
        })
        .detach();
    }

    /// F2: asks for a new name for the symbol under the cursor.
    pub(crate) fn lsp_rename_start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((f, doc)) = self.lsp_active(window, cx) else {
            return;
        };
        if doc.caps.rename_provider.is_none() {
            return;
        }
        let state = f.input.read(cx);
        let offset = state.cursor();
        let Some(word) = state
            .text()
            .word_range(offset)
            .map(|r| state.text().slice(r).to_string())
        else {
            return;
        };
        if word.trim().is_empty() {
            return;
        }
        self.editor_open_rename_bar(f.id, offset, word, window, cx);
    }

    /// The rename prompt was answered.
    pub(crate) fn lsp_rename(
        &mut self,
        id: BufferId,
        offset: usize,
        new_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(f) = self.editor_local_file(id) else {
            return;
        };
        let text = f.input.read(cx).text().clone();
        let Some(doc) = LspStore::context(
            &f.path,
            f.input.entity_id(),
            Freshen::Text(&text),
            Some(window.window_handle()),
            cx,
        ) else {
            return;
        };
        // Every open buffer as the server sees it now; the answer is applied
        // only if none of those it touches has changed by then.
        let baseline = LspStore::snapshot(cx);
        let old_name = text
            .word_range(offset)
            .map(|r| text.slice(r).to_string())
            .unwrap_or_default();
        let request = doc
            .client
            .request::<lsp_types::request::Rename>(lsp_types::RenameParams {
                text_document_position: lsp_types::TextDocumentPositionParams::new(
                    lsp_types::TextDocumentIdentifier::new(doc.uri.clone()),
                    convert::offset_to_lsp(&text, offset, doc.encoding),
                ),
                new_name,
                work_done_progress_params: Default::default(),
            });
        let encoding = doc.encoding;
        cx.spawn_in(window, async move |_, cx| {
            let result = request.await;
            let _ = cx.update(|window, cx| match result {
                Ok(Some(edit)) => {
                    super::apply_workspace_edit(edit, encoding, Some(baseline), cx);
                }
                Ok(None) => {}
                Err(e) => {
                    let context = t_fmt(L10nKey::LspRenameFailed, &[("name", &old_name)]);
                    window.push_notification(
                        crate::ui::host_ops::failure(context, &format!("{e:#}")),
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    /// ⌘.: the server's fixes and refactors at the cursor, in the editor's
    /// own code-action menu.
    pub(crate) fn lsp_code_actions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor_active_local_file().is_none() {
            return;
        }
        window.dispatch_action(Box::new(gpui_component::input::ToggleCodeActions), cx);
    }

    /// The language-server commands for the editor's right-click menu, once
    /// the buffer's server is up.
    pub(crate) fn lsp_menu_items(
        &self,
        menu: PopupMenu,
        id: BufferId,
        cx: &gpui::App,
    ) -> PopupMenu {
        use crate::core::actions::{
            EditorFindReferences, EditorFormatDocument, EditorGoToDefinition, EditorQuickFix,
            EditorRenameSymbol,
        };
        let Some(f) = self.editor_local_file(id) else {
            return menu;
        };
        if !matches!(
            LspStore::status(&f.path, f.language, cx),
            Some(LspStatus::Running { .. })
        ) {
            return menu;
        }
        menu.separator()
            .menu(
                t(L10nKey::LspGoToDefinition),
                Box::new(EditorGoToDefinition),
            )
            .menu(
                t(L10nKey::LspFindReferences),
                Box::new(EditorFindReferences),
            )
            .menu(t(L10nKey::LspQuickFix), Box::new(EditorQuickFix))
            .menu(
                t(L10nKey::LspRenameSymbolAction),
                Box::new(EditorRenameSymbol),
            )
            .menu(
                t(L10nKey::LspFormatDocument),
                Box::new(EditorFormatDocument),
            )
    }

    /// The status bar's word on the front file's server: its problem counts
    /// once it runs, or why there are none.
    /// The language server's state for the status bar, and whether it is the
    /// missing-server note, which leads the bar rather than joining the
    /// readouts at its end.
    pub(crate) fn render_lsp_status(&self, cx: &mut Context<Self>) -> Option<(AnyElement, bool)> {
        if !Self::lsp_enabled(cx) {
            return None;
        }
        let f = self.editor_active_local_file()?;
        let status = LspStore::status(&f.path, f.language, cx)?;
        let muted = cx.theme().muted_foreground;
        let element = match status {
            LspStatus::Missing(name) => {
                return Some((
                    h_flex()
                        .min_w_0()
                        .gap(px(6.))
                        .items_center()
                        .child(
                            div()
                                .flex_none()
                                .size(px(5.))
                                .rounded_full()
                                .bg(cx.theme().warning),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(t_fmt(L10nKey::LspServerMissing, &[("name", name)])),
                        )
                        .into_any_element(),
                    true,
                ));
            }
            LspStatus::Starting(name) => div()
                .flex_none()
                .child(t_fmt(L10nKey::LspServerStarting, &[("name", &name)]))
                .into_any_element(),
            LspStatus::Down(name) => div()
                .flex_none()
                .child(t_fmt(L10nKey::LspServerDown, &[("name", &name)]))
                .into_any_element(),
            LspStatus::Running {
                name,
                errors,
                warnings,
            } => {
                let tip = format!(
                    "{name} — {}",
                    t_fmt(
                        L10nKey::LspProblemsTooltip,
                        &[
                            ("errors", &errors.to_string()),
                            ("warnings", &warnings.to_string())
                        ],
                    )
                );
                let count = |icon: IconName, n: usize, color: gpui::Hsla| {
                    h_flex()
                        .gap(px(3.))
                        .items_center()
                        .child(Icon::new(icon).xsmall().text_color(if n > 0 {
                            color
                        } else {
                            muted
                        }))
                        .child(n.to_string())
                };
                h_flex()
                    .id("lsp-status")
                    .flex_none()
                    .gap_2()
                    .items_center()
                    .font_features(crate::ui::theme::tabular_figures())
                    .child(count(IconName::CircleX, errors, cx.theme().danger))
                    .child(count(IconName::TriangleAlert, warnings, cx.theme().warning))
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                    })
                    .into_any_element()
            }
        };
        Some((element, false))
    }
}

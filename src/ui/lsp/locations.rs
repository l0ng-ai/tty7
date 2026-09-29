//! Places a language server points at — every reference to a symbol, or a
//! definition with more than one answer — listed in the search's Locations
//! tab: arrowing through it previews a place in the file in front, Return
//! goes there.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::{App, AppContext as _, Context, Window};
use gpui_component::{Rope, RopeExt as _};
use lsp_types::Position;

use super::convert::{self, Encoding};
use super::{Freshen, LspStore};
use crate::ui::app::Tty7App;
use crate::ui::i18n::{L10nKey, t};
use crate::ui::search::{CommandKind, Item, SearchTab};

/// One place, in the editor's terms: 0-based, the column counted in chars,
/// with the line it is on for the row to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) path: PathBuf,
    pub(crate) line: u32,
    pub(crate) column: u32,
    pub(crate) text: String,
}

/// The longest line text a row shows.
const LINE_PREVIEW: usize = 160;

/// Converts server positions to [`Found`]s. Files open in the editor are
/// measured against their buffer (`open`), the rest against the disk — so
/// this runs off the main thread. Sorted by file, then position, duplicates
/// dropped.
pub(crate) fn resolve(
    places: Vec<(PathBuf, Position)>,
    open: HashMap<PathBuf, Rope>,
    enc: Encoding,
) -> Vec<Found> {
    let mut found: Vec<Found> = measure(places, &open, enc).into_iter().flatten().collect();
    found.sort_by(|a, b| (&a.path, a.line, a.column).cmp(&(&b.path, b.line, b.column)));
    found.dedup();
    found
}

/// [`resolve`] without the sorting: one answer per place, in order, `None`
/// for a place past the end of its file.
pub(crate) fn measure(
    places: Vec<(PathBuf, Position)>,
    open: &HashMap<PathBuf, Rope>,
    enc: Encoding,
) -> Vec<Option<Found>> {
    let mut read: HashMap<PathBuf, Option<Vec<String>>> = HashMap::new();
    places
        .into_iter()
        .map(|(path, pos)| {
            let line = match open.get(&path) {
                Some(rope) if (pos.line as usize) < rope.lines_len() => {
                    rope.slice_line(pos.line as usize).to_string()
                }
                Some(_) => return None,
                None => read
                    .entry(path.clone())
                    .or_insert_with(|| {
                        std::fs::read_to_string(&path)
                            .ok()
                            .map(|t| t.lines().map(str::to_owned).collect())
                    })
                    .as_ref()
                    .and_then(|lines| lines.get(pos.line as usize).cloned())
                    .unwrap_or_default(),
            };
            let byte = convert::byte_of_column(&line, pos.character, enc);
            let column = line[..byte].chars().count() as u32;
            let text: String = line.trim().chars().take(LINE_PREVIEW).collect();
            Some(Found {
                path,
                line: pos.line,
                column,
                text,
            })
        })
        .collect()
}

/// A search row for `found`, its path shown relative to `root`.
pub(crate) fn item(found: &Found, root: &Path) -> Item {
    let shown = found.path.strip_prefix(root).unwrap_or(&found.path);
    Item::new(
        format!("{}:{}", shown.display(), found.line + 1),
        CommandKind::GoToLocation {
            path: found.path.clone(),
            line: found.line,
            column: found.column,
        },
    )
    .with_subtitle(found.text.clone())
    .with_alias(found.path.display().to_string())
}

impl Tty7App {
    /// The open local buffers' texts, for measuring places in them.
    fn lsp_open_texts(&self, cx: &App) -> HashMap<PathBuf, Rope> {
        self.editor_local_files()
            .into_iter()
            .map(|f| (f.path, f.input.read(cx).text().clone()))
            .collect()
    }

    /// Shows what a server found: one place is gone to at once, several
    /// are listed to pick from.
    pub(crate) fn lsp_show_places(
        &mut self,
        heading: L10nKey,
        places: Vec<(PathBuf, Position)>,
        encoding: Encoding,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if places.is_empty() {
            return;
        }
        let open = self.lsp_open_texts(cx);
        cx.spawn_in(window, async move |app, cx| {
            let found = cx
                .background_spawn(async move { resolve(places, open, encoding) })
                .await;
            let _ = app.update_in(cx, |app, window, cx| match found.as_slice() {
                [] => {}
                [one] => app.open_file_in_editor_at(
                    &one.path,
                    Some(one.line + 1),
                    Some(one.column + 1),
                    window,
                    cx,
                ),
                many => {
                    let items = many.iter().map(|f| item(f, &root)).collect();
                    app.open_search(SearchTab::Locations, "", window, cx);
                    app.editor_begin_preview(cx);
                    if let Some(view) = app.search.clone() {
                        view.update(cx, |view, cx| {
                            view.set_locations(Some(t(heading)), items, window, cx)
                        });
                    }
                }
            });
        })
        .detach();
    }

    /// ⇧F12: every reference to the symbol under the cursor.
    pub(crate) fn lsp_find_references(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(f) = self.editor_active_local_file() else {
            return;
        };
        let (text, offset) = {
            let state = f.input.read(cx);
            (state.text().clone(), state.cursor())
        };
        let Some(doc) = LspStore::context(
            &f.path,
            f.input.entity_id(),
            Freshen::Text(&text),
            Some(window.window_handle()),
            cx,
        ) else {
            return;
        };
        if doc.caps.references_provider.is_none() {
            return;
        }
        let request =
            doc.client
                .request::<lsp_types::request::References>(lsp_types::ReferenceParams {
                    text_document_position: lsp_types::TextDocumentPositionParams::new(
                        lsp_types::TextDocumentIdentifier::new(doc.uri.clone()),
                        convert::offset_to_lsp(&text, offset, doc.encoding),
                    ),
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                    context: lsp_types::ReferenceContext {
                        include_declaration: true,
                    },
                });
        let (encoding, root) = (doc.encoding, doc.root.clone());
        cx.spawn_in(window, async move |app, cx| {
            let locations = match request.await {
                Ok(locations) => locations.unwrap_or_default(),
                Err(e) => {
                    log::debug!("lsp: references: {e:#}");
                    return;
                }
            };
            let places = locations
                .into_iter()
                .filter_map(|l| Some((super::uri_to_path(&l.uri)?, l.range.start)))
                .collect();
            let _ = app.update_in(cx, |app, window, cx| {
                app.lsp_show_places(
                    L10nKey::SearchHeadingReferences,
                    places,
                    encoding,
                    root,
                    window,
                    cx,
                )
            });
        })
        .detach();
    }

    /// What the Symbols tab asks as its query changes: the front file's
    /// language server, for symbols anywhere in the project. `None` when
    /// there is no such server, and the tab lists the file's own alone.
    pub(crate) fn lsp_project_symbol_query(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<crate::ui::search::LiveQuery> {
        let f = self.editor_active_local_file()?;
        let doc = LspStore::context(&f.path, f.input.entity_id(), Freshen::Skip, None, cx)?;
        doc.caps.workspace_symbol_provider.as_ref()?;
        let app = cx.entity().downgrade();
        let handle = window.window_handle();
        let path = f.path.clone();
        let requester = f.input.entity_id();
        Some(std::rc::Rc::new(move |query: &str, cx: &mut App| {
            let (app, path, query) = (app.clone(), path.clone(), query.to_owned());
            // Deferred: the query arrives from inside the search's own
            // update, and answering it updates the search.
            cx.defer(move |cx| {
                let _ = handle.update(cx, |_, window, cx| {
                    let _ = app.update(cx, |app, cx| {
                        app.lsp_workspace_query(&path, requester, query, window, cx)
                    });
                });
            });
        }))
    }

    fn lsp_workspace_query(
        &mut self,
        path: &Path,
        requester: gpui::EntityId,
        query: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use std::sync::atomic::{AtomicU64, Ordering};
        // The latest query wins: an answer to an older one is dropped.
        static LATEST: AtomicU64 = AtomicU64::new(0);
        let seq = LATEST.fetch_add(1, Ordering::Relaxed) + 1;
        if query.trim().is_empty() {
            if let Some(view) = self.search.clone() {
                view.update(cx, |view, cx| {
                    view.set_project_symbols(Vec::new(), window, cx)
                });
            }
            return;
        }
        let Some(doc) = LspStore::context(path, requester, Freshen::Skip, None, cx) else {
            return;
        };
        let client = doc.client.clone();
        let (encoding, root) = (doc.encoding, doc.root.clone());
        let open = self.lsp_open_texts(cx);
        let front = path.to_path_buf();
        cx.spawn_in(window, async move |app, cx| {
            // Typing fast asks once, for what was typed last.
            cx.background_executor()
                .timer(std::time::Duration::from_millis(150))
                .await;
            if LATEST.load(Ordering::Relaxed) != seq {
                return;
            }
            let answer = client
                .request::<lsp_types::request::WorkspaceSymbolRequest>(
                    lsp_types::WorkspaceSymbolParams {
                        query,
                        work_done_progress_params: Default::default(),
                        partial_result_params: Default::default(),
                    },
                )
                .await;
            // The front file's own symbols are listed above these already.
            let symbols = match answer {
                Ok(Some(symbols)) => workspace_symbols(symbols)
                    .into_iter()
                    .filter(|s| s.path != front)
                    .collect::<Vec<_>>(),
                Ok(None) => Vec::new(),
                Err(e) => {
                    log::debug!("lsp: workspace/symbol: {e:#}");
                    return;
                }
            };
            let items = cx
                .background_spawn(async move {
                    let places = symbols
                        .iter()
                        .map(|s| (s.path.clone(), s.position))
                        .collect();
                    measure(places, &open, encoding)
                        .into_iter()
                        .zip(symbols)
                        .filter_map(|(found, symbol)| Some(symbol_item(&found?, &symbol, &root)))
                        .collect::<Vec<_>>()
                })
                .await;
            if LATEST.load(Ordering::Relaxed) != seq {
                return;
            }
            let _ = app.update_in(cx, |app, window, cx| {
                if let Some(view) = app.search.clone()
                    && view.read(cx).tab() == SearchTab::Symbols
                {
                    view.update(cx, |view, cx| view.set_project_symbols(items, window, cx));
                }
            });
        })
        .detach();
    }

    /// The Locations list moved onto a place: shown at once when it is in
    /// the file in front, where the preview runs.
    pub(crate) fn lsp_preview_location(
        &mut self,
        path: &Path,
        line: u32,
        column: u32,
        cx: &mut Context<Self>,
    ) {
        let in_front = self
            .editor_preview_buffer()
            .and_then(|id| self.editor_local_file(id))
            .is_some_and(|f| f.path == path);
        if in_front {
            self.editor_symbol_preview(line, column, cx);
        }
    }
}

/// The most project symbols (LSP `workspace/symbol`) listed for one query.
const MAX_WORKSPACE_SYMBOLS: usize = 200;

/// One project symbol, before its column is measured. "Workspace" is the
/// protocol's word for the server's project root — not a tty7 workspace.
pub(crate) struct WorkspaceHit {
    pub(crate) name: String,
    pub(crate) kind: lsp_types::SymbolKind,
    pub(crate) container: Option<String>,
    pub(crate) path: PathBuf,
    pub(crate) position: Position,
}

/// A `workspace/symbol` answer in either of its shapes. A symbol known only
/// by its file (no range yet) lands on the file's first line.
#[allow(deprecated)]
pub(crate) fn workspace_symbols(answer: lsp_types::WorkspaceSymbolResponse) -> Vec<WorkspaceHit> {
    use lsp_types::{OneOf, WorkspaceSymbolResponse};
    let hits: Vec<WorkspaceHit> = match answer {
        WorkspaceSymbolResponse::Flat(symbols) => symbols
            .into_iter()
            .filter_map(|s| {
                Some(WorkspaceHit {
                    path: super::uri_to_path(&s.location.uri)?,
                    position: s.location.range.start,
                    name: s.name,
                    kind: s.kind,
                    container: s.container_name,
                })
            })
            .collect(),
        WorkspaceSymbolResponse::Nested(symbols) => symbols
            .into_iter()
            .filter_map(|s| {
                let (uri, position) = match s.location {
                    OneOf::Left(l) => (l.uri, l.range.start),
                    OneOf::Right(l) => (l.uri, Position::default()),
                };
                Some(WorkspaceHit {
                    path: super::uri_to_path(&uri)?,
                    position,
                    name: s.name,
                    kind: s.kind,
                    container: s.container_name,
                })
            })
            .collect(),
    };
    hits.into_iter().take(MAX_WORKSPACE_SYMBOLS).collect()
}

fn symbol_item(found: &Found, symbol: &WorkspaceHit, root: &Path) -> Item {
    let kind = serde_json::to_value(symbol.kind)
        .ok()
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let shown = found.path.strip_prefix(root).unwrap_or(&found.path);
    let place = format!("{}:{}", shown.display(), found.line + 1);
    let subtitle = match symbol.container.as_deref().filter(|c| !c.is_empty()) {
        Some(container) => format!("{container} · {place}"),
        None => place,
    };
    Item::new(
        symbol.name.clone(),
        CommandKind::GoToLocation {
            path: found.path.clone(),
            line: found.line,
            column: found.column,
        },
    )
    .with_subtitle(subtitle)
    .with_note(crate::ui::code_editor::outline::SymbolKind::from_lsp(kind as u32).tag())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_are_measured_against_open_buffers_and_the_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let on_disk = dir.path().join("b.rs");
        std::fs::write(&on_disk, "fn b() {\n    let 🎉 = a();\n}\n").unwrap();
        let open_path = dir.path().join("a.rs");
        let open = HashMap::from([(open_path.clone(), Rope::from("pub fn é_a() {}\n"))]);
        let found = resolve(
            vec![
                // `a` after the emoji: UTF-16 column 13, char column 12.
                (on_disk.clone(), Position::new(1, 13)),
                (open_path.clone(), Position::new(0, 7)),
                (open_path.clone(), Position::new(0, 7)),
                (open_path.clone(), Position::new(9, 0)),
            ],
            open,
            Encoding::Utf16,
        );
        assert_eq!(
            found,
            [
                Found {
                    path: open_path,
                    line: 0,
                    column: 7,
                    text: "pub fn é_a() {}".into(),
                },
                Found {
                    path: on_disk,
                    line: 1,
                    column: 12,
                    text: "let 🎉 = a();".into(),
                },
            ]
        );
    }

    #[test]
    #[allow(deprecated)]
    fn workspace_symbols_become_rows_naming_where_they_live() {
        let root = super::super::test_path("/p");
        let uri = super::super::path_to_uri(&root.join("src").join("net.rs")).unwrap();
        let hits = workspace_symbols(lsp_types::WorkspaceSymbolResponse::Flat(vec![
            lsp_types::SymbolInformation {
                name: "Server".into(),
                kind: lsp_types::SymbolKind::STRUCT,
                tags: None,
                deprecated: None,
                location: lsp_types::Location::new(
                    uri,
                    lsp_types::Range::new(Position::new(3, 11), Position::new(3, 17)),
                ),
                container_name: Some("net".into()),
            },
        ]));
        assert_eq!(hits.len(), 1);
        let found = Found {
            path: hits[0].path.clone(),
            line: 3,
            column: 11,
            text: String::new(),
        };
        let row = symbol_item(&found, &hits[0], &root);
        assert_eq!(row.title, "Server");
        let rel = Path::new("src").join("net.rs");
        assert_eq!(
            row.subtitle.as_deref(),
            Some(format!("net · {}:4", rel.display()).as_str())
        );
        assert_eq!(row.note.as_deref(), Some("struct"));
    }

    #[test]
    fn a_row_names_its_file_relative_to_the_project() {
        let found = Found {
            path: PathBuf::from("/p/src/lib.rs"),
            line: 4,
            column: 2,
            text: "x".into(),
        };
        let row = item(&found, Path::new("/p"));
        assert_eq!(row.title, "src/lib.rs:5");
        assert_eq!(row.subtitle.as_deref(), Some("x"));
    }
}

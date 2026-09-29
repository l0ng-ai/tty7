//! Language servers for the code editor.
//!
//! One [`LspStore`] per process holds every running server and every open
//! document. A server is keyed by (server, project root), so every buffer
//! under one Cargo workspace shares one rust-analyzer, whichever window it is
//! open in. A document is keyed by its path and owned by the first buffer that
//! opened it; the same file open in a second window shows the first one's
//! diagnostics only if it is the owner, since two buffers can hold two
//! different texts and a server only ever knows one.
//!
//! The editor talks to this module in a handful of places (see `editor.rs`):
//! the set of open buffers changed, a buffer was edited, a buffer was saved.
//! Everything else — starting a server when a file of its language opens,
//! shutting it down once its last file has been closed for a while, restarting
//! it when it crashes — happens here.
//!
//! Only files on this machine are served: a server runs next to the files it
//! reads. A buffer from an SFTP or remote host is skipped in
//! [`LspStore::sync_window`]; serving those means running the server on that
//! host, behind the same `Host` boundary the editor already reads files
//! through.

pub(crate) mod client;
pub(crate) mod convert;
mod editor;
mod locations;
mod providers;
pub(crate) mod rpc;
pub(crate) mod servers;
mod signature;
mod symbols;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyWindowHandle, App, AppContext as _, BorrowAppContext as _, Entity, EntityId, Global,
    Subscription, WeakEntity,
};
use gpui_component::Rope;
use gpui_component::input::InputState;
use lsp_types::{ServerCapabilities, TextDocumentSyncKind, Uri};
use serde_json::{Value, json};

use self::client::{LspClient, ServerEvent};
use self::convert::Encoding;
use self::rpc::ResponseError;
use self::servers::ServerSpec;

pub(crate) use self::editor::LspBuffer;

/// How long typing may go on before the server hears about it. Requests
/// (completion, hover…) send any change still waiting first, so this only
/// decides how soon diagnostics catch up after a pause.
const CHANGE_DEBOUNCE: Duration = Duration::from_millis(150);

/// How long typing must pause before the outline is asked for again. Longer
/// than [`CHANGE_DEBOUNCE`]: nobody reads the breadcrumbs mid-word.
const SYMBOLS_DEBOUNCE: Duration = Duration::from_millis(500);

/// How long a server outlives its last open file. Closing a file and opening
/// its neighbour is common; restarting rust-analyzer for it is not cheap.
const IDLE_SHUTDOWN: Duration = Duration::from_secs(60);

/// Crashes allowed within [`CRASH_WINDOW`] before a server is left down.
const MAX_CRASHES: usize = 3;
const CRASH_WINDOW: Duration = Duration::from_secs(180);

/// How long a program that was not on `PATH` is believed to still be
/// missing — long enough that opening ten files does not scan `PATH` ten
/// times, short enough that installing it mid-session is noticed.
const MISSING_RECHECK: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ServerKey {
    name: &'static str,
    root: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Starting,
    Running,
    Stopping,
    /// Crashed more than [`MAX_CRASHES`] times, or would not start.
    Dead,
}

struct Server {
    spec: &'static ServerSpec,
    /// The program that actually runs — `pylsp` when pyright was not found.
    label: String,
    generation: u64,
    phase: Phase,
    client: Option<Arc<LspClient>>,
    caps: Rc<ServerCapabilities>,
    encoding: Encoding,
    crashes: Vec<Instant>,
    idle_seq: u64,
    _tasks: Vec<gpui::Task<()>>,
}

struct Doc {
    uri: Uri,
    language_id: &'static str,
    server: ServerKey,
    version: i32,
    owner: WeakEntity<InputState>,
    owner_id: EntityId,
    /// The window-level editor (`Tty7App`) that holds the owning buffer.
    app_id: EntityId,
    app: WeakEntity<crate::ui::app::Tty7App>,
    /// Where the owning buffer is drawn, once known — edits the server asks
    /// for are applied through it.
    window: Option<AnyWindowHandle>,
    /// The text the server last heard.
    sent: Option<Rope>,
    /// An edit the server has not heard about yet.
    dirty: bool,
    change_seq: u64,
    symbols_seq: u64,
    diagnostics: Vec<lsp_types::Diagnostic>,
    problems: (usize, usize),
}

/// What the status bar says about a file's language server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LspStatus {
    /// The language has a server, and it is not installed.
    Missing(&'static str),
    Starting(String),
    Running {
        name: String,
        errors: usize,
        warnings: usize,
    },
    /// Gave up after repeated crashes.
    Down(String),
}

/// How [`LspStore::context`] brings the server up to date before a request.
pub(crate) enum Freshen<'a> {
    /// With this text — the caller's view of the buffer, from inside the
    /// buffer's own update, where the buffer cannot be read.
    Text(&'a Rope),
    /// Not at all: the caller is inside the buffer's update and has no text
    /// to offer, and the request does not depend on it.
    Skip,
}

/// Everything a request about one document needs, taken in one borrow.
#[derive(Clone)]
pub(crate) struct DocContext {
    pub(crate) client: Arc<LspClient>,
    pub(crate) uri: Uri,
    pub(crate) encoding: Encoding,
    pub(crate) caps: Rc<ServerCapabilities>,
    /// The server's raw diagnostics for this document, in its own columns.
    pub(crate) diagnostics: Vec<lsp_types::Diagnostic>,
    /// The project the server was started on.
    pub(crate) root: PathBuf,
}

#[derive(Default)]
pub(crate) struct LspStore {
    servers: HashMap<ServerKey, Server>,
    docs: HashMap<PathBuf, Doc>,
    missing: HashMap<&'static str, Instant>,
    next_generation: u64,
    /// Each window editor's way to offer its buffers again, for when the
    /// setting is turned back on. Returns `false` once the window is gone.
    windows: HashMap<EntityId, Resync>,
    /// `editor_lsp` as last seen, to tell a turn-on from any other change.
    enabled: bool,
    _subscriptions: Vec<Subscription>,
}

pub(crate) type Resync = Rc<dyn Fn(&mut App) -> bool>;

impl Global for LspStore {}

fn try_store_mut(cx: &mut App) -> Option<&mut LspStore> {
    cx.has_global::<LspStore>()
        .then(|| cx.global_mut::<LspStore>())
}

pub(crate) fn path_to_uri(path: &Path) -> Option<Uri> {
    url::Url::from_file_path(path).ok()?.as_str().parse().ok()
}

/// An absolute path for a test, spelled for this platform: `/p/a.rs` as
/// given, `C:\p\a.rs` on Windows, where a leading slash alone is not
/// absolute and so has no `file://` URI.
#[cfg(test)]
pub(crate) fn test_path(unix: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(format!("C:{}", unix.replace('/', "\\")))
    } else {
        PathBuf::from(unix)
    }
}

pub(crate) fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    let url = url::Url::parse(uri.as_str()).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    url.to_file_path().ok()
}

impl LspStore {
    fn ensure(cx: &mut App) {
        if cx.try_global::<LspStore>().is_some() {
            return;
        }
        let quit = cx.on_app_quit(|cx| LspStore::on_quit(cx));
        let config = cx.observe_global::<crate::core::config::Config>(|cx| {
            let enabled = cx.global::<crate::core::config::Config>().editor_lsp;
            if cx.try_global::<LspStore>().is_none() {
                return;
            }
            cx.update_global::<LspStore, _>(|store, cx| store.set_enabled(enabled, cx));
        });
        let enabled = cx
            .try_global::<crate::core::config::Config>()
            .is_some_and(|c| c.editor_lsp);
        cx.set_global(LspStore {
            enabled,
            _subscriptions: vec![quit, config],
            ..Default::default()
        });
    }

    fn update<R>(cx: &mut App, f: impl FnOnce(&mut LspStore, &mut App) -> R) -> R {
        Self::ensure(cx);
        cx.update_global::<LspStore, _>(f)
    }

    // ---- The editor's side ----

    /// Remembers how a window's editor offers its buffers, so turning the
    /// setting back on can ask every window for them again.
    pub(crate) fn register_window(app_id: EntityId, resync: Resync, cx: &mut App) {
        Self::update(cx, |store, _| {
            store.windows.entry(app_id).or_insert(resync);
        });
    }

    /// The `editor_lsp` setting, as it changes. Off closes every document
    /// and lets the servers go; on asks each window for its open buffers,
    /// which starts them again — deferred, since the setting is changed from
    /// inside some window's own update.
    fn set_enabled(&mut self, enabled: bool, cx: &mut App) {
        let was = std::mem::replace(&mut self.enabled, enabled);
        if !enabled {
            if !self.docs.is_empty() || !self.servers.is_empty() {
                self.close_all(cx);
            }
            return;
        }
        if was {
            return;
        }
        let windows: Vec<(EntityId, Resync)> = self
            .windows
            .iter()
            .map(|(id, r)| (*id, r.clone()))
            .collect();
        cx.defer(move |cx| {
            let gone: Vec<EntityId> = windows
                .into_iter()
                .filter(|(_, resync)| !resync(cx))
                .map(|(id, _)| id)
                .collect();
            if let Some(store) = try_store_mut(cx) {
                for id in gone {
                    store.windows.remove(&id);
                }
            }
        });
    }

    /// Brings the documents one window's editor owns in line with the
    /// buffers it has: opens the new ones, closes the ones that went away or
    /// moved, and leaves the rest alone. Cheap when nothing changed.
    pub(crate) fn sync_window(app_id: EntityId, buffers: Vec<LspBuffer>, cx: &mut App) {
        if buffers.is_empty() && cx.try_global::<LspStore>().is_none() {
            return;
        }
        Self::update(cx, |store, cx| {
            let stale: Vec<PathBuf> = store
                .docs
                .iter()
                .filter(|(path, doc)| {
                    doc.owner.upgrade().is_none()
                        || (doc.app_id == app_id
                            && !buffers
                                .iter()
                                .any(|b| b.input.entity_id() == doc.owner_id && &b.path == *path))
                })
                .map(|(path, _)| path.clone())
                .collect();
            for path in stale {
                store.close_doc(&path, cx);
            }
            for buffer in buffers {
                if !store.docs.contains_key(&buffer.path) {
                    store.open_doc(app_id, buffer, cx);
                }
            }
        });
    }

    /// A buffer's text changed. The server hears about it after a pause, or
    /// before the next request, whichever comes first.
    pub(crate) fn edited(input_id: EntityId, path: &Path, cx: &mut App) {
        let Some(store) = try_store_mut(cx) else {
            return;
        };
        let Some(doc) = store.docs.get_mut(path).filter(|d| d.owner_id == input_id) else {
            return;
        };
        doc.dirty = true;
        doc.change_seq += 1;
        let seq = doc.change_seq;
        let path = path.to_path_buf();
        let current = move |cx: &App, path: &Path| {
            cx.try_global::<LspStore>()
                .and_then(|s| s.docs.get(path))
                .is_some_and(|d| d.change_seq == seq)
        };
        cx.spawn(async move |cx| {
            cx.background_executor().timer(CHANGE_DEBOUNCE).await;
            if !cx.update(|cx| current(cx, &path)) {
                return;
            }
            cx.update(|cx| Self::update(cx, |store, cx| store.flush_from_owner(&path, cx)));
            cx.background_executor()
                .timer(SYMBOLS_DEBOUNCE.saturating_sub(CHANGE_DEBOUNCE))
                .await;
            cx.update(|cx| {
                if current(cx, &path) {
                    Self::update(cx, |store, cx| store.refresh_symbols(&path, cx));
                }
            });
        })
        .detach();
    }

    /// A buffer was written to disk.
    pub(crate) fn saved(input_id: EntityId, path: &Path, cx: &mut App) {
        if cx.try_global::<LspStore>().is_none() {
            return;
        }
        Self::update(cx, |store, cx| {
            if !store.docs.get(path).is_some_and(|d| d.owner_id == input_id) {
                return;
            }
            store.flush_from_owner(path, cx);
            let Some(doc) = store.docs.get(path) else {
                return;
            };
            let Some(server) = store.servers.get(&doc.server) else {
                return;
            };
            let Some(client) = server.client.as_ref() else {
                return;
            };
            let include_text = match &server.caps.text_document_sync {
                Some(lsp_types::TextDocumentSyncCapability::Options(o)) => match &o.save {
                    Some(lsp_types::TextDocumentSyncSaveOptions::SaveOptions(s)) => {
                        s.include_text.unwrap_or(false)
                    }
                    _ => false,
                },
                _ => false,
            };
            client.notify::<lsp_types::notification::DidSaveTextDocument>(
                lsp_types::DidSaveTextDocumentParams {
                    text_document: lsp_types::TextDocumentIdentifier::new(doc.uri.clone()),
                    text: include_text
                        .then(|| doc.sent.as_ref().map(|t| t.to_string()))
                        .flatten(),
                },
            );
        });
    }

    pub(crate) fn status(path: &Path, language: &str, cx: &App) -> Option<LspStatus> {
        let store = cx.try_global::<LspStore>()?;
        if let Some(doc) = store.docs.get(path) {
            let server = store.servers.get(&doc.server)?;
            return Some(match server.phase {
                Phase::Starting | Phase::Stopping => LspStatus::Starting(server.label.clone()),
                Phase::Running => LspStatus::Running {
                    name: server.label.clone(),
                    errors: doc.problems.0,
                    warnings: doc.problems.1,
                },
                Phase::Dead => LspStatus::Down(server.label.clone()),
            });
        }
        let spec = servers::spec_for_language(language)?;
        store
            .missing
            .contains_key(spec.name)
            .then_some(LspStatus::Missing(spec.name))
    }

    /// Every open document's diagnostics, for the editor's Problems list: in
    /// editor columns where the text the server saw is known, its own
    /// otherwise.
    pub(crate) fn diagnostics_snapshot(cx: &App) -> Vec<(PathBuf, Vec<lsp_types::Diagnostic>)> {
        let Some(store) = cx.try_global::<LspStore>() else {
            return Vec::new();
        };
        store
            .docs
            .iter()
            .filter(|(_, doc)| !doc.diagnostics.is_empty())
            .map(|(path, doc)| {
                let encoding = store.servers.get(&doc.server).map(|s| s.encoding);
                let diagnostics = match (doc.sent.as_ref(), encoding) {
                    (Some(text), Some(enc)) => {
                        convert::diagnostics_to_editor(text, &doc.diagnostics, enc)
                    }
                    _ => doc.diagnostics.clone(),
                };
                (path.clone(), diagnostics)
            })
            .collect()
    }

    /// The server behind `path`, with any edit still waiting sent first (see
    /// [`Freshen`]) so the request that follows sees the text the caller sees.
    ///
    /// Only the buffer that owns the document may ask (`requester` is its
    /// `InputState`'s id). The same file open in a second window is a second
    /// buffer whose text can differ from the owner's; letting it send that
    /// text would swap the document under the owner, and letting it ask
    /// against the owner's text would answer with positions for a text it
    /// does not hold. So a non-owner is refused, quietly: that buffer simply
    /// has no language features until the owner closes and it takes over.
    pub(crate) fn context(
        path: &Path,
        requester: EntityId,
        sync: Freshen<'_>,
        window: Option<AnyWindowHandle>,
        cx: &mut App,
    ) -> Option<DocContext> {
        cx.try_global::<LspStore>()?;
        Self::update(cx, |store, _| {
            store.context_for(path, requester, sync, window)
        })
    }

    fn context_for(
        &mut self,
        path: &Path,
        requester: EntityId,
        sync: Freshen<'_>,
        window: Option<AnyWindowHandle>,
    ) -> Option<DocContext> {
        // Checked before anything is sent: a non-owner must never sync.
        if self.docs.get(path)?.owner_id != requester {
            return None;
        }
        match sync {
            Freshen::Text(text) => self.flush_text(path, text),
            Freshen::Skip => {}
        }
        let doc = self.docs.get_mut(path)?;
        if window.is_some() {
            doc.window = window;
        }
        let server = self.servers.get(&doc.server)?;
        if server.phase != Phase::Running {
            return None;
        }
        Some(DocContext {
            client: server.client.clone()?,
            uri: doc.uri.clone(),
            encoding: server.encoding,
            caps: server.caps.clone(),
            diagnostics: doc.diagnostics.clone(),
            root: doc.server.root.clone(),
        })
    }

    /// The server's trigger characters for completion in `path`.
    pub(crate) fn completion_triggers(path: &Path, cx: &App) -> Vec<String> {
        let Some(store) = cx.try_global::<LspStore>() else {
            return Vec::new();
        };
        store
            .docs
            .get(path)
            .and_then(|d| store.servers.get(&d.server))
            .and_then(|s| s.caps.completion_provider.as_ref())
            .and_then(|c| c.trigger_characters.clone())
            .unwrap_or_default()
    }

    // ---- Documents ----

    fn open_doc(&mut self, app_id: EntityId, buffer: LspBuffer, cx: &mut App) {
        let Some(spec) = servers::spec_for_language(buffer.language) else {
            return;
        };
        let Some(uri) = path_to_uri(&buffer.path) else {
            return;
        };
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let root = servers::find_root(&buffer.path, spec, home.as_deref());
        let key = ServerKey {
            name: spec.name,
            root,
        };
        let needs_start = self
            .servers
            .get(&key)
            .is_none_or(|s| s.phase == Phase::Stopping);
        if needs_start && !self.start_server(key.clone(), spec, cx) {
            return;
        }
        let text = buffer.input.read(cx).text().clone();
        let doc = Doc {
            uri,
            language_id: servers::language_id(&buffer.path, buffer.language),
            server: key.clone(),
            version: 0,
            owner: buffer.input.downgrade(),
            owner_id: buffer.input.entity_id(),
            app_id,
            app: buffer.app.clone(),
            window: None,
            sent: Some(text.clone()),
            dirty: false,
            change_seq: 0,
            symbols_seq: 0,
            diagnostics: Vec::new(),
            problems: (0, 0),
        };
        if let Some(server) = self.servers.get_mut(&key) {
            // A file opening cancels a pending idle shutdown.
            server.idle_seq += 1;
            if let Some(client) = &server.client {
                send_did_open(client, &doc, &text);
            }
        }
        self.docs.insert(buffer.path.clone(), doc);
        providers::install(&buffer.input, &buffer.path, buffer.app.clone(), cx);
        self.refresh_symbols(&buffer.path, cx);
    }

    fn close_doc(&mut self, path: &Path, cx: &mut App) {
        let Some(doc) = self.docs.remove(path) else {
            return;
        };
        if let Some(input) = doc.owner.upgrade() {
            providers::uninstall(&input, cx);
            clear_diagnostics(&input, cx);
        }
        // Deferred: this runs while the editor that holds the buffer is
        // itself being updated (it is what asked for the close).
        let (app, buffer) = (doc.app.clone(), doc.owner_id);
        cx.defer(move |cx| {
            let _ = app.update(cx, |app, cx| {
                app.editor_set_document_symbols(buffer, None, cx)
            });
        });
        let key = doc.server.clone();
        let Some(server) = self.servers.get_mut(&key) else {
            return;
        };
        if let Some(client) = &server.client {
            client.notify::<lsp_types::notification::DidCloseTextDocument>(
                lsp_types::DidCloseTextDocumentParams {
                    text_document: lsp_types::TextDocumentIdentifier::new(doc.uri),
                },
            );
        }
        if self.docs.values().any(|d| d.server == key) {
            return;
        }
        server.idle_seq += 1;
        let seq = server.idle_seq;
        let generation = server.generation;
        cx.spawn(async move |cx| {
            cx.background_executor().timer(IDLE_SHUTDOWN).await;
            cx.update(|cx| {
                let Some(store) = try_store_mut(cx) else {
                    return;
                };
                let still_idle = store
                    .servers
                    .get(&key)
                    .is_some_and(|s| s.idle_seq == seq && s.generation == generation)
                    && !store.docs.values().any(|d| d.server == key);
                if still_idle {
                    LspStore::update(cx, |store, cx| store.shutdown_server(&key, cx));
                }
            });
        })
        .detach();
    }

    fn close_all(&mut self, cx: &mut App) {
        let paths: Vec<PathBuf> = self.docs.keys().cloned().collect();
        for path in paths {
            self.close_doc(&path, cx);
        }
        let keys: Vec<ServerKey> = self.servers.keys().cloned().collect();
        for key in keys {
            self.shutdown_server(&key, cx);
        }
    }

    fn flush_from_owner(&mut self, path: &Path, cx: &mut App) {
        let Some(text) = self
            .docs
            .get(path)
            .and_then(|d| d.owner.upgrade())
            .map(|input| input.read(cx).text().clone())
        else {
            return;
        };
        self.flush_text(path, &text);
    }

    /// Sends `text` as the document's new content, if it is new. Full-text
    /// sync: one change event without a range replaces the whole document,
    /// which every server accepts whatever sync kind it asked for.
    fn flush_text(&mut self, path: &Path, text: &Rope) {
        let Some(doc) = self.docs.get_mut(path) else {
            return;
        };
        doc.dirty = false;
        if doc.sent.as_ref() == Some(text) {
            return;
        }
        let Some(server) = self.servers.get(&doc.server) else {
            return;
        };
        let wants_changes = !matches!(
            &server.caps.text_document_sync,
            Some(lsp_types::TextDocumentSyncCapability::Kind(
                TextDocumentSyncKind::NONE
            ))
        );
        doc.version += 1;
        doc.sent = Some(text.clone());
        let (Some(client), true) = (&server.client, wants_changes) else {
            return;
        };
        client.notify::<lsp_types::notification::DidChangeTextDocument>(
            lsp_types::DidChangeTextDocumentParams {
                text_document: lsp_types::VersionedTextDocumentIdentifier::new(
                    doc.uri.clone(),
                    doc.version,
                ),
                content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: text.to_string(),
                }],
            },
        );
    }

    /// Asks the server for the document's outline, and hands it to the
    /// editor if the text has not moved on by the time it answers.
    fn refresh_symbols(&mut self, path: &Path, cx: &mut App) {
        let Some(doc) = self.docs.get_mut(path) else {
            return;
        };
        let Some(server) = self.servers.get(&doc.server) else {
            return;
        };
        let supported = matches!(
            &server.caps.document_symbol_provider,
            Some(lsp_types::OneOf::Left(true) | lsp_types::OneOf::Right(_))
        );
        let (Phase::Running, true, Some(client), Some(text)) = (
            server.phase,
            supported,
            server.client.clone(),
            doc.sent.clone(),
        ) else {
            return;
        };
        doc.symbols_seq += 1;
        let (seq, version) = (doc.symbols_seq, doc.version);
        let (app, buffer) = (doc.app.clone(), doc.owner_id);
        let encoding = server.encoding;
        let request = client.request::<lsp_types::request::DocumentSymbolRequest>(
            lsp_types::DocumentSymbolParams {
                text_document: lsp_types::TextDocumentIdentifier::new(doc.uri.clone()),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
        );
        let path = path.to_path_buf();
        cx.spawn(async move |cx| {
            let response = match request.await {
                Ok(Some(response)) => response,
                Ok(None) => return,
                Err(e) => {
                    log::debug!("lsp: documentSymbol: {e:#}");
                    return;
                }
            };
            let outline = symbols::outline(response, &text, encoding);
            cx.update(|cx| {
                let current = cx
                    .try_global::<LspStore>()
                    .and_then(|s| s.docs.get(&path))
                    .is_some_and(|d| d.symbols_seq == seq && d.version == version && !d.dirty);
                if current {
                    // Nothing yet — a server still indexing answers with an
                    // empty list — leaves the tree's outline in place.
                    let outline = (!outline.is_empty()).then_some(outline);
                    let _ = app.update(cx, |app, cx| {
                        app.editor_set_document_symbols(buffer, outline, cx)
                    });
                }
            });
        })
        .detach();
    }

    // ---- Servers ----

    fn start_server(&mut self, key: ServerKey, spec: &'static ServerSpec, cx: &mut App) -> bool {
        if self
            .missing
            .get(spec.name)
            .is_some_and(|at| at.elapsed() < MISSING_RECHECK)
        {
            return false;
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        let Some((program, args)) = servers::resolve(spec, &path) else {
            self.missing.insert(spec.name, Instant::now());
            return false;
        };
        self.missing.remove(spec.name);
        let label = program
            .file_name()
            .map(|n| n.to_string_lossy().trim_end_matches(".exe").to_owned())
            .unwrap_or_else(|| spec.name.to_owned());
        let (client, events) = match LspClient::spawn(&label, &program, args, &key.root) {
            Ok(started) => started,
            Err(e) => {
                log::warn!("lsp: could not start {label}: {e:#}");
                self.missing.insert(spec.name, Instant::now());
                return false;
            }
        };
        log::info!("lsp: started {label} for {}", key.root.display());
        self.next_generation += 1;
        let generation = self.next_generation;
        let crashes = self
            .servers
            .get(&key)
            .map(|s| s.crashes.clone())
            .unwrap_or_default();

        let init = client.initialize(initialize_params(&key.root, spec));
        let init_task = {
            let key = key.clone();
            cx.spawn(async move |cx| {
                let result = init.await;
                cx.update(|cx| {
                    LspStore::update(cx, |store, cx| {
                        store.initialized(&key, generation, result, cx)
                    })
                });
            })
        };
        let events_task = {
            let key = key.clone();
            cx.spawn(async move |cx| {
                while let Ok(event) = events.recv().await {
                    let exited = matches!(event, ServerEvent::Exited);
                    cx.update(|cx| {
                        LspStore::update(cx, |store, cx| {
                            store.handle_event(&key, generation, event, cx)
                        })
                    });
                    if exited {
                        break;
                    }
                }
            })
        };
        // A server being replaced — one still shutting down — goes with its
        // own tasks; its client is killed when the last handle drops.
        self.servers.insert(
            key,
            Server {
                spec,
                label,
                generation,
                phase: Phase::Starting,
                client: Some(client),
                caps: Rc::new(ServerCapabilities::default()),
                encoding: Encoding::Utf16,
                crashes,
                idle_seq: 0,
                _tasks: vec![init_task, events_task],
            },
        );
        true
    }

    fn initialized(
        &mut self,
        key: &ServerKey,
        generation: u64,
        result: anyhow::Result<Value>,
        cx: &mut App,
    ) {
        let Some(server) = self
            .servers
            .get_mut(key)
            .filter(|s| s.generation == generation && s.phase == Phase::Starting)
        else {
            return;
        };
        let Some(client) = server.client.clone() else {
            return;
        };
        let init = result.and_then(|v| {
            serde_json::from_value::<lsp_types::InitializeResult>(v).map_err(Into::into)
        });
        match init {
            Ok(init) => {
                server.encoding = Encoding::from_kind(init.capabilities.position_encoding.as_ref());
                server.caps = Rc::new(init.capabilities);
                server.phase = Phase::Running;
                client.notify_raw("initialized", json!({}));
                // Servers that pull their settings (pyright) wait for this
                // before asking; the rest take them as sent.
                client.notify_raw(
                    "workspace/didChangeConfiguration",
                    json!({ "settings": server.spec.settings() }),
                );
                client.open_gate();
                log::info!("lsp: {} is ready", server.label);
                let paths: Vec<PathBuf> = self
                    .docs
                    .iter()
                    .filter(|(_, d)| &d.server == key)
                    .map(|(p, _)| p.clone())
                    .collect();
                for path in paths {
                    self.refresh_symbols(&path, cx);
                }
            }
            Err(e) => {
                log::warn!("lsp: {} failed to initialize: {e:#}", server.label);
                // Treated as a crash: the exit that follows (or the kill)
                // decides whether it is worth another try.
                client.kill();
            }
        }
        self.notify_docs_of(key, cx);
    }

    fn handle_event(&mut self, key: &ServerKey, generation: u64, event: ServerEvent, cx: &mut App) {
        let Some(server) = self.servers.get(key).filter(|s| s.generation == generation) else {
            return;
        };
        let Some(client) = server.client.clone() else {
            return;
        };
        match event {
            ServerEvent::Notification { method, params } => match method.as_str() {
                "textDocument/publishDiagnostics" => {
                    if let Ok(params) =
                        serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(params)
                    {
                        let encoding = server.encoding;
                        self.publish_diagnostics(params, encoding, cx);
                    }
                }
                "window/showMessage" | "window/logMessage" => {
                    let message = params.get("message").and_then(Value::as_str).unwrap_or("");
                    let error = params.get("type").and_then(Value::as_i64) == Some(1);
                    if error {
                        log::warn!("lsp {}: {message}", server.label);
                    } else {
                        log::debug!("lsp {}: {message}", server.label);
                    }
                }
                _ => {}
            },
            ServerEvent::Request { id, method, params } => {
                let reply = match method.as_str() {
                    "workspace/configuration" => {
                        let settings = server.spec.settings();
                        let items = params
                            .get("items")
                            .and_then(Value::as_array)
                            .map(|items| {
                                items
                                    .iter()
                                    .map(|item| {
                                        let section = item.get("section").and_then(Value::as_str);
                                        servers::configuration_item(&settings, section)
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        Ok(Value::Array(items))
                    }
                    "client/registerCapability"
                    | "client/unregisterCapability"
                    | "window/workDoneProgress/create"
                    | "window/showMessageRequest"
                    | "workspace/diagnostic/refresh"
                    | "workspace/semanticTokens/refresh"
                    | "workspace/inlayHint/refresh"
                    | "workspace/codeLens/refresh" => Ok(Value::Null),
                    "workspace/workspaceFolders" => Ok(json!([{
                        "uri": path_to_uri(&key.root).map(|u| u.as_str().to_owned()),
                        "name": folder_name(&key.root),
                    }])),
                    "workspace/applyEdit" => {
                        let encoding = server.encoding;
                        match serde_json::from_value::<lsp_types::ApplyWorkspaceEditParams>(params)
                        {
                            Ok(params) => {
                                // Measured against what this server last
                                // heard: a buffer typed in since would take
                                // the edit at the wrong places.
                                let per_file = workspace_edit_files(params.edit);
                                match self.stale_target(&per_file, None, cx) {
                                    Some(path) => Ok(json!({
                                        "applied": false,
                                        "failureReason": format!(
                                            "{} changed since the server last saw it",
                                            path.display()
                                        ),
                                    })),
                                    None => {
                                        spawn_apply(per_file, encoding, cx);
                                        Ok(json!({ "applied": true }))
                                    }
                                }
                            }
                            Err(e) => {
                                Ok(json!({ "applied": false, "failureReason": e.to_string() }))
                            }
                        }
                    }
                    other => Err(ResponseError::method_not_found(other)),
                };
                client.respond(id, reply);
            }
            ServerEvent::Exited => self.server_exited(key, cx),
        }
    }

    fn publish_diagnostics(
        &mut self,
        params: lsp_types::PublishDiagnosticsParams,
        encoding: Encoding,
        cx: &mut App,
    ) {
        let path = self
            .docs
            .iter()
            .find(|(_, d)| d.uri == params.uri)
            .map(|(p, _)| p.clone())
            .or_else(|| uri_to_path(&params.uri).filter(|p| self.docs.contains_key(p)));
        let Some(doc) = path.and_then(|p| self.docs.get_mut(&p)) else {
            return;
        };
        doc.problems = convert::count_problems(&params.diagnostics);
        doc.diagnostics = params.diagnostics;
        // Positions for a text the buffer no longer holds would underline the
        // wrong characters. The server publishes again once it has caught
        // up, so these are counted and kept for code actions, not drawn; the
        // underlines already shown have been moving with the edits.
        let stale = doc.dirty || params.version.is_some_and(|v| v != doc.version);
        let Some(input) = doc.owner.upgrade() else {
            return;
        };
        let diagnostics = doc.diagnostics.clone();
        let sent = doc.sent.clone();
        input.update(cx, |state, cx| {
            let text = state.text().clone();
            if !stale && sent.as_ref() == Some(&text) {
                let mapped = convert::diagnostics_to_editor(&text, &diagnostics, encoding);
                if let Some(set) = state.diagnostics_mut() {
                    set.replace_all(&text, mapped);
                }
            }
            cx.notify();
        });
    }

    fn server_exited(&mut self, key: &ServerKey, cx: &mut App) {
        let Some(server) = self.servers.get_mut(key) else {
            return;
        };
        if let Some(client) = server.client.take() {
            client.kill();
        }
        if server.phase == Phase::Stopping {
            self.servers.remove(key);
            return;
        }
        if server.phase == Phase::Starting && server.crashes.is_empty() {
            // Gone before it ever answered `initialize`, on its first try:
            // not a crash a restart would fix, but a program that cannot run
            // here — rustup's `rust-analyzer` proxy without the component
            // installed exits exactly like this. Treated as missing, so the
            // status bar says so and the next file tries again later.
            log::info!("lsp: {} exited before initializing", server.label);
            self.missing.insert(server.spec.name, Instant::now());
            self.servers.remove(key);
            let orphans: Vec<PathBuf> = self
                .docs
                .iter()
                .filter(|(_, d)| &d.server == key)
                .map(|(p, _)| p.clone())
                .collect();
            for path in orphans {
                if let Some(input) = self.docs.remove(&path).and_then(|d| d.owner.upgrade()) {
                    providers::uninstall(&input, cx);
                }
            }
            return;
        }
        let now = Instant::now();
        server
            .crashes
            .retain(|at| now.duration_since(*at) < CRASH_WINDOW);
        server.crashes.push(now);
        let has_docs = self.docs.values().any(|d| &d.server == key);
        let server = self.servers.get_mut(key).expect("looked up above");
        log::warn!(
            "lsp: {} exited unexpectedly ({} time(s) recently)",
            server.label,
            server.crashes.len()
        );
        if !has_docs {
            self.servers.remove(key);
            return;
        }
        if server.crashes.len() >= MAX_CRASHES {
            server.phase = Phase::Dead;
            self.notify_docs_of(key, cx);
            return;
        }
        server.phase = Phase::Starting;
        let spec = server.spec;
        let delay = Duration::from_secs(server.crashes.len() as u64);
        let key = key.clone();
        self.notify_docs_of(&key, cx);
        cx.spawn(async move |cx| {
            cx.background_executor().timer(delay).await;
            cx.update(|cx| LspStore::update(cx, |store, cx| store.restart(&key, spec, cx)));
        })
        .detach();
    }

    fn restart(&mut self, key: &ServerKey, spec: &'static ServerSpec, cx: &mut App) {
        if !self.docs.values().any(|d| &d.server == key) {
            self.servers.remove(key);
            return;
        }
        // Forget the grace period for a program that vanished: the restart
        // is the check.
        self.missing.remove(spec.name);
        if !self.start_server(key.clone(), spec, cx) {
            if let Some(server) = self.servers.get_mut(key) {
                server.phase = Phase::Dead;
            }
            self.notify_docs_of(key, cx);
            return;
        }
        let Some(client) = self.servers.get(key).and_then(|s| s.client.clone()) else {
            return;
        };
        for doc in self.docs.values_mut().filter(|d| &d.server == key) {
            let Some(input) = doc.owner.upgrade() else {
                continue;
            };
            let text = input.read(cx).text().clone();
            doc.version = 0;
            doc.dirty = false;
            doc.sent = Some(text.clone());
            send_did_open(&client, doc, &text);
        }
    }

    fn shutdown_server(&mut self, key: &ServerKey, cx: &mut App) {
        let Some(server) = self.servers.get_mut(key) else {
            return;
        };
        let Some(client) = server.client.clone() else {
            self.servers.remove(key);
            return;
        };
        server.phase = Phase::Stopping;
        log::info!(
            "lsp: shutting down {} for {}",
            server.label,
            key.root.display()
        );
        let generation = server.generation;
        let key = key.clone();
        cx.spawn(async move |cx| {
            let executor = cx.background_executor().clone();
            // Two seconds to say it is done; it is told to exit either way.
            let _ = client
                .request_raw_within("shutdown", Value::Null, Duration::from_secs(2))
                .await;
            client.notify_raw("exit", Value::Null);
            for _ in 0..20 {
                if client.reap_if_exited() {
                    break;
                }
                executor.timer(Duration::from_millis(50)).await;
            }
            client.kill();
            cx.update(|cx| {
                if let Some(store) = try_store_mut(cx)
                    && store
                        .servers
                        .get(&key)
                        .is_some_and(|s| s.generation == generation && s.phase == Phase::Stopping)
                {
                    store.servers.remove(&key);
                }
            });
        })
        .detach();
    }

    /// The app is quitting: every server is told to exit and, whatever it
    /// does with that, is gone before tty7 is.
    fn on_quit(cx: &mut App) -> impl std::future::Future<Output = ()> + use<> {
        let clients: Vec<Arc<LspClient>> = try_store_mut(cx)
            .map(|store| {
                store
                    .servers
                    .drain()
                    .filter_map(|(_, s)| s.client)
                    .collect()
            })
            .unwrap_or_default();
        for client in &clients {
            client.notify_raw("exit", Value::Null);
        }
        let executor = cx.background_executor().clone();
        async move {
            // gpui gives quit handlers a fraction of a second; most of it goes
            // to letting servers leave on their own.
            for _ in 0..3 {
                if clients.iter().all(|c| c.has_exited()) {
                    break;
                }
                executor.timer(Duration::from_millis(40)).await;
            }
            for client in &clients {
                client.kill();
            }
        }
    }

    fn notify_docs_of(&self, key: &ServerKey, cx: &mut App) {
        let inputs: Vec<Entity<InputState>> = self
            .docs
            .values()
            .filter(|d| &d.server == key)
            .filter_map(|d| d.owner.upgrade())
            .collect();
        for input in inputs {
            input.update(cx, |_, cx| cx.notify());
        }
    }
}

fn send_did_open(client: &LspClient, doc: &Doc, text: &Rope) {
    client.notify::<lsp_types::notification::DidOpenTextDocument>(
        lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem::new(
                doc.uri.clone(),
                doc.language_id.to_owned(),
                doc.version,
                text.to_string(),
            ),
        },
    );
}

fn clear_diagnostics(input: &Entity<InputState>, cx: &mut App) {
    input.update(cx, |state, cx| {
        if let Some(set) = state.diagnostics_mut() {
            set.clear();
        }
        cx.notify();
    });
}

fn folder_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string())
}

/// What tty7 tells a server it can do. Snippets are declined — the editor
/// has no snippet engine — so servers send plain insert text where they can;
/// the few that send snippets anyway are flattened by
/// [`convert::snippet_to_plain`].
fn initialize_params(root: &Path, spec: &ServerSpec) -> Value {
    let uri = path_to_uri(root).map(|u| u.as_str().to_owned());
    json!({
        "processId": std::process::id(),
        "clientInfo": { "name": "tty7", "version": env!("CARGO_PKG_VERSION") },
        "rootUri": uri,
        "rootPath": root.display().to_string(),
        "workspaceFolders": [{ "uri": uri, "name": folder_name(root) }],
        "initializationOptions": spec.initialization_options(),
        "capabilities": {
            "general": { "positionEncodings": ["utf-16"] },
            "workspace": {
                "applyEdit": true,
                "workspaceEdit": { "documentChanges": true },
                "configuration": true,
                "workspaceFolders": true,
            },
            "textDocument": {
                "synchronization": { "didSave": true, "willSave": false, "dynamicRegistration": false },
                "completion": {
                    "completionItem": {
                        "snippetSupport": false,
                        "documentationFormat": ["markdown", "plaintext"],
                        "insertReplaceSupport": false,
                        "resolveSupport": {
                            "properties": ["documentation", "detail", "additionalTextEdits"],
                        },
                    },
                    "contextSupport": true,
                },
                "hover": { "contentFormat": ["markdown", "plaintext"] },
                "signatureHelp": {
                    "signatureInformation": {
                        "documentationFormat": ["markdown", "plaintext"],
                        "parameterInformation": { "labelOffsetSupport": true },
                        "activeParameterSupport": true,
                    },
                },
                "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                "definition": { "linkSupport": true },
                "codeAction": {
                    "codeActionLiteralSupport": {
                        "codeActionKind": {
                            "valueSet": [
                                "", "quickfix", "refactor", "refactor.extract",
                                "refactor.inline", "refactor.rewrite", "source",
                                "source.organizeImports", "source.fixAll",
                            ],
                        },
                    },
                    "dataSupport": true,
                    "resolveSupport": { "properties": ["edit"] },
                },
                "formatting": { "dynamicRegistration": false },
                "rename": { "prepareSupport": false },
                "publishDiagnostics": { "versionSupport": true, "relatedInformation": false },
            },
            "window": { "workDoneProgress": false },
        },
    })
}

/// Applies a server's workspace edit: to the buffer holding a file when one
/// does, straight to disk otherwise. Run on a later turn of the event loop,
/// never inside the update that asked for it — a buffer's own window may be
/// the one being updated.
///
/// `baseline` is what every open document held when the request that
/// produced the edit went out ([`LspStore::snapshot`]). If any buffer the
/// edit touches holds something else now — typed in, opened or closed since
/// — nothing at all is applied: half a rename is worse than none. Without a
/// baseline, the text the server last heard is the measure. Returns whether
/// the edit was applied.
pub(crate) fn apply_workspace_edit(
    edit: lsp_types::WorkspaceEdit,
    encoding: Encoding,
    baseline: Option<HashMap<PathBuf, Rope>>,
    cx: &mut App,
) -> bool {
    let per_file = workspace_edit_files(edit);
    let stale = cx
        .try_global::<LspStore>()
        .and_then(|store| store.stale_target(&per_file, baseline.as_ref(), cx));
    if let Some(path) = stale {
        log::info!(
            "lsp: not applying an edit: {} changed since it was asked for",
            path.display()
        );
        return false;
    }
    spawn_apply(per_file, encoding, cx);
    true
}

/// Whether every text in `now` is the one in `baseline`, for the files an
/// edit touches: the first that is not. A file open in only one of the two
/// was opened or closed in between, which counts as changed.
pub(crate) fn first_stale(
    targets: &[PathBuf],
    now: &HashMap<PathBuf, Rope>,
    baseline: &HashMap<PathBuf, Rope>,
) -> Option<PathBuf> {
    targets
        .iter()
        .find(|path| match (now.get(*path), baseline.get(*path)) {
            (Some(now), Some(then)) => now != then,
            (None, None) => false,
            _ => true,
        })
        .cloned()
}

impl LspStore {
    /// What every open document's buffer holds right now — the baseline a
    /// request that answers with edits is checked against.
    pub(crate) fn snapshot(cx: &App) -> HashMap<PathBuf, Rope> {
        let Some(store) = cx.try_global::<LspStore>() else {
            return HashMap::new();
        };
        store
            .docs
            .iter()
            .filter_map(|(path, doc)| {
                Some((path.clone(), doc.owner.upgrade()?.read(cx).text().clone()))
            })
            .collect()
    }

    fn stale_target(
        &self,
        per_file: &[(Uri, Vec<lsp_types::TextEdit>)],
        baseline: Option<&HashMap<PathBuf, Rope>>,
        cx: &App,
    ) -> Option<PathBuf> {
        let targets: Vec<PathBuf> = per_file
            .iter()
            .filter_map(|(u, _)| uri_to_path(u))
            .collect();
        let open = |path: &PathBuf| {
            let doc = self.docs.get(path)?;
            Some((doc, doc.owner.upgrade()?))
        };
        let now: HashMap<PathBuf, Rope> = targets
            .iter()
            .filter_map(|p| Some((p.clone(), open(p)?.1.read(cx).text().clone())))
            .collect();
        let server_heard: HashMap<PathBuf, Rope>;
        let baseline = match baseline {
            Some(baseline) => baseline,
            None => {
                server_heard = targets
                    .iter()
                    .filter_map(|p| {
                        let (doc, _) = open(p)?;
                        Some((p.clone(), doc.sent.clone().filter(|_| !doc.dirty)?))
                    })
                    .collect();
                &server_heard
            }
        };
        first_stale(&targets, &now, baseline)
    }
}

fn spawn_apply(per_file: Vec<(Uri, Vec<lsp_types::TextEdit>)>, encoding: Encoding, cx: &mut App) {
    cx.spawn(async move |cx| {
        for (uri, edits) in per_file {
            let Some(path) = uri_to_path(&uri) else {
                continue;
            };
            let target = cx.update(|cx| {
                let store = cx.try_global::<LspStore>()?;
                let doc = store.docs.get(&path)?;
                let input = doc.owner.upgrade()?;
                let window = doc
                    .window
                    .or_else(|| cx.active_window())
                    .or_else(|| cx.windows().first().copied())?;
                Some((input, window))
            });
            match target {
                Some((input, window)) => {
                    let _ = window.update(cx, |_, window, cx| {
                        input.update(cx, |state, cx| {
                            let text = state.text().clone();
                            let edits = convert::edits_to_editor(&text, &edits, encoding);
                            state.apply_lsp_edits(&edits, window, cx);
                        });
                    });
                }
                None => {
                    let written = cx
                        .background_spawn(async move {
                            let text = std::fs::read_to_string(&path)?;
                            let new = convert::apply_edits(&text, &edits, encoding);
                            crate::core::config::write_atomic(&path, new.as_bytes())?;
                            anyhow::Ok(())
                        })
                        .await;
                    if let Err(e) = written {
                        log::warn!("lsp: could not apply an edit to {}: {e:#}", uri.as_str());
                    }
                }
            }
        }
    })
    .detach();
}

/// A workspace edit's text edits, file by file. Creating, renaming and
/// deleting files are not supported (tty7 does not advertise them).
pub(crate) fn workspace_edit_files(
    edit: lsp_types::WorkspaceEdit,
) -> Vec<(Uri, Vec<lsp_types::TextEdit>)> {
    use lsp_types::{DocumentChangeOperation, DocumentChanges, OneOf};
    let mut out: Vec<(Uri, Vec<lsp_types::TextEdit>)> = Vec::new();
    let mut push =
        |uri: Uri, edits: Vec<lsp_types::TextEdit>| match out.iter_mut().find(|(u, _)| *u == uri) {
            Some((_, existing)) => existing.extend(edits),
            None => out.push((uri, edits)),
        };
    let text_edits = |edits: Vec<OneOf<lsp_types::TextEdit, lsp_types::AnnotatedTextEdit>>| {
        edits
            .into_iter()
            .map(|e| match e {
                OneOf::Left(e) => e,
                OneOf::Right(a) => a.text_edit,
            })
            .collect::<Vec<_>>()
    };
    match edit.document_changes {
        Some(DocumentChanges::Edits(edits)) => {
            for e in edits {
                push(e.text_document.uri, text_edits(e.edits));
            }
        }
        Some(DocumentChanges::Operations(ops)) => {
            for op in ops {
                if let DocumentChangeOperation::Edit(e) = op {
                    push(e.text_document.uri, text_edits(e.edits));
                }
            }
        }
        None => {
            let mut changes: Vec<_> = edit.changes.unwrap_or_default().into_iter().collect();
            changes.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
            for (uri, edits) in changes {
                push(uri, edits);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;

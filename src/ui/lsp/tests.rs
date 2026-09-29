use std::collections::HashMap;

use lsp_types::{
    AnnotatedTextEdit, DocumentChangeOperation, DocumentChanges, OneOf,
    OptionalVersionedTextDocumentIdentifier, Position, Range, ResourceOp, TextDocumentEdit,
    TextEdit, WorkspaceEdit,
};

use super::*;

/// A `file://` URI spelled out, not built from a path: these tests are
/// about grouping URIs, and `/a.rs` is not an absolute path on Windows.
fn uri(path: &str) -> Uri {
    format!("file://{path}").parse().unwrap()
}

fn edit(line: u32, text: &str) -> TextEdit {
    TextEdit {
        range: Range::new(Position::new(line, 0), Position::new(line, 1)),
        new_text: text.into(),
    }
}

#[cfg(unix)]
#[test]
fn file_uris_round_trip_including_spaces_and_non_ascii() {
    let path = Path::new("/tmp/a dir/ü.rs");
    let u = path_to_uri(path).unwrap();
    assert_eq!(u.as_str(), "file:///tmp/a%20dir/%C3%BC.rs");
    assert_eq!(uri_to_path(&u).as_deref(), Some(path));
    let web: Uri = "https://docs.rs/x".parse().unwrap();
    assert_eq!(uri_to_path(&web), None);
}

#[test]
fn a_workspace_edit_is_grouped_by_file_from_either_shape() {
    let mut changes = HashMap::new();
    changes.insert(uri("/b.rs"), vec![edit(1, "b")]);
    changes.insert(uri("/a.rs"), vec![edit(0, "a")]);
    let files = workspace_edit_files(WorkspaceEdit {
        changes: Some(changes),
        ..Default::default()
    });
    let names: Vec<&str> = files.iter().map(|(u, _)| u.as_str()).collect();
    assert_eq!(names, ["file:///a.rs", "file:///b.rs"]);

    let doc_edit = |path: &str, edits: Vec<OneOf<TextEdit, AnnotatedTextEdit>>| TextDocumentEdit {
        text_document: OptionalVersionedTextDocumentIdentifier {
            uri: uri(path),
            version: None,
        },
        edits,
    };
    let files = workspace_edit_files(WorkspaceEdit {
        document_changes: Some(DocumentChanges::Operations(vec![
            DocumentChangeOperation::Edit(doc_edit("/a.rs", vec![OneOf::Left(edit(0, "x"))])),
            DocumentChangeOperation::Op(ResourceOp::Create(lsp_types::CreateFile {
                uri: uri("/new.rs"),
                options: None,
                annotation_id: None,
            })),
            DocumentChangeOperation::Edit(doc_edit(
                "/a.rs",
                vec![OneOf::Right(AnnotatedTextEdit {
                    text_edit: edit(2, "y"),
                    annotation_id: "rename".into(),
                })],
            )),
        ])),
        ..Default::default()
    });
    assert_eq!(
        files.len(),
        1,
        "edits to one file are merged; resource ops skipped"
    );
    let texts: Vec<&str> = files[0].1.iter().map(|e| e.new_text.as_str()).collect();
    assert_eq!(texts, ["x", "y"]);
}

#[test]
fn a_rename_across_a_file_applies_every_occurrence() {
    // What a server sends for renaming `foo` to `renamed`, applied to a file
    // with no buffer open.
    let text = "fn foo() {}\nfn main() { foo(); /* 🎉 */ foo(); }\n";
    let at = |line, start, end| TextEdit {
        range: Range::new(Position::new(line, start), Position::new(line, end)),
        new_text: "renamed".into(),
    };
    let edits = vec![at(1, 12, 15), at(0, 3, 6), at(1, 28, 31)];
    assert_eq!(
        convert::apply_edits(text, &edits, Encoding::Utf16),
        "fn renamed() {}\nfn main() { renamed(); /* 🎉 */ renamed(); }\n"
    );
}

/// Drives a real language server through the whole client: spawn,
/// `initialize` behind the gate, `didOpen`, diagnostics arriving as an
/// event, a hover request, and a clean `shutdown`/`exit` with the process
/// reaped. clangd, because it ships with Xcode's command line tools and needs
/// no project to index; the file's type error is what it has to find.
///
/// `cargo test -p tty7 --bin tty7-app lsp_smoke -- --ignored`
#[test]
#[ignore = "needs clangd on PATH"]
fn lsp_smoke_a_real_server_publishes_diagnostics_and_shuts_down() {
    use std::time::Duration;

    let spec = servers::spec_for_language("c").unwrap();
    let Some((program, args)) =
        servers::resolve(spec, &std::env::var_os("PATH").unwrap_or_default())
    else {
        eprintln!("clangd not on PATH; skipping");
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let file = root.join("main.c");
    let text = "int main(void) {\n    int x = \"not a number\";\n    return x;\n}\n";
    std::fs::write(&file, text).unwrap();

    let (client, events) = LspClient::spawn("clangd", &program, args, &root).unwrap();
    let within =
        |secs: u64, fut: std::pin::Pin<Box<dyn std::future::Future<Output = Option<Value>>>>| {
            smol::block_on(smol::future::or(fut, async move {
                smol::Timer::after(Duration::from_secs(secs)).await;
                None
            }))
        };

    let init = client.initialize(initialize_params(&root, spec));
    let result = within(30, Box::pin(async move { init.await.ok() })).expect("initialize answered");
    let init: lsp_types::InitializeResult = serde_json::from_value(result).unwrap();
    assert!(init.capabilities.hover_provider.is_some());
    client.notify_raw("initialized", json!({}));
    client.open_gate();

    let uri = path_to_uri(&file).unwrap();
    client.notify::<lsp_types::notification::DidOpenTextDocument>(
        lsp_types::DidOpenTextDocumentParams {
            text_document: lsp_types::TextDocumentItem::new(
                uri.clone(),
                "c".into(),
                0,
                text.into(),
            ),
        },
    );

    let events2 = events.clone();
    let published = within(
        60,
        Box::pin(async move {
            while let Ok(event) = events2.recv().await {
                match event {
                    ServerEvent::Notification { method, params }
                        if method == "textDocument/publishDiagnostics" =>
                    {
                        return Some(params);
                    }
                    _ => {}
                }
            }
            None
        }),
    )
    .expect("diagnostics published");
    let params: lsp_types::PublishDiagnosticsParams = serde_json::from_value(published).unwrap();
    assert_eq!(params.uri, uri);
    let (errors, _) = convert::count_problems(&params.diagnostics);
    assert!(errors >= 1, "{:?}", params.diagnostics);
    assert!(params.diagnostics.iter().any(|d| d.range.start.line == 1));

    let hover = client.request::<lsp_types::request::HoverRequest>(lsp_types::HoverParams {
        text_document_position_params: lsp_types::TextDocumentPositionParams::new(
            lsp_types::TextDocumentIdentifier::new(uri),
            Position::new(2, 11),
        ),
        work_done_progress_params: Default::default(),
    });
    let hovered = within(
        30,
        Box::pin(async move { hover.await.ok().map(|h| serde_json::to_value(h).unwrap()) }),
    );
    assert!(
        hovered.is_some_and(|h| !h.is_null()),
        "hover over `x` says something"
    );

    // The outline, the references to `x`, and signature help inside a call
    // all come back in shapes the editor reads.
    let doc_id = || lsp_types::TextDocumentIdentifier::new(path_to_uri(&file).unwrap());
    let symbols = client.request::<lsp_types::request::DocumentSymbolRequest>(
        lsp_types::DocumentSymbolParams {
            text_document: doc_id(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        },
    );
    let outline = within(
        30,
        Box::pin(async move {
            symbols
                .await
                .ok()
                .flatten()
                .map(|s| serde_json::to_value(s).unwrap())
        }),
    )
    .map(|v| {
        symbols::outline(
            serde_json::from_value(v).unwrap(),
            &gpui_component::Rope::from(text),
            Encoding::Utf16,
        )
    })
    .expect("an outline");
    assert!(
        outline.symbols.iter().any(|s| s.name == "main"),
        "{outline:?}"
    );

    let references = client.request::<lsp_types::request::References>(lsp_types::ReferenceParams {
        text_document_position: lsp_types::TextDocumentPositionParams::new(
            doc_id(),
            Position::new(2, 11),
        ),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: lsp_types::ReferenceContext {
            include_declaration: true,
        },
    });
    let found = within(
        30,
        Box::pin(async move {
            references
                .await
                .ok()
                .flatten()
                .map(|r| serde_json::to_value(r).unwrap())
        }),
    )
    .expect("references");
    let found: Vec<lsp_types::Location> = serde_json::from_value(found).unwrap();
    assert!(
        found.len() >= 2,
        "the declaration and the use of x: {found:?}"
    );

    let shutdown = client.request_raw("shutdown", Value::Null);
    within(10, Box::pin(async move { shutdown.await.ok() }));
    client.notify_raw("exit", Value::Null);
    let mut exited = false;
    for _ in 0..50 {
        if client.reap_if_exited() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    client.kill();
    assert!(exited, "clangd exits on `exit`");
}

/// A store with one running server on the far end of two pipes, and one
/// document owned by `owner`. Returns the path and what the server reads.
mod harness {
    use std::io::Read as _;

    use gpui::{Entity, TestAppContext, WindowHandle};
    use gpui_component::input::InputState;

    use super::super::*;
    use crate::ui::lsp::rpc::FrameReader;

    pub(super) struct FakeServer {
        frames: FrameReader,
        from_client: std::io::PipeReader,
        _to_client: std::io::PipeWriter,
    }

    impl FakeServer {
        pub(super) fn next(&mut self) -> Value {
            let mut chunk = [0u8; 4096];
            loop {
                if let Some(body) = self.frames.next_frame() {
                    return serde_json::from_slice(&body).unwrap();
                }
                let n = self.from_client.read(&mut chunk).unwrap();
                assert!(n > 0, "the client closed its end");
                self.frames.push(&chunk[..n]);
            }
        }
    }

    pub(super) struct Setup {
        pub(super) window: WindowHandle<InputState>,
        pub(super) owner: Entity<InputState>,
        pub(super) other: Entity<InputState>,
        pub(super) path: PathBuf,
        pub(super) server: FakeServer,
    }

    pub(super) fn setup(cx: &mut TestAppContext) -> Setup {
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(crate::core::config::Config::default());
        });
        let window = cx.add_window(|window, cx| {
            InputState::new(window, cx)
                .code_editor("rust")
                .multi_line(true)
                .default_value("fn foo() {}\n")
        });
        let owner = window.root(cx).unwrap();
        let other = window
            .update(cx, |_, window, cx| {
                cx.new(|cx| {
                    InputState::new(window, cx)
                        .code_editor("rust")
                        .multi_line(true)
                        .default_value("fn foo() { /* the other window's edit */ }\n")
                })
            })
            .unwrap();
        let path = test_path("/tmp/tty7-lsp-test/src/lib.rs");
        let (client_reads, server_writes) = std::io::pipe().unwrap();
        let (server_reads, client_writes) = std::io::pipe().unwrap();
        let (client, _events) =
            LspClient::from_streams("fake", client_reads, client_writes).unwrap();
        client.open_gate();
        let key = ServerKey {
            name: "rust-analyzer",
            root: test_path("/tmp/tty7-lsp-test"),
        };
        cx.update(|cx| {
            LspStore::ensure(cx);
            let text = owner.read(cx).text().clone();
            let store = cx.global_mut::<LspStore>();
            store.servers.insert(
                key.clone(),
                Server {
                    spec: servers::spec_for_language("rust").unwrap(),
                    label: "fake".into(),
                    generation: 1,
                    phase: Phase::Running,
                    client: Some(client),
                    caps: Rc::new(lsp_types::ServerCapabilities::default()),
                    encoding: Encoding::Utf16,
                    crashes: Vec::new(),
                    idle_seq: 0,
                    _tasks: Vec::new(),
                },
            );
            store.docs.insert(
                path.clone(),
                Doc {
                    uri: path_to_uri(&path).unwrap(),
                    language_id: "rust",
                    server: key,
                    version: 0,
                    owner: owner.downgrade(),
                    owner_id: owner.entity_id(),
                    app_id: owner.entity_id(),
                    app: WeakEntity::new_invalid(),
                    window: Some(window.into()),
                    sent: Some(text),
                    dirty: false,
                    change_seq: 0,
                    symbols_seq: 0,
                    diagnostics: Vec::new(),
                    problems: (0, 0),
                },
            );
        });
        Setup {
            window,
            owner,
            other,
            path,
            server: FakeServer {
                frames: FrameReader::default(),
                from_client: server_reads,
                _to_client: server_writes,
            },
        }
    }
}

/// The same file open in a second window is a second buffer, maybe with
/// other text. Its requests are refused before anything is sent, so the
/// server's copy stays the owner's.
#[gpui::test]
fn a_buffer_that_does_not_own_the_document_never_syncs(cx: &mut gpui::TestAppContext) {
    let mut s = harness::setup(cx);
    cx.update(|cx| {
        let other_text = s.other.read(cx).text().clone();
        let refused = LspStore::context(
            &s.path,
            s.other.entity_id(),
            Freshen::Text(&other_text),
            None,
            cx,
        );
        assert!(refused.is_none(), "a non-owner gets no server");

        let owner_text = gpui_component::Rope::from("fn foo() { owner(); }\n");
        let allowed = LspStore::context(
            &s.path,
            s.owner.entity_id(),
            Freshen::Text(&owner_text),
            None,
            cx,
        );
        assert!(allowed.is_some());
    });
    // The first thing the server hears is the owner's change: the other
    // buffer's text never went out.
    let change = s.server.next();
    assert_eq!(change["method"], "textDocument/didChange");
    assert_eq!(
        change["params"]["contentChanges"][0]["text"],
        "fn foo() { owner(); }\n"
    );
}

#[test]
fn an_edit_is_stale_when_a_file_it_touches_changed_opened_or_closed() {
    let rope = gpui_component::Rope::from;
    let (a, b, c) = (
        PathBuf::from("/a"),
        PathBuf::from("/b"),
        PathBuf::from("/c"),
    );
    let baseline = HashMap::from([(a.clone(), rope("x")), (b.clone(), rope("y"))]);
    let targets = vec![a.clone(), c.clone()];
    // `a` unchanged, `c` on disk then and now.
    let now = HashMap::from([(a.clone(), rope("x")), (b.clone(), rope("typed"))]);
    assert_eq!(
        first_stale(&targets, &now, &baseline),
        None,
        "only targets count"
    );
    // `a` typed in.
    let now = HashMap::from([(a.clone(), rope("x!"))]);
    assert_eq!(first_stale(&targets, &now, &baseline), Some(a.clone()));
    // `c` opened since.
    let now = HashMap::from([(a.clone(), rope("x")), (c.clone(), rope("z"))]);
    assert_eq!(first_stale(&targets, &now, &baseline), Some(c.clone()));
    // `a` closed since.
    assert_eq!(first_stale(&targets, &HashMap::new(), &baseline), Some(a));
}

/// A rename or code action's edit, answered against a text the buffer has
/// since moved on from, is dropped whole; answered against the text it
/// still holds, it is applied.
#[gpui::test]
fn a_workspace_edit_for_a_changed_buffer_is_not_applied(cx: &mut gpui::TestAppContext) {
    let s = harness::setup(cx);
    let rename = |cx: &mut gpui::App, baseline| {
        let mut changes = HashMap::new();
        changes.insert(
            path_to_uri(&s.path).unwrap(),
            vec![TextEdit {
                range: Range::new(Position::new(0, 3), Position::new(0, 6)),
                new_text: "bar".into(),
            }],
        );
        let edit = WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        };
        apply_workspace_edit(edit, Encoding::Utf16, Some(baseline), cx)
    };

    let baseline = cx.update(|cx| LspStore::snapshot(cx));
    // Typed in after the request went out.
    s.window
        .update(cx, |state, window, cx| {
            state.set_value("fn foo() { typed }\n", window, cx)
        })
        .unwrap();
    assert!(!cx.update(|cx| rename(cx, baseline)));
    cx.run_until_parked();
    assert_eq!(
        cx.update(|cx| s.owner.read(cx).text().to_string()),
        "fn foo() { typed }\n"
    );

    let baseline = cx.update(|cx| LspStore::snapshot(cx));
    assert!(cx.update(|cx| rename(cx, baseline)));
    cx.run_until_parked();
    assert_eq!(
        cx.update(|cx| s.owner.read(cx).text().to_string()),
        "fn bar() { typed }\n"
    );
}

/// Turning the setting back on asks every window for its open files again,
/// which is what starts their servers.
#[gpui::test]
fn turning_lsp_back_on_offers_every_window_again(cx: &mut gpui::TestAppContext) {
    use std::cell::Cell;
    cx.update(|cx| cx.set_global(crate::core::config::Config::default()));
    let asked = Rc::new(Cell::new(0));
    let window_gone = Rc::new(Cell::new(false));
    cx.update(|cx| {
        let asked = asked.clone();
        LspStore::register_window(
            EntityId::from(1u64),
            Rc::new(move |_| {
                asked.set(asked.get() + 1);
                true
            }),
            cx,
        );
        let gone = window_gone.clone();
        LspStore::register_window(
            EntityId::from(2u64),
            Rc::new(move |_| {
                gone.set(true);
                false
            }),
            cx,
        );
    });
    let set = |cx: &mut gpui::TestAppContext, on: bool| {
        cx.update(|cx| {
            cx.update_global::<crate::core::config::Config, _>(|c, _| c.editor_lsp = on)
        });
        cx.run_until_parked();
    };
    set(cx, false);
    assert_eq!(asked.get(), 0, "turning it off asks nobody");
    set(cx, true);
    assert_eq!(asked.get(), 1);
    assert!(window_gone.get());
    // A closed window is forgotten; one still open is asked on every turn-on.
    set(cx, false);
    set(cx, true);
    assert_eq!(asked.get(), 2);
    assert_eq!(cx.update(|cx| cx.global::<LspStore>().windows.len()), 1);
}

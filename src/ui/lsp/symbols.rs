//! A server's `textDocument/documentSymbol`, as the editor's outline.
//!
//! The editor already builds an outline from its own parse tree; a server's
//! is better where there is one — it knows `impl` blocks from macros, and
//! languages the tree queries do not cover. It replaces the tree's through
//! `Tty7App::editor_set_document_symbols`, refreshed a moment after the
//! server hears about each edit.

use gpui_component::Rope;
use lsp_types::{DocumentSymbol, DocumentSymbolResponse, SymbolInformation};

use super::convert::{self, Encoding};
use crate::ui::code_editor::outline::{Outline, SymbolKind, SymbolNode};

fn kind(kind: lsp_types::SymbolKind) -> SymbolKind {
    let n = serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    SymbolKind::from_lsp(n as u32)
}

/// The answer as an outline, its positions converted to the editor's
/// columns against `text` — the text the server answered about.
pub(crate) fn outline(response: DocumentSymbolResponse, text: &Rope, enc: Encoding) -> Outline {
    let nodes = match response {
        DocumentSymbolResponse::Nested(symbols) => {
            symbols.into_iter().map(|s| node(s, text, enc)).collect()
        }
        DocumentSymbolResponse::Flat(symbols) => nest_flat(symbols, text, enc),
    };
    Outline::from_nodes(nodes)
}

fn node(symbol: DocumentSymbol, text: &Rope, enc: Encoding) -> SymbolNode {
    let range = convert::range_to_editor(text, symbol.range, enc);
    SymbolNode {
        name: symbol.name,
        kind: kind(symbol.kind),
        range: range.start..range.end,
        selection: convert::lsp_to_editor(text, symbol.selection_range.start, enc),
        children: symbol
            .children
            .unwrap_or_default()
            .into_iter()
            .map(|c| node(c, text, enc))
            .collect(),
    }
}

/// The older, flat answer: nested by which range holds which, since the
/// `containerName` it carries is a name, not a reference.
fn nest_flat(mut symbols: Vec<SymbolInformation>, text: &Rope, enc: Encoding) -> Vec<SymbolNode> {
    let key = |p: lsp_types::Position| (p.line, p.character);
    // Outer before inner: by start, then the longer range first.
    symbols.sort_by(|a, b| {
        key(a.location.range.start)
            .cmp(&key(b.location.range.start))
            .then(key(b.location.range.end).cmp(&key(a.location.range.end)))
    });
    let contains = |outer: &lsp_types::Range, inner: &lsp_types::Range| {
        key(outer.start) <= key(inner.start) && key(inner.end) <= key(outer.end)
    };
    // A stack of open containers: (their server range, the node).
    let mut stack: Vec<(lsp_types::Range, SymbolNode)> = Vec::new();
    let mut roots = Vec::new();
    fn pop_into(stack: &mut Vec<(lsp_types::Range, SymbolNode)>, roots: &mut Vec<SymbolNode>) {
        if let Some((_, done)) = stack.pop() {
            match stack.last_mut() {
                Some((_, parent)) => parent.children.push(done),
                None => roots.push(done),
            }
        }
    }
    for s in symbols {
        let range = s.location.range;
        while stack
            .last()
            .is_some_and(|(outer, _)| !contains(outer, &range))
        {
            pop_into(&mut stack, &mut roots);
        }
        let editor = convert::range_to_editor(text, range, enc);
        stack.push((
            range,
            SymbolNode {
                name: s.name,
                kind: kind(s.kind),
                range: editor.start..editor.end,
                selection: editor.start,
                children: Vec::new(),
            },
        ));
    }
    while !stack.is_empty() {
        pop_into(&mut stack, &mut roots);
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Location, Position, Range};

    fn range(a: u32, b: u32, c: u32, d: u32) -> Range {
        Range::new(Position::new(a, b), Position::new(c, d))
    }

    #[test]
    #[allow(deprecated)]
    fn a_flat_answer_is_nested_by_range() {
        let text = Rope::from("struct A {\n  f: u8,\n}\nfn g() {}\n");
        let uri: lsp_types::Uri = "file:///a.rs".parse().unwrap();
        let info = |name: &str, kind, r| SymbolInformation {
            name: name.into(),
            kind,
            tags: None,
            deprecated: None,
            location: Location::new(uri.clone(), r),
            container_name: None,
        };
        let outline = outline(
            DocumentSymbolResponse::Flat(vec![
                info("g", lsp_types::SymbolKind::FUNCTION, range(3, 0, 3, 9)),
                info("f", lsp_types::SymbolKind::FIELD, range(1, 2, 1, 7)),
                info("A", lsp_types::SymbolKind::STRUCT, range(0, 0, 2, 1)),
            ]),
            &text,
            Encoding::Utf16,
        );
        let shape: Vec<(&str, usize, SymbolKind)> = outline
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), s.depth, s.kind))
            .collect();
        assert_eq!(
            shape,
            [
                ("A", 0, SymbolKind::Struct),
                ("f", 1, SymbolKind::Field),
                ("g", 0, SymbolKind::Function),
            ]
        );
    }

    #[test]
    #[allow(deprecated)]
    fn a_nested_answer_keeps_its_shape_in_editor_columns() {
        let text = Rope::from("mod 🎉m {\n    fn f() {}\n}\n");
        let sym = |name: &str, kind, r: Range, sel: Range, children| DocumentSymbol {
            name: name.into(),
            detail: None,
            kind,
            tags: None,
            deprecated: None,
            range: r,
            selection_range: sel,
            children,
        };
        let outline = outline(
            DocumentSymbolResponse::Nested(vec![sym(
                "m",
                lsp_types::SymbolKind::MODULE,
                range(0, 0, 2, 1),
                // `m` after the emoji: UTF-16 column 6, char column 5.
                range(0, 6, 0, 7),
                Some(vec![sym(
                    "f",
                    lsp_types::SymbolKind::FUNCTION,
                    range(1, 4, 1, 14),
                    range(1, 7, 1, 8),
                    None,
                )]),
            )]),
            &text,
            Encoding::Utf16,
        );
        assert_eq!(outline.symbols.len(), 2);
        assert_eq!(outline.symbols[0].selection, Position::new(0, 5));
        assert_eq!(outline.symbols[1].parent, Some(0));
        assert_eq!(outline.symbols[1].kind, SymbolKind::Function);
    }
}

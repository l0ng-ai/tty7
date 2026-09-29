//! A file's symbols — what Go to Symbol lists and the breadcrumbs walk.
//!
//! Built from the tree the editor already parsed for highlighting
//! ([`gpui_component::input::InputState::syntax_tree`]), with one small query
//! per language saying which nodes are symbols and where their names are. No
//! second parse, and nothing to install.
//!
//! An [`Outline`] is plain data, so it need not come from tree-sitter at all:
//! [`Outline::from_nodes`] takes the same nested shape a language server's
//! `textDocument/documentSymbol` answers with, and a buffer given one through
//! `Tty7App::editor_set_document_symbols` shows that instead of the tree's.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, LazyLock, Mutex};

use gpui_component::Rope;
use gpui_component::highlighter::LanguageRegistry;
use gpui_component::highlighter::tree_sitter::{self, Query, QueryCursor, StreamingIterator as _};
use gpui_component::input::{Position, RopeExt as _};

/// What a symbol is — the shapes the picker and the breadcrumbs tell apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SymbolKind {
    Module,
    Namespace,
    Class,
    Struct,
    Enum,
    Interface,
    Trait,
    Impl,
    Function,
    Method,
    Constructor,
    Constant,
    Variable,
    Field,
    Type,
    Macro,
    Heading,
}

impl SymbolKind {
    /// The capture suffix a query names this kind by (`@item.function`).
    fn from_capture(suffix: &str) -> Option<Self> {
        Some(match suffix {
            "module" => Self::Module,
            "namespace" => Self::Namespace,
            "class" => Self::Class,
            "struct" => Self::Struct,
            "enum" => Self::Enum,
            "interface" => Self::Interface,
            "trait" => Self::Trait,
            "impl" => Self::Impl,
            "function" => Self::Function,
            "method" => Self::Method,
            "constructor" => Self::Constructor,
            "constant" => Self::Constant,
            "variable" => Self::Variable,
            "field" => Self::Field,
            "type" => Self::Type,
            "macro" => Self::Macro,
            "heading" => Self::Heading,
            _ => return None,
        })
    }

    /// LSP's `SymbolKind` number, for a language server's answer. Kinds with
    /// no counterpart here (a file, a package, an event…) fold into the
    /// nearest one that draws the same.
    pub(crate) fn from_lsp(kind: u32) -> Self {
        match kind {
            1 | 2 | 4 => Self::Module, // File, Module, Package
            3 => Self::Namespace,
            5 => Self::Class,
            6 => Self::Method,
            7 | 8 => Self::Field, // Property, Field
            9 => Self::Constructor,
            10 => Self::Enum,
            11 => Self::Interface,
            12 => Self::Function,
            13 => Self::Variable,
            14 | 22 => Self::Constant, // Constant, EnumMember
            23 => Self::Struct,
            26 => Self::Type, // TypeParameter
            _ => Self::Variable,
        }
    }

    /// Whether a function directly inside this is a method of it.
    fn holds_methods(self) -> bool {
        matches!(
            self,
            Self::Class | Self::Struct | Self::Enum | Self::Interface | Self::Trait | Self::Impl
        )
    }

    /// The short tag the picker shows beside a name. Code's own words, the
    /// same in every locale, like the keywords they stand for.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Module => "mod",
            Self::Namespace => "namespace",
            Self::Class => "class",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Interface => "interface",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Function => "fn",
            Self::Method => "method",
            Self::Constructor => "ctor",
            Self::Constant => "const",
            Self::Variable => "var",
            Self::Field => "field",
            Self::Type => "type",
            Self::Macro => "macro",
            Self::Heading => "#",
        }
    }
}

/// One symbol, flattened into its outline.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// The whole item, 0-based, in the editor's own [`Position`]s.
    pub range: Range<Position>,
    /// Where a jump to it lands: its name.
    pub selection: Position,
    /// How many symbols it sits inside.
    pub depth: usize,
    /// The symbol it sits directly inside, as an index into the outline.
    pub parent: Option<usize>,
}

/// A nested symbol, the shape an LSP `DocumentSymbol` has — what an outline
/// from anywhere other than the tree is built out of.
#[derive(Clone, Debug)]
pub(crate) struct SymbolNode {
    pub name: String,
    pub kind: SymbolKind,
    pub range: Range<Position>,
    pub selection: Position,
    pub children: Vec<SymbolNode>,
}

/// A file's symbols in document order, every container before what it holds.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Outline {
    pub symbols: Vec<Symbol>,
}

fn key(p: Position) -> (u32, u32) {
    (p.line, p.character)
}

impl Outline {
    /// Flattens a nested answer (a language server's `documentSymbol`).
    pub(crate) fn from_nodes(nodes: Vec<SymbolNode>) -> Self {
        fn walk(
            nodes: Vec<SymbolNode>,
            parent: Option<usize>,
            depth: usize,
            out: &mut Vec<Symbol>,
        ) {
            let mut nodes = nodes;
            nodes.sort_by_key(|n| key(n.range.start));
            for node in nodes {
                let ix = out.len();
                out.push(Symbol {
                    name: node.name,
                    kind: node.kind,
                    range: node.range,
                    selection: node.selection,
                    depth,
                    parent,
                });
                walk(node.children, Some(ix), depth + 1, out);
            }
        }
        let mut symbols = Vec::new();
        walk(nodes, None, 0, &mut symbols);
        Self { symbols }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    /// The symbols `pos` is inside, outermost first — the breadcrumbs' trail.
    pub(crate) fn chain_at(&self, pos: Position) -> Vec<usize> {
        let at = key(pos);
        // Children follow their parent and siblings never overlap, so the
        // last symbol that holds the position is the innermost one.
        let Some(innermost) = self
            .symbols
            .iter()
            .rposition(|s| key(s.range.start) <= at && at <= key(s.range.end))
        else {
            return Vec::new();
        };
        let mut chain = vec![innermost];
        let mut cur = innermost;
        while let Some(parent) = self.symbols[cur].parent {
            chain.push(parent);
            cur = parent;
        }
        chain.reverse();
        chain
    }

    /// The names of what symbol `ix` sits inside, outermost first.
    pub(crate) fn container_path(&self, ix: usize) -> Vec<&str> {
        let mut names = Vec::new();
        let mut cur = self.symbols.get(ix).and_then(|s| s.parent);
        while let Some(p) = cur {
            names.push(self.symbols[p].name.as_str());
            cur = self.symbols[p].parent;
        }
        names.reverse();
        names
    }
}

// ---- tree-sitter ----
//
// Captures: `@item.<kind>` on the node that is the symbol, `@name` on its name
// (several are joined with a space), `@body` on an impl's block — which names
// the symbol by everything before it, `impl<T> Display for Wrapper<T>` — and
// `@receiver` on a Go method's receiver list, which goes in front of the name
// as `(*Server).Serve`. When two patterns claim one node the earlier wins, so
// a catch-all belongs last.

const RUST: &str = r#"
(mod_item name: (identifier) @name) @item.module
(struct_item name: (type_identifier) @name) @item.struct
(union_item name: (type_identifier) @name) @item.struct
(enum_item name: (type_identifier) @name) @item.enum
(trait_item name: (type_identifier) @name) @item.trait
(impl_item body: (declaration_list) @body) @item.impl
(function_item name: (identifier) @name) @item.function
(function_signature_item name: (identifier) @name) @item.function
(const_item name: (identifier) @name) @item.constant
(static_item name: (identifier) @name) @item.constant
(type_item name: (type_identifier) @name) @item.type
(macro_definition name: (identifier) @name) @item.macro
"#;

const GO: &str = r#"
(function_declaration name: (identifier) @name) @item.function
(method_declaration receiver: (parameter_list) @receiver name: (field_identifier) @name) @item.method
(type_spec name: (type_identifier) @name type: (struct_type)) @item.struct
(type_spec name: (type_identifier) @name type: (interface_type)) @item.interface
(type_spec name: (type_identifier) @name) @item.type
(const_spec name: (identifier) @name) @item.constant
"#;

const PYTHON: &str = r#"
(class_definition name: (identifier) @name) @item.class
(function_definition name: (identifier) @name) @item.function
"#;

const JAVASCRIPT: &str = r#"
(class_declaration name: (_) @name) @item.class
(function_declaration name: (identifier) @name) @item.function
(generator_function_declaration name: (identifier) @name) @item.function
(method_definition name: (_) @name) @item.method
(variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @item.function
"#;

const TYPESCRIPT_EXTRA: &str = r#"
(abstract_class_declaration name: (_) @name) @item.class
(interface_declaration name: (_) @name) @item.interface
(type_alias_declaration name: (_) @name) @item.type
(enum_declaration name: (_) @name) @item.enum
(internal_module name: (_) @name) @item.module
(function_signature name: (_) @name) @item.function
(method_signature name: (_) @name) @item.method
(abstract_method_signature name: (_) @name) @item.method
"#;

const C: &str = r#"
(function_definition declarator: (function_declarator declarator: (_) @name)) @item.function
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (_) @name))) @item.function
(struct_specifier name: (_) @name body: (_)) @item.struct
(union_specifier name: (_) @name body: (_)) @item.struct
(enum_specifier name: (_) @name body: (_)) @item.enum
(type_definition declarator: (type_identifier) @name) @item.type
"#;

const CPP_EXTRA: &str = r#"
(namespace_definition name: (_) @name) @item.namespace
(class_specifier name: (_) @name body: (_)) @item.class
(function_definition declarator: (reference_declarator (function_declarator declarator: (_) @name))) @item.function
(field_declaration declarator: (function_declarator declarator: (_) @name)) @item.method
(alias_declaration name: (type_identifier) @name) @item.type
"#;

const JAVA: &str = r#"
(class_declaration name: (identifier) @name) @item.class
(interface_declaration name: (identifier) @name) @item.interface
(enum_declaration name: (identifier) @name) @item.enum
(record_declaration name: (identifier) @name) @item.class
(annotation_type_declaration name: (identifier) @name) @item.interface
(method_declaration name: (identifier) @name) @item.method
(constructor_declaration name: (identifier) @name) @item.constructor
"#;

const MARKDOWN: &str = r#"
(section (atx_heading (inline) @name)) @item.heading
(setext_heading (paragraph (inline) @name)) @item.heading
"#;

const RUBY: &str = r#"
(class name: (_) @name) @item.class
(module name: (_) @name) @item.module
(method name: (_) @name) @item.method
(singleton_method name: (_) @name) @item.method
"#;

const BASH: &str = r#"
(function_definition name: (word) @name) @item.function
"#;

/// The query for a language the editor highlights, or `None` for one it has
/// no outline for.
fn query_source(language: &str) -> Option<String> {
    Some(match language {
        "rust" => RUST.into(),
        "go" => GO.into(),
        "python" => PYTHON.into(),
        "javascript" => JAVASCRIPT.into(),
        "typescript" | "tsx" => format!("{JAVASCRIPT}{TYPESCRIPT_EXTRA}"),
        "c" => C.into(),
        "cpp" => format!("{C}{CPP_EXTRA}"),
        "java" => JAVA.into(),
        "markdown" => MARKDOWN.into(),
        "ruby" => RUBY.into(),
        "bash" => BASH.into(),
        _ => return None,
    })
}

/// Compiled once per language per process. A query that does not compile
/// against its grammar is a bug here, not in the file: it is logged once and
/// the language goes without an outline.
fn query_for(language: &str) -> Option<Arc<Query>> {
    static QUERIES: LazyLock<Mutex<HashMap<String, Option<Arc<Query>>>>> =
        LazyLock::new(Default::default);
    let mut queries = QUERIES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(q) = queries.get(language) {
        return q.clone();
    }
    let compiled = query_source(language).and_then(|source| {
        let config = LanguageRegistry::singleton().language(language)?;
        Query::new(&config.language, &source)
            .map_err(|e| log::warn!("outline: the {language} query does not compile: {e}"))
            .ok()
            .map(Arc::new)
    });
    queries.insert(language.to_string(), compiled.clone());
    compiled
}

/// Whether `language` has an outline at all.
pub(crate) fn supports(language: &str) -> bool {
    query_source(language).is_some()
}

/// The longest name a symbol shows; an impl header with a where clause can
/// otherwise run the width of the screen.
const NAME_MAX_CHARS: usize = 80;

fn collapse(text: &str) -> String {
    let mut out = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() > NAME_MAX_CHARS {
        out = out.chars().take(NAME_MAX_CHARS - 1).collect::<String>() + "…";
    }
    out
}

fn text_of(text: &Rope, range: Range<usize>) -> String {
    let end = range.end.min(text.len());
    let start = range.start.min(end);
    text.slice(start..end).to_string()
}

/// The outline of `text`, as parsed into `tree`.
pub(crate) fn outline_from_tree(language: &str, tree: &tree_sitter::Tree, text: &Rope) -> Outline {
    let Some(query) = query_for(language) else {
        return Outline::default();
    };
    let names = query.capture_names();
    struct Found {
        pattern: usize,
        kind: SymbolKind,
        range: Range<usize>,
        name: String,
        selection: usize,
    }
    let mut found: HashMap<usize, Found> = HashMap::new();
    let mut cursor = QueryCursor::new();
    // No pattern has a predicate, so the text is never consulted.
    let mut matches = cursor.matches(&query, tree.root_node(), &[] as &[u8]);
    while let Some(m) = matches.next() {
        let mut item = None;
        let mut name_parts: Vec<(usize, String)> = Vec::new();
        let mut body = None;
        let mut receiver = None;
        for cap in m.captures {
            let capture = names[cap.index as usize];
            let node = cap.node;
            if let Some(kind) = capture.strip_prefix("item.") {
                item = SymbolKind::from_capture(kind).map(|k| (k, node));
            } else if capture == "name" {
                name_parts.push((node.start_byte(), text_of(text, node.byte_range())));
            } else if capture == "body" {
                body = Some(node.start_byte());
            } else if capture == "receiver" {
                receiver = Some(text_of(text, node.byte_range()));
            }
        }
        let Some((kind, node)) = item else { continue };
        if found
            .get(&node.id())
            .is_some_and(|f| f.pattern <= m.pattern_index)
        {
            continue;
        }
        name_parts.sort_by_key(|(at, _)| *at);
        let (name, selection) = match (name_parts.first(), body) {
            (Some((at, _)), _) => (
                name_parts
                    .iter()
                    .map(|(_, n)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                *at,
            ),
            (None, Some(body)) => (text_of(text, node.start_byte()..body), node.start_byte()),
            (None, None) => continue,
        };
        let mut name = collapse(&name);
        if let Some(receiver) = receiver {
            let ty = receiver
                .trim_matches(|c| c == '(' || c == ')')
                .split_whitespace()
                .last()
                .unwrap_or_default()
                .to_string();
            if !ty.is_empty() {
                name = format!("({ty}).{name}");
            }
        }
        if name.is_empty() {
            continue;
        }
        found.insert(
            node.id(),
            Found {
                pattern: m.pattern_index,
                kind,
                range: node.byte_range(),
                name,
                selection,
            },
        );
    }

    // Document order, a container before what it holds: by start, and the
    // longer of two that start together first.
    let mut items: Vec<Found> = found.into_values().collect();
    items.sort_by(|a, b| {
        a.range
            .start
            .cmp(&b.range.start)
            .then(b.range.end.cmp(&a.range.end))
    });
    let mut symbols: Vec<Symbol> = Vec::with_capacity(items.len());
    let mut open: Vec<(usize, usize)> = Vec::new(); // (index, end byte)
    for f in items {
        while open.last().is_some_and(|(_, end)| *end <= f.range.start) {
            open.pop();
        }
        let parent = open.last().map(|(ix, _)| *ix);
        let kind = match (f.kind, parent) {
            (SymbolKind::Function, Some(p)) if symbols[p].kind.holds_methods() => {
                SymbolKind::Method
            }
            (kind, _) => kind,
        };
        let ix = symbols.len();
        symbols.push(Symbol {
            name: f.name,
            kind,
            range: text.offset_to_position(f.range.start)..text.offset_to_position(f.range.end),
            selection: text.offset_to_position(f.selection),
            depth: open.len(),
            parent,
        });
        open.push((ix, f.range.end));
    }
    Outline { symbols }
}

/// The outline of a text nobody has parsed yet — a buffer that has not been
/// drawn. Parses it once, here.
pub(crate) fn outline_of_text(language: &str, text: &Rope) -> Outline {
    let Some(config) = LanguageRegistry::singleton().language(language) else {
        return Outline::default();
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&config.language).is_err() {
        return Outline::default();
    }
    let source = text.to_string();
    match parser.parse(&source, None) {
        Some(tree) => outline_from_tree(language, &tree, text),
        None => Outline::default(),
    }
}

#[cfg(test)]
pub(crate) fn outline_of(language: &str, source: &str) -> Outline {
    outline_of_text(language, &Rope::from(source))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `name@depth:kind` for every symbol, in order.
    fn shape(language: &str, source: &str) -> Vec<String> {
        outline_of(language, source)
            .symbols
            .iter()
            .map(|s| format!("{}@{}:{}", s.name, s.depth, s.kind.tag()))
            .collect()
    }

    #[test]
    fn every_outline_query_compiles_against_its_grammar() {
        for language in [
            "rust",
            "go",
            "python",
            "javascript",
            "typescript",
            "tsx",
            "c",
            "cpp",
            "java",
            "markdown",
            "ruby",
            "bash",
        ] {
            assert!(query_for(language).is_some(), "{language}");
        }
        assert!(query_for("json").is_none());
    }

    #[test]
    fn rust_nests_methods_under_their_impl() {
        let src = "mod net {\n    pub struct Server { port: u16 }\n    impl<T> Display for Wrapper<T> where T: Debug {\n        fn fmt(&self) {}\n    }\n    impl Server {\n        pub fn serve(&self) {}\n        const MAX: usize = 3;\n    }\n}\ntrait Run { fn run(&self); }\nenum Mode { A, B }\nfn main() {}\n";
        assert_eq!(
            shape("rust", src),
            [
                "net@0:mod",
                "Server@1:struct",
                "impl<T> Display for Wrapper<T> where T: Debug@1:impl",
                "fmt@2:method",
                "impl Server@1:impl",
                "serve@2:method",
                "MAX@2:const",
                "Run@0:trait",
                "run@1:method",
                "Mode@0:enum",
                "main@0:fn",
            ]
        );
    }

    #[test]
    fn go_methods_carry_their_receiver() {
        let src = "package main\n\ntype Server struct{}\ntype Handler interface{ Serve() }\ntype ID int\nconst Max = 3\nfunc (s *Server) Serve() {}\nfunc main() {}\n";
        assert_eq!(
            shape("go", src),
            [
                "Server@0:struct",
                "Handler@0:interface",
                "ID@0:type",
                "Max@0:const",
                "(*Server).Serve@0:method",
                "main@0:fn",
            ]
        );
    }

    #[test]
    fn python_classes_hold_their_methods() {
        let src = "class A:\n    def f(self):\n        def inner():\n            pass\n\n@decorator\ndef g():\n    pass\n";
        assert_eq!(
            shape("python", src),
            ["A@0:class", "f@1:method", "inner@2:fn", "g@0:fn"]
        );
    }

    #[test]
    fn javascript_finds_classes_methods_and_arrow_functions() {
        let src = "class Widget {\n  render() {}\n}\nfunction boot() {}\nconst handler = () => {};\nconst n = 3;\n";
        assert_eq!(
            shape("javascript", src),
            [
                "Widget@0:class",
                "render@1:method",
                "boot@0:fn",
                "handler@0:fn"
            ]
        );
    }

    #[test]
    fn typescript_adds_its_type_declarations() {
        let src = "interface Props { name: string }\ntype Id = number;\nenum Color { Red }\nexport class View {\n  show(): void {}\n}\nnamespace Util { export function f() {} }\n";
        let got = shape("typescript", src);
        assert_eq!(
            got,
            [
                "Props@0:interface",
                "Id@0:type",
                "Color@0:enum",
                "View@0:class",
                "show@1:method",
                "Util@0:mod",
                "f@1:fn",
            ]
        );
        let tsx = shape("tsx", "function App() { return <div/>; }\n");
        assert_eq!(tsx, ["App@0:fn"]);
    }

    #[test]
    fn c_and_cpp_find_functions_through_their_declarators() {
        let c = "struct point { int x; };\ntypedef int id_t;\nstatic char *name(void) { return 0; }\nint main(void) { return 0; }\n";
        assert_eq!(
            shape("c", c),
            ["point@0:struct", "id_t@0:type", "name@0:fn", "main@0:fn"]
        );
        let cpp = "namespace app {\nclass Server {\n public:\n  void serve();\n};\nvoid Server::serve() {}\n}\n";
        assert_eq!(
            shape("cpp", cpp),
            [
                "app@0:namespace",
                "Server@1:class",
                "serve@2:method",
                "Server::serve@1:fn",
            ]
        );
    }

    #[test]
    fn java_nests_members_in_their_class() {
        let src = "class App {\n  App() {}\n  void run() {}\n  interface Listener { void on(); }\n}\nenum Kind { A }\n";
        assert_eq!(
            shape("java", src),
            [
                "App@0:class",
                "App@1:ctor",
                "run@1:method",
                "Listener@1:interface",
                "on@2:method",
                "Kind@0:enum",
            ]
        );
    }

    #[test]
    fn markdown_headings_nest_by_level() {
        let src = "# Title\n\nintro\n\n## Install\n\ntext\n\n### From source\n\n## Usage\n";
        assert_eq!(
            shape("markdown", src),
            ["Title@0:#", "Install@1:#", "From source@2:#", "Usage@1:#",]
        );
        // tree-sitter-md opens no section for an underlined heading, so it
        // names only its own line.
        assert_eq!(
            shape("markdown", "Setext\n======\n\ntext\n"),
            ["Setext@0:#"]
        );
    }

    #[test]
    fn ruby_and_bash_have_outlines_too() {
        assert_eq!(
            shape("ruby", "module M\n  class C\n    def go; end\n  end\nend\n"),
            ["M@0:mod", "C@1:class", "go@2:method"]
        );
        assert_eq!(
            shape("bash", "setup() {\n  :\n}\nfunction run { :; }\n"),
            ["setup@0:fn", "run@0:fn"]
        );
    }

    #[test]
    fn a_language_without_a_query_has_an_empty_outline() {
        assert!(outline_of("json", "{\"a\": 1}").is_empty());
    }

    #[test]
    fn the_chain_at_a_position_runs_outermost_to_innermost() {
        let src = "mod net {\n    impl Server {\n        fn serve(&self) {\n            let x = 1;\n        }\n    }\n}\nfn main() {}\n";
        let outline = outline_of("rust", src);
        let names = |pos: Position| -> Vec<String> {
            outline
                .chain_at(pos)
                .into_iter()
                .map(|i| outline.symbols[i].name.clone())
                .collect()
        };
        assert_eq!(names(Position::new(3, 12)), ["net", "impl Server", "serve"]);
        assert_eq!(names(Position::new(1, 0)), ["net"]);
        assert_eq!(names(Position::new(7, 3)), ["main"]);
        assert_eq!(names(Position::new(6, 0)), ["net"]);
        assert!(names(Position::new(6, 2)).is_empty());
        let serve = outline
            .symbols
            .iter()
            .position(|s| s.name == "serve")
            .unwrap();
        assert_eq!(outline.container_path(serve), ["net", "impl Server"]);
        // Lands on the name, not on the start of the item.
        assert_eq!(outline.symbols[serve].selection, Position::new(2, 11));
    }

    #[test]
    fn a_language_servers_nested_answer_flattens_the_same_way() {
        let p = |l, c| Position::new(l, c);
        let outline = Outline::from_nodes(vec![
            SymbolNode {
                name: "later".into(),
                kind: SymbolKind::from_lsp(12),
                range: p(10, 0)..p(12, 1),
                selection: p(10, 3),
                children: vec![],
            },
            SymbolNode {
                name: "Server".into(),
                kind: SymbolKind::from_lsp(5),
                range: p(0, 0)..p(8, 1),
                selection: p(0, 6),
                children: vec![SymbolNode {
                    name: "serve".into(),
                    kind: SymbolKind::from_lsp(6),
                    range: p(2, 2)..p(4, 3),
                    selection: p(2, 6),
                    children: vec![],
                }],
            },
        ]);
        let got: Vec<_> = outline
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), s.depth, s.parent))
            .collect();
        assert_eq!(
            got,
            [
                ("Server", 0, None),
                ("serve", 1, Some(0)),
                ("later", 0, None)
            ]
        );
        assert_eq!(outline.chain_at(p(3, 0)), [0, 1]);
    }
}

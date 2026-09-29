//! Translating between the positions a language server speaks and the ones
//! the editor does.
//!
//! A server counts columns in UTF-16 code units unless it negotiated
//! otherwise. The editor keeps byte offsets into its rope, and
//! gpui-component's own LSP plumbing (`RopeExt::position_to_offset`, the
//! diagnostic set, the completion menu) reads a [`Position`]'s `character` as
//! a count of `char`s. So everything crossing the boundary is converted here,
//! once: server → editor positions before they reach gpui-component, editor
//! byte offsets → server positions before a request goes out.
//!
//! The three agree on ASCII and part ways on everything else: `é` is one
//! char and one UTF-16 unit but two bytes; `🎉` is one char, two UTF-16
//! units and four bytes.

use std::ops::Range;

use gpui_component::{Rope, RopeExt as _};
use lsp_types::{Position, TextEdit};

/// What a server's `character` counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Encoding {
    Utf8,
    #[default]
    Utf16,
    Utf32,
}

impl Encoding {
    pub(crate) fn from_kind(kind: Option<&lsp_types::PositionEncodingKind>) -> Self {
        match kind.map(|k| k.as_str()) {
            Some("utf-8") => Self::Utf8,
            Some("utf-32") => Self::Utf32,
            _ => Self::Utf16,
        }
    }

    fn units(self, c: char) -> usize {
        match self {
            Self::Utf8 => c.len_utf8(),
            Self::Utf16 => c.len_utf16(),
            Self::Utf32 => 1,
        }
    }
}

/// The server's column for the byte offset `byte` into `line`.
pub(crate) fn column_of_byte(line: &str, byte: usize, enc: Encoding) -> u32 {
    line.char_indices()
        .take_while(|(i, _)| *i < byte)
        .map(|(_, c)| enc.units(c))
        .sum::<usize>() as u32
}

/// The byte offset into `line` of the server's column `units`. A column past
/// the end of the line lands at its end; one inside a character (half a
/// surrogate pair) lands before that character.
pub(crate) fn byte_of_column(line: &str, units: u32, enc: Encoding) -> usize {
    let units = units as usize;
    let mut seen = 0;
    for (i, c) in line.char_indices() {
        let next = seen + enc.units(c);
        if next > units {
            return i;
        }
        seen = next;
    }
    line.len()
}

fn line_text(rope: &Rope, row: usize) -> String {
    rope.slice_line(row).to_string()
}

/// A byte offset in the editor → a server position.
pub(crate) fn offset_to_lsp(rope: &Rope, offset: usize, enc: Encoding) -> Position {
    let point = rope.offset_to_point(offset);
    let line = line_text(rope, point.row);
    Position::new(point.row as u32, column_of_byte(&line, point.column, enc))
}

/// A server position → a byte offset in the editor, clamped into the text.
#[cfg(test)]
pub(crate) fn lsp_to_offset(rope: &Rope, pos: Position, enc: Encoding) -> usize {
    let row = pos.line as usize;
    if row >= rope.lines_len() {
        return rope.len();
    }
    let line = line_text(rope, row);
    rope.line_start_offset(row) + byte_of_column(&line, pos.character, enc)
}

/// A server position → the char-counted position gpui-component expects.
pub(crate) fn lsp_to_editor(rope: &Rope, pos: Position, enc: Encoding) -> Position {
    if enc == Encoding::Utf32 {
        return pos;
    }
    let row = pos.line as usize;
    if row >= rope.lines_len() {
        return pos;
    }
    let line = line_text(rope, row);
    let byte = byte_of_column(&line, pos.character, enc);
    Position::new(pos.line, line[..byte].chars().count() as u32)
}

pub(crate) fn range_to_editor(
    rope: &Rope,
    range: lsp_types::Range,
    enc: Encoding,
) -> lsp_types::Range {
    lsp_types::Range::new(
        lsp_to_editor(rope, range.start, enc),
        lsp_to_editor(rope, range.end, enc),
    )
}

/// A server's diagnostics with their ranges in editor columns, ready for
/// `DiagnosticSet::extend`.
pub(crate) fn diagnostics_to_editor(
    rope: &Rope,
    diagnostics: &[lsp_types::Diagnostic],
    enc: Encoding,
) -> Vec<lsp_types::Diagnostic> {
    diagnostics
        .iter()
        .map(|d| lsp_types::Diagnostic {
            range: range_to_editor(rope, d.range, enc),
            ..d.clone()
        })
        .collect()
}

/// How many errors and warnings a set of diagnostics holds. A diagnostic
/// without a severity counts as neither — the client decides, and the
/// status bar only claims what the server did.
pub(crate) fn count_problems(diagnostics: &[lsp_types::Diagnostic]) -> (usize, usize) {
    diagnostics
        .iter()
        .fold((0, 0), |(e, w), d| match d.severity {
            Some(lsp_types::DiagnosticSeverity::ERROR) => (e + 1, w),
            Some(lsp_types::DiagnosticSeverity::WARNING) => (e, w + 1),
            _ => (e, w),
        })
}

/// `edits` in the order they can be applied one at a time.
///
/// Every edit in a `TextEdit[]` is written against the original text, so
/// applying the one furthest into the document first keeps the positions of
/// the rest valid. Edits that start at the same place are inserted in the
/// order the server listed them, which going backwards means reversing them.
pub(crate) fn in_application_order(edits: &[TextEdit]) -> Vec<TextEdit> {
    let mut indexed: Vec<(usize, &TextEdit)> = edits.iter().enumerate().collect();
    indexed.sort_by(|(ia, a), (ib, b)| {
        let ka = (a.range.start.line, a.range.start.character, *ia);
        let kb = (b.range.start.line, b.range.start.character, *ib);
        kb.cmp(&ka)
    });
    indexed.into_iter().map(|(_, e)| e.clone()).collect()
}

/// `edits` with their ranges in editor columns and in application order —
/// what `InputState::apply_lsp_edits`, which applies them as given, needs.
pub(crate) fn edits_to_editor(rope: &Rope, edits: &[TextEdit], enc: Encoding) -> Vec<TextEdit> {
    in_application_order(edits)
        .into_iter()
        .map(|e| TextEdit {
            range: range_to_editor(rope, e.range, enc),
            new_text: e.new_text,
        })
        .collect()
}

/// `text` with `edits` applied — for a file a rename touches that has no
/// buffer open.
pub(crate) fn apply_edits(text: &str, edits: &[TextEdit], enc: Encoding) -> String {
    let starts = line_starts(text);
    let offset = |pos: Position| -> usize {
        let Some(&start) = starts.get(pos.line as usize) else {
            return text.len();
        };
        let end = starts
            .get(pos.line as usize + 1)
            .map(|&next| next - 1)
            .unwrap_or(text.len());
        let line = &text[start..end];
        let line = line.strip_suffix('\r').unwrap_or(line);
        start + byte_of_column(line, pos.character, enc)
    };
    let mut out = text.to_owned();
    for edit in in_application_order(edits) {
        let start = offset(edit.range.start);
        let end = offset(edit.range.end).max(start);
        out.replace_range(start..end, &edit.new_text);
    }
    out
}

fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect()
}

/// The byte range an editor range covers, for a request about a selection.
pub(crate) fn offsets_to_lsp_range(
    rope: &Rope,
    range: Range<usize>,
    enc: Encoding,
) -> lsp_types::Range {
    lsp_types::Range::new(
        offset_to_lsp(rope, range.start, enc),
        offset_to_lsp(rope, range.end, enc),
    )
}

/// A completion's snippet as the plain text it would expand to with every
/// placeholder left at its default. The editor has no snippet engine; this
/// is what it inserts instead of `${1:x}` litter.
pub(crate) fn snippet_to_plain(snippet: &str) -> String {
    let mut out = String::with_capacity(snippet.len());
    let mut chars = snippet.chars().peekable();
    expand(&mut chars, &mut out, false);
    out
}

/// Expands until the end, or until the `}` closing the placeholder being
/// expanded when `nested`.
fn expand(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, out: &mut String, nested: bool) {
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '}' if nested => return,
            '$' => match chars.peek().copied() {
                Some(d) if d.is_ascii_digit() => {
                    while chars.peek().is_some_and(|d| d.is_ascii_digit()) {
                        chars.next();
                    }
                }
                Some('{') => {
                    chars.next();
                    // `${1:default}`, `${1|one,two|}`, `${1}` or `${name}`.
                    while chars
                        .peek()
                        .is_some_and(|d| d.is_ascii_alphanumeric() || *d == '_')
                    {
                        chars.next();
                    }
                    match chars.next() {
                        Some(':') => expand(chars, out, true),
                        Some('|') => {
                            let mut first = true;
                            while let Some(c) = chars.next() {
                                match c {
                                    '|' => {
                                        // The closing `}`.
                                        chars.next();
                                        break;
                                    }
                                    ',' => first = false,
                                    '\\' => {
                                        if let Some(next) = chars.next()
                                            && first
                                        {
                                            out.push(next);
                                        }
                                    }
                                    c if first => out.push(c),
                                    _ => {}
                                }
                            }
                        }
                        // `${1}`, or something this does not understand:
                        // skip to its end.
                        Some('}') | None => {}
                        Some(_) => {
                            let mut depth = 1;
                            for c in chars.by_ref() {
                                match c {
                                    '{' => depth += 1,
                                    '}' => {
                                        depth -= 1;
                                        if depth == 0 {
                                            break;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                // `$name` (a variable) expands to nothing we know.
                Some(d) if d.is_ascii_alphabetic() || d == '_' => {
                    while chars
                        .peek()
                        .is_some_and(|d| d.is_ascii_alphanumeric() || *d == '_')
                    {
                        chars.next();
                    }
                }
                _ => out.push('$'),
            },
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Range as LspRange;

    fn pos(line: u32, character: u32) -> Position {
        Position::new(line, character)
    }

    fn edit(sl: u32, sc: u32, el: u32, ec: u32, text: &str) -> TextEdit {
        TextEdit {
            range: LspRange::new(pos(sl, sc), pos(el, ec)),
            new_text: text.into(),
        }
    }

    #[test]
    fn columns_count_utf16_units_by_default() {
        // a é 🎉 b: bytes 0,1,3,7 — UTF-16 units 0,1,2,4 — chars 0,1,2,3.
        let line = "aé🎉b";
        let enc = Encoding::Utf16;
        assert_eq!(column_of_byte(line, 0, enc), 0);
        assert_eq!(column_of_byte(line, 1, enc), 1);
        assert_eq!(column_of_byte(line, 3, enc), 2);
        assert_eq!(column_of_byte(line, 7, enc), 4);
        assert_eq!(column_of_byte(line, 8, enc), 5);
        assert_eq!(byte_of_column(line, 2, enc), 3);
        assert_eq!(byte_of_column(line, 4, enc), 7);
        assert_eq!(byte_of_column(line, 5, enc), 8);
        // Half a surrogate pair lands before the character, never inside it.
        assert_eq!(byte_of_column(line, 3, enc), 3);
        // Past the end of the line: the end of the line.
        assert_eq!(byte_of_column(line, 99, enc), 8);
    }

    #[test]
    fn utf8_and_utf32_count_bytes_and_chars() {
        let line = "aé🎉b";
        assert_eq!(column_of_byte(line, 7, Encoding::Utf8), 7);
        assert_eq!(byte_of_column(line, 3, Encoding::Utf8), 3);
        assert_eq!(column_of_byte(line, 7, Encoding::Utf32), 3);
        assert_eq!(byte_of_column(line, 3, Encoding::Utf32), 7);
    }

    #[test]
    fn rope_offsets_and_server_positions_round_trip() {
        let rope = Rope::from("fn a() {}\nlet 🎉 = \"é\";\n");
        let enc = Encoding::Utf16;
        let second = "fn a() {}\n".len();
        let quote = second + "let 🎉 = ".len();
        assert_eq!(offset_to_lsp(&rope, quote, enc), pos(1, 9));
        assert_eq!(lsp_to_offset(&rope, pos(1, 9), enc), quote);
        // A line past the end of the text clamps to the end.
        assert_eq!(lsp_to_offset(&rope, pos(9, 0), enc), rope.len());
        // And the editor's own positions count chars.
        assert_eq!(lsp_to_editor(&rope, pos(1, 9), enc), pos(1, 8));
        assert_eq!(lsp_to_editor(&rope, pos(0, 3), enc), pos(0, 3));
    }

    #[test]
    fn diagnostics_move_to_editor_columns_and_are_counted() {
        let rope = Rope::from("🎉🎉x\n");
        let d = |sev, c| lsp_types::Diagnostic {
            range: LspRange::new(pos(0, c), pos(0, c + 1)),
            severity: sev,
            message: "m".into(),
            ..Default::default()
        };
        let diags = vec![
            d(Some(lsp_types::DiagnosticSeverity::ERROR), 4),
            d(Some(lsp_types::DiagnosticSeverity::WARNING), 0),
            d(None, 0),
        ];
        let mapped = diagnostics_to_editor(&rope, &diags, Encoding::Utf16);
        assert_eq!(mapped[0].range, LspRange::new(pos(0, 2), pos(0, 3)));
        assert_eq!(mapped[0].message, "m");
        assert_eq!(count_problems(&diags), (1, 1));
    }

    #[test]
    fn edits_apply_back_to_front_and_same_place_inserts_keep_their_order() {
        let edits = vec![
            edit(0, 0, 0, 0, "A"),
            edit(1, 0, 1, 3, "LET"),
            edit(0, 0, 0, 0, "B"),
            edit(0, 3, 0, 5, "__"),
        ];
        let ordered = in_application_order(&edits);
        let texts: Vec<&str> = ordered.iter().map(|e| e.new_text.as_str()).collect();
        assert_eq!(texts, ["LET", "__", "B", "A"]);
        assert_eq!(
            apply_edits("abcdef\nlet x\n", &edits, Encoding::Utf16),
            "ABabc__f\nLET x\n"
        );
    }

    #[test]
    fn edits_on_a_file_count_utf16_columns_and_ignore_crlf() {
        let text = "let 🎉x = 1;\r\nx;\r\n";
        // `x` after the emoji is UTF-16 column 6.
        let edits = vec![edit(0, 6, 0, 7, "y"), edit(1, 0, 1, 1, "y")];
        assert_eq!(
            apply_edits(text, &edits, Encoding::Utf16),
            "let 🎉y = 1;\r\ny;\r\n"
        );
    }

    #[test]
    fn editor_edits_convert_each_range_against_the_original_text() {
        let rope = Rope::from("🎉a🎉b\n");
        let edits = vec![edit(0, 2, 0, 3, "A"), edit(0, 5, 0, 6, "B")];
        let mapped = edits_to_editor(&rope, &edits, Encoding::Utf16);
        assert_eq!(mapped[0].range, LspRange::new(pos(0, 3), pos(0, 4)));
        assert_eq!(mapped[0].new_text, "B");
        assert_eq!(mapped[1].range, LspRange::new(pos(0, 1), pos(0, 2)));
    }

    #[test]
    fn snippets_become_their_default_text() {
        assert_eq!(snippet_to_plain("println!(\"$1\")$0"), "println!(\"\")");
        assert_eq!(
            snippet_to_plain("fn ${1:name}(${2:args}) {\n\t$0\n}"),
            "fn name(args) {\n\t\n}"
        );
        assert_eq!(snippet_to_plain("${1:outer ${2:inner}}"), "outer inner");
        assert_eq!(snippet_to_plain("${1|one,two|}"), "one");
        assert_eq!(snippet_to_plain("cost: \\$5 ${TM_FILENAME}"), "cost: $5 ");
        assert_eq!(snippet_to_plain("a $ b"), "a $ b");
        assert_eq!(snippet_to_plain("x${1}y"), "xy");
    }
}

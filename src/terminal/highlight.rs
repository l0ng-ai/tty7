//! A small shell-command syntax highlighter for the command editor — tty7's own
//! highlighter, independent of any zsh highlighting plugin.
//!
//! It splits a line into contiguous spans whose concatenated text reproduces the
//! input exactly (whitespace included), tagging each with a [`TokenKind`] the
//! renderer maps to a color. The grammar is deliberately shallow — enough to
//! color commands, arguments, flags, paths, quoted strings, operators and
//! comments — not a real shell parser.

/// What a span of the command line represents, for coloring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// A command name: the first word, and the first word after a `|`/`&&`/`;`.
    Command,
    /// A plain argument.
    Arg,
    /// A `-f` / `--flag` option.
    Flag,
    /// A word containing `/` (treated as a path).
    Path,
    /// A single- or double-quoted string (quotes included).
    StringLit,
    /// A shell operator: `| & ; < >` (and runs like `&&`, `||`, `>>`).
    Operator,
    /// A `# …` comment to end of line.
    Comment,
    /// Inter-token whitespace (kept so spans tile the whole line).
    Whitespace,
}

/// A contiguous run of the line with a single [`TokenKind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub kind: TokenKind,
}

fn is_operator(c: char) -> bool {
    matches!(c, '|' | '&' | ';' | '<' | '>')
}

/// Split `line` into colored spans. Concatenating the spans' `text` yields `line`.
pub fn highlight(line: &str) -> Vec<Span> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut spans = Vec::new();
    let mut i = 0;
    // The next bare word is a command at the start of the line and right after a
    // pipe / list operator.
    let mut expect_command = true;

    while i < n {
        let c = chars[i];

        if c.is_whitespace() {
            let start = i;
            while i < n && chars[i].is_whitespace() {
                i += 1;
            }
            spans.push(Span {
                text: chars[start..i].iter().collect(),
                kind: TokenKind::Whitespace,
            });
            continue;
        }

        if c == '#' {
            // Comment to end of line.
            spans.push(Span {
                text: chars[i..].iter().collect(),
                kind: TokenKind::Comment,
            });
            break;
        }

        if is_operator(c) {
            let start = i;
            while i < n && is_operator(chars[i]) {
                i += 1;
            }
            spans.push(Span {
                text: chars[start..i].iter().collect(),
                kind: TokenKind::Operator,
            });
            expect_command = true; // a command follows the operator
            continue;
        }

        if c == '\'' || c == '"' {
            let quote = c;
            let start = i;
            i += 1;
            while i < n && chars[i] != quote {
                i += 1;
            }
            if i < n {
                i += 1; // include the closing quote
            }
            spans.push(Span {
                text: chars[start..i].iter().collect(),
                kind: TokenKind::StringLit,
            });
            expect_command = false;
            continue;
        }

        // A bare word: up to the next whitespace / operator / quote / comment.
        let start = i;
        while i < n
            && !chars[i].is_whitespace()
            && !is_operator(chars[i])
            && !matches!(chars[i], '\'' | '"' | '#')
        {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        let kind = if expect_command {
            TokenKind::Command
        } else if word.starts_with('-') {
            TokenKind::Flag
        } else if word.contains('/') {
            TokenKind::Path
        } else {
            TokenKind::Arg
        };
        spans.push(Span { text: word, kind });
        expect_command = false;
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &str) -> Vec<(String, TokenKind)> {
        highlight(line)
            .into_iter()
            .map(|s| (s.text, s.kind))
            .collect()
    }

    /// Spans must tile the line exactly.
    fn assert_tiles(line: &str) {
        let joined: String = highlight(line).into_iter().map(|s| s.text).collect();
        assert_eq!(joined, line);
    }

    #[test]
    fn command_args_flags_strings() {
        assert_tiles("git commit -m \"a msg\"");
        let k = kinds("git commit -m \"a msg\"");
        assert_eq!(k[0], ("git".into(), TokenKind::Command));
        assert_eq!(k[2], ("commit".into(), TokenKind::Arg));
        assert_eq!(k[4], ("-m".into(), TokenKind::Flag));
        assert_eq!(k[6], ("\"a msg\"".into(), TokenKind::StringLit));
    }

    #[test]
    fn command_resets_after_pipe_and_operators() {
        let k = kinds("cat f | grep x");
        assert_eq!(k[0].1, TokenKind::Command); // cat
        assert_eq!(k[2].1, TokenKind::Arg); // f
        assert_eq!(k[4].1, TokenKind::Operator); // |
        assert_eq!(k[6].1, TokenKind::Command); // grep (command after pipe)
        assert_tiles("cat f | grep x");
    }

    #[test]
    fn paths_and_comments() {
        let k = kinds("ls src/main.rs # look");
        assert_eq!(k[0].1, TokenKind::Command);
        assert_eq!(k[2].1, TokenKind::Path); // src/main.rs
        assert!(k.iter().any(|(_, kind)| *kind == TokenKind::Comment));
        assert_tiles("ls src/main.rs # look");
    }

    #[test]
    fn unterminated_quote_consumes_rest() {
        assert_tiles("echo \"open");
        let k = kinds("echo \"open");
        assert_eq!(k.last().unwrap().1, TokenKind::StringLit);
    }

    #[test]
    fn double_operator_runs() {
        let k = kinds("a && b");
        assert_eq!(k[2], ("&&".into(), TokenKind::Operator));
        assert_eq!(k[4].1, TokenKind::Command);
        assert_tiles("a && b");
    }

    #[test]
    fn command_position_wins_over_flag_and_path_shapes() {
        // The first word is always a Command, even when it looks like a flag or
        // a path — command position takes precedence in the classifier.
        assert_eq!(kinds("-v")[0], ("-v".into(), TokenKind::Command));
        assert_eq!(
            kinds("./run.sh now")[0],
            ("./run.sh".into(), TokenKind::Command)
        );
        // Off command position the same shapes classify as Flag / Path.
        let k = kinds("ls -v ./run.sh");
        assert_eq!(k[2].1, TokenKind::Flag);
        assert_eq!(k[4].1, TokenKind::Path);
    }

    #[test]
    fn leading_operator_and_quoted_first_word() {
        // An operator at the very start still tiles, and the word after it is a
        // command.
        let k = kinds("| grep x");
        assert_eq!(k[0], ("|".into(), TokenKind::Operator));
        assert_eq!(k[2].1, TokenKind::Command);
        assert_tiles("| grep x");
        // A quoted string in command position stays a StringLit (quotes are not
        // classified as commands), and the argument after it is a plain Arg.
        let k = kinds("'./a b' c");
        assert_eq!(k[0], ("'./a b'".into(), TokenKind::StringLit));
        assert_eq!(k[2].1, TokenKind::Arg);
    }

    #[test]
    fn multibyte_text_tiles_exactly() {
        // Span boundaries are char-based; CJK args must reassemble losslessly.
        assert_tiles("echo 你好 世界 | grep 好");
        let k = kinds("echo 你好");
        assert_eq!(k[2], ("你好".into(), TokenKind::Arg));
    }
}

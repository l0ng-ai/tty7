//! One set of rules for putting a real path onto a command line, shared by
//! everything that inserts one.
//!
//! Three places need this: the file tree's `cd` and paste, the terminal's
//! drop / clipboard / staged-image insertion, and completion accepting a
//! candidate. They used to carry three separate implementations, and on
//! Windows two of them disagreed — `file_tree::shell_quote_for` wrapped the
//! path in quotes and worked, while `view::shell_escape_path` escaped with
//! backslashes and ate the path separators of `C:\Users\me` (#593 fixed the
//! first one and never reached the other two).
//!
//! The rules are short:
//!
//! - cmd.exe treats only double quotes as quoting. A single quote is an
//!   ordinary character there, so the POSIX form splits the path at its first
//!   space. Windows paths cannot contain `"`, so there is nothing to escape
//!   inside the quotes.
//! - PowerShell and every POSIX shell take the single-quoted form, so an
//!   unknown shell keeps it too.
//! - Backslash escaping is not used anywhere. Only POSIX shells understand it,
//!   and on Windows it collides head-on with the path separator.
//!
//! A leading `~/` stays outside the quotes: quoting it would make it a literal
//! and lose the home expansion the user is asking for.

/// Characters that need no quoting in any shell we target.
fn is_bare(c: char) -> bool {
    c.is_alphanumeric() || "/.-_~+".contains(c)
}

fn is_cmd_exe(shell_program: Option<&str>) -> bool {
    shell_program
        .map(|p| {
            let base = p.rsplit(['\\', '/']).next().unwrap_or(p);
            base.eq_ignore_ascii_case("cmd") || base.eq_ignore_ascii_case("cmd.exe")
        })
        .unwrap_or(false)
}

/// Quote `path` as a single argument for the shell the pane is running.
///
/// `shell_program` is the pane's shell binary as `ShellSpec::program` reports
/// it — `None` when the pane has not resolved one yet, which keeps the
/// single-quoted form that every shell but cmd.exe accepts.
pub fn quote_for_shell(path: &str, shell_program: Option<&str>) -> String {
    if path.is_empty() {
        return if is_cmd_exe(shell_program) {
            "\"\"".to_string()
        } else {
            "''".to_string()
        };
    }
    // `~/` has to stay unquoted for the shell to expand it, so quote only the
    // rest. A bare `~` is already covered by `is_bare`.
    if let Some(rest) = path.strip_prefix("~/") {
        if rest.is_empty() {
            return "~/".to_string();
        }
        return format!("~/{}", quote_for_shell(rest, shell_program));
    }
    if path.chars().all(is_bare) {
        return path.to_string();
    }
    if is_cmd_exe(shell_program) {
        format!("\"{path}\"")
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

/// Undo [`quote_for_shell`] far enough to look the path up on disk.
///
/// Completion re-reads the word under the cursor after the user has already
/// accepted one candidate, so whatever quoting went in has to come back out
/// before the word can be resolved against the filesystem. The word is
/// mid-typing and therefore usually *un*terminated, so a lone leading quote
/// counts.
///
/// `posix_escapes` says whether a backslash is an escape character in the
/// pane's shell. On a POSIX shell it is, and a user who typed `My\ Docs` by
/// hand expects it honoured; on Windows it is a path separator and must
/// survive untouched.
pub fn unquote_word(word: &str, posix_escapes: bool) -> String {
    let inner = match word.as_bytes().first() {
        Some(b'\'') => strip_single_quoted(&word[1..]),
        Some(b'"') => word[1..]
            .strip_suffix('"')
            .unwrap_or(&word[1..])
            .to_string(),
        _ => word.to_string(),
    };
    if !posix_escapes || !inner.contains('\\') {
        return inner;
    }
    let mut out = String::with_capacity(inner.len());
    let mut escaped = false;
    for ch in inner.chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    if escaped {
        out.push('\\');
    }
    out
}

/// Undo the `'\''` seam a POSIX single-quoted string uses to carry a quote.
fn strip_single_quoted(rest: &str) -> String {
    let body = rest.strip_suffix('\'').unwrap_or(rest);
    body.replace(r"'\''", "'")
}

/// Whether the pane's shell treats a backslash as an escape character.
///
/// Callers that know the shell should say so; the platform default is the
/// right answer for the rest, since a Windows pane runs a Windows shell unless
/// the user went out of their way (WSL panes get their paths rewritten to
/// `/mnt/...` before they reach here).
pub fn posix_escapes_for(shell_program: Option<&str>) -> bool {
    if is_cmd_exe(shell_program) {
        return false;
    }
    if let Some(p) = shell_program {
        let base = p.rsplit(['\\', '/']).next().unwrap_or(p);
        let base = base.strip_suffix(".exe").unwrap_or(base);
        if base.eq_ignore_ascii_case("powershell") || base.eq_ignore_ascii_case("pwsh") {
            return false;
        }
        return true;
    }
    !cfg!(windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_path_is_left_alone() {
        assert_eq!(
            quote_for_shell("/Users/me/notes.txt", None),
            "/Users/me/notes.txt"
        );
        assert_eq!(quote_for_shell("notes.txt", None), "notes.txt");
        assert_eq!(quote_for_shell("--message", None), "--message");
    }

    #[test]
    fn posix_shells_get_single_quotes() {
        assert_eq!(
            quote_for_shell("/Users/me/My File (1).txt", Some("zsh")),
            "'/Users/me/My File (1).txt'"
        );
        assert_eq!(
            quote_for_shell("/a/$HOME & more", None),
            "'/a/$HOME & more'"
        );
        assert_eq!(quote_for_shell("it's here", None), r"'it'\''s here'");
        assert_eq!(quote_for_shell("", None), "''");
    }

    #[test]
    fn a_newline_survives_inside_the_quotes() {
        assert_eq!(quote_for_shell("a\nb", None), "'a\nb'");
    }

    /// The bug this module exists for: a Windows path used to come out as
    /// `C:\\Users\\me\\My\ Docs`, which the shell then un-escaped back into a
    /// path with no separators at all.
    #[test]
    fn a_windows_path_keeps_its_separators() {
        assert_eq!(
            quote_for_shell(r"C:\Users\me\My Docs", Some("powershell.exe")),
            r"'C:\Users\me\My Docs'"
        );
        assert_eq!(
            quote_for_shell(r"C:\Users\me\My Docs", Some(r"C:\Windows\System32\cmd.exe")),
            "\"C:\\Users\\me\\My Docs\""
        );
    }

    #[test]
    fn cmd_exe_is_recognised_by_basename_on_either_separator() {
        for p in [
            "cmd",
            "cmd.exe",
            "CMD.EXE",
            r"C:\Windows\System32\cmd.exe",
            "/c/Windows/System32/cmd.exe",
        ] {
            assert_eq!(
                quote_for_shell("a b", Some(p)),
                "\"a b\"",
                "{p} should be recognised as cmd.exe"
            );
        }
        assert_eq!(quote_for_shell("a b", Some("pwsh")), "'a b'");
        assert_eq!(quote_for_shell("a b", Some("powershell.exe")), "'a b'");
    }

    #[test]
    fn a_tilde_stays_outside_the_quotes_so_the_shell_expands_it() {
        assert_eq!(quote_for_shell("~/My Documents", None), "~/'My Documents'");
        assert_eq!(quote_for_shell("~/notes.txt", None), "~/notes.txt");
        assert_eq!(quote_for_shell("~", None), "~");
        // Not a home reference — a file whose name starts with a tilde.
        assert_eq!(quote_for_shell("~weird name", None), "'~weird name'");
    }

    #[test]
    fn unquoting_undoes_what_quoting_did() {
        for path in [
            "/Users/me/My File (1).txt",
            "it's here",
            r"C:\Users\me\My Docs",
            "~/My Documents",
        ] {
            let quoted = quote_for_shell(path, Some("zsh"));
            assert_eq!(unquote_word(&quoted, true), path, "round trip of {path}");
        }
    }

    #[test]
    fn an_unterminated_quote_still_unquotes() {
        // What completion actually sees: the user is mid-word.
        assert_eq!(unquote_word("'My Doc", true), "My Doc");
        assert_eq!(unquote_word("\"My Doc", false), "My Doc");
    }

    #[test]
    fn a_hand_typed_backslash_escape_is_honoured_only_where_it_is_one() {
        assert_eq!(unquote_word(r"My\ Documents", true), "My Documents");
        // On Windows the same bytes are a path, not an escape — this is the
        // half of the bug that made inline path completion unable to resolve
        // any directory there.
        assert_eq!(unquote_word(r"C:\Users\me", false), r"C:\Users\me");
        assert_eq!(unquote_word(r"trailing\", true), r"trailing\");
    }

    #[test]
    fn the_shell_decides_whether_backslashes_escape() {
        assert!(posix_escapes_for(Some("zsh")));
        assert!(posix_escapes_for(Some("/bin/bash")));
        assert!(!posix_escapes_for(Some("cmd.exe")));
        assert!(!posix_escapes_for(Some("powershell.exe")));
        assert!(!posix_escapes_for(Some("pwsh")));
        assert_eq!(posix_escapes_for(None), !cfg!(windows));
    }
}

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
//! Three shells, three rules:
//!
//! - cmd.exe treats only double quotes as quoting. A single quote is an
//!   ordinary character there, so the POSIX form splits the path at its first
//!   space. Windows paths cannot contain `"`, so there is nothing to escape
//!   inside the quotes.
//! - PowerShell takes `'...'`, and writes an embedded `'` twice. The POSIX
//!   `'\''` seam is not a seam there — PowerShell does not join a quoted
//!   string to the bare word beside it — so `C:\Users\O'Brien` came out as
//!   something PowerShell reads as three tokens.
//! - Every POSIX shell takes `'...'` too, and breaks out for an embedded `'`
//!   via `'\''`.
//!
//! Backslash escaping is not used to quote anywhere. Only POSIX shells
//! understand it, and on Windows it collides head-on with the path separator.
//!
//! A leading `~/` stays outside the quotes: quoting it would make it a literal
//! and lose the home expansion the user is asking for.
//!
//! This is for the shell in front of the *user*. What tty7 sends to a machine
//! — an SSH command line, a WSL bootstrap, an agent hook installed on the far
//! side — is always re-read by `sh`, and uses `tty7_core::core::shells::
//! shell_quote` instead. Two rules for two questions, rather than one that has
//! to guess which it is being asked.

/// Characters that need no quoting in any shell we target.
fn is_bare(c: char) -> bool {
    c.is_alphanumeric() || "/.-_~+".contains(c)
}

/// How a shell wants a literal string written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quoting {
    /// cmd.exe: `"..."`, and a Windows path cannot hold a `"` to escape.
    ///
    /// A `%` is a different matter: it is legal in a Windows filename, and cmd
    /// expands `%NAME%` *inside* double quotes, before it parses them. So a
    /// file actually called `%USERPROFILE%.txt` pastes as a command line
    /// naming some other path entirely. cmd offers no escape for this on an
    /// interactive line — `%%` is a batch-file rule, and `^` is not read
    /// inside quotes — so it is a limitation rather than an oversight, and
    /// `a_percent_in_a_name_is_the_one_thing_cmd_cannot_be_told_to_hold`
    /// records exactly how far the quoting gets. It costs the wrong path, not
    /// a command: `%` substitutes an environment variable and can do nothing
    /// else.
    Cmd,
    /// PowerShell and pwsh: `'...'`, with an embedded `'` written `''`.
    PowerShell,
    /// Everything else: `'...'`, with an embedded `'` written `'\''`.
    Posix,
}

/// The shell binary's name, without a directory and without a `.exe` suffix.
fn base_name(program: &str) -> &str {
    let base = program.rsplit(['\\', '/']).next().unwrap_or(program);
    let cut = base.len().saturating_sub(4);
    match base.get(cut..) {
        Some(tail) if tail.eq_ignore_ascii_case(".exe") => &base[..cut],
        _ => base,
    }
}

/// Which dialect the pane's shell speaks.
///
/// `shell_program` is the pane's shell binary as `ShellSpec::program` reports
/// it. `None` means the pane has not resolved one yet, and the platform is the
/// only evidence there is: a Windows pane is overwhelmingly PowerShell, and
/// everywhere else it is something POSIX. (WSL panes get their paths rewritten
/// to `/mnt/...` before they reach here.)
pub fn quoting_for(shell_program: Option<&str>) -> Quoting {
    let Some(base) = shell_program.map(base_name) else {
        return if cfg!(windows) {
            Quoting::PowerShell
        } else {
            Quoting::Posix
        };
    };
    if base.eq_ignore_ascii_case("cmd") {
        Quoting::Cmd
    } else if base.eq_ignore_ascii_case("powershell") || base.eq_ignore_ascii_case("pwsh") {
        Quoting::PowerShell
    } else {
        Quoting::Posix
    }
}

/// Quote `path` as a single argument for the shell the pane is running.
pub fn quote_for_shell(path: &str, shell_program: Option<&str>) -> String {
    quote_as(path, quoting_for(shell_program))
}

/// [`quote_for_shell`] with the dialect already decided.
fn quote_as(path: &str, quoting: Quoting) -> String {
    if path.is_empty() {
        return match quoting {
            Quoting::Cmd => "\"\"".to_string(),
            _ => "''".to_string(),
        };
    }
    // `~/` has to stay unquoted for the shell to expand it, so quote only the
    // rest. A bare `~` is already covered by `is_bare`.
    if let Some(rest) = path.strip_prefix("~/") {
        if rest.is_empty() {
            return "~/".to_string();
        }
        return format!("~/{}", quote_as(rest, quoting));
    }
    if path.chars().all(is_bare) {
        return path.to_string();
    }
    match quoting {
        Quoting::Cmd => format!("\"{path}\""),
        Quoting::PowerShell => format!("'{}'", path.replace('\'', "''")),
        Quoting::Posix => format!("'{}'", path.replace('\'', r"'\''")),
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
/// Only a POSIX shell treats a backslash as an escape character. A user who
/// typed `My\ Docs` by hand there expects it honoured; on Windows the same
/// character is a path separator and must survive untouched.
///
/// Inside single quotes it is neither, on any shell: a single-quoted string is
/// literal from end to end. Unescaping there would take the separators out of
/// `'C:\Users\me'` — exactly the form [`quote_for_shell`] produces for that
/// path.
///
/// The scan tracks quoting across the whole word rather than looking at the
/// first character, because a quote can open partway in: `~/'My Documents'` has
/// to keep its `~/` outside so the shell expands it, and the `'\''` seam that
/// carries a quote through a POSIX single-quoted string is three state changes
/// in a row rather than a special case.
pub fn unquote_word(word: &str, quoting: Quoting) -> String {
    let posix_escapes = quoting == Quoting::Posix;
    let mut out = String::with_capacity(word.len());
    let mut quote: Option<char> = None;
    let mut chars = word.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            // PowerShell's own seam: inside `'...'` a doubled quote is one
            // literal quote and closes nothing.
            (Some('\''), '\'') if quoting == Quoting::PowerShell && chars.peek() == Some(&'\'') => {
                chars.next();
                out.push('\'');
            }
            (Some(q), _) if c == q => quote = None,
            // A backslash escapes inside double quotes and outside quotes, but
            // never inside single ones. A trailing one has nothing to escape
            // and stands for itself.
            (Some('"') | None, '\\') if posix_escapes => match chars.next() {
                Some(next) => out.push(next),
                None => out.push('\\'),
            },
            (Some(_), _) => out.push(c),
            (None, '\'' | '"') => quote = Some(c),
            (None, _) => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Completion leans on these two being inverses: it inserts a quoted
    /// candidate, and the next Tab reads that word back off the line and has
    /// to resolve it against the filesystem again. A path that does not survive
    /// the round trip is a directory the second Tab cannot enter.
    ///
    /// Every path here is one that can exist. The single exception is a `"`
    /// under `Quoting::Cmd`: `quote_as` wraps a cmd argument in double quotes
    /// without a seam for a literal one, so `a"b` comes back as `ab`. Cmd is
    /// reached only for a Windows shell, and Windows forbids `"` in a file
    /// name along with `< > : / \ | ? *` — so there is no such path to lose.
    /// Asserted as it stands rather than fixed, because the fix would be a
    /// quoting rule for a case that cannot arrive.
    #[test]
    fn a_quoted_path_reads_back_as_the_path_it_was() {
        let paths = [
            "/Users/me/notes.txt",
            "a b",
            "a'b",
            "a\\b",
            "~/x",
            "~/a b",
            "~",
            "~/",
            "don't",
            "a$b",
            "a`b",
            "a;b",
            "a|b",
            "a\nb",
            "",
            " ",
            "a  b",
            "'",
            "\\",
            "a\\",
            "ünïcø∂é",
            "a*b",
            "C:\\Users\\me",
            "~/My Documents",
            "#a",
            "-x",
            "a=b",
        ];
        for quoting in [Quoting::Posix, Quoting::Cmd, Quoting::PowerShell] {
            for path in paths {
                let quoted = quote_as(path, quoting);
                assert_eq!(
                    unquote_word(&quoted, quoting),
                    path,
                    "{quoting:?} did not round-trip {path:?} (as {quoted:?})"
                );
            }
        }

        // The one that does not, and cannot arise. Stated so that a future
        // reader meets the limit here rather than in a bug report.
        assert_eq!(
            unquote_word(&quote_as("a\"b", Quoting::Cmd), Quoting::Cmd),
            "ab"
        );
        for quoting in [Quoting::Posix, Quoting::PowerShell] {
            let path = "a\"b";
            assert_eq!(unquote_word(&quote_as(path, quoting), quoting), path);
        }
    }

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
        assert_eq!(quote_for_shell("it's here", Some("zsh")), r"'it'\''s here'");
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

    /// An apostrophe is the one character the three dialects disagree about,
    /// and `C:\Users\O'Brien` is a real Windows home directory. Whatever a
    /// shell is handed has to be what comes back out of it.
    #[test]
    fn unquoting_undoes_what_quoting_did_in_every_dialect() {
        for shell in [Some("zsh"), Some("powershell.exe"), Some("cmd.exe")] {
            for path in [
                "/Users/me/My File (1).txt",
                "it's here",
                r"C:\Users\me\My Docs",
                r"C:\Users\O'Brien\notes.txt",
                "~/My Documents",
            ] {
                let quoted = quote_for_shell(path, shell);
                assert_eq!(
                    unquote_word(&quoted, quoting_for(shell)),
                    path,
                    "round trip of {path} under {shell:?} (quoted as {quoted})"
                );
            }
        }
    }

    /// PowerShell does not join a quoted string to the bare word beside it, so
    /// the POSIX `'\''` seam is not a seam there — it is three tokens.
    #[test]
    fn powershell_doubles_an_embedded_quote_where_posix_breaks_out() {
        assert_eq!(
            quote_for_shell(r"C:\Users\O'Brien\a.txt", Some("powershell.exe")),
            r"'C:\Users\O''Brien\a.txt'"
        );
        assert_eq!(
            quote_for_shell("it's here", Some("pwsh")),
            "'it''s here'",
            "pwsh speaks the same dialect"
        );
        assert_eq!(
            quote_for_shell(r"C:\Users\O'Brien\a.txt", Some("CMD.EXE")),
            "\"C:\\Users\\O'Brien\\a.txt\"",
            "cmd.exe quotes with \", so an apostrophe needs nothing"
        );
    }

    #[test]
    fn an_unterminated_quote_still_unquotes() {
        // What completion actually sees: the user is mid-word.
        assert_eq!(unquote_word("'My Doc", Quoting::Posix), "My Doc");
        assert_eq!(unquote_word("\"My Doc", Quoting::Cmd), "My Doc");
    }

    #[test]
    fn a_hand_typed_backslash_escape_is_honoured_only_where_it_is_one() {
        assert_eq!(
            unquote_word(r"My\ Documents", Quoting::Posix),
            "My Documents"
        );
        // On Windows the same bytes are a path, not an escape — this is the
        // half of the bug that made inline path completion unable to resolve
        // any directory there.
        assert_eq!(unquote_word(r"C:\Users\me", Quoting::Cmd), r"C:\Users\me");
        assert_eq!(
            unquote_word(r"C:\Users\me", Quoting::PowerShell),
            r"C:\Users\me"
        );
        assert_eq!(unquote_word(r"trailing\", Quoting::Posix), r"trailing\");
    }

    #[test]
    fn the_shell_decides_the_dialect() {
        assert_eq!(quoting_for(Some("zsh")), Quoting::Posix);
        assert_eq!(quoting_for(Some("/bin/bash")), Quoting::Posix);
        assert_eq!(quoting_for(Some("cmd.exe")), Quoting::Cmd);
        assert_eq!(quoting_for(Some("powershell.exe")), Quoting::PowerShell);
        assert_eq!(quoting_for(Some("POWERSHELL.EXE")), Quoting::PowerShell);
        assert_eq!(quoting_for(Some("pwsh")), Quoting::PowerShell);
        let posix_escapes_for = |s| quoting_for(s) == Quoting::Posix;
        assert!(posix_escapes_for(Some("zsh")));
        assert!(posix_escapes_for(Some("/bin/bash")));
        assert!(!posix_escapes_for(Some("cmd.exe")));
        assert!(!posix_escapes_for(Some("powershell.exe")));
        assert!(!posix_escapes_for(Some("pwsh")));
        assert_eq!(posix_escapes_for(None), !cfg!(windows));
    }

    /// What each dialect makes of a name that argues with it.
    ///
    /// cmd had no quoting test at all — only `unquote_word` and `quoting_for`
    /// — so what it does with an awkward name was written down nowhere.
    #[test]
    fn every_dialect_quotes_a_name_that_argues_with_it() {
        // A space is the ordinary reason to quote, and all three manage it.
        assert_eq!(quote_as("My Documents", Quoting::Cmd), "\"My Documents\"");
        assert_eq!(
            quote_as("My Documents", Quoting::PowerShell),
            "'My Documents'"
        );
        assert_eq!(quote_as("My Documents", Quoting::Posix), "'My Documents'");

        // The quote character itself, which is the one each dialect has its
        // own answer for. A Windows path cannot contain one, so cmd is not
        // asked the question.
        assert_eq!(quote_as("it's", Quoting::PowerShell), "'it''s'");
        assert_eq!(quote_as("it's", Quoting::Posix), r"'it'\''s'");

        // Characters a POSIX shell would act on are inert inside single
        // quotes, which is the whole point of choosing them.
        for hostile in ["a b; rm -rf ~", "$(id)", "`id`", "a|b", "x&y", "*"] {
            let quoted = quote_as(hostile, Quoting::Posix);
            assert!(
                quoted.starts_with('\'') && quoted.ends_with('\''),
                "{hostile:?} came back unquoted as {quoted:?}"
            );
        }

        // And nothing hostile slips through `is_bare` unquoted.
        for bare in ["plain", "a-b_c.d", "~", "path/to/file", "1+1"] {
            assert_eq!(quote_as(bare, Quoting::Posix), bare, "{bare:?} was quoted");
        }
    }

    /// A `%` in a name is the one thing cmd cannot be told to hold literally.
    ///
    /// Legal in a Windows filename, and expanded by cmd inside double quotes
    /// before they are parsed, with no escape available on an interactive
    /// line. Pinned rather than fixed: what it costs is the wrong path, since
    /// `%` substitutes an environment variable and can do nothing else. If a
    /// later cmd ever grows an escape, this test is where to change the
    /// answer.
    #[test]
    fn a_percent_in_a_name_is_the_one_thing_cmd_cannot_be_told_to_hold() {
        assert_eq!(
            quote_as("%USERPROFILE%.txt", Quoting::Cmd),
            "\"%USERPROFILE%.txt\"",
            "the quoting is all cmd has; the expansion happens anyway"
        );
        // The other two dialects do hold it, so this is cmd's alone.
        assert_eq!(
            quote_as("%USERPROFILE%.txt", Quoting::PowerShell),
            "'%USERPROFILE%.txt'"
        );
        assert_eq!(
            quote_as("%USERPROFILE%.txt", Quoting::Posix),
            "'%USERPROFILE%.txt'"
        );
        // And it is quoted at all only because `is_bare` excludes `%`; were it
        // bare, the name would go through with no quoting whatsoever.
        assert!(!"%".chars().all(is_bare), "`%` must never count as bare");
    }
}

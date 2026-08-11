//! Key names for `tty7 send --key`.
//!
//! `send` types text, and text is enough right up until the pane is showing
//! something that text cannot answer: a permission prompt whose options are
//! chosen with the arrow keys, a TUI to be dismissed with Escape, a build to be
//! interrupted with Ctrl-C. Those are keystrokes, not characters — the caller
//! would otherwise have to know that Ctrl-C is byte 0x03 and that Up is
//! `ESC [ A`, and write them into a shell string without a typo.
//!
//! The vocabulary is deliberately the orchestration subset rather than every
//! key a terminal can encode: the modifier-plus-function-key combinations live
//! in a much larger table (and a mode-dependent one), and nothing here needs
//! them. What is missing can still be sent as literal text.

use std::fmt;

/// A parsed key: the bytes to write, plus the spelling the caller used so
/// errors and `--json` can name it back to them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub name: String,
    pub bytes: Vec<u8>,
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

/// The named keys, in the order `--help` should list them.
///
/// Cursor keys are written in their CSI ("normal") form rather than the SS3
/// form a terminal in application-cursor mode sends. Both are widely accepted
/// by readline and by TUI toolkits, and CSI is what a pane emits until an
/// application asks for the other — so it is the form that is right when we
/// cannot know which mode the pane is in.
const NAMED: &[(&str, &[u8])] = &[
    ("enter", b"\r"),
    ("escape", b"\x1b"),
    ("tab", b"\t"),
    ("backtab", b"\x1b[Z"),
    ("space", b" "),
    ("backspace", b"\x7f"),
    ("delete", b"\x1b[3~"),
    ("up", b"\x1b[A"),
    ("down", b"\x1b[B"),
    ("right", b"\x1b[C"),
    ("left", b"\x1b[D"),
    ("home", b"\x1b[H"),
    ("end", b"\x1b[F"),
    ("pageup", b"\x1b[5~"),
    ("pagedown", b"\x1b[6~"),
];

/// Spellings that mean one of the above. Keeping these as aliases rather than
/// entries of their own keeps the `--help` list short while accepting the name
/// whichever terminal's documentation the caller learned it from.
const ALIASES: &[(&str, &str)] = &[
    ("return", "enter"),
    ("cr", "enter"),
    ("esc", "escape"),
    ("del", "delete"),
    ("bs", "backspace"),
    ("shift-tab", "backtab"),
    ("pgup", "pageup"),
    ("pgdn", "pagedown"),
    ("pgdown", "pagedown"),
];

/// What `--help` prints, and what an unknown name is answered with.
pub fn vocabulary() -> String {
    let named: Vec<&str> = NAMED.iter().map(|(name, _)| *name).collect();
    format!("{}, C-<char> (Ctrl), M-<char> (Alt)", named.join(", "))
}

/// clap's `value_parser` for `--key`, so an unknown name is a usage error
/// caught before a single byte reaches the pane — sending half a key sequence
/// and then failing would leave the pane in a state nobody asked for.
pub fn parse(spelling: &str) -> Result<Key, String> {
    let folded = spelling.trim().to_ascii_lowercase();
    let canonical = ALIASES
        .iter()
        .find_map(|(from, to)| (*from == folded).then_some(*to))
        .unwrap_or(folded.as_str());

    if let Some((_, bytes)) = NAMED.iter().find(|(name, _)| *name == canonical) {
        return Ok(Key {
            name: canonical.to_string(),
            bytes: bytes.to_vec(),
        });
    }
    if let Some(rest) = strip_modifier(canonical, &["c-", "ctrl-", "control-"]) {
        return control(rest).map(|byte| Key {
            name: format!("c-{rest}"),
            bytes: vec![byte],
        });
    }
    // Alt is a prefixed ESC — the encoding every Unix terminal has used for it
    // since long before there was a modifier-reporting protocol to do better.
    if let Some(rest) = strip_modifier(canonical, &["m-", "alt-", "meta-"]) {
        let mut chars = rest.chars();
        return match (chars.next(), chars.next()) {
            (Some(ch), None) => {
                let mut bytes = vec![0x1b];
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                Ok(Key {
                    name: format!("m-{rest}"),
                    bytes,
                })
            }
            _ => Err(format!(
                "'{spelling}' is not a key — Alt takes a single character, as in M-x"
            )),
        };
    }
    Err(format!(
        "'{spelling}' is not a key. Known keys: {}. Anything else can be sent as text.",
        vocabulary()
    ))
}

fn strip_modifier<'a>(name: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))
        .filter(|rest| !rest.is_empty())
}

/// The C0 control byte a Ctrl-chord produces. This is the ASCII table's own
/// rule — clear the top three bits — which is why the range runs past the
/// letters and into `[ \ ] ^ _`, and why Ctrl-? is the odd one out at 0x7f.
fn control(rest: &str) -> Result<u8, String> {
    let mut chars = rest.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        return Err(format!(
            "'{rest}' is not a Ctrl chord — it takes a single character, as in C-c"
        ));
    };
    match ch {
        'a'..='z' => Ok(ch as u8 - b'a' + 1),
        '@' => Ok(0),
        '[' => Ok(0x1b),
        '\\' => Ok(0x1c),
        ']' => Ok(0x1d),
        '^' => Ok(0x1e),
        '_' => Ok(0x1f),
        '?' => Ok(0x7f),
        _ => Err(format!(
            "Ctrl-{ch} is not a control character — Ctrl takes a-z or one of @ [ \\ ] ^ _ ?"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(spelling: &str) -> Vec<u8> {
        parse(spelling)
            .unwrap_or_else(|e| panic!("{spelling} should parse: {e}"))
            .bytes
    }

    #[test]
    fn the_keys_an_orchestrator_actually_reaches_for() {
        // Interrupting a runaway command, and answering a prompt that only
        // takes keystrokes: the two things text alone cannot do.
        assert_eq!(bytes("C-c"), vec![0x03]);
        assert_eq!(bytes("escape"), vec![0x1b]);
        assert_eq!(bytes("up"), b"\x1b[A".to_vec());
        assert_eq!(bytes("down"), b"\x1b[B".to_vec());
        assert_eq!(bytes("enter"), b"\r".to_vec());
        assert_eq!(bytes("tab"), b"\t".to_vec());
    }

    #[test]
    fn spelling_is_forgiving_but_the_bytes_are_not() {
        // Case and the common aliases all land on the same key, because the
        // caller learned the name from whichever terminal they came from.
        for spelling in ["enter", "Enter", "ENTER", "return", "CR"] {
            assert_eq!(bytes(spelling), b"\r".to_vec(), "{spelling}");
        }
        for spelling in ["C-c", "c-c", "ctrl-c", "Control-C"] {
            assert_eq!(bytes(spelling), vec![0x03], "{spelling}");
        }
        assert_eq!(bytes("shift-tab"), b"\x1b[Z".to_vec());
        assert_eq!(bytes("pgdn"), b"\x1b[6~".to_vec());
    }

    #[test]
    fn the_control_range_follows_the_ascii_rule_not_a_lookup_table() {
        assert_eq!(bytes("C-a"), vec![0x01]);
        assert_eq!(bytes("C-d"), vec![0x04], "end of input");
        assert_eq!(bytes("C-l"), vec![0x0c], "clear");
        assert_eq!(bytes("C-z"), vec![0x1a], "suspend");
        assert_eq!(bytes("C-["), vec![0x1b], "the same byte as Escape");
        assert_eq!(bytes("C-?"), vec![0x7f], "the one that is not 0x00..0x1f");
    }

    #[test]
    fn alt_is_a_prefixed_escape() {
        assert_eq!(bytes("M-x"), vec![0x1b, b'x']);
        assert_eq!(bytes("alt-b"), vec![0x1b, b'b']);
    }

    /// An unknown name has to fail before anything is written: a key sequence
    /// half-delivered into a live pane is worse than one not delivered at all.
    /// So the error names the vocabulary rather than just refusing.
    #[test]
    fn an_unknown_key_is_refused_with_the_list() {
        let err = parse("f7").expect_err("f7 is outside the vocabulary");
        assert!(err.contains("not a key"), "{err}");
        assert!(
            err.contains("escape"),
            "the error should list what is known: {err}"
        );

        let err = parse("C-cc").expect_err("a Ctrl chord takes one character");
        assert!(err.contains("single character"), "{err}");

        let err = parse("C-1").expect_err("Ctrl-1 is not a control character");
        assert!(err.contains("a-z"), "{err}");
    }
}

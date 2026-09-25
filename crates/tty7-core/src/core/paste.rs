//! What a paste puts on the wire.
//!
//! Lives here rather than beside the GUI's clipboard handling because the GUI
//! is not its only sender: `tty7 send --paste` has to frame a payload exactly
//! the way a paste into the window does, and a second copy of the framing is
//! how the two would come to disagree about what sending multi-line text means.

/// Opens a bracketed paste (DEC mode 2004).
pub const START: &[u8] = b"\x1b[200~";
/// Closes one.
pub const END: &[u8] = b"\x1b[201~";

/// `payload` inside paste brackets, with every ESC taken out.
///
/// The stripping is the point, not a cleanup: a payload that carries its own
/// `ESC[201~` would otherwise end the paste early, and whatever follows it
/// would reach the shell as typed input — which is how pasted text turns into
/// commands. No escape sequence survives the strip, so none can close the
/// bracket from inside.
pub fn bracket(payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(START.len() + payload.len() + END.len());
    bytes.extend_from_slice(START);
    bytes.extend(payload.iter().copied().filter(|&b| b != 0x1b));
    bytes.extend_from_slice(END);
    bytes
}

/// The bytes a paste of `text` sends, for a pane that has (`bracketed`) or has
/// not switched bracketed paste on.
///
/// CRLF folds to one line break either way, so a Windows clipboard pastes like
/// any other. Framed, the breaks stay `\n` and the shell inserts them as text.
/// Unframed there is no way to say "text" to a pty, so each break goes as the
/// `\r` a keyboard's Enter sends — the lines run one at a time, which is what
/// pasting into a terminal without the mode has always meant.
pub fn paste_bytes(text: &[u8], bracketed: bool) -> Vec<u8> {
    let mut folded = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i] == b'\r' && text.get(i + 1) == Some(&b'\n') {
            folded.push(b'\n');
            i += 2;
        } else {
            folded.push(text[i]);
            i += 1;
        }
    }
    if bracketed {
        bracket(&folded)
    } else {
        for b in &mut folded {
            if *b == b'\n' {
                *b = b'\r';
            }
        }
        folded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_payload_cannot_close_its_own_bracket() {
        let out = bracket(b"a\x1b[201~; rm -rf /\nb");
        let inner = &out[START.len()..out.len() - END.len()];
        assert!(out.starts_with(START) && out.ends_with(END));
        assert!(!inner.contains(&0x1b), "{inner:?}");
        assert_eq!(inner, b"a[201~; rm -rf /\nb");
    }

    #[test]
    fn crlf_folds_before_either_framing() {
        assert_eq!(paste_bytes(b"a\r\nb\n", true), b"\x1b[200~a\nb\n\x1b[201~");
        assert_eq!(paste_bytes(b"a\r\nb\nc", false), b"a\rb\rc");
    }

    #[test]
    fn a_lone_cr_is_left_alone() {
        assert_eq!(paste_bytes(b"a\rb", true), b"\x1b[200~a\rb\x1b[201~");
        assert_eq!(paste_bytes(b"a\rb", false), b"a\rb");
    }

    #[test]
    fn unframed_paste_keeps_escapes() {
        // Only the frame needs protecting; without one there is nothing to
        // close, and the GUI has always sent these bytes through untouched.
        assert_eq!(paste_bytes(b"a\x1b[201~b", false), b"a\x1b[201~b");
    }

    #[test]
    fn bytes_that_are_not_utf8_survive() {
        assert_eq!(
            paste_bytes(b"\xff\xfe", true),
            b"\x1b[200~\xff\xfe\x1b[201~"
        );
    }
}

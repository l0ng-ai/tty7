//! Keyboard input for the terminal view: translating GPUI keystrokes into the
//! byte sequences a PTY expects, and bridging the platform IME (NSTextInputClient
//! on macOS) so CJK and dead-key input composes and commits into the terminal.

use alacritty_terminal::term::TermMode;
use gpui::{App, Bounds, InputHandler, Pixels, UTF16Selection, Window};

use super::view::TerminalView;

/// The Kitty keyboard-protocol progressive-enhancement flags currently active in
/// the terminal, distilled from `TermMode`. We read them straight off the client's
/// local `Term` (which the reader thread advances over *all* child output, so its
/// mode bits already reflect every `CSI = flags u` push/pop the app sent — the fork
/// runs that state machine for us). Only the bits the encoder actually consults are
/// kept, so the struct stays small and `Copy`.
#[derive(Clone, Copy, Default)]
pub(super) struct KittyFlags {
    /// `DISAMBIGUATE_ESC_CODES` (level 1): escape otherwise-ambiguous keys
    /// (Tab vs Ctrl+I, Esc, Ctrl+letter, …) as `CSI … u`.
    disambiguate: bool,
    /// `REPORT_ALL_KEYS_AS_ESC`: encode *every* key as `CSI … u`, including plain
    /// text keys — not just the ambiguous ones.
    report_all_keys: bool,
    /// `REPORT_ASSOCIATED_TEXT`: include the produced text as a third `CSI u`
    /// field, so full-mode apps still receive the character.
    report_text: bool,
}

impl KittyFlags {
    pub(super) fn from_mode(mode: &TermMode) -> Self {
        Self {
            disambiguate: mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
            report_all_keys: mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC),
            report_text: mode.contains(TermMode::REPORT_ASSOCIATED_TEXT),
        }
    }

    /// Whether any level of the protocol is active (so the encoder should run).
    pub(super) fn active(self) -> bool {
        self.disambiguate || self.report_all_keys
    }
}

/// Translate a GPUI keystroke into the bytes a PTY expects.
///
/// When the app has enabled the Kitty keyboard protocol (`kitty.active()`) we try
/// the `CSI u` encoder first; anything it declines to encode (plain text keys at the
/// disambiguate level, keys it doesn't special-case) falls through to the *unchanged*
/// legacy path. So with the protocol off — the overwhelmingly common case — the
/// output is byte-for-byte identical to before.
pub(super) fn keystroke_to_bytes(ks: &gpui::Keystroke, kitty: KittyFlags) -> Option<Vec<u8>> {
    // Cmd (platform) chords are app-shortcut territory, resolved before we get here;
    // never Kitty-encode them, so their behavior is unchanged whether or not the
    // protocol is on.
    if kitty.active() && !ks.modifiers.platform {
        if let Some(bytes) = encode_kitty(ks, kitty) {
            return Some(bytes);
        }
    }
    legacy_keystroke_to_bytes(ks)
}

/// Bytes for a Tab / Shift-Tab press. These keys reach the PTY through the
/// `SendTab` / `SendBackTab` actions (not `on_key_down`), so the Kitty encoding
/// lives here rather than in [`encode_kitty`]. Mirrors the encoder's rule for the
/// legacy control keys: plain unmodified Tab stays legacy `\t` even under
/// DISAMBIGUATE (so a shell survives a crashed TUI leaving the mode on); it
/// becomes `CSI 9 u` only when Shift makes it ambiguous or REPORT_ALL_KEYS_AS_ESC
/// escapes every key. Back-tab keeps its legacy `CSI Z` form when the protocol is
/// off.
pub(super) fn tab_bytes(shift: bool, kitty: KittyFlags) -> Vec<u8> {
    if kitty.active() && (shift || kitty.report_all_keys) {
        // Shift adds the modifier subfield (mods = 1 + shift = 2).
        if shift {
            b"\x1b[9;2u".to_vec()
        } else {
            b"\x1b[9u".to_vec()
        }
    } else if shift {
        b"\x1b[Z".to_vec()
    } else {
        b"\t".to_vec()
    }
}

/// Kitty keyboard-protocol (`CSI u`) encoder. Covers the DISAMBIGUATE_ESC_CODES
/// level well, plus enough of REPORT_ALL_KEYS_AS_ESC / REPORT_ASSOCIATED_TEXT to be
/// usable at the full level. Returns `None` for keys it deliberately leaves to the
/// legacy path — chiefly plain text keys at the disambiguate level, which must still
/// be sent as raw UTF-8.
///
/// Spec: <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>
///
/// TODO: REPORT_EVENT_TYPES (press/repeat/release event-type subfield) and
/// REPORT_ALTERNATE_KEYS (shifted / base-layout alternate key codes) are not encoded
/// yet — we report key *presses* at the primary code only. That's a safe subset:
/// apps degrade to press-only behavior rather than misbehaving.
fn encode_kitty(ks: &gpui::Keystroke, kitty: KittyFlags) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    // Modifier bitmask per spec: value = 1 + shift(1) + alt(2) + ctrl(4) + super(8).
    // Super (Cmd) is intentionally excluded — platform chords never reach here.
    let mut mods = 1u32;
    if m.shift {
        mods += 1;
    }
    if m.alt {
        mods += 2;
    }
    if m.control {
        mods += 4;
    }

    // Escape is disambiguated to `CSI 27 u` whenever the protocol is active — that
    // is the whole point of DISAMBIGUATE_ESC_CODES (tell a plain Esc apart from an
    // escape-sequence introducer).
    if ks.key.as_str() == "escape" {
        return Some(csi_u(27, mods, None));
    }

    // Enter / Tab / Backspace are the three legacy control keys the spec keeps as
    // plain `\r` / `\t` / 0x7f under DISAMBIGUATE alone, so a shell stays usable if
    // a crashed app leaves the mode on (typing `reset⏎` must still send a real CR).
    // They escalate to `CSI u` only when a modifier makes them ambiguous, or under
    // REPORT_ALL_KEYS_AS_ESC (which reports *every* key as an escape code).
    let legacy_ctrl_code = match ks.key.as_str() {
        "enter" => Some(13u32),
        "tab" => Some(9),
        "backspace" => Some(127),
        _ => None,
    };
    if let Some(code) = legacy_ctrl_code {
        if mods == 1 && !kitty.report_all_keys {
            return None; // unmodified at the disambiguate level → legacy path
        }
        return Some(csi_u(code, mods, None));
    }

    // Functional keys encoded in the legacy CSI layout (letter- or tilde-suffixed).
    // The Kitty protocol keeps these forms and just adds the modifier subfield.
    if let Some(seq) = kitty_functional(ks.key.as_str(), mods) {
        return Some(seq);
    }

    // Text-producing keys. At the disambiguate level these are only escaped when a
    // Ctrl/Alt modifier makes them ambiguous (e.g. Ctrl+I vs Tab); otherwise we
    // return None so the legacy path sends the raw character. With
    // REPORT_ALL_KEYS_AS_ESC, every text key is escaped.
    let modified = m.control || m.alt;
    if modified || kitty.report_all_keys {
        if let Some(code) = text_key_code(ks) {
            let text = kitty.report_text.then(|| associated_text(ks)).flatten();
            return Some(csi_u(code, mods, text.as_deref()));
        }
    }

    None
}

/// Build a `CSI <code> ; <mods> [; <text>] u` sequence. The modifier subfield is
/// omitted when it's the default (1) and there's no text; when text is present the
/// (possibly-default) modifier subfield must be kept so the text lands in field 3.
fn csi_u(code: u32, mods: u32, text: Option<&[u32]>) -> Vec<u8> {
    let mut s = format!("\x1b[{code}");
    match text {
        Some(cps) => {
            let joined = cps.iter().map(u32::to_string).collect::<Vec<_>>().join(":");
            s.push_str(&format!(";{mods};{joined}"));
        }
        None if mods != 1 => s.push_str(&format!(";{mods}")),
        None => {}
    }
    s.push('u');
    s.into_bytes()
}

/// Kitty encoding for the CSI-layout functional keys (arrows / Home / End as
/// `CSI [1;mods] letter`, Insert / Delete / Page keys as `CSI n[;mods] ~`). Returns
/// `None` for keys handled elsewhere. With no modifiers these collapse to exactly
/// the legacy forms, so unmodified navigation is unchanged.
fn kitty_functional(key: &str, mods: u32) -> Option<Vec<u8>> {
    let letter = match key {
        "up" => Some('A'),
        "down" => Some('B'),
        "right" => Some('C'),
        "left" => Some('D'),
        "home" => Some('H'),
        "end" => Some('F'),
        _ => None,
    };
    if let Some(l) = letter {
        let s = if mods != 1 {
            format!("\x1b[1;{mods}{l}")
        } else {
            format!("\x1b[{l}")
        };
        return Some(s.into_bytes());
    }
    let num = match key {
        "insert" => Some(2u32),
        "delete" => Some(3),
        "pageup" => Some(5),
        "pagedown" => Some(6),
        _ => None,
    };
    if let Some(n) = num {
        let s = if mods != 1 {
            format!("\x1b[{n};{mods}~")
        } else {
            format!("\x1b[{n}~")
        };
        return Some(s.into_bytes());
    }
    None
}

/// The primary Kitty key code for a text-producing key: the Unicode codepoint of the
/// key's *unshifted* value (lowercased for ASCII letters), per the spec. `None` for
/// multi-character named keys (which aren't single text keys).
fn text_key_code(ks: &gpui::Keystroke) -> Option<u32> {
    match ks.key.as_str() {
        "space" => Some(0x20),
        key => {
            let mut chars = key.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None; // a multi-char key name, not a single text key
            }
            Some(c.to_ascii_lowercase() as u32)
        }
    }
}

/// The associated text (field 3) for REPORT_ASSOCIATED_TEXT: the codepoints of the
/// character(s) the key would produce, or `None` when it produces none (e.g. a
/// control chord) so the field is omitted.
fn associated_text(ks: &gpui::Keystroke) -> Option<Vec<u32>> {
    let ch = ks.key_char.as_deref()?;
    // Drop control codes: the Kitty spec requires the associated-text field to
    // contain no control characters — "code points below U+0020 and codepoints in
    // the C0 and C1 blocks". That's C0 (< 0x20) plus DEL (0x7f) and the C1 block
    // (0x80..=0x9f); leaving those in would emit a control codepoint a conformant
    // receiver must reject. A control chord's "char" carries no meaningful text
    // anyway, so filtering them just omits the field.
    let cps: Vec<u32> = ch
        .chars()
        .map(|c| c as u32)
        .filter(|&c| c >= 0x20 && !(0x7f..=0x9f).contains(&c))
        .collect();
    (!cps.is_empty()).then_some(cps)
}

/// The legacy (pre-Kitty) keystroke encoding. Untouched from the original
/// `keystroke_to_bytes` body, so behavior with the Kitty protocol off is unchanged.
fn legacy_keystroke_to_bytes(ks: &gpui::Keystroke) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    let key = ks.key.as_str();

    // Control combinations → C0 control bytes.
    if m.control && !m.platform {
        let b = match key {
            "space" | "2" => Some(0x00),
            "a" => Some(0x01),
            "b" => Some(0x02),
            "c" => Some(0x03),
            "d" => Some(0x04),
            "e" => Some(0x05),
            "f" => Some(0x06),
            "g" => Some(0x07),
            "h" => Some(0x08),
            "i" => Some(0x09),
            "j" => Some(0x0a),
            "k" => Some(0x0b),
            "l" => Some(0x0c),
            "m" => Some(0x0d),
            "n" => Some(0x0e),
            "o" => Some(0x0f),
            "p" => Some(0x10),
            "q" => Some(0x11),
            "r" => Some(0x12),
            "s" => Some(0x13),
            "t" => Some(0x14),
            "u" => Some(0x15),
            "v" => Some(0x16),
            "w" => Some(0x17),
            "x" => Some(0x18),
            "y" => Some(0x19),
            "z" => Some(0x1a),
            "[" => Some(0x1b),
            "\\" => Some(0x1c),
            "]" => Some(0x1d),
            _ => None,
        };
        if let Some(b) = b {
            // Alt (Meta) held with the Ctrl chord prefixes ESC, matching xterm's
            // metaSendsEscape (default on) and the Alt handling in the special-key
            // and printable branches below — so `Ctrl+Alt+c` sends `\x1b\x03`, not a
            // bare `\x03` that's indistinguishable from plain Ctrl+C. Without this,
            // `M-C-<key>` bindings (Emacs, readline, tmux) silently lose the Meta bit.
            if m.alt {
                return Some(vec![0x1b, b]);
            }
            return Some(vec![b]);
        }
    }

    // Named / special keys.
    let seq: Option<&[u8]> = match key {
        "enter" => Some(b"\r"),
        "tab" => Some(b"\t"),
        "backspace" => Some(b"\x7f"),
        "escape" => Some(b"\x1b"),
        "up" => Some(b"\x1b[A"),
        "down" => Some(b"\x1b[B"),
        "right" => Some(b"\x1b[C"),
        "left" => Some(b"\x1b[D"),
        "home" => Some(b"\x1b[H"),
        "end" => Some(b"\x1b[F"),
        "pageup" => Some(b"\x1b[5~"),
        "pagedown" => Some(b"\x1b[6~"),
        "delete" => Some(b"\x1b[3~"),
        "insert" => Some(b"\x1b[2~"),
        _ => None,
    };
    if let Some(seq) = seq {
        // Alt + special key → ESC prefix.
        if m.alt {
            let mut v = vec![0x1b];
            v.extend_from_slice(seq);
            return Some(v);
        }
        return Some(seq.to_vec());
    }

    // Printable text. Ignore when Cmd is held (app shortcut territory).
    if m.platform {
        return None;
    }
    if let Some(ch) = &ks.key_char {
        if !ch.is_empty() {
            let mut v = Vec::new();
            if m.alt {
                v.push(0x1b);
            }
            v.extend_from_slice(ch.as_bytes());
            return Some(v);
        }
    }
    None
}

/// Bridges the platform IME (NSTextInputClient on macOS) to the terminal.
///
/// Without this, a CJK input method's composed text is never delivered: pinyin
/// keystrokes leak through as raw latin and the committed characters go nowhere.
/// `prefers_ime_for_printable_keys` is the crucial bit — it tells GPUI to route
/// printable keys to the IME first when a non-ASCII input source is active, so
/// composition actually starts.
pub struct TerminalInputHandler {
    view: gpui::Entity<TerminalView>,
    /// Cursor cell bounds in window coordinates, for placing the candidate window.
    cursor_bounds: Option<Bounds<Pixels>>,
}

impl TerminalInputHandler {
    pub fn new(view: gpui::Entity<TerminalView>, cursor_bounds: Option<Bounds<Pixels>>) -> Self {
        Self {
            view,
            cursor_bounds,
        }
    }
}

impl InputHandler for TerminalInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &mut self,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<std::ops::Range<usize>> {
        let marked = &self.view.read(cx).marked_text;
        if marked.is_empty() {
            None
        } else {
            Some(0..marked.encode_utf16().count())
        }
    }

    fn text_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        _adjusted: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut App,
    ) {
        let text = text.to_string();
        self.view.update(cx, |view, cx| {
            view.clear_marked_text(cx);
            view.input_text(&text, cx);
        });
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut App,
    ) {
        let new_text = new_text.to_string();
        self.view
            .update(cx, |view, cx| view.set_marked_text(new_text, cx));
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut App) {
        self.view.update(cx, |view, cx| view.clear_marked_text(cx));
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        let mut bounds = self.cursor_bounds?;
        let cell_width = self.view.read(cx).cell_width;
        bounds.origin.x += cell_width * range_utf16.start as f32;
        Some(bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }

    fn prefers_ime_for_printable_keys(&mut self, _window: &mut Window, _cx: &mut App) -> bool {
        // Route printable keys to the IME so CJK composes. Whether the committed
        // text lands in the terminal or the search query is decided by focus in
        // `input_text` — so opening the search bar no longer disables CJK input in
        // the terminal, and the search field composes too.
        //
        // Linux exception: gpui's IBus integration does not reliably commit plain
        // ASCII back through `replace_text_in_range`, so forcing IME routing here
        // swallows ordinary letters — the key never reaches the terminal at all
        // (Enter/Tab/arrows still work because they bypass the IME as non-printable
        // keys). Until that gpui path handles pass-through ASCII, keep printable
        // keys on the direct `on_key_down`/`key_char` path on Linux. Trade-off:
        // CJK composition is disabled on Linux for now (Linux support is still
        // experimental); ASCII typing is restored.
        !cfg!(target_os = "linux")
    }
}

#[cfg(test)]
mod tests {
    use super::{KittyFlags, keystroke_to_bytes, tab_bytes};
    use gpui::{Keystroke, Modifiers};

    /// The legacy call shape used by the pre-existing tests: encode with the Kitty
    /// protocol off, exercising exactly the byte output shells see by default.
    fn legacy(ks: &Keystroke) -> Option<Vec<u8>> {
        keystroke_to_bytes(ks, KittyFlags::default())
    }

    fn ks(mods: Modifiers, key: &str, key_char: Option<&str>) -> Keystroke {
        Keystroke {
            modifiers: mods,
            key: key.to_string(),
            key_char: key_char.map(str::to_string),
        }
    }

    #[test]
    fn keystroke_to_bytes_maps_control_letters() {
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(legacy(&ks(ctrl, "c", None)), Some(vec![0x03]));
        assert_eq!(legacy(&ks(ctrl, "a", None)), Some(vec![0x01]));
        assert_eq!(legacy(&ks(ctrl, "space", None)), Some(vec![0x00]));
    }

    #[test]
    fn keystroke_to_bytes_ctrl_alt_letter_prefixes_meta_escape() {
        // Ctrl+Alt+letter must carry the Meta ESC prefix (xterm metaSendsEscape),
        // just like Alt+special-key and Alt+printable do below — otherwise the Alt
        // bit is silently dropped and `Ctrl+Alt+c` is indistinguishable from Ctrl+C.
        let ctrl_alt = Modifiers {
            control: true,
            alt: true,
            ..Default::default()
        };
        assert_eq!(legacy(&ks(ctrl_alt, "c", None)), Some(vec![0x1b, 0x03]));
        assert_eq!(legacy(&ks(ctrl_alt, "a", None)), Some(vec![0x1b, 0x01]));
        assert_eq!(legacy(&ks(ctrl_alt, "[", None)), Some(vec![0x1b, 0x1b]));
        // Ctrl alone (no Alt) is unchanged: a bare C0 byte, no ESC prefix.
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(legacy(&ks(ctrl, "c", None)), Some(vec![0x03]));
    }

    #[test]
    fn keystroke_to_bytes_maps_named_keys_and_alt_prefix() {
        let none = Modifiers::default();
        assert_eq!(legacy(&ks(none, "enter", None)), Some(b"\r".to_vec()));
        assert_eq!(legacy(&ks(none, "up", None)), Some(b"\x1b[A".to_vec()));
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        // Alt + a special key is prefixed with ESC.
        assert_eq!(legacy(&ks(alt, "up", None)), Some(b"\x1b\x1b[A".to_vec()));
    }

    #[test]
    fn keystroke_to_bytes_emits_printable_text_but_not_under_cmd() {
        let none = Modifiers::default();
        assert_eq!(legacy(&ks(none, "a", Some("a"))), Some(b"a".to_vec()));
        // Cmd-held printable keys are app-shortcut territory -> no PTY bytes.
        let cmd = Modifiers {
            platform: true,
            ..Default::default()
        };
        assert_eq!(legacy(&ks(cmd, "a", Some("a"))), None);
    }

    #[test]
    fn keystroke_to_bytes_maps_control_symbols_and_digit_two() {
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        // Ctrl+[ / Ctrl+\ / Ctrl+] map to the ESC/FS/GS control bytes.
        assert_eq!(legacy(&ks(ctrl, "[", None)), Some(vec![0x1b]));
        assert_eq!(legacy(&ks(ctrl, "\\", None)), Some(vec![0x1c]));
        assert_eq!(legacy(&ks(ctrl, "]", None)), Some(vec![0x1d]));
        // Ctrl+2 is another spelling of NUL.
        assert_eq!(legacy(&ks(ctrl, "2", None)), Some(vec![0x00]));
        // The full letter range boundaries.
        assert_eq!(legacy(&ks(ctrl, "h", None)), Some(vec![0x08]));
        assert_eq!(legacy(&ks(ctrl, "z", None)), Some(vec![0x1a]));
    }

    #[test]
    fn keystroke_to_bytes_ctrl_plus_cmd_is_not_a_c0_byte() {
        // Ctrl held together with Cmd (platform) is app territory, not a C0 byte;
        // it falls through the C0 table and, being non-printable under Cmd, yields None.
        let ctrl_cmd = Modifiers {
            control: true,
            platform: true,
            ..Default::default()
        };
        assert_eq!(legacy(&ks(ctrl_cmd, "c", None)), None);
    }

    #[test]
    fn keystroke_to_bytes_covers_the_named_key_table() {
        let none = Modifiers::default();
        let cases: &[(&str, &[u8])] = &[
            ("tab", b"\t"),
            ("backspace", b"\x7f"),
            ("escape", b"\x1b"),
            ("down", b"\x1b[B"),
            ("right", b"\x1b[C"),
            ("left", b"\x1b[D"),
            ("home", b"\x1b[H"),
            ("end", b"\x1b[F"),
            ("pageup", b"\x1b[5~"),
            ("pagedown", b"\x1b[6~"),
            ("delete", b"\x1b[3~"),
            ("insert", b"\x1b[2~"),
        ];
        for (key, seq) in cases {
            assert_eq!(
                legacy(&ks(none, key, None)).as_deref(),
                Some(*seq),
                "named key {key}"
            );
        }
    }

    #[test]
    fn keystroke_to_bytes_alt_prefixes_printable_and_ignores_empty_char() {
        // Alt + a printable char is prefixed with ESC (meta) before the bytes.
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(legacy(&ks(alt, "b", Some("b"))), Some(b"\x1bb".to_vec()));
        // An empty key_char produces no bytes (nothing to send).
        let none = Modifiers::default();
        assert_eq!(legacy(&ks(none, "f7", Some(""))), None);
        // An unknown key with no char is unmapped.
        assert_eq!(legacy(&ks(none, "f7", None)), None);
    }

    #[test]
    fn keystroke_to_bytes_emits_multibyte_utf8_char() {
        let none = Modifiers::default();
        // A composed character commits its UTF-8 bytes verbatim.
        assert_eq!(
            legacy(&ks(none, "é", Some("é"))),
            Some("é".as_bytes().to_vec())
        );
    }

    /// A disambiguate-level `KittyFlags` for the encoder tests.
    fn kitty() -> KittyFlags {
        KittyFlags {
            disambiguate: true,
            report_all_keys: false,
            report_text: false,
        }
    }

    #[test]
    fn kitty_disambiguates_special_keys_and_ctrl_i_vs_tab() {
        let none = Modifiers::default();
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        // Tab and Ctrl+I stay distinct: plain Tab keeps its legacy `\t` at the
        // disambiguate level, while Ctrl+I is escaped to CSI 105;5 u.
        assert_eq!(
            keystroke_to_bytes(&ks(none, "tab", None), kitty()),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(ctrl, "i", None), kitty()),
            Some(b"\x1b[105;5u".to_vec())
        );
        // Escape IS disambiguated to CSI 27 u at this level...
        assert_eq!(
            keystroke_to_bytes(&ks(none, "escape", None), kitty()),
            Some(b"\x1b[27u".to_vec())
        );
        // ...but the spec keeps plain Enter / Backspace on their legacy bytes so a
        // shell stays usable if a crashed app leaves the mode on.
        assert_eq!(
            keystroke_to_bytes(&ks(none, "enter", None), kitty()),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(none, "backspace", None), kitty()),
            Some(b"\x7f".to_vec())
        );
    }

    #[test]
    fn kitty_disambiguate_keeps_plain_enter_tab_backspace_legacy() {
        // Regression: at the DISAMBIGUATE level, plain (unmodified) Enter / Tab /
        // Backspace must stay legacy `\r` / `\t` / 0x7f — otherwise `reset⏎` can't
        // rescue a shell after a crashed TUI leaves the mode set (the exact case the
        // spec's exception exists for). Before the fix these emitted CSI 13/9/127 u.
        let none = Modifiers::default();
        assert_eq!(
            keystroke_to_bytes(&ks(none, "enter", None), kitty()),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(none, "tab", None), kitty()),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(none, "backspace", None), kitty()),
            Some(b"\x7f".to_vec())
        );

        // A modifier makes them ambiguous, so they DO escalate to CSI u carrying the
        // modifier subfield: Ctrl+Enter -> CSI 13;5 u, Alt+Backspace -> CSI 127;3 u,
        // Shift+Enter -> CSI 13;2 u.
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            keystroke_to_bytes(&ks(ctrl, "enter", None), kitty()),
            Some(b"\x1b[13;5u".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(alt, "backspace", None), kitty()),
            Some(b"\x1b[127;3u".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(shift, "enter", None), kitty()),
            Some(b"\x1b[13;2u".to_vec())
        );
    }

    #[test]
    fn kitty_report_all_keys_escapes_plain_enter_tab_backspace() {
        // Under REPORT_ALL_KEYS_AS_ESC every key is an escape code, including the
        // three legacy control keys even with no modifier.
        let full = KittyFlags {
            disambiguate: true,
            report_all_keys: true,
            report_text: false,
        };
        let none = Modifiers::default();
        assert_eq!(
            keystroke_to_bytes(&ks(none, "enter", None), full),
            Some(b"\x1b[13u".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(none, "tab", None), full),
            Some(b"\x1b[9u".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(none, "backspace", None), full),
            Some(b"\x1b[127u".to_vec())
        );
    }

    #[test]
    fn tab_bytes_follows_the_disambiguate_rule() {
        // Tab reaches the PTY via the SendTab action, so its Kitty encoding lives in
        // `tab_bytes`; it must follow the same rule as the on_key_down encoder.
        let off = KittyFlags::default();
        // Protocol off: legacy Tab / back-tab, unchanged.
        assert_eq!(tab_bytes(false, off), b"\t".to_vec());
        assert_eq!(tab_bytes(true, off), b"\x1b[Z".to_vec());
        // Disambiguate: plain Tab stays legacy `\t`; Shift-Tab escalates to CSI 9;2 u.
        assert_eq!(tab_bytes(false, kitty()), b"\t".to_vec());
        assert_eq!(tab_bytes(true, kitty()), b"\x1b[9;2u".to_vec());
        // Report-all: even plain Tab is escaped.
        let full = KittyFlags {
            disambiguate: true,
            report_all_keys: true,
            report_text: false,
        };
        assert_eq!(tab_bytes(false, full), b"\x1b[9u".to_vec());
    }

    #[test]
    fn kitty_defers_plain_text_to_legacy() {
        let none = Modifiers::default();
        // A plain letter still sends raw text at the disambiguate level.
        assert_eq!(
            keystroke_to_bytes(&ks(none, "a", Some("a")), kitty()),
            Some(b"a".to_vec())
        );
    }

    #[test]
    fn kitty_escapes_ctrl_and_alt_text_chords() {
        // At the disambiguate level, a Ctrl/Alt modifier makes a text key
        // ambiguous, so it escalates to CSI u with the modifier subfield —
        // instead of the legacy ESC-prefix / C0 forms.
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        // Ctrl+Space would be an ambiguous NUL byte → CSI 32;5 u.
        assert_eq!(
            keystroke_to_bytes(&ks(ctrl, "space", None), kitty()),
            Some(b"\x1b[32;5u".to_vec())
        );
        // Alt+b escapes as CSI 98;3 u (not the legacy ESC-prefixed "b").
        assert_eq!(
            keystroke_to_bytes(&ks(alt, "b", Some("b")), kitty()),
            Some(b"\x1b[98;3u".to_vec())
        );
    }

    #[test]
    fn kitty_encodes_functional_keys_with_modifiers() {
        let none = Modifiers::default();
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        // Shift+Up carries the modifier subfield; unmodified Up keeps the bare form.
        assert_eq!(
            keystroke_to_bytes(&ks(shift, "up", None), kitty()),
            Some(b"\x1b[1;2A".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(none, "up", None), kitty()),
            Some(b"\x1b[A".to_vec())
        );
        // Tilde-form keys likewise: Shift+Delete -> CSI 3;2 ~.
        assert_eq!(
            keystroke_to_bytes(&ks(shift, "delete", None), kitty()),
            Some(b"\x1b[3;2~".to_vec())
        );
    }

    #[test]
    fn kitty_report_all_keys_escapes_plain_text_with_associated_text() {
        let full = KittyFlags {
            disambiguate: true,
            report_all_keys: true,
            report_text: true,
        };
        let none = Modifiers::default();
        // 'a' -> CSI 97 ; 1 ; 97 u  (code ; mods ; text codepoint).
        assert_eq!(
            keystroke_to_bytes(&ks(none, "a", Some("a")), full),
            Some(b"\x1b[97;1;97u".to_vec())
        );
    }

    #[test]
    fn kitty_associated_text_drops_del_and_c1_controls() {
        let full = KittyFlags {
            disambiguate: true,
            report_all_keys: true,
            report_text: true,
        };
        let none = Modifiers::default();
        // The Kitty spec forbids control codes in the associated-text field (C0,
        // DEL and the C1 block). A key whose reported char is a lone DEL (U+007F)
        // or a C1 control (e.g. U+0085) must NOT land that codepoint in field 3;
        // with no printable text left, the field is omitted entirely -> CSI 97 u.
        assert_eq!(
            keystroke_to_bytes(&ks(none, "a", Some("\u{7f}")), full),
            Some(b"\x1b[97u".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(none, "a", Some("\u{85}")), full),
            Some(b"\x1b[97u".to_vec())
        );
        // A printable char mixed with a control keeps only the printable codepoint
        // in field 3 (the control is dropped, not the whole field): 'a' + DEL -> 97.
        assert_eq!(
            keystroke_to_bytes(&ks(none, "a", Some("a\u{7f}")), full),
            Some(b"\x1b[97;1;97u".to_vec())
        );
    }

    #[test]
    fn kitty_off_is_byte_identical_to_legacy() {
        let none = KittyFlags::default();
        assert!(!none.active());
        let mods = Modifiers::default();
        // With the protocol off, output matches the legacy path exactly.
        assert_eq!(
            keystroke_to_bytes(&ks(mods, "tab", None), none),
            Some(b"\t".to_vec())
        );
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(
            keystroke_to_bytes(&ks(ctrl, "i", None), none),
            Some(vec![0x09])
        );
    }

    #[test]
    fn kitty_never_encodes_cmd_chords() {
        // Cmd (platform) chords stay app-shortcut territory even with Kitty on:
        // the same `None`/legacy result as before.
        let cmd = Modifiers {
            platform: true,
            ..Default::default()
        };
        assert_eq!(keystroke_to_bytes(&ks(cmd, "a", Some("a")), kitty()), None);
    }
}

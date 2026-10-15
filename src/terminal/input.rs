use alacritty_terminal::term::TermMode;
use gpui::{App, Bounds, InputHandler, Pixels, UTF16Selection, Window};

use super::view::TerminalView;
#[cfg(target_os = "macos")]
use crate::core::config::Config;

/// Everything about the terminal's current state that changes how a keystroke
/// is encoded: keyboard modes and the PTY that receives the bytes.
#[derive(Clone, Copy, Default)]
pub(super) struct KeyFlags {
    disambiguate: bool,
    report_all_keys: bool,
    report_text: bool,
    /// DECCKM. ncurses apps turn this on via `smkx` and then only recognise the
    /// SS3 form of the arrow keys, because that is what `kcuu1` & co. spell.
    app_cursor: bool,
    local_conpty: bool,
}

impl KeyFlags {
    pub(super) fn from_mode(mode: &TermMode) -> Self {
        Self {
            disambiguate: mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
            report_all_keys: mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC),
            report_text: mode.contains(TermMode::REPORT_ASSOCIATED_TEXT),
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            local_conpty: false,
        }
    }

    pub(super) fn from_mode_with_local_conpty(mode: &TermMode, local_conpty: bool) -> Self {
        Self {
            local_conpty,
            ..Self::from_mode(mode)
        }
    }

    pub(super) fn kitty_active(self) -> bool {
        self.disambiguate || self.report_all_keys
    }

    pub(super) fn legacy_newline_bytes(self) -> &'static [u8] {
        if self.local_conpty {
            // ConPTY decodes bare LF as Ctrl+Enter. Explicit Ctrl+J events
            // preserve the key for native readers and still produce LF for
            // VT readers such as ssh and WSL.
            b"\x1b[74;36;10;1;8;1_\x1b[74;36;10;0;8;1_"
        } else {
            b"\n"
        }
    }

    pub(super) fn app_cursor(self) -> bool {
        self.app_cursor
    }
}

pub(super) fn reshape_option_keystroke(
    ks: &gpui::Keystroke,
    option_as_alt: bool,
) -> Option<gpui::Keystroke> {
    let m = &ks.modifiers;
    if !m.alt || m.platform || m.control {
        return None;
    }
    if option_as_alt {
        let mut chars = ks.key.chars();
        let base = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        let ch = if m.shift {
            base.to_uppercase().to_string()
        } else {
            base.to_string()
        };
        if ks.key_char.as_deref() == Some(ch.as_str()) {
            return None;
        }
        let mut out = ks.clone();
        out.key_char = Some(ch);
        Some(out)
    } else {
        let ch = ks.key_char.as_deref()?;
        if ch.is_empty() || ch.chars().any(|c| c < '\u{20}' || c == '\u{7f}') {
            return None;
        }
        let mut out = ks.clone();
        out.modifiers.alt = false;
        Some(out)
    }
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn defer_to_ime(ks: &gpui::Keystroke, flags: KeyFlags) -> bool {
    if flags.report_all_keys {
        return false;
    }
    let m = &ks.modifiers;
    if m.control || m.platform || m.function || m.alt {
        return false;
    }
    ks.key_char
        .as_deref()
        .is_some_and(|ch| !ch.is_empty() && ch.chars().all(|c| c >= '\u{20}' && c != '\u{7f}'))
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn meta_chord_bypasses_ime(ks: &gpui::Keystroke, option_as_alt: bool) -> bool {
    let m = &ks.modifiers;
    option_as_alt && m.alt && !m.platform && !m.control
}

pub(super) fn keystroke_to_bytes(ks: &gpui::Keystroke, flags: KeyFlags) -> Option<Vec<u8>> {
    if flags.kitty_active() && !ks.modifiers.platform {
        if let Some(bytes) = encode_kitty(ks, flags) {
            return Some(bytes);
        }
    }
    legacy_keystroke_to_bytes(ks, flags)
}

pub(super) fn tab_bytes(shift: bool, flags: KeyFlags) -> Vec<u8> {
    if flags.kitty_active() && (shift || flags.report_all_keys) {
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

/// xterm's modifier parameter: 1 plus a bitmask of shift/alt/control. The
/// platform (cmd) modifier has no xterm encoding and is deliberately left out.
fn xterm_mods(m: &gpui::Modifiers) -> u32 {
    1 + u32::from(m.shift) + 2 * u32::from(m.alt) + 4 * u32::from(m.control)
}

fn encode_kitty(ks: &gpui::Keystroke, kitty: KeyFlags) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    let mods = xterm_mods(m);

    if ks.key.as_str() == "escape" {
        return Some(csi_u(27, mods, None));
    }

    let legacy_ctrl_code = match ks.key.as_str() {
        "enter" => Some(13u32),
        "tab" => Some(9),
        "backspace" => Some(127),
        _ => None,
    };
    if let Some(code) = legacy_ctrl_code {
        if mods == 1 && !kitty.report_all_keys {
            return None;
        }
        return Some(csi_u(code, mods, None));
    }

    // F3 is the one function key the kitty protocol does not share with
    // terminfo. Its first version allowed both `CSI R` and `CSI 13~`, then
    // dropped the letter form outright: `CSI 1;2R` is also a Cursor Position
    // Report for row 1, column 2, so a client that negotiated the protocol
    // cannot tell Shift+F3 from an answer to its own DSR. The table gives F3
    // as `CSI 13~` alone -- the VT220 `kf3` -- so that is what an app that
    // asked for the protocol is told, while the legacy path below keeps the
    // `\EOR` our `$TERM` spells.
    if ks.key.as_str() == "f3" {
        let s = if mods == 1 {
            "\x1b[13~".to_string()
        } else {
            format!("\x1b[13;{mods}~")
        };
        return Some(s.into_bytes());
    }

    if let Some(seq) = functional_key(ks.key.as_str(), mods, kitty.app_cursor()) {
        return Some(seq);
    }

    if let Some(code) = kitty_function_key(ks.key.as_str()) {
        return Some(csi_u(code, mods, None));
    }

    let modified = m.control || m.alt;
    if modified || kitty.report_all_keys {
        if let Some(code) = text_key_code(ks) {
            let text = kitty.report_text.then(|| associated_text(ks)).flatten();
            return Some(csi_u(code, mods, text.as_deref()));
        }
    }

    None
}

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

/// The cursor, editing and function keys, encoded the way `xterm-256color`'s terminfo
/// says they are — which is what we advertise in `$TERM`, and what ncurses
/// matches against byte for byte.
///
/// Unmodified, the letter keys follow DECCKM: `CSI A` normally, `SS3 A` once
/// the app has turned on application cursor keys (`smkx`). Modified, they are
/// always the `CSI 1;<mods>A` form — xterm ignores DECCKM there, and so does
/// terminfo (`kUP3=\E[1;3A` and friends).
fn functional_key(key: &str, mods: u32, app_cursor: bool) -> Option<Vec<u8>> {
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
        let s = match (mods, app_cursor) {
            (1, false) => format!("\x1b[{l}"),
            (1, true) => format!("\x1bO{l}"),
            _ => format!("\x1b[1;{mods}{l}"),
        };
        return Some(s.into_bytes());
    }
    if let Some(form) = function_key(key) {
        let s = match form {
            // `kf1=\EOP` .. `kf4=\EOS`, and modified the `CSI 1;<mods>` form
            // the cursor keys use — terminfo spells Shift+F1 `kf13=\E[1;2P`.
            // DECCKM has no say here: unlike `kcuu1`, `kf1` is SS3 under both
            // `smkx` and `rmkx`.
            FunctionKey::Ss3(l) if mods == 1 => format!("\x1bO{l}"),
            FunctionKey::Ss3(l) => format!("\x1b[1;{mods}{l}"),
            FunctionKey::Tilde(n) if mods == 1 => format!("\x1b[{n}~"),
            FunctionKey::Tilde(n) => format!("\x1b[{n};{mods}~"),
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

/// The two shapes `xterm-256color` gives a function key.
enum FunctionKey {
    /// `SS3 <letter>` unmodified, `CSI 1;<mods> <letter>` with a modifier.
    Ss3(char),
    /// `CSI <n> ~`, or `CSI <n>;<mods> ~` with a modifier.
    Tilde(u32),
}

/// `f1`..`f12` — the name gpui gives these keys on all three platforms, and
/// the range `xterm-256color` gives a key of its own.
///
/// The numbering is the PC-style table xterm has used since patch #94 and the
/// one `kf5`..`kf12` spell: it starts at 15 and skips both 16 and 22, because
/// those two were DEC's "do" and "help" on the VT220 keypad. Guessing a
/// contiguous run here is the classic way to make F6 arrive as F5.
///
/// This deliberately stops at F12. In the entry we advertise, `kf13` onwards
/// are not further keys — they are the *modified* forms of F1..F8 (`kf13` is
/// `\E[1;2P`, Shift+F1), which the `mods` parameter above already produces.
/// A physical F13 therefore has no encoding of its own under this `$TERM`;
/// sending the VT220 `\E[25~` for it would hand ncurses a sequence its own
/// table reads back as Shift+F1, which is worse than sending nothing.
/// `kitty_function_key` picks them up for the one protocol that *can* name
/// them without that clash.
fn function_key(key: &str) -> Option<FunctionKey> {
    Some(match key {
        "f1" => FunctionKey::Ss3('P'),
        "f2" => FunctionKey::Ss3('Q'),
        "f3" => FunctionKey::Ss3('R'),
        "f4" => FunctionKey::Ss3('S'),
        "f5" => FunctionKey::Tilde(15),
        "f6" => FunctionKey::Tilde(17),
        "f7" => FunctionKey::Tilde(18),
        "f8" => FunctionKey::Tilde(19),
        "f9" => FunctionKey::Tilde(20),
        "f10" => FunctionKey::Tilde(21),
        "f11" => FunctionKey::Tilde(23),
        "f12" => FunctionKey::Tilde(24),
        _ => return None,
    })
}

/// `f13`..`f24`, encodable only once the kitty protocol is negotiated. Kitty
/// gives them codepoints of its own in the private use area — `CSI 57376 u` is
/// F13, up to `CSI 57387 u` for F24 — so the ambiguity that stops
/// `function_key` at F12 does not arise: nothing else in that protocol spells
/// 57376. An app that never asked for the protocol still gets nothing, because
/// there is nothing in `xterm-256color` to send it.
fn kitty_function_key(key: &str) -> Option<u32> {
    let n: u32 = key.strip_prefix('f')?.parse().ok()?;
    (13..=24).contains(&n).then(|| 57376 + (n - 13))
}

/// Whether a key name is one of the function keys — the range that means
/// nothing to a text editor and everything to a shell. The inline editor asks
/// this to know a keystroke it holds no meaning for but the shell does; it
/// still only hands over what actually encodes, which for F13 and up is the
/// kitty path alone.
pub(crate) fn is_function_key(key: &str) -> bool {
    function_key(key).is_some() || kitty_function_key(key).is_some()
}

fn text_key_code(ks: &gpui::Keystroke) -> Option<u32> {
    match ks.key.as_str() {
        "space" => Some(0x20),
        key => {
            let mut chars = key.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Some(c.to_ascii_lowercase() as u32)
        }
    }
}

fn associated_text(ks: &gpui::Keystroke) -> Option<Vec<u32>> {
    let ch = ks.key_char.as_deref()?;
    let cps: Vec<u32> = ch
        .chars()
        .map(|c| c as u32)
        .filter(|&c| c >= 0x20 && !(0x7f..=0x9f).contains(&c))
        .collect();
    (!cps.is_empty()).then_some(cps)
}

/// The C0 control byte a `Ctrl+<key>` chord stands for, or `None` when the
/// chord is not a control code at all.
///
/// The letters fold onto `0x01..=0x1A` — `Ctrl+A` is 1, `Ctrl+Z` is 26 — which
/// is the whole alphabet in one line instead of twenty-six. The rest is the
/// VT-220 table (chapter 3.2.5): the digits 2..8, and beside each the
/// punctuation that shares its key, because `Ctrl+^` is typed as Ctrl+Shift+6
/// and every platform hands that over as `^` with the Shift already spent.
///
/// `Ctrl+/` is not in that table. xterm and every terminal since encode it as
/// US and editors bind against it — vim's `<C-/>` — but unlike `Ctrl+[` and
/// its neighbours no keyboard layer folds it into a control byte for us, so it
/// has to be spelled out here. `Ctrl+-` is deliberately absent: off macOS that
/// is Decrease Font Size, and the chord this table owes readline's undo is
/// `Ctrl+_`, which arrives as `_`.
fn ctrl_c0(key: &str) -> Option<u8> {
    if let [b] = key.as_bytes()
        && b.is_ascii_alphabetic()
    {
        return Some(b.to_ascii_uppercase() & 0x1f);
    }
    Some(match key {
        "space" | "2" | "@" => 0x00,
        "3" | "[" => 0x1b,
        "4" | "\\" => 0x1c,
        "5" | "]" => 0x1d,
        "6" | "^" => 0x1e,
        "7" | "_" | "/" => 0x1f,
        "8" | "?" => 0x7f,
        _ => return None,
    })
}

fn legacy_keystroke_to_bytes(ks: &gpui::Keystroke, flags: KeyFlags) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    let key = ks.key.as_str();

    if m.control && !m.platform {
        if let Some(b) = ctrl_c0(key) {
            if b == b'\n' && !m.alt && !m.shift {
                return Some(flags.legacy_newline_bytes().to_vec());
            }
            if m.alt {
                return Some(vec![0x1b, b]);
            }
            return Some(vec![b]);
        }
    }

    if let Some(seq) = functional_key(key, xterm_mods(m), flags.app_cursor()) {
        return Some(seq);
    }

    let seq: Option<&[u8]> = match key {
        "enter" => Some(b"\r"),
        "tab" => Some(b"\t"),
        "backspace" => Some(b"\x7f"),
        "escape" => Some(b"\x1b"),
        _ => None,
    };
    if let Some(seq) = seq {
        if m.alt {
            let mut v = vec![0x1b];
            v.extend_from_slice(seq);
            return Some(v);
        }
        return Some(seq.to_vec());
    }

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

pub struct TerminalInputHandler {
    view: gpui::Entity<TerminalView>,
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

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }

    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    fn prefers_ime_for_printable_keys(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        #[cfg(target_os = "macos")]
        if meta_chord_bypasses_ime(keystroke, cx.global::<Config>().macos_option_as_alt) {
            return false;
        }
        if self.view.read(cx).key_flags().report_all_keys {
            return false;
        }
        if window.has_pending_keystrokes() {
            return false;
        }
        !cfg!(target_os = "linux")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        KeyFlags, defer_to_ime, keystroke_to_bytes, meta_chord_bypasses_ime,
        reshape_option_keystroke, tab_bytes,
    };
    use alacritty_terminal::term::TermMode;
    use gpui::{Keystroke, Modifiers};

    fn full_mode() -> KeyFlags {
        KeyFlags {
            disambiguate: true,
            report_all_keys: true,
            report_text: true,
            app_cursor: false,
            local_conpty: false,
        }
    }

    fn disambiguate_only() -> KeyFlags {
        KeyFlags {
            disambiguate: true,
            report_all_keys: false,
            report_text: false,
            app_cursor: false,
            local_conpty: false,
        }
    }

    fn legacy(ks: &Keystroke) -> Option<Vec<u8>> {
        keystroke_to_bytes(ks, KeyFlags::default())
    }

    fn ks(mods: Modifiers, key: &str, key_char: Option<&str>) -> Keystroke {
        Keystroke {
            modifiers: mods,
            key: key.to_string(),
            key_char: key_char.map(str::to_string),
        }
    }

    #[test]
    fn legacy_newline_preserves_ctrl_j_for_native_console_readers() {
        let ctrl_j = Keystroke::parse("ctrl-j").unwrap();
        for (local_conpty, expected) in [
            (false, b"\n".as_slice()),
            (true, b"\x1b[74;36;10;1;8;1_\x1b[74;36;10;0;8;1_".as_slice()),
        ] {
            let flags = KeyFlags::from_mode_with_local_conpty(&TermMode::empty(), local_conpty);
            assert_eq!(flags.legacy_newline_bytes(), expected);
            assert_eq!(
                keystroke_to_bytes(&ctrl_j, flags).as_deref(),
                Some(expected)
            );
            for (chord, expected) in [
                ("enter", b"\r".as_slice()),
                ("ctrl-c", b"\x03".as_slice()),
                ("alt-enter", b"\x1b\r".as_slice()),
                ("ctrl-alt-j", b"\x1b\n".as_slice()),
            ] {
                assert_eq!(
                    keystroke_to_bytes(&Keystroke::parse(chord).unwrap(), flags).as_deref(),
                    Some(expected),
                    "{chord}, local_conpty={local_conpty}"
                );
            }
        }
    }

    #[test]
    fn kitty_newline_chords_take_precedence_over_conpty_encoding() {
        for mode in [
            TermMode::DISAMBIGUATE_ESC_CODES,
            TermMode::REPORT_ALL_KEYS_AS_ESC,
        ] {
            let flags = KeyFlags::from_mode_with_local_conpty(&mode, true);
            for (chord, expected) in [
                ("ctrl-j", b"\x1b[106;5u".as_slice()),
                ("shift-enter", b"\x1b[13;2u".as_slice()),
            ] {
                assert_eq!(
                    keystroke_to_bytes(&Keystroke::parse(chord).unwrap(), flags).as_deref(),
                    Some(expected),
                    "{chord}"
                );
            }
        }
    }

    #[test]
    fn plain_text_defers_to_the_ime_unless_kitty_wants_every_key() {
        let plain = Modifiers::default();
        let a = ks(plain, "a", Some("a"));

        assert!(defer_to_ime(&a, KeyFlags::default()));
        assert!(defer_to_ime(&a, disambiguate_only()));

        assert!(!defer_to_ime(&a, full_mode()));
        assert_eq!(
            keystroke_to_bytes(&a, full_mode()),
            Some(b"\x1b[97;1;97u".to_vec()),
        );

        let space = ks(plain, "space", Some(" "));
        assert!(defer_to_ime(&space, KeyFlags::default()));
        assert!(!defer_to_ime(&space, full_mode()));
    }

    #[test]
    fn shifted_text_follows_the_same_ime_rule() {
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        let upper = ks(shift, "a", Some("A"));
        assert!(defer_to_ime(&upper, KeyFlags::default()));
        assert!(!defer_to_ime(&upper, full_mode()));
    }

    #[test]
    fn non_text_keys_never_defer_to_the_ime() {
        let plain = Modifiers::default();
        assert!(!defer_to_ime(&ks(plain, "left", None), KeyFlags::default()));
        assert!(!defer_to_ime(
            &ks(plain, "backspace", None),
            KeyFlags::default()
        ));
        assert!(!defer_to_ime(
            &ks(plain, "enter", Some("\n")),
            KeyFlags::default()
        ));
        assert!(!defer_to_ime(
            &ks(plain, "tab", Some("\t")),
            KeyFlags::default()
        ));
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        assert!(!defer_to_ime(
            &ks(ctrl, "c", Some("c")),
            KeyFlags::default()
        ));
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
        let ctrl_alt = Modifiers {
            control: true,
            alt: true,
            ..Default::default()
        };
        assert_eq!(legacy(&ks(ctrl_alt, "c", None)), Some(vec![0x1b, 0x03]));
        assert_eq!(legacy(&ks(ctrl_alt, "a", None)), Some(vec![0x1b, 0x01]));
        assert_eq!(legacy(&ks(ctrl_alt, "[", None)), Some(vec![0x1b, 0x1b]));
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
        // Named keys carry their modifiers in the sequence (kUP3), rather than
        // taking the meta-ESC prefix the way enter/tab/backspace do.
        assert_eq!(legacy(&ks(alt, "up", None)), Some(b"\x1b[1;3A".to_vec()));
        assert_eq!(legacy(&ks(alt, "enter", None)), Some(b"\x1b\r".to_vec()));
    }

    #[test]
    fn keystroke_to_bytes_emits_printable_text_but_not_under_cmd() {
        let none = Modifiers::default();
        assert_eq!(legacy(&ks(none, "a", Some("a"))), Some(b"a".to_vec()));
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
        assert_eq!(legacy(&ks(ctrl, "[", None)), Some(vec![0x1b]));
        assert_eq!(legacy(&ks(ctrl, "\\", None)), Some(vec![0x1c]));
        assert_eq!(legacy(&ks(ctrl, "]", None)), Some(vec![0x1d]));
        assert_eq!(legacy(&ks(ctrl, "2", None)), Some(vec![0x00]));
        assert_eq!(legacy(&ks(ctrl, "h", None)), Some(vec![0x08]));
        assert_eq!(legacy(&ks(ctrl, "z", None)), Some(vec![0x1a]));
    }

    #[test]
    fn keystroke_to_bytes_maps_the_whole_vt220_control_table() {
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        // The VT-220 table, each digit next to the punctuation that shares its
        // key: whichever of the two the platform reports, the byte is the same.
        let cases: &[(&str, u8)] = &[
            ("2", 0x00),
            ("@", 0x00),
            ("3", 0x1b),
            ("4", 0x1c),
            ("5", 0x1d),
            ("6", 0x1e),
            // vim's `Ctrl-^`, the whole reason the digits are here.
            ("^", 0x1e),
            ("7", 0x1f),
            ("_", 0x1f),
            // readline's undo; xterm's addition to the table, not VT-220's.
            ("/", 0x1f),
            ("8", 0x7f),
            ("?", 0x7f),
        ];
        for (key, byte) in cases {
            assert_eq!(
                legacy(&ks(ctrl, key, None)),
                Some(vec![*byte]),
                "ctrl-{key}"
            );
        }
        // Decrease Font Size owns Ctrl+- off macOS, and readline is served by
        // Ctrl+_ above, so the bare minus stays out of the table.
        assert_eq!(legacy(&ks(ctrl, "-", None)), None);
    }

    #[test]
    fn keystroke_to_bytes_ctrl_plus_cmd_is_not_a_c0_byte() {
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

    /// Issue #834: the function keys produced no bytes at all, on any
    /// platform. PSReadLine puts CharacterSearch on F3 and HistorySearch on
    /// F8, so on Windows this took part of the shell's normal editing surface
    /// away.
    ///
    /// The table is `xterm-256color`'s `kf1`..`kf12` verbatim, gaps included:
    /// F5 is 15 and F6 is 17, F10 is 21 and F11 is 23.
    #[test]
    fn keystroke_to_bytes_encodes_f1_through_f12_like_terminfo() {
        let none = Modifiers::default();
        let cases: &[(&str, &[u8])] = &[
            ("f1", b"\x1bOP"),
            ("f2", b"\x1bOQ"),
            ("f3", b"\x1bOR"),
            ("f4", b"\x1bOS"),
            ("f5", b"\x1b[15~"),
            ("f6", b"\x1b[17~"),
            ("f7", b"\x1b[18~"),
            ("f8", b"\x1b[19~"),
            ("f9", b"\x1b[20~"),
            ("f10", b"\x1b[21~"),
            ("f11", b"\x1b[23~"),
            ("f12", b"\x1b[24~"),
        ];
        for (key, seq) in cases {
            assert_eq!(
                legacy(&ks(none, key, None)).as_deref(),
                Some(*seq),
                "{key} unmodified"
            );
            // gpui hands a function key over with no `key_char`, but the
            // Windows backend has been known to attach an empty one; neither
            // may reach the `key_char` fallback and send nothing.
            assert_eq!(
                legacy(&ks(none, key, Some(""))).as_deref(),
                Some(*seq),
                "{key} with an empty key_char"
            );
        }
    }

    /// The modified forms are the same ones terminfo lists under `kf13`
    /// onwards: `kf13=\E[1;2P` is Shift+F1, `kf17=\E[15;2~` is Shift+F5,
    /// `kf25=\E[1;5P` is Ctrl+F1. Alt is xterm's 3, which PSReadLine wants for
    /// Alt+F7 (ClearHistory).
    #[test]
    fn keystroke_to_bytes_modifies_function_keys_like_terminfo() {
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        let ctrl_shift = Modifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        // kf13, kf16
        assert_eq!(legacy(&ks(shift, "f1", None)), Some(b"\x1b[1;2P".to_vec()));
        assert_eq!(legacy(&ks(shift, "f4", None)), Some(b"\x1b[1;2S".to_vec()));
        // kf25
        assert_eq!(legacy(&ks(ctrl, "f1", None)), Some(b"\x1b[1;5P".to_vec()));
        // kf37
        assert_eq!(
            legacy(&ks(ctrl_shift, "f1", None)),
            Some(b"\x1b[1;6P".to_vec())
        );
        // kf49
        assert_eq!(legacy(&ks(alt, "f1", None)), Some(b"\x1b[1;3P".to_vec()));
        // kf20 -- Shift+F8, PSReadLine's HistorySearchForward
        assert_eq!(legacy(&ks(shift, "f8", None)), Some(b"\x1b[19;2~".to_vec()));
        // kf55 -- Alt+F7, PSReadLine's ClearHistory
        assert_eq!(legacy(&ks(alt, "f7", None)), Some(b"\x1b[18;3~".to_vec()));
        // kf35 -- Ctrl+F11
        assert_eq!(legacy(&ks(ctrl, "f11", None)), Some(b"\x1b[23;5~".to_vec()));
    }

    /// The `mods` parameter is what makes F13 onwards ambiguous: in
    /// `xterm-256color` those capability names are already spoken for by the
    /// modified F1..F8, so a physical F13 has no sequence of its own to send.
    ///
    /// The kitty protocol has no such clash — it puts F13..F24 in the private
    /// use area, `CSI 57376 u` upwards — so a client that negotiated it does
    /// get those keys, and only those twelve: F25 is past the end of the range
    /// gpui names.
    #[test]
    fn terminfo_stops_at_f12_and_kitty_carries_on_to_f24() {
        let none = Modifiers::default();
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        let kitty_cases: &[(&str, &[u8])] = &[
            ("f13", b"\x1b[57376u"),
            ("f14", b"\x1b[57377u"),
            ("f20", b"\x1b[57383u"),
            ("f24", b"\x1b[57387u"),
        ];
        for (key, seq) in kitty_cases {
            assert_eq!(
                legacy(&ks(none, key, None)),
                None,
                "{key} has no terminfo capability"
            );
            assert_eq!(
                keystroke_to_bytes(&ks(none, key, None), kitty()).as_deref(),
                Some(*seq),
                "{key} under kitty"
            );
        }
        assert_eq!(
            keystroke_to_bytes(&ks(shift, "f13", None), kitty()),
            Some(b"\x1b[57376;2u".to_vec())
        );
        // Ctrl+F13 must not fold into a C0 byte on its first letter either.
        assert_eq!(legacy(&ks(ctrl, "f13", None)), None);
        // And the range ends where gpui's names do.
        assert_eq!(keystroke_to_bytes(&ks(none, "f25", None), kitty()), None);
        assert_eq!(legacy(&ks(none, "f25", None)), None);
    }

    /// The one function key where the two protocols disagree. Kitty's spec
    /// allowed `CSI R` for F3 in its first version and then removed it,
    /// because `CSI 1;2R` is also a Cursor Position Report for row 1, column
    /// 2 — so a client that negotiated the protocol is given `CSI 13~`, the
    /// VT220 `kf3`, while `$TERM`'s own `kf3=\EOR` still goes to everyone
    /// else. alacritty draws the same line.
    #[test]
    fn kitty_spells_f3_thirteen_because_csi_r_is_a_cursor_report() {
        let none = Modifiers::default();
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(legacy(&ks(none, "f3", None)), Some(b"\x1bOR".to_vec()));
        assert_eq!(legacy(&ks(shift, "f3", None)), Some(b"\x1b[1;2R".to_vec()));
        let full = KeyFlags {
            disambiguate: true,
            report_all_keys: true,
            report_text: true,
            app_cursor: false,
            local_conpty: false,
        };
        for flags in [kitty(), full] {
            assert_eq!(
                keystroke_to_bytes(&ks(none, "f3", None), flags),
                Some(b"\x1b[13~".to_vec())
            );
            assert_eq!(
                keystroke_to_bytes(&ks(shift, "f3", None), flags),
                Some(b"\x1b[13;2~".to_vec())
            );
            assert_eq!(
                keystroke_to_bytes(&ks(ctrl, "f3", None), flags),
                Some(b"\x1b[13;5~".to_vec())
            );
        }
        // Cmd is not a kitty modifier either, and the platform key sends the
        // keystroke down the legacy path in the first place.
        let cmd = Modifiers {
            platform: true,
            ..Default::default()
        };
        assert_eq!(
            keystroke_to_bytes(&ks(cmd, "f3", None), kitty()),
            Some(b"\x1bOR".to_vec())
        );
    }

    /// Cmd is not an xterm modifier, so it must not turn a bare F-key into a
    /// modified one — and on macOS Cmd+F-keys belong to the window anyway.
    #[test]
    fn cmd_does_not_modify_a_function_key() {
        let cmd = Modifiers {
            platform: true,
            ..Default::default()
        };
        assert_eq!(legacy(&ks(cmd, "f5", None)), Some(b"\x1b[15~".to_vec()));
    }

    fn app_cursor() -> KeyFlags {
        KeyFlags {
            app_cursor: true,
            ..Default::default()
        }
    }

    /// DECCKM governs `kcuu1` and friends, not `kf1`: terminfo spells `kf1`
    /// `\EOP` under both `smkx` and `rmkx`, so the F keys must not move when
    /// an ncurses app turns application cursor keys on.
    #[test]
    fn app_cursor_mode_leaves_the_function_keys_alone() {
        let none = Modifiers::default();
        for key in ["f1", "f4", "f5", "f12"] {
            assert_eq!(
                keystroke_to_bytes(&ks(none, key, None), app_cursor()),
                legacy(&ks(none, key, None)),
                "{key} does not follow DECCKM"
            );
        }
    }

    /// The bug behind issue #361: htop turns on DECCKM, `xterm-256color` spells
    /// `kcuu1` as `\EOA`, and ncurses matches nothing else. Sending `\E[A` there
    /// left htop reading the bytes one at a time -- and `[` is bound to "lower
    /// priority", so every Up/Down bumped the selected process's nice value.
    #[test]
    fn app_cursor_mode_switches_the_arrow_keys_to_ss3() {
        let none = Modifiers::default();
        let cases: &[(&str, &[u8])] = &[
            ("up", b"\x1bOA"),
            ("down", b"\x1bOB"),
            ("right", b"\x1bOC"),
            ("left", b"\x1bOD"),
            ("home", b"\x1bOH"),
            ("end", b"\x1bOF"),
        ];
        for (key, seq) in cases {
            assert_eq!(
                keystroke_to_bytes(&ks(none, key, None), app_cursor()).as_deref(),
                Some(*seq),
                "{key} under DECCKM"
            );
            // The kitty encoder shares the same table, so it has to agree.
            let kitty_app = KeyFlags {
                app_cursor: true,
                ..kitty()
            };
            assert_eq!(
                keystroke_to_bytes(&ks(none, key, None), kitty_app).as_deref(),
                Some(*seq),
                "{key} under DECCKM + kitty"
            );
        }
    }

    /// DECCKM only governs the unmodified form. xterm -- and terminfo's `kUP5`
    /// & co. -- keep the CSI form once a modifier is in play.
    #[test]
    fn app_cursor_mode_leaves_modified_arrows_and_tilde_keys_alone() {
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(
            keystroke_to_bytes(&ks(ctrl, "up", None), app_cursor()),
            Some(b"\x1b[1;5A".to_vec())
        );
        let none = Modifiers::default();
        for key in ["pageup", "pagedown", "delete", "insert"] {
            assert_eq!(
                keystroke_to_bytes(&ks(none, key, None), app_cursor()),
                legacy(&ks(none, key, None)),
                "{key} does not follow DECCKM"
            );
        }
    }

    #[test]
    fn keystroke_to_bytes_encodes_modified_named_keys_like_xterm() {
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        let ctrl_shift = Modifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            legacy(&ks(shift, "left", None)),
            Some(b"\x1b[1;2D".to_vec())
        );
        assert_eq!(
            legacy(&ks(ctrl, "right", None)),
            Some(b"\x1b[1;5C".to_vec())
        );
        assert_eq!(legacy(&ks(ctrl, "home", None)), Some(b"\x1b[1;5H".to_vec()));
        assert_eq!(
            legacy(&ks(ctrl_shift, "up", None)),
            Some(b"\x1b[1;6A".to_vec())
        );
        assert_eq!(
            legacy(&ks(shift, "delete", None)),
            Some(b"\x1b[3;2~".to_vec())
        );
        assert_eq!(
            legacy(&ks(ctrl, "pageup", None)),
            Some(b"\x1b[5;5~".to_vec())
        );
    }

    /// Cmd has no xterm modifier encoding, so it must not leak into the
    /// parameter and turn a bare arrow into a modified one.
    #[test]
    fn keystroke_to_bytes_ignores_cmd_when_encoding_named_keys() {
        let cmd = Modifiers {
            platform: true,
            ..Default::default()
        };
        assert_eq!(legacy(&ks(cmd, "up", None)), Some(b"\x1b[A".to_vec()));
    }

    #[test]
    fn keystroke_to_bytes_alt_prefixes_printable_and_ignores_empty_char() {
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(legacy(&ks(alt, "b", Some("b"))), Some(b"\x1bb".to_vec()));
        // `f13` stands in for "a named key with nothing behind it" — it used
        // to be `f7`, back when no function key encoded at all (#834).
        let none = Modifiers::default();
        assert_eq!(legacy(&ks(none, "f13", Some(""))), None);
        assert_eq!(legacy(&ks(none, "f13", None)), None);
    }

    #[test]
    fn keystroke_to_bytes_emits_multibyte_utf8_char() {
        let none = Modifiers::default();
        assert_eq!(
            legacy(&ks(none, "é", Some("é"))),
            Some("é".as_bytes().to_vec())
        );
    }

    fn kitty() -> KeyFlags {
        KeyFlags {
            disambiguate: true,
            report_all_keys: false,
            report_text: false,
            app_cursor: false,
            local_conpty: false,
        }
    }

    #[test]
    fn kitty_disambiguates_special_keys_and_ctrl_i_vs_tab() {
        let none = Modifiers::default();
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(
            keystroke_to_bytes(&ks(none, "tab", None), kitty()),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(ctrl, "i", None), kitty()),
            Some(b"\x1b[105;5u".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(none, "escape", None), kitty()),
            Some(b"\x1b[27u".to_vec())
        );
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
        let full = KeyFlags {
            disambiguate: true,
            report_all_keys: true,
            report_text: false,
            app_cursor: false,
            local_conpty: false,
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
        let off = KeyFlags::default();
        assert_eq!(tab_bytes(false, off), b"\t".to_vec());
        assert_eq!(tab_bytes(true, off), b"\x1b[Z".to_vec());
        assert_eq!(tab_bytes(false, kitty()), b"\t".to_vec());
        assert_eq!(tab_bytes(true, kitty()), b"\x1b[9;2u".to_vec());
        let full = KeyFlags {
            disambiguate: true,
            report_all_keys: true,
            report_text: false,
            app_cursor: false,
            local_conpty: false,
        };
        assert_eq!(tab_bytes(false, full), b"\x1b[9u".to_vec());
    }

    #[test]
    fn kitty_defers_plain_text_to_legacy() {
        let none = Modifiers::default();
        assert_eq!(
            keystroke_to_bytes(&ks(none, "a", Some("a")), kitty()),
            Some(b"a".to_vec())
        );
    }

    #[test]
    fn kitty_escapes_ctrl_and_alt_text_chords() {
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(
            keystroke_to_bytes(&ks(ctrl, "space", None), kitty()),
            Some(b"\x1b[32;5u".to_vec())
        );
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
        assert_eq!(
            keystroke_to_bytes(&ks(shift, "up", None), kitty()),
            Some(b"\x1b[1;2A".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(none, "up", None), kitty()),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(shift, "delete", None), kitty()),
            Some(b"\x1b[3;2~".to_vec())
        );
    }

    /// The kitty encoder shares `functional_key`, so the F keys have to come
    /// out of it byte-identical to the legacy path — kitty's own spec keeps
    /// the legacy CSI/SS3 forms for F1..F12 and only appends the modifier
    /// parameter, which is what that shared table already does. F3 is the sole
    /// exception and has a test of its own:
    /// `kitty_spells_f3_thirteen_because_csi_r_is_a_cursor_report`.
    ///
    /// `report_all_keys` changes nothing here either: the F keys are already
    /// escape sequences, so there is no bare byte for it to promote.
    #[test]
    fn kitty_encodes_function_keys_like_the_legacy_path() {
        let none = Modifiers::default();
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        let full = KeyFlags {
            disambiguate: true,
            report_all_keys: true,
            report_text: true,
            app_cursor: false,
            local_conpty: false,
        };
        for key in ["f1", "f2", "f4", "f5", "f7", "f10", "f11", "f12"] {
            for mods in [none, shift, ctrl] {
                let want = legacy(&ks(mods, key, None));
                assert!(want.is_some(), "{key} encodes on the legacy path");
                assert_eq!(
                    keystroke_to_bytes(&ks(mods, key, None), kitty()),
                    want,
                    "{key} under kitty disambiguate"
                );
                assert_eq!(
                    keystroke_to_bytes(&ks(mods, key, None), full),
                    want,
                    "{key} under kitty report-all-keys"
                );
            }
        }
        // Spot-check the actual bytes, so a change to `legacy` cannot quietly
        // move both sides at once.
        assert_eq!(
            keystroke_to_bytes(&ks(none, "f7", None), full),
            Some(b"\x1b[18~".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(shift, "f1", None), full),
            Some(b"\x1b[1;2P".to_vec())
        );
    }

    #[test]
    fn kitty_report_all_keys_escapes_plain_text_with_associated_text() {
        let full = KeyFlags {
            disambiguate: true,
            report_all_keys: true,
            report_text: true,
            app_cursor: false,
            local_conpty: false,
        };
        let none = Modifiers::default();
        assert_eq!(
            keystroke_to_bytes(&ks(none, "a", Some("a")), full),
            Some(b"\x1b[97;1;97u".to_vec())
        );
    }

    #[test]
    fn kitty_associated_text_drops_del_and_c1_controls() {
        let full = KeyFlags {
            disambiguate: true,
            report_all_keys: true,
            report_text: true,
            app_cursor: false,
            local_conpty: false,
        };
        let none = Modifiers::default();
        assert_eq!(
            keystroke_to_bytes(&ks(none, "a", Some("\u{7f}")), full),
            Some(b"\x1b[97u".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(none, "a", Some("\u{85}")), full),
            Some(b"\x1b[97u".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes(&ks(none, "a", Some("a\u{7f}")), full),
            Some(b"\x1b[97;1;97u".to_vec())
        );
    }

    #[test]
    fn kitty_off_is_byte_identical_to_legacy() {
        let none = KeyFlags::default();
        assert!(!none.kitty_active());
        let mods = Modifiers::default();
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

    fn reshaped_bytes(ks: &Keystroke, option_as_alt: bool, kitty: KeyFlags) -> Option<Vec<u8>> {
        let reshaped = reshape_option_keystroke(ks, option_as_alt);
        keystroke_to_bytes(reshaped.as_ref().unwrap_or(ks), kitty)
    }

    fn option_b() -> Keystroke {
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        ks(alt, "b", Some("∫"))
    }

    #[test]
    fn option_as_alt_on_sends_esc_plus_base_key() {
        assert_eq!(
            reshaped_bytes(&option_b(), true, KeyFlags::default()),
            Some(b"\x1bb".to_vec())
        );
        let alt_shift = Modifiers {
            alt: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            reshaped_bytes(&ks(alt_shift, "b", Some("ı")), true, KeyFlags::default()),
            Some(b"\x1bB".to_vec())
        );
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(
            reshaped_bytes(&ks(alt, "2", Some("™")), true, KeyFlags::default()),
            Some(b"\x1b2".to_vec())
        );
    }

    #[test]
    fn option_as_alt_off_sends_composed_text_bare() {
        assert_eq!(
            reshaped_bytes(&option_b(), false, KeyFlags::default()),
            Some("∫".as_bytes().to_vec())
        );
    }

    #[test]
    fn meta_chords_skip_the_ime_only_when_option_is_meta() {
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert!(meta_chord_bypasses_ime(&option_b(), true));
        let alt_shift = Modifiers {
            alt: true,
            shift: true,
            ..Default::default()
        };
        assert!(meta_chord_bypasses_ime(
            &ks(alt_shift, "b", Some("ı")),
            true
        ));

        assert!(!meta_chord_bypasses_ime(&option_b(), false));

        assert!(!meta_chord_bypasses_ime(
            &ks(Modifiers::default(), "n", Some("n")),
            true
        ));

        let cmd_alt = Modifiers {
            alt: true,
            platform: true,
            ..Default::default()
        };
        assert!(!meta_chord_bypasses_ime(&ks(cmd_alt, "b", None), true));
        let ctrl_alt = Modifiers {
            alt: true,
            control: true,
            ..Default::default()
        };
        assert!(!meta_chord_bypasses_ime(&ks(ctrl_alt, "b", None), true));

        assert!(meta_chord_bypasses_ime(&ks(alt, "left", None), true));
    }

    #[test]
    fn option_reshape_leaves_named_keys_and_ctrl_chords_alone() {
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        for on in [true, false] {
            assert!(reshape_option_keystroke(&ks(alt, "up", None), on).is_none());
            assert_eq!(
                reshaped_bytes(&ks(alt, "up", None), on, KeyFlags::default()),
                Some(b"\x1b[1;3A".to_vec())
            );
        }
        assert!(reshape_option_keystroke(&ks(alt, "enter", Some("\n")), false).is_none());
        let ctrl_alt = Modifiers {
            control: true,
            alt: true,
            ..Default::default()
        };
        for on in [true, false] {
            assert!(reshape_option_keystroke(&ks(ctrl_alt, "c", None), on).is_none());
            assert_eq!(
                reshaped_bytes(&ks(ctrl_alt, "c", None), on, KeyFlags::default()),
                Some(vec![0x1b, 0x03])
            );
        }
        assert!(
            reshape_option_keystroke(&ks(Modifiers::default(), "a", Some("a")), true).is_none()
        );
        assert!(reshape_option_keystroke(&ks(alt, "b", Some("b")), true).is_none());
    }

    #[test]
    fn option_reshape_composes_with_the_kitty_encoder() {
        assert_eq!(
            reshaped_bytes(&option_b(), true, kitty()),
            Some(b"\x1b[98;3u".to_vec())
        );
        assert_eq!(
            reshaped_bytes(&option_b(), false, kitty()),
            Some("∫".as_bytes().to_vec())
        );
    }

    #[test]
    fn kitty_never_encodes_cmd_chords() {
        let cmd = Modifiers {
            platform: true,
            ..Default::default()
        };
        assert_eq!(keystroke_to_bytes(&ks(cmd, "a", Some("a")), kitty()), None);
    }
}

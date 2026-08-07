use alacritty_terminal::term::TermMode;
use gpui::{App, Bounds, InputHandler, Pixels, UTF16Selection, Window};

use super::view::TerminalView;
#[cfg(target_os = "macos")]
use crate::core::config::Config;

/// Everything about the terminal's current state that changes how a keystroke
/// is encoded: the kitty protocol flags, plus DECCKM (application cursor keys).
#[derive(Clone, Copy, Default)]
pub(super) struct KeyFlags {
    disambiguate: bool,
    report_all_keys: bool,
    report_text: bool,
    /// DECCKM. ncurses apps turn this on via `smkx` and then only recognise the
    /// SS3 form of the arrow keys, because that is what `kcuu1` & co. spell.
    app_cursor: bool,
}

impl KeyFlags {
    pub(super) fn from_mode(mode: &TermMode) -> Self {
        Self {
            disambiguate: mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
            report_all_keys: mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC),
            report_text: mode.contains(TermMode::REPORT_ASSOCIATED_TEXT),
            app_cursor: mode.contains(TermMode::APP_CURSOR),
        }
    }

    pub(super) fn kitty_active(self) -> bool {
        self.disambiguate || self.report_all_keys
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

    if let Some(seq) = functional_key(ks.key.as_str(), mods, kitty.app_cursor()) {
        return Some(seq);
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

/// The cursor and editing keys, encoded the way `xterm-256color`'s terminfo
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

fn legacy_keystroke_to_bytes(ks: &gpui::Keystroke, flags: KeyFlags) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    let key = ks.key.as_str();

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
    use gpui::{Keystroke, Modifiers};

    fn full_mode() -> KeyFlags {
        KeyFlags {
            disambiguate: true,
            report_all_keys: true,
            report_text: true,
            app_cursor: false,
        }
    }

    fn disambiguate_only() -> KeyFlags {
        KeyFlags {
            disambiguate: true,
            report_all_keys: false,
            report_text: false,
            app_cursor: false,
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

    fn app_cursor() -> KeyFlags {
        KeyFlags {
            app_cursor: true,
            ..Default::default()
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
        let none = Modifiers::default();
        assert_eq!(legacy(&ks(none, "f7", Some(""))), None);
        assert_eq!(legacy(&ks(none, "f7", None)), None);
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

    #[test]
    fn kitty_report_all_keys_escapes_plain_text_with_associated_text() {
        let full = KeyFlags {
            disambiguate: true,
            report_all_keys: true,
            report_text: true,
            app_cursor: false,
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

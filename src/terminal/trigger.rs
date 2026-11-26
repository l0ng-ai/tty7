//! Triggers: type text into a pane when what is before its cursor matches a
//! pattern, e.g. to answer a `Password:` prompt. See [`Trigger`].
//!
//! The grid only exists in this process, so matching happens in the reader
//! thread right after a batch of output is fed to it.

use std::sync::{Arc, RwLock};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermMode};
use regex::Regex;

use crate::core::config::Trigger;

/// The compiled triggers of the current config, shared between the pane (which
/// swaps it on a config change) and its reader thread.
pub type SharedTriggers = Arc<RwLock<Arc<TriggerSet>>>;

#[derive(Default)]
pub struct TriggerSet {
    rules: Vec<(Regex, String)>,
}

impl TriggerSet {
    /// Disabled and empty triggers are dropped; one that doesn't compile is
    /// logged and dropped, so a typo can't take the others down with it.
    pub fn compile(triggers: &[Trigger]) -> Self {
        let rules = triggers
            .iter()
            .filter(|t| t.enabled && !t.regex.is_empty())
            .filter_map(|t| match Regex::new(&t.regex) {
                Ok(re) => Some((re, t.text.clone())),
                Err(e) => {
                    log::warn!("ignoring trigger {:?}: {e}", t.regex);
                    None
                }
            })
            .collect();
        Self { rules }
    }

    pub fn shared(triggers: &[Trigger]) -> SharedTriggers {
        Arc::new(RwLock::new(Arc::new(Self::compile(triggers))))
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// Swap in the triggers of a new config.
pub fn replace(shared: &SharedTriggers, triggers: &[Trigger]) {
    if let Ok(mut set) = shared.write() {
        *set = Arc::new(TriggerSet::compile(triggers));
    }
}

/// One pane's trigger state: which triggers already fired for the line the
/// cursor is on.
pub struct Matcher {
    shared: SharedTriggers,
    set: Arc<TriggerSet>,
    fired: Vec<bool>,
}

impl Matcher {
    pub fn new(shared: SharedTriggers) -> Self {
        Self {
            shared,
            set: Arc::default(),
            fired: Vec::new(),
        }
    }

    /// Whether any trigger is configured. Cheap enough to ask per batch, and
    /// it lets the reader skip locking the grid when there are none.
    pub fn active(&mut self) -> bool {
        self.refresh();
        !self.set.is_empty()
    }

    fn refresh(&mut self) {
        let Ok(current) = self.shared.read() else {
            return;
        };
        if !Arc::ptr_eq(&current, &self.set) {
            self.set = current.clone();
            self.fired = vec![false; self.set.rules.len()];
        }
    }

    /// The texts to send now, given the line before the cursor (`None` when
    /// triggers don't apply, which also re-arms them all).
    pub fn check(&mut self, line: Option<&str>) -> Vec<String> {
        let mut out = Vec::new();
        for ((re, text), fired) in self.set.rules.iter().zip(&mut self.fired) {
            let matched = line.is_some_and(|l| re.is_match(l));
            if matched && !*fired {
                out.push(text.clone());
            }
            *fired = matched;
        }
        out
    }
}

/// A trigger's text the way the settings page shows it: control characters
/// spelled as escapes, so a trailing Return is a visible `\r`.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// The inverse of [`escape`], for what was typed into the settings page.
/// `None` when a backslash is followed by something that isn't an escape.
pub fn unescape(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '\\' => out.push('\\'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'e' => out.push('\x1b'),
            'x' => {
                let hex: String = chars.by_ref().take(2).collect();
                if hex.len() != 2 {
                    return None;
                }
                out.push(char::from(u8::from_str_radix(&hex, 16).ok()?));
            }
            _ => return None,
        }
    }
    Some(out)
}

/// How many soft-wrapped rows above the cursor's are read as part of its line.
const MAX_WRAPPED_ROWS: i32 = 8;

/// Writes the logical line up to the cursor into `out` and says whether there
/// is any. A prompt that soft-wrapped is read from where it starts.
///
/// Nothing is reported on the alternate screen: a full-screen program isn't a
/// prompt. `out` is the caller's so that a batch doesn't cost an allocation.
pub fn text_before_cursor<T: EventListener>(term: &Term<T>, out: &mut String) -> bool {
    out.clear();
    if term.mode().contains(TermMode::ALT_SCREEN) {
        return false;
    }
    let grid = term.grid();
    let cursor = grid.cursor.point;
    let columns = grid.columns();
    // With a wrap pending the cursor sits on the cell it last wrote.
    let end = (cursor.column.0 + usize::from(grid.cursor.input_needs_wrap)).min(columns);

    let wraps = |line: i32| {
        grid[Line(line)][Column(columns - 1)]
            .flags
            .contains(Flags::WRAPLINE)
    };
    let mut first = cursor.line.0;
    while first > grid.topmost_line().0
        && cursor.line.0 - first < MAX_WRAPPED_ROWS
        && wraps(first - 1)
    {
        first -= 1;
    }

    for line in first..=cursor.line.0 {
        let row = &grid[Line(line)];
        let cols = if line == cursor.line.0 { end } else { columns };
        out.extend(
            (0..cols)
                .map(|c| &row[Column(c)])
                .filter(|cell| {
                    !cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                })
                .map(|cell| cell.c),
        );
    }
    !out.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trigger(regex: &str, text: &str) -> Trigger {
        Trigger {
            regex: regex.into(),
            text: text.into(),
            ..Trigger::default()
        }
    }

    fn matcher(triggers: &[Trigger]) -> Matcher {
        let mut m = Matcher::new(TriggerSet::shared(triggers));
        assert_eq!(m.active(), triggers.iter().any(|t| t.enabled));
        m
    }

    #[test]
    fn fires_once_per_match_and_rearms_when_the_line_stops_matching() {
        let mut m = matcher(&[trigger(r"(?i)password:\s*$", "hunter2\r")]);
        assert!(m.check(Some("ls")).is_empty());
        assert_eq!(m.check(Some("Password: ")), ["hunter2\r"]);
        // More output with the same line on screen must not answer twice.
        assert!(m.check(Some("Password: ")).is_empty());
        // Enter was pressed: the cursor is on a fresh line.
        assert!(m.check(None).is_empty());
        assert_eq!(m.check(Some("Password: ")), ["hunter2\r"]);
    }

    #[test]
    fn skips_disabled_and_invalid_triggers() {
        let mut off = trigger("a", "x");
        off.enabled = false;
        let set = TriggerSet::compile(&[off, trigger("(", "x"), trigger("", "x")]);
        assert!(set.is_empty());
    }

    #[test]
    fn a_config_change_swaps_the_rules() {
        let shared = TriggerSet::shared(&[trigger("a$", "1")]);
        let mut m = Matcher::new(shared.clone());
        assert_eq!(
            m.check(Some("a")),
            Vec::<String>::new(),
            "not refreshed yet"
        );
        assert!(m.active());
        assert_eq!(m.check(Some("a")), ["1"]);
        replace(&shared, &[trigger("b$", "2")]);
        assert!(m.active());
        assert_eq!(m.check(Some("b")), ["2"]);
    }

    fn line_after(bytes: &[u8]) -> Option<String> {
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::vte::ansi;

        let size = crate::terminal::size::TermSize::new(10, 4);
        let mut term = Term::new(
            alacritty_terminal::term::Config::default(),
            &size,
            VoidListener,
        );
        let mut processor: ansi::Processor = ansi::Processor::new();
        processor.advance(&mut term, bytes);
        let mut out = String::new();
        text_before_cursor(&term, &mut out).then_some(out)
    }

    #[test]
    fn reads_the_line_up_to_the_cursor() {
        assert_eq!(line_after(b"ls\r\nPass: ").as_deref(), Some("Pass: "));
        assert_eq!(line_after(b"done\r\n"), None);
    }

    #[test]
    fn a_line_that_fills_the_row_keeps_its_last_cell() {
        // Ten columns, ten characters: the wrap is pending, not taken.
        assert_eq!(line_after(b"Password: ").as_deref(), Some("Password: "));
        assert_eq!(line_after(b"Passphrase").as_deref(), Some("Passphrase"));
    }

    #[test]
    fn a_soft_wrapped_prompt_is_read_from_its_start() {
        assert_eq!(
            line_after(b"x\r\nEnter password: ").as_deref(),
            Some("Enter password: ")
        );
    }

    #[test]
    fn nothing_is_read_on_the_alternate_screen() {
        assert_eq!(line_after(b"\x1b[?1049hPassword: "), None);
    }

    #[test]
    fn escapes_round_trip() {
        let text = "pass\\word\r\n\t\x1b[A";
        assert_eq!(escape(text), r"pass\\word\r\n\t\x1b[A");
        assert_eq!(unescape(&escape(text)).as_deref(), Some(text));
        assert_eq!(unescape(r"a\e\x41").as_deref(), Some("a\x1bA"));
        assert_eq!(unescape(r"a\q"), None);
        assert_eq!(unescape(r"a\x4"), None);
        assert_eq!(unescape("a\\"), None);
    }
}

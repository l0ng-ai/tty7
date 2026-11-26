//! Triggers: type text into a pane when what is before its cursor matches a
//! pattern, e.g. to answer a `Password:` prompt. See [`Trigger`].
//!
//! The grid only exists in this process, so matching happens in the reader
//! thread right after a batch of output is fed to it.

use std::sync::{Arc, RwLock};

use alacritty_terminal::event::EventListener;
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

/// The text on the cursor's row up to the cursor, or `None` on the alternate
/// screen (a full-screen program isn't a prompt) or at column zero.
pub fn text_before_cursor<T: EventListener>(term: &Term<T>) -> Option<String> {
    if term.mode().contains(TermMode::ALT_SCREEN) {
        return None;
    }
    let cursor = term.grid().cursor.point;
    if cursor.column.0 == 0 {
        return None;
    }
    let row = &term.grid()[Line(cursor.line.0)];
    Some(
        (0..cursor.column.0)
            .map(|c| &row[Column(c)])
            .filter(|cell| !cell.flags.contains(Flags::WIDE_CHAR_SPACER))
            .map(|cell| cell.c)
            .collect(),
    )
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
}

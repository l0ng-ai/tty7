//! Ctrl+R history search, extracted from the terminal view so the search
//! *logic* (query editing + ranking `history` into a match list) lives apart
//! from the GPUI plumbing (focus, repaint). The view owns an
//! `Option<ReverseSearch>`, forwards keys and typed text to it, and acts on the
//! returned [`Action`] — it never reaches into the query or match list beyond
//! the read-only accessors the menu renderer uses.
//!
//! Matching is fuzzy (see the [`fuzzy`](super::fuzzy) module), blended with the
//! entry's frecency so a command you run constantly — or ran *in this
//! directory* — outranks an equally-good textual match you typed once. An
//! empty query ranks the whole history by frecency alone, so bare Ctrl+R is a
//! browsable "recent & relevant" list rather than a blank prompt.

use super::fuzzy;
use std::collections::HashSet;

/// How much an entry's frecency score (roughly `0..7`: recency `0..1` +
/// dampened frequency + current-directory bonus) adds to its fuzzy match
/// score (16+ per matched char). At 2× it decides ties and near-ties between
/// textually similar matches without ever drowning a clearly better match.
const FRECENCY_WEIGHT: f64 = 2.0;

/// In-progress search: the typed query and the ranked matches, best first.
pub(super) struct ReverseSearch {
    query: String,
    matches: Vec<Match>,
    /// Cursor into `matches`: the entry Enter accepts, highlighted in the menu.
    selected: usize,
}

/// One ranked match: where it lives in the view's chronological `history`, and
/// which of its chars the query matched (for menu highlighting; empty for the
/// empty-query frecency listing).
pub(super) struct Match {
    pub index: usize,
    pub positions: Vec<usize>,
}

/// What the view should do after handing a key to an active search.
pub(super) enum Action {
    /// Stay open; just repaint (query, matches or selection changed).
    Redraw,
    /// Close the search and leave the edited line untouched (Esc / Ctrl+G / Ctrl+C).
    Cancel,
    /// Close the search; if `Some`, load that history line into the editor
    /// (Enter — the user still presses Enter again to run it).
    Accept(Option<String>),
    /// Close the search and run that history line outright (Cmd+Enter).
    Run(String),
}

impl ReverseSearch {
    /// Open a search: the empty query immediately lists the history by
    /// frecency, so the menu is useful before a single key is typed.
    pub(super) fn new(history: &[String], frecency: &[f64]) -> Self {
        let mut rs = Self {
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
        };
        rs.update(history, frecency);
        rs
    }

    /// The typed query, for the prompt the view renders.
    pub(super) fn query(&self) -> &str {
        &self.query
    }

    /// The ranked matches, best first — the menu renders a window of these.
    pub(super) fn matches(&self) -> &[Match] {
        &self.matches
    }

    /// Index of the selected match within [`matches`](Self::matches).
    pub(super) fn selected(&self) -> usize {
        self.selected
    }

    /// The history line the selection sits on, if any.
    pub(super) fn selected_line<'a>(&self, history: &'a [String]) -> Option<&'a str> {
        self.matches
            .get(self.selected)
            .map(|m| history[m.index].as_str())
    }

    /// Recompute the match list. Entries are deduplicated by content (the most
    /// recent occurrence wins) and ranked by fuzzy score blended with frecency;
    /// an empty query ranks everything by frecency alone. `frecency` is
    /// index-aligned with `history`. Resets the selection to the best match.
    fn update(&mut self, history: &[String], frecency: &[f64]) {
        self.selected = 0;
        let list_all = self.query.trim().is_empty();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut scored: Vec<(f64, Match)> = Vec::new();
        // Newest → oldest, so the stable sort below keeps recent entries first
        // among equal scores.
        for i in (0..history.len()).rev() {
            let line = history[i].as_str();
            if !seen.insert(line) {
                continue;
            }
            let f = frecency.get(i).copied().unwrap_or(0.0);
            if list_all {
                scored.push((
                    f,
                    Match {
                        index: i,
                        positions: Vec::new(),
                    },
                ));
            } else if let Some(m) = fuzzy::match_line(line, &self.query) {
                scored.push((
                    f64::from(m.score) + FRECENCY_WEIGHT * f,
                    Match {
                        index: i,
                        positions: m.positions,
                    },
                ));
            }
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        self.matches = scored.into_iter().map(|(_, m)| m).collect();
    }

    /// Move the selection `delta` steps down the ranked list (positive → worse
    /// matches, the classic "older hit" direction of a repeated Ctrl+R),
    /// sticking at the ends.
    fn step(&mut self, delta: isize) {
        let last = self.matches.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    /// Append typed text to the query and re-rank. Text arrives either via the
    /// IME path (`replace_text_in_range` → the view's `input_text`) or, for a
    /// plain ASCII input source, as a direct `key_char` the view forwards from
    /// `handle_reverse_search_key`.
    pub(super) fn push_query(&mut self, text: &str, history: &[String], frecency: &[f64]) {
        self.query.push_str(text);
        self.update(history, frecency);
    }

    /// Handle a key while the search is active. Query text itself arrives via
    /// [`push_query`](Self::push_query); this covers the control keys only.
    pub(super) fn handle_key(
        &mut self,
        ks: &gpui::Keystroke,
        history: &[String],
        frecency: &[f64],
    ) -> Action {
        let m = &ks.modifiers;
        let key = ks.key.as_str();
        if (m.control && key == "r") || key == "down" {
            // Next (worse-ranked) match — the classic repeated-Ctrl+R step.
            self.step(1);
            Action::Redraw
        } else if (m.control && key == "s") || key == "up" {
            // Back toward the best match (readline's forward-search direction).
            self.step(-1);
            Action::Redraw
        } else if (m.control && (key == "g" || key == "c")) || key == "escape" {
            Action::Cancel
        } else if key == "enter" {
            let line = self.selected_line(history).map(str::to_string);
            match (m.platform, line) {
                // Cmd+Enter: run the selected line outright.
                (true, Some(line)) => Action::Run(line),
                // Enter: hand back the match (the user still presses Enter to
                // run it). A bare Enter with no match just exits the search.
                (_, line) => Action::Accept(line),
            }
        } else if key == "backspace" {
            self.query.pop();
            self.update(history, frecency);
            Action::Redraw
        } else {
            // Other keys are ignored while searching.
            Action::Redraw
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> Vec<String> {
        // oldest → newest
        ["git status", "cargo build", "git commit -m x", "cargo test"]
            .into_iter()
            .map(String::from)
            .collect()
    }

    /// Uniform frecency: ranking falls back to fuzzy score + recency order.
    fn flat(h: &[String]) -> Vec<f64> {
        vec![0.0; h.len()]
    }

    fn key(spec: &str) -> gpui::Keystroke {
        gpui::Keystroke::parse(spec).expect("valid keystroke spec")
    }

    #[test]
    fn empty_query_lists_everything_newest_first_under_flat_frecency() {
        let h = history();
        let rs = ReverseSearch::new(&h, &flat(&h));
        let order: Vec<usize> = rs.matches().iter().map(|m| m.index).collect();
        assert_eq!(order, [3, 2, 1, 0]);
        assert_eq!(rs.selected_line(&h), Some("cargo test"));
    }

    #[test]
    fn query_ranks_the_most_recent_equal_match_first() {
        let h = history();
        let mut rs = ReverseSearch::new(&h, &flat(&h));
        rs.push_query("git", &h, &flat(&h));
        // Both git commands match equally well; the newer one wins the tie.
        assert_eq!(rs.selected_line(&h), Some("git commit -m x"));
        assert_eq!(rs.matches().len(), 2);
    }

    #[test]
    fn fuzzy_matching_spans_words() {
        // `gst` is a subsequence of `git status` — the substring search this
        // replaces could never find it.
        let h = history();
        let mut rs = ReverseSearch::new(&h, &flat(&h));
        rs.push_query("gst", &h, &flat(&h));
        assert_eq!(rs.selected_line(&h), Some("git status"));
        // The matched positions point at g, s, t for the menu highlight.
        assert_eq!(rs.matches()[0].positions, vec![0, 4, 5]);
    }

    #[test]
    fn frecency_outranks_recency_between_equal_text_matches() {
        let h = history();
        // "git status" (oldest) is heavily used; the newer "git commit -m x"
        // is a one-off. The blend should float the frequent one on top.
        let frecency = vec![5.0, 0.0, 0.0, 0.0];
        let mut rs = ReverseSearch::new(&h, &frecency);
        rs.push_query("git", &h, &frecency);
        assert_eq!(rs.selected_line(&h), Some("git status"));
    }

    #[test]
    fn duplicates_collapse_to_their_most_recent_occurrence() {
        let h: Vec<String> = ["ls", "make", "ls"].into_iter().map(String::from).collect();
        let rs = ReverseSearch::new(&h, &flat(&h));
        let idx: Vec<usize> = rs.matches().iter().map(|m| m.index).collect();
        assert_eq!(idx, [2, 1]); // one "ls", at its newest position
    }

    #[test]
    fn ctrl_r_and_arrows_step_through_matches_and_stick_at_the_ends() {
        let h = history();
        let mut rs = ReverseSearch::new(&h, &flat(&h));
        rs.push_query("git", &h, &flat(&h));
        assert_eq!(rs.selected(), 0);
        assert!(matches!(
            rs.handle_key(&key("ctrl-r"), &h, &flat(&h)),
            Action::Redraw
        ));
        assert_eq!(rs.selected_line(&h), Some("git status"));
        // Already on the last match — a further step sticks.
        rs.handle_key(&key("ctrl-r"), &h, &flat(&h));
        assert_eq!(rs.selected(), 1);
        // Ctrl+S / Up steps back toward the best match, sticking at the top.
        rs.handle_key(&key("ctrl-s"), &h, &flat(&h));
        assert_eq!(rs.selected(), 0);
        rs.handle_key(&key("up"), &h, &flat(&h));
        assert_eq!(rs.selected(), 0);
        rs.handle_key(&key("down"), &h, &flat(&h));
        assert_eq!(rs.selected(), 1);
    }

    #[test]
    fn handle_key_cancel_keys() {
        let h = history();
        let mut rs = ReverseSearch::new(&h, &flat(&h));
        assert!(matches!(
            rs.handle_key(&key("ctrl-g"), &h, &flat(&h)),
            Action::Cancel
        ));
        assert!(matches!(
            rs.handle_key(&key("ctrl-c"), &h, &flat(&h)),
            Action::Cancel
        ));
        assert!(matches!(
            rs.handle_key(&key("escape"), &h, &flat(&h)),
            Action::Cancel
        ));
    }

    #[test]
    fn enter_accepts_and_cmd_enter_runs_the_selection() {
        let h = history();
        let mut rs = ReverseSearch::new(&h, &flat(&h));
        rs.push_query("cargo", &h, &flat(&h));
        match rs.handle_key(&key("enter"), &h, &flat(&h)) {
            Action::Accept(Some(line)) => assert_eq!(line, "cargo test"),
            _ => panic!("expected Accept(Some) with the selected line"),
        }
        let mut rs = ReverseSearch::new(&h, &flat(&h));
        rs.push_query("cargo", &h, &flat(&h));
        match rs.handle_key(&key("cmd-enter"), &h, &flat(&h)) {
            Action::Run(line) => assert_eq!(line, "cargo test"),
            _ => panic!("expected Run with the selected line"),
        }
        // A bare Enter with no match accepts nothing (just exits).
        let mut rs = ReverseSearch::new(&h, &flat(&h));
        rs.push_query("zzz_nope", &h, &flat(&h));
        assert!(rs.matches().is_empty());
        match rs.handle_key(&key("enter"), &h, &flat(&h)) {
            Action::Accept(None) => {}
            _ => panic!("expected Accept(None) with no match"),
        }
    }

    #[test]
    fn handle_key_backspace_pops_query_and_re_ranks() {
        let h = history();
        let mut rs = ReverseSearch::new(&h, &flat(&h));
        rs.push_query("gitq", &h, &flat(&h)); // no match (no q anywhere)
        assert!(rs.matches().is_empty());
        // Backspace drops the trailing 'q', restoring the git matches.
        assert!(matches!(
            rs.handle_key(&key("backspace"), &h, &flat(&h)),
            Action::Redraw
        ));
        assert_eq!(rs.query(), "git");
        assert_eq!(rs.selected_line(&h), Some("git commit -m x"));
    }

    #[test]
    fn handle_key_other_keys_are_ignored_with_redraw() {
        let h = history();
        let mut rs = ReverseSearch::new(&h, &flat(&h));
        // A plain letter is handled via push_query, not handle_key; here it's a no-op redraw.
        assert!(matches!(
            rs.handle_key(&key("a"), &h, &flat(&h)),
            Action::Redraw
        ));
    }
}

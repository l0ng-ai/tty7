//! Ctrl+R reverse-history search, extracted from the terminal view so the search
//! *logic* (query editing + scanning `history` for a match) lives apart from the
//! GPUI plumbing (focus, repaint). The view owns an `Option<ReverseSearch>`,
//! forwards keys and typed text to it, and acts on the returned [`Action`] —
//! it never reaches into the query or match index directly.

/// In-progress reverse search: the typed query and the history index of the
/// current match (the most recent match at or older than the last step).
pub(super) struct ReverseSearch {
    query: String,
    match_index: Option<usize>,
}

/// What the view should do after handing a key to an active search.
pub(super) enum Action {
    /// Stay open; just repaint (query or match changed, or the key was ignored).
    Redraw,
    /// Close the search and leave the edited line untouched (Esc / Ctrl+G / Ctrl+C).
    Cancel,
    /// Close the search; if `Some`, load that history line into the editor (Enter).
    Accept(Option<String>),
}

impl ReverseSearch {
    pub(super) fn new() -> Self {
        Self {
            query: String::new(),
            match_index: None,
        }
    }

    /// The `(reverse-i-search)` query text, for the prompt the view renders.
    pub(super) fn query(&self) -> &str {
        &self.query
    }

    /// History index of the current match, if any — the view highlights it.
    pub(super) fn match_index(&self) -> Option<usize> {
        self.match_index
    }

    /// Recompute the current match. `advance` continues to the next *older* match
    /// (Ctrl+R again); otherwise the scan starts from the newest entry.
    pub(super) fn update(&mut self, history: &[String], advance: bool) {
        if self.query.is_empty() {
            self.match_index = None;
            return;
        }
        let q = self.query.to_lowercase();
        // Upper bound (exclusive): everything when refining the query, or the
        // current match when stepping to an older one.
        let upper = if advance {
            self.match_index.unwrap_or(history.len())
        } else {
            history.len()
        };
        let found = (0..upper)
            .rev()
            .find(|&i| history[i].to_lowercase().contains(&q));
        // Stepping to an older match (Ctrl+R again) that finds nothing means we're
        // already on the oldest hit — keep it rather than blanking the match. When
        // refining the query, an empty result genuinely means "no match".
        if !(advance && found.is_none()) {
            self.match_index = found;
        }
    }

    /// Append typed text (arriving via the IME path) to the query and re-search.
    pub(super) fn push_query(&mut self, text: &str, history: &[String]) {
        self.query.push_str(text);
        self.update(history, false);
    }

    /// Handle a key while the search is active. Query text itself arrives via
    /// [`push_query`](Self::push_query); this covers the control keys only.
    pub(super) fn handle_key(&mut self, ks: &gpui::Keystroke, history: &[String]) -> Action {
        let m = &ks.modifiers;
        let key = ks.key.as_str();
        if m.control && key == "r" {
            self.update(history, true);
            Action::Redraw
        } else if (m.control && (key == "g" || key == "c")) || key == "escape" {
            Action::Cancel
        } else if key == "enter" {
            // Accept: hand back the match (the user still presses Enter to run it).
            // A bare Enter with no match just exits the search.
            Action::Accept(self.match_index.map(|i| history[i].clone()))
        } else if key == "backspace" {
            self.query.pop();
            self.update(history, false);
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

    #[test]
    fn update_finds_the_most_recent_match() {
        let h = history();
        let mut rs = ReverseSearch::new();
        rs.push_query("git", &h);
        assert_eq!(rs.match_index(), Some(2)); // "git commit -m x"
    }

    #[test]
    fn advance_steps_to_older_matches_then_sticks_on_the_oldest() {
        let h = history();
        let mut rs = ReverseSearch::new();
        rs.push_query("git", &h);
        assert_eq!(rs.match_index(), Some(2));
        rs.update(&h, true);
        assert_eq!(rs.match_index(), Some(0)); // "git status"
        rs.update(&h, true); // no older match — keep the oldest hit
        assert_eq!(rs.match_index(), Some(0));
    }

    #[test]
    fn refining_the_query_can_drop_the_match() {
        let h = history();
        let mut rs = ReverseSearch::new();
        rs.push_query("cargo", &h);
        assert_eq!(rs.match_index(), Some(3)); // "cargo test"
        rs.push_query("_nope", &h);
        assert_eq!(rs.match_index(), None);
    }

    #[test]
    fn empty_query_has_no_match() {
        let h = history();
        let mut rs = ReverseSearch::new();
        rs.update(&h, false);
        assert_eq!(rs.match_index(), None);
    }

    fn key(spec: &str) -> gpui::Keystroke {
        gpui::Keystroke::parse(spec).expect("valid keystroke spec")
    }

    #[test]
    fn handle_key_ctrl_r_steps_to_older_match_and_redraws() {
        let h = history();
        let mut rs = ReverseSearch::new();
        rs.push_query("git", &h);
        assert_eq!(rs.match_index(), Some(2));
        // Ctrl+R again advances to the older match and asks for a redraw.
        assert!(matches!(rs.handle_key(&key("ctrl-r"), &h), Action::Redraw));
        assert_eq!(rs.match_index(), Some(0));
    }

    #[test]
    fn handle_key_cancel_keys() {
        let h = history();
        let mut rs = ReverseSearch::new();
        assert!(matches!(rs.handle_key(&key("ctrl-g"), &h), Action::Cancel));
        assert!(matches!(rs.handle_key(&key("ctrl-c"), &h), Action::Cancel));
        assert!(matches!(rs.handle_key(&key("escape"), &h), Action::Cancel));
    }

    #[test]
    fn handle_key_enter_accepts_current_match_or_none() {
        let h = history();
        let mut rs = ReverseSearch::new();
        rs.push_query("cargo", &h);
        match rs.handle_key(&key("enter"), &h) {
            Action::Accept(Some(line)) => assert_eq!(line, "cargo test"),
            _ => panic!("expected Accept(Some) with the matched line"),
        }
        // A bare Enter with no active match accepts nothing (just exits).
        let mut empty = ReverseSearch::new();
        match empty.handle_key(&key("enter"), &h) {
            Action::Accept(None) => {}
            _ => panic!("expected Accept(None) with no match"),
        }
    }

    #[test]
    fn handle_key_backspace_pops_query_and_re_searches() {
        let h = history();
        let mut rs = ReverseSearch::new();
        rs.push_query("gitx", &h); // no match (no "gitx" in history)
        assert_eq!(rs.match_index(), None);
        // Backspace drops the trailing 'x', restoring the "git" match.
        assert!(matches!(
            rs.handle_key(&key("backspace"), &h),
            Action::Redraw
        ));
        assert_eq!(rs.query(), "git");
        assert_eq!(rs.match_index(), Some(2));
    }

    #[test]
    fn handle_key_other_keys_are_ignored_with_redraw() {
        let h = history();
        let mut rs = ReverseSearch::new();
        // A plain letter is handled via push_query, not handle_key; here it's a no-op redraw.
        assert!(matches!(rs.handle_key(&key("a"), &h), Action::Redraw));
    }
}

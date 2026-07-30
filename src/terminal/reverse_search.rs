use super::fuzzy;
use std::collections::HashSet;

const FRECENCY_WEIGHT: f64 = 2.0;

pub(super) struct ReverseSearch {
    query: String,
    matches: Vec<Match>,
    selected: usize,
}

pub(super) struct Match {
    pub index: usize,
    pub positions: Vec<usize>,
}

pub(super) enum Action {
    Redraw,
    Cancel,
    Accept(Option<String>),
    Run(String),
}

impl ReverseSearch {
    pub(super) fn new(history: &[String], frecency: &[f64]) -> Self {
        let mut rs = Self {
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
        };
        rs.update(history, frecency);
        rs
    }

    pub(super) fn query(&self) -> &str {
        &self.query
    }

    pub(super) fn matches(&self) -> &[Match] {
        &self.matches
    }

    pub(super) fn selected(&self) -> usize {
        self.selected
    }

    pub(super) fn selected_line<'a>(&self, history: &'a [String]) -> Option<&'a str> {
        self.matches
            .get(self.selected)
            .map(|m| history[m.index].as_str())
    }

    fn update(&mut self, history: &[String], frecency: &[f64]) {
        self.selected = 0;
        let list_all = self.query.trim().is_empty();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut scored: Vec<(f64, Match)> = Vec::new();
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

    fn step(&mut self, delta: isize) {
        let last = self.matches.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    pub(super) fn push_query(&mut self, text: &str, history: &[String], frecency: &[f64]) {
        self.query.push_str(text);
        self.update(history, frecency);
    }

    pub(super) fn handle_key(
        &mut self,
        ks: &gpui::Keystroke,
        history: &[String],
        frecency: &[f64],
    ) -> Action {
        let m = &ks.modifiers;
        let key = ks.key.as_str();
        if (m.control && key == "r") || key == "down" {
            self.step(1);
            Action::Redraw
        } else if (m.control && key == "s") || key == "up" {
            self.step(-1);
            Action::Redraw
        } else if (m.control && (key == "g" || key == "c")) || key == "escape" {
            Action::Cancel
        } else if key == "enter" || (m.control && (key == "j" || key == "m")) {
            let line = self.selected_line(history).map(str::to_string);
            match (m.platform, line) {
                (true, Some(line)) => Action::Run(line),
                (_, line) => Action::Accept(line),
            }
        } else if key == "backspace" {
            self.query.pop();
            self.update(history, frecency);
            Action::Redraw
        } else {
            Action::Redraw
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> Vec<String> {
        ["git status", "cargo build", "git commit -m x", "cargo test"]
            .into_iter()
            .map(String::from)
            .collect()
    }

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
        assert_eq!(rs.selected_line(&h), Some("git commit -m x"));
        assert_eq!(rs.matches().len(), 2);
    }

    #[test]
    fn fuzzy_matching_spans_words() {
        let h = history();
        let mut rs = ReverseSearch::new(&h, &flat(&h));
        rs.push_query("gst", &h, &flat(&h));
        assert_eq!(rs.selected_line(&h), Some("git status"));
        assert_eq!(rs.matches()[0].positions, vec![0, 4, 5]);
    }

    #[test]
    fn frecency_outranks_recency_between_equal_text_matches() {
        let h = history();
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
        assert_eq!(idx, [2, 1]);
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
        rs.handle_key(&key("ctrl-r"), &h, &flat(&h));
        assert_eq!(rs.selected(), 1);
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
        let mut rs = ReverseSearch::new(&h, &flat(&h));
        rs.push_query("zzz_nope", &h, &flat(&h));
        assert!(rs.matches().is_empty());
        match rs.handle_key(&key("enter"), &h, &flat(&h)) {
            Action::Accept(None) => {}
            _ => panic!("expected Accept(None) with no match"),
        }
    }

    #[test]
    fn ctrl_j_and_ctrl_m_accept_like_enter() {
        let h = history();
        for chord in ["ctrl-j", "ctrl-m"] {
            let mut rs = ReverseSearch::new(&h, &flat(&h));
            rs.push_query("cargo", &h, &flat(&h));
            match rs.handle_key(&key(chord), &h, &flat(&h)) {
                Action::Accept(Some(line)) => assert_eq!(line, "cargo test"),
                _ => panic!("{chord} should accept the selected line"),
            }
        }
    }

    #[test]
    fn handle_key_backspace_pops_query_and_re_ranks() {
        let h = history();
        let mut rs = ReverseSearch::new(&h, &flat(&h));
        rs.push_query("gitq", &h, &flat(&h));
        assert!(rs.matches().is_empty());
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
        assert!(matches!(
            rs.handle_key(&key("a"), &h, &flat(&h)),
            Action::Redraw
        ));
    }
}

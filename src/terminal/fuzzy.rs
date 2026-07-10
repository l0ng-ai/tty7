//! Fuzzy subsequence matching for the Ctrl+R history search.
//!
//! A small affine-gap aligner in the fzf/skim family: every query character
//! must appear in the haystack in order (a subsequence), and the returned score
//! rewards runs of consecutive matches and matches at word boundaries while
//! penalizing gaps — so `gst` prefers `git status` over `grep -rn "s" tests`.
//! The matched character positions come back too, so the menu can highlight
//! exactly which characters matched.
//!
//! Whitespace in the query splits it into terms that must *all* match
//! (anywhere, in any order) — `git push` finds `git push -f origin` but also
//! `push-all git-mirrors`. Matching is always case-insensitive, like the
//! substring search this replaces.
//!
//! Kept dependency-free on purpose: command lines are short, so the O(m×n)
//! dynamic program is comfortably cheap even against thousands of history
//! entries per keystroke.

/// A successful match: the alignment score (higher is better; only comparable
/// between matches of the *same query*) and the matched char indices into the
/// haystack, ascending and deduplicated.
pub(super) struct FuzzyMatch {
    pub score: i32,
    pub positions: Vec<usize>,
}

/// Every matched character is worth this much before bonuses.
const SCORE_MATCH: i32 = 16;
/// Bonus for a match at a word boundary (start of the line, or right after a
/// separator) — `st` should land on the `status` in `git status`.
const BONUS_BOUNDARY: i32 = 12;
/// Bonus for extending a run of consecutive matches — favours tight matches
/// over the same letters scattered across the line. Deliberately worth more
/// than a boundary bonus reached across a gap (`BONUS_BOUNDARY +
/// PENALTY_GAP_START = 9`), so `ab` still prefers the literal `ab` over the
/// two word heads of `a-b`.
const BONUS_CONSECUTIVE: i32 = 10;
/// Cost of opening a gap between two matched characters…
const PENALTY_GAP_START: i32 = -3;
/// …and of each further character that gap skips.
const PENALTY_GAP_EXTEND: i32 = -1;

/// "Impossible" sentinel. Kept far from `i32::MIN` so adding penalties/bonuses
/// to a sentinel value can never wrap around into a plausible score.
const NEG: i32 = i32::MIN / 2;

/// Match `query` against `line`. Whitespace splits the query into terms which
/// must all match; scores add up and positions merge. `None` when the query is
/// blank or any term fails to match.
pub(super) fn match_line(line: &str, query: &str) -> Option<FuzzyMatch> {
    let terms: Vec<&str> = query.split_whitespace().collect();
    if terms.is_empty() {
        return None;
    }
    let hay: Vec<char> = line.chars().collect();
    let hay_lc: Vec<char> = hay.iter().map(|&c| lc(c)).collect();
    let bonus: Vec<i32> = (0..hay.len())
        .map(|j| char_bonus(if j == 0 { None } else { Some(hay[j - 1]) }))
        .collect();

    let mut score = 0;
    let mut positions = std::collections::BTreeSet::new();
    for term in terms {
        let t: Vec<char> = term.chars().map(lc).collect();
        let (s, pos) = match_term(&hay_lc, &bonus, &t)?;
        score += s;
        positions.extend(pos);
    }
    Some(FuzzyMatch {
        score,
        positions: positions.into_iter().collect(),
    })
}

/// Lowercase a char for comparison (first mapping only — `ß`→`ss` expansions
/// don't matter for scoring command lines).
fn lc(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// The word-boundary bonus a match at a position earns, given the preceding
/// character (`None` at the start of the line).
fn char_bonus(prev: Option<char>) -> i32 {
    match prev {
        None => BONUS_BOUNDARY,
        Some(c)
            if c.is_whitespace() || matches!(c, '/' | '-' | '_' | '.' | ':' | '=' | ',' | '\\') =>
        {
            BONUS_BOUNDARY
        }
        _ => 0,
    }
}

/// Align one lowercased `term` against the lowercased haystack, returning the
/// best score and the matched positions. Classic affine-gap DP:
/// `m[i][j]` is the best score with `term[i]` matched at `hay[j]`, reachable
/// either consecutively from `m[i-1][j-1]` or across a gap (tracked by a
/// running per-row maximum so each cell is O(1)); `parent[i][j]` remembers the
/// chosen predecessor for the backtrack that recovers the positions.
fn match_term(hay_lc: &[char], bonus: &[i32], term: &[char]) -> Option<(i32, Vec<usize>)> {
    let (m, n) = (term.len(), hay_lc.len());
    if m == 0 || m > n {
        return None;
    }
    let mut score = vec![NEG; m * n];
    let mut parent = vec![usize::MAX; m * n];

    for j in 0..n {
        if hay_lc[j] == term[0] {
            score[j] = SCORE_MATCH + bonus[j];
        }
    }
    for i in 1..m {
        // Best gapped predecessor for the current j: max over k ≤ j-2 of
        // `score[i-1][k]` plus the affine penalty for the k→j gap.
        let mut gap_best = NEG;
        let mut gap_arg = usize::MAX;
        for j in 0..n {
            if j >= 2 {
                let fresh = score[(i - 1) * n + (j - 2)];
                let fresh = if fresh > NEG {
                    fresh + PENALTY_GAP_START
                } else {
                    NEG
                };
                let extended = if gap_best > NEG {
                    gap_best + PENALTY_GAP_EXTEND
                } else {
                    NEG
                };
                if fresh >= extended {
                    gap_best = fresh;
                    gap_arg = j - 2;
                } else {
                    gap_best = extended;
                }
            }
            if hay_lc[j] != term[i] {
                continue;
            }
            let cons = if j >= 1 && score[(i - 1) * n + (j - 1)] > NEG {
                score[(i - 1) * n + (j - 1)] + BONUS_CONSECUTIVE
            } else {
                NEG
            };
            let (prev, arg) = if cons >= gap_best {
                (cons, j.wrapping_sub(1))
            } else {
                (gap_best, gap_arg)
            };
            if prev > NEG {
                score[i * n + j] = prev + SCORE_MATCH + bonus[j];
                parent[i * n + j] = arg;
            }
        }
    }

    // Best end position for the last term char; ties go to the earliest.
    let (mut best_j, mut best) = (usize::MAX, NEG);
    for j in 0..n {
        if score[(m - 1) * n + j] > best {
            best = score[(m - 1) * n + j];
            best_j = j;
        }
    }
    if best <= NEG {
        return None;
    }
    let mut positions = vec![0usize; m];
    let mut j = best_j;
    for i in (0..m).rev() {
        positions[i] = j;
        j = parent[i * n + j];
    }
    Some((best, positions))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(line: &str, query: &str) -> i32 {
        match_line(line, query).expect("expected a match").score
    }

    fn positions(line: &str, query: &str) -> Vec<usize> {
        match_line(line, query).expect("expected a match").positions
    }

    #[test]
    fn non_subsequence_is_no_match() {
        assert!(match_line("git status", "xyz").is_none());
        assert!(match_line("ls", "lss").is_none()); // longer than the line
        assert!(match_line("git status", "tg").is_none()); // out of order
    }

    #[test]
    fn blank_query_is_no_match() {
        assert!(match_line("git status", "").is_none());
        assert!(match_line("git status", "   ").is_none());
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(score("Git Status", "git"), score("git status", "GIT"));
        assert!(match_line("MAKE ALL", "make").is_some());
    }

    #[test]
    fn consecutive_run_beats_scattered_letters() {
        // Both contain g,i,t as a subsequence; only one has them adjacent.
        assert!(score("git log", "git") > score("going to lunch", "git"));
    }

    #[test]
    fn word_boundary_beats_mid_word() {
        // `st` at the start of "status" (after a space) vs inside "faster".
        assert!(score("git status", "st") > score("faster", "st"));
    }

    #[test]
    fn positions_pick_the_best_alignment() {
        // `gs` should land on the `g` of git and the boundary `s` of status,
        // not some later `s`.
        assert_eq!(positions("git status", "gs"), vec![0, 4]);
        // A consecutive alignment is recovered exactly.
        assert_eq!(positions("cargo build", "build"), vec![6, 7, 8, 9, 10]);
    }

    #[test]
    fn multi_term_queries_must_all_match_and_merge_positions() {
        // Terms match independently (order-free) and positions merge sorted.
        let m = match_line("git push --force origin", "push git").unwrap();
        assert_eq!(m.positions, vec![0, 1, 2, 4, 5, 6, 7]);
        // One term failing fails the whole query.
        assert!(match_line("git push", "git nope").is_none());
    }

    #[test]
    fn gaps_are_penalized_by_length() {
        // Same letters, tighter gap scores higher.
        assert!(score("ab", "ab") > score("a-b", "ab"));
        assert!(score("a-b", "ab") > score("a---------b", "ab"));
    }

    #[test]
    fn unicode_haystacks_match_by_char() {
        // Positions are char indices, not bytes: the CJK prefix occupies
        // char cells 0..2, so `ls` lands at 3..=4.
        assert_eq!(positions("构建 ls", "ls"), vec![3, 4]);
    }
}

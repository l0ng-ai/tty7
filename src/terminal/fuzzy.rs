pub(super) struct FuzzyMatch {
    pub score: i32,
    pub positions: Vec<usize>,
}

const SCORE_MATCH: i32 = 16;
const BONUS_BOUNDARY: i32 = 12;
const BONUS_CONSECUTIVE: i32 = 10;
const PENALTY_GAP_START: i32 = -3;
const PENALTY_GAP_EXTEND: i32 = -1;

const NEG: i32 = i32::MIN / 2;

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

fn lc(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

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

    /// Scores one chosen set of positions by the model the DP claims to
    /// implement: a point per match plus its boundary bonus, a bonus for
    /// staying adjacent, and for a gap of `g` characters one start penalty
    /// plus an extend penalty for each character after the first.
    fn score_positions(bonus: &[i32], pick: &[usize]) -> i32 {
        let mut s = 0;
        for (k, &j) in pick.iter().enumerate() {
            s += SCORE_MATCH + bonus[j];
            if k > 0 {
                let gap = j - pick[k - 1] - 1;
                s += match gap {
                    0 => BONUS_CONSECUTIVE,
                    g => PENALTY_GAP_START + (g as i32 - 1) * PENALTY_GAP_EXTEND,
                };
            }
        }
        s
    }

    /// Every way the term could be laid over the line, scored the same way.
    fn best_by_hand(hay: &[char], bonus: &[i32], term: &[char]) -> Option<i32> {
        fn go(
            i: usize,
            from: usize,
            hay: &[char],
            bonus: &[i32],
            term: &[char],
            pick: &mut Vec<usize>,
            best: &mut Option<i32>,
        ) {
            if i == term.len() {
                let s = score_positions(bonus, pick);
                if best.is_none_or(|b| s > b) {
                    *best = Some(s);
                }
                return;
            }
            for j in from..hay.len() {
                if hay[j] == term[i] {
                    pick.push(j);
                    go(i + 1, j + 1, hay, bonus, term, pick, best);
                    pick.pop();
                }
            }
        }
        if term.is_empty() || term.len() > hay.len() {
            return None;
        }
        let mut best = None;
        go(0, 0, hay, bonus, term, &mut Vec::new(), &mut best);
        best
    }

    /// The ranking every fuzzy list in the app is sorted by comes out of one
    /// dynamic program, and a dynamic program is exactly the kind of code that
    /// is a little bit wrong for a long time: it keeps returning *a* match, so
    /// nothing looks broken, and the list is merely ordered slightly worse than
    /// it should be.
    ///
    /// So it is checked against the definition rather than against itself —
    /// eight thousand short lines laid over by hand, every placement
    /// enumerated. The alphabet is small and includes two of the characters
    /// that earn a boundary bonus, so ties and boundaries come up constantly
    /// rather than by luck.
    ///
    /// The positions are checked too, not just the score. They are what the UI
    /// underlines, and a backtrack that walked the wrong parents would
    /// highlight characters that had nothing to do with the score it reported.
    #[test]
    fn the_score_is_the_best_available_and_the_positions_are_the_ones_that_earned_it() {
        let alphabet = ['a', 'b', 'c', '-', '/'];
        let mut seed: u64 = 0x2545F491_4F6CDD1D;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..8000 {
            let hn = 1 + (next() % 12) as usize;
            let tn = 1 + (next() % 4) as usize;
            let hay: Vec<char> = (0..hn).map(|_| alphabet[(next() % 5) as usize]).collect();
            let term: Vec<char> = (0..tn).map(|_| alphabet[(next() % 3) as usize]).collect();
            let hay_lc: Vec<char> = hay.iter().map(|&c| lc(c)).collect();
            let bonus: Vec<i32> = (0..hay.len())
                .map(|j| char_bonus(if j == 0 { None } else { Some(hay[j - 1]) }))
                .collect();

            let line: String = hay.iter().collect();
            let word: String = term.iter().collect();
            let got = match_term(&hay_lc, &bonus, &term);
            let want = best_by_hand(&hay_lc, &bonus, &term);

            assert_eq!(
                got.as_ref().map(|(s, _)| *s),
                want,
                "{line:?} / {word:?}: the best score was not found"
            );

            if let Some((score, positions)) = got {
                assert_eq!(positions.len(), term.len(), "{line:?} / {word:?}");
                assert!(
                    positions.windows(2).all(|w| w[0] < w[1]),
                    "{line:?} / {word:?}: positions {positions:?} are not in order"
                );
                for (i, &j) in positions.iter().enumerate() {
                    assert_eq!(hay_lc[j], term[i], "{line:?} / {word:?} at {j}");
                }
                assert_eq!(
                    score_positions(&bonus, &positions),
                    score,
                    "{line:?} / {word:?}: {positions:?} do not add up to the score reported"
                );
            }
        }
    }

    fn score(line: &str, query: &str) -> i32 {
        match_line(line, query).expect("expected a match").score
    }

    fn positions(line: &str, query: &str) -> Vec<usize> {
        match_line(line, query).expect("expected a match").positions
    }

    #[test]
    fn non_subsequence_is_no_match() {
        assert!(match_line("git status", "xyz").is_none());
        assert!(match_line("ls", "lss").is_none());
        assert!(match_line("git status", "tg").is_none());
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
        assert!(score("git log", "git") > score("going to lunch", "git"));
    }

    #[test]
    fn word_boundary_beats_mid_word() {
        assert!(score("git status", "st") > score("faster", "st"));
    }

    #[test]
    fn positions_pick_the_best_alignment() {
        assert_eq!(positions("git status", "gs"), vec![0, 4]);
        assert_eq!(positions("cargo build", "build"), vec![6, 7, 8, 9, 10]);
    }

    #[test]
    fn multi_term_queries_must_all_match_and_merge_positions() {
        let m = match_line("git push --force origin", "push git").unwrap();
        assert_eq!(m.positions, vec![0, 1, 2, 4, 5, 6, 7]);
        assert!(match_line("git push", "git nope").is_none());
    }

    #[test]
    fn gaps_are_penalized_by_length() {
        assert!(score("ab", "ab") > score("a-b", "ab"));
        assert!(score("a-b", "ab") > score("a---------b", "ab"));
    }

    #[test]
    fn unicode_haystacks_match_by_char() {
        assert_eq!(positions("构建 ls", "ls"), vec![3, 4]);
    }
}

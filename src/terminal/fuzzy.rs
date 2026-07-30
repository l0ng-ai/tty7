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

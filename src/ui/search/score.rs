//! How well a row answers a query. One scorer for every tab, so the All tab
//! can put two tabs' best rows side by side and have the order mean something.

use super::command::Item;

pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    let needle: Vec<char> = query
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| !c.is_whitespace())
        .collect();
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    if needle.len() > hay.len() {
        return None;
    }

    let mut qi = 0usize;
    let mut score = 0i32;
    let mut run = 0i32;
    let mut prev_hit = false;
    for (i, ch) in hay.iter().enumerate() {
        if qi >= needle.len() {
            break;
        }
        if *ch != needle[qi] {
            prev_hit = false;
            run = 0;
            continue;
        }
        score += 1;
        let word_start = i == 0 || !hay[i - 1].is_alphanumeric();
        if word_start {
            score += 12;
        }
        if i == 0 {
            score += 10;
        }
        if prev_hit {
            run += 1;
            score += 6 + run.min(8);
        } else {
            run = 0;
        }
        prev_hit = true;
        qi += 1;
    }
    if qi < needle.len() {
        return None;
    }

    if hay == needle {
        score += 120;
    } else if hay.starts_with(&needle) {
        score += 50;
    }
    score -= (hay.len() as i32) / 6;
    Some(score)
}

/// How far behind the visible label an alias hit lands. An alias *is* the
/// command's own name, only in another language, so the gap is small — but two
/// commands that both match must still be ordered by the text on screen.
const ALIAS_PENALTY: i32 = 10;

pub(crate) fn item_score(query: &str, cmd: &Item) -> Option<i32> {
    let title = fuzzy_score(query, &cmd.title);
    let subtitle = cmd
        .subtitle
        .as_deref()
        .and_then(|s| fuzzy_score(query, s))
        .map(|s| s / 2 - 25);
    // Aliases only ever decide whether a row is in the list and where it sits.
    // The row keeps rendering `title` untouched, so a hit on wording the user
    // cannot see can never end up underlined against the wrong characters.
    let alias = cmd
        .aliases
        .iter()
        .filter_map(|a| fuzzy_score(query, a))
        .max()
        .map(|s| s - ALIAS_PENALTY);
    [title, subtitle, alias].into_iter().flatten().max()
}

/// A bounded nudge, not a re-ranking. Two commands that match the query about
/// equally well should come out in the order they actually get run, but a
/// command used daily must not outrank a plainly better match — a single extra
/// matched character is worth 16 before bonuses.
pub(crate) fn frecency_bonus(used: f64) -> i32 {
    const CEILING: f64 = 24.0;
    if used <= 0.0 {
        return 0;
    }
    (used.ln_1p() * 8.0).min(CEILING).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::i18n::L10nKey;
    use crate::ui::search::CommandKind;

    #[test]
    fn frecency_nudges_without_overruling_the_match() {
        assert_eq!(frecency_bonus(0.0), 0, "an unused command gets nothing");
        assert!(frecency_bonus(1.0) > 0, "one use is worth something");
        // Monotone in usage.
        assert!(frecency_bonus(20.0) > frecency_bonus(1.0));
        // Bounded: never worth more than one and a half matched characters,
        // so a command run a thousand times still loses to a better match.
        assert!(
            frecency_bonus(1_000.0) <= 24,
            "frecency is a tiebreak, not a ranking"
        );
        assert_eq!(frecency_bonus(1_000.0), frecency_bonus(10_000.0));
    }

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn non_subsequence_does_not_match() {
        assert_eq!(fuzzy_score("zzz", "Split Right"), None);
        assert_eq!(fuzzy_score("thgir", "Split Right"), None);
    }

    #[test]
    fn word_initials_outrank_scattered_letters() {
        let target = fuzzy_score("sr", "Split Right").expect("matches");
        let scattered = fuzzy_score("sr", "SSH: Manage Profiles…").expect("matches");
        assert!(
            target > scattered,
            "expected 'Split Right' ({target}) to outrank 'SSH: Manage Profiles…' ({scattered})"
        );
    }

    #[test]
    fn exact_and_prefix_beat_mid_string() {
        let exact = fuzzy_score("copy", "Copy").expect("matches");
        let longer = fuzzy_score("copy", "Copy Working Directory").expect("matches");
        assert!(
            exact > longer,
            "expected exact 'Copy' ({exact}) above 'Copy Working Directory' ({longer})"
        );
    }

    #[test]
    fn subtitle_matches_are_found_but_discounted() {
        let cmd =
            Item::new("prod-web", CommandKind::NewTab).with_subtitle("deploy@10.0.0.5".to_string());
        assert!(item_score("10.0.0", &cmd).is_some());
        let title_hit = item_score("prod", &cmd).expect("title matches");
        let subtitle_hit = item_score("deploy", &cmd).expect("subtitle matches");
        assert!(title_hit > subtitle_hit);
    }

    #[test]
    fn a_translated_command_is_still_found_by_its_english_name() {
        crate::ui::i18n::set_locale("zh-CN");
        let cmd = Item::localized(L10nKey::CmdChangeTheme, CommandKind::OpenThemePicker);
        assert!(
            !cmd.title.to_lowercase().contains("theme"),
            "the row shows the Chinese label: {:?}",
            cmd.title
        );
        assert!(
            item_score("theme", &cmd).is_some(),
            "an English query must find the Chinese-labelled row"
        );
        assert!(
            item_score("テーマ", &cmd).is_some(),
            "so must the Japanese one"
        );
        // The displayed label still works, and still wins: the same query
        // against the locale that shows it scores higher.
        let zh = item_score("更改主题", &cmd).expect("the shown label matches");
        crate::ui::i18n::set_locale("en");
        let en = Item::localized(L10nKey::CmdChangeTheme, CommandKind::OpenThemePicker);
        assert!(zh > 0 && en.title.contains("Theme"));
        assert!(
            item_score("theme", &en).unwrap() > item_score("theme", &cmd).unwrap(),
            "a hit on the visible label must outrank the same hit on an alias"
        );
    }
}

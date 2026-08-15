//! Turning a repo-relative path and a timestamp into something that fits in a
//! 260px column. All pure, all cheap, all unit-tested — the panel calls these
//! once per visible row per frame.

use std::borrow::Cow;
use unicode_segmentation::UnicodeSegmentation as _;

/// The ellipsis every eliding function here uses. One cluster, so a budget in
/// characters is a budget the caller can reason about.
const ELLIPSIS: char = '…';

/// Split `src/ui/app.rs` into `("app.rs", "src/ui")`.
///
/// The panel renders these as two runs with different sizes and colours, so
/// they have to come back as separate slices rather than one pre-joined
/// string. A path with no directory gets an empty second half.
pub(crate) fn split_display_path(rel: &str) -> (&str, &str) {
    // A trailing slash means the caller handed us a directory; the last
    // component is still the name, so drop the slash before splitting.
    let trimmed = rel.strip_suffix('/').unwrap_or(rel);
    match trimmed.rsplit_once('/') {
        Some((dir, name)) => (name, dir),
        None => (trimmed, ""),
    }
}

/// Keep the head and the tail, drop the middle. Paths and branch names both
/// carry their meaning at the ends — `feature/…/auth-retry` still says which
/// area and which change, where a plain truncate says neither.
///
/// `max_chars` counts what a reader counts as one character — grapheme
/// clusters — including the ellipsis, so the result never renders wider than
/// the caller budgeted.
///
/// Walking `chars()` would already put every cut on a `char` boundary and so
/// never produce invalid UTF-8, which is the hazard that usually gets the
/// attention. It is not the hazard that shows: a filename holding `❤️` or
/// `🇨🇳` cut mid-cluster loses the variation selector or one regional
/// indicator, and the row then draws a character the path does not contain.
/// `tab_strip::clusters` states the same rule for tab labels.
pub(crate) fn elide_middle(s: &str, max_chars: usize) -> Cow<'_, str> {
    let clusters: Vec<&str> = s.graphemes(true).collect();
    let total = clusters.len();
    if total <= max_chars {
        return Cow::Borrowed(s);
    }
    // Below three there is no room for head + ellipsis + tail; fall back to a
    // plain head cut rather than returning something wider than asked for.
    if max_chars <= 2 {
        return Cow::Owned(clusters[..max_chars].concat());
    }
    let keep = max_chars - 1;
    // Bias the extra character to the head: the tail is usually a file name,
    // and its last few characters (the extension) repeat across rows anyway.
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut out = String::with_capacity(s.len());
    out.push_str(&clusters[..head].concat());
    out.push(ELLIPSIS);
    out.push_str(&clusters[total - tail..].concat());
    Cow::Owned(out)
}

const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
/// Calendar-average, so "12mo" and "1y" describe the same distance instead of
/// leaving a gap where 365 days is neither.
const MONTH: i64 = DAY * 30;
const YEAR: i64 = DAY * 365;

/// `"2h"` / `"3d"` / `"5mo"` — a graph row has about 26px for this.
///
/// Through the i18n table like `home::relative_time`, in the compact spelling
/// this column's width demands — "now" is still an English word, and the row,
/// the tooltip, the detail byline and the overlay header all read it.
///
/// `now` is a parameter rather than a clock read so the whole thing stays a
/// pure function, and so a test can sit exactly on a boundary.
pub(crate) fn relative_time(now_unix: i64, then_unix: i64) -> String {
    use crate::ui::i18n::{L10nKey, t, t_fmt};
    // A commit stamped in the future (clock skew across machines is routine in
    // a shared repo) reads as "now" rather than as a negative age.
    let delta = (now_unix - then_unix).max(0);
    let unit = |key: L10nKey, n: i64| t_fmt(key, &[("n", &n.to_string())]);
    match delta {
        d if d < MINUTE => t(L10nKey::ScmTimeNow).to_string(),
        d if d < HOUR => unit(L10nKey::ScmTimeMinutes, d / MINUTE),
        d if d < DAY => unit(L10nKey::ScmTimeHours, d / HOUR),
        d if d < MONTH => unit(L10nKey::ScmTimeDays, d / DAY),
        d if d < YEAR => unit(L10nKey::ScmTimeMonths, d / MONTH),
        d => unit(L10nKey::ScmTimeYears, d / YEAR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_display_path_separates_the_name_from_its_directory() {
        assert_eq!(split_display_path("src/ui/app.rs"), ("app.rs", "src/ui"));
        assert_eq!(split_display_path("README.md"), ("README.md", ""));
        assert_eq!(split_display_path("a/b"), ("b", "a"));
        assert_eq!(split_display_path(""), ("", ""));
    }

    #[test]
    fn split_display_path_ignores_a_trailing_slash() {
        assert_eq!(split_display_path("src/ui/"), ("ui", "src"));
        assert_eq!(split_display_path("src/"), ("src", ""));
        // A leading slash leaves an empty directory half rather than dropping
        // the root — the caller decides how to render that.
        assert_eq!(split_display_path("/etc"), ("etc", ""));
    }

    #[test]
    fn elide_middle_leaves_short_strings_borrowed() {
        assert!(matches!(elide_middle("short", 10), Cow::Borrowed("short")));
        assert!(matches!(elide_middle("exact", 5), Cow::Borrowed("exact")));
    }

    #[test]
    fn elide_middle_cuts_between_clusters_not_inside_them() {
        // Both ends are clusters that mean something else in pieces: a family
        // is four code points joined by ZWJ, a flag is two indicators.
        let s = "👨‍👩‍👧/aaaaaaaaaaaaaaaaaaaa/🇨🇳";
        let out = elide_middle(s, 8);
        assert_eq!(out.graphemes(true).count(), 8, "{out:?}");
        assert!(out.starts_with("👨‍👩‍👧"), "{out:?}");
        assert!(out.ends_with("🇨🇳"), "{out:?}");
        // Cutting by `char` would have spent four of its seven on the family
        // alone and split it, so the head would have ended in a dangling ZWJ
        // that then attaches itself to the ellipsis.
        let by_char: String = s.chars().take(4).collect();
        assert!(by_char.ends_with('\u{200D}'), "{by_char:?}");
        assert!(!out.contains("👨\u{200D}👩\u{200D}…"), "{out:?}");
    }

    #[test]
    fn elide_middle_keeps_both_ends_and_respects_the_budget() {
        let out = elide_middle("crates/tty7-core/src/core/git/status.rs", 20);
        assert_eq!(out.chars().count(), 20);
        assert_eq!(out.matches(ELLIPSIS).count(), 1);
        assert!(out.starts_with("crates/"), "{out}");
        assert!(out.ends_with("status.rs"), "{out}");
    }

    #[test]
    fn elide_middle_spends_exactly_one_char_on_the_ellipsis() {
        // U+2026, not three ASCII dots: three dots would eat three columns of
        // a budget measured in characters.
        let out = elide_middle("abcdefghij", 5);
        assert_eq!(out, "ab…ij");
        assert_eq!(out.chars().count(), 5);
        // An odd budget gives the head the spare character.
        assert_eq!(elide_middle("abcdefghij", 6), "abc…ij");
    }

    #[test]
    fn elide_middle_handles_degenerate_budgets() {
        assert_eq!(elide_middle("abcdef", 3), "a…f");
        assert_eq!(elide_middle("abcdef", 2), "ab");
        assert_eq!(elide_middle("abcdef", 1), "a");
        assert_eq!(elide_middle("abcdef", 0), "");
    }

    #[test]
    fn elide_middle_never_cuts_a_multibyte_char_in_half() {
        // Every one of these is 3 bytes; a byte-indexed implementation panics
        // here rather than returning something wrong, which is why this test
        // asserts on the value and not just on not panicking.
        let path = "文档/设计/源代码管理方案.md";
        for budget in 0..=path.chars().count() + 2 {
            let out = elide_middle(path, budget);
            assert!(
                out.chars().count() <= budget,
                "budget {budget} produced {out:?}"
            );
        }
        let out = elide_middle(path, 8);
        assert_eq!(out.chars().count(), 8);
        assert!(out.contains(ELLIPSIS));
        assert!(out.starts_with("文档/设"), "{out}");
        assert!(out.ends_with(".md"), "{out}");
    }

    #[test]
    fn relative_time_covers_every_bucket() {
        crate::ui::i18n::set_locale("en");
        let now = 1_800_000_000i64;
        let ago = |secs: i64| relative_time(now, now - secs);
        assert_eq!(ago(0), "now");
        assert_eq!(ago(59), "now");
        assert_eq!(ago(60), "1m");
        assert_eq!(ago(90), "1m");
        assert_eq!(ago(59 * MINUTE), "59m");
        assert_eq!(ago(HOUR), "1h");
        assert_eq!(ago(23 * HOUR), "23h");
        assert_eq!(ago(DAY), "1d");
        assert_eq!(ago(29 * DAY), "29d");
        assert_eq!(ago(MONTH), "1mo");
        assert_eq!(ago(YEAR - 1), "12mo");
        assert_eq!(ago(YEAR), "1y");
        assert_eq!(ago(5 * YEAR), "5y");
    }

    #[test]
    fn relative_time_clamps_commits_from_the_future() {
        crate::ui::i18n::set_locale("en");
        let now = 1_800_000_000i64;
        assert_eq!(relative_time(now, now + DAY), "now");
    }

    /// The row, the tooltip and the overlay byline all read this — it goes
    /// through the i18n table like `home::relative_time`, in compact form.
    #[test]
    fn relative_time_speaks_the_ui_language() {
        crate::ui::i18n::set_locale("zh-CN");
        let now = 1_800_000_000i64;
        assert_eq!(relative_time(now, now), "刚刚");
        assert_eq!(relative_time(now, now - 2 * HOUR), "2时");
        // “3月”会被读成月份名,所以是“个月”。
        assert_eq!(relative_time(now, now - 3 * MONTH), "3个月");
        crate::ui::i18n::set_locale("en");
    }
}

//! One definition of how a git status looks, shared by the source control
//! panel, the file tree and the diff overlay's file cards.
//!
//! Keeping it in one place is the point: three hand-written A/M/D/R tables
//! drift, and the drift is invisible until someone notices the same file wears
//! two different letters in two different places.

use gpui_component::ActiveTheme as _;
use tty7_core::core::git::status::DecoStatus;

/// The single letter shown in the 14px badge column. `Ignored` has none — a
/// tree full of `!` is noise, not information.
pub(crate) fn status_glyph(s: DecoStatus) -> &'static str {
    match s {
        DecoStatus::Ignored => "",
        DecoStatus::Untracked => "?",
        DecoStatus::Added => "A",
        DecoStatus::Modified => "M",
        DecoStatus::Renamed => "R",
        DecoStatus::Deleted => "D",
        DecoStatus::Conflict => "U",
    }
}

/// Every colour here comes from `Semantics` (ansi 1/2/3/6 pushed over the
/// contrast floor), so it already tracks the theme and is already covered by
/// the contrast tests in `presets.rs`. No new token is introduced.
pub(crate) fn status_color(s: DecoStatus, cx: &gpui::App) -> gpui::Hsla {
    let theme = cx.theme();
    match s {
        DecoStatus::Conflict => theme.danger,
        // Muted rather than danger: a deleted file is gone, not broken, and
        // the strikethrough on its name already carries the message.
        DecoStatus::Deleted => theme.muted_foreground,
        DecoStatus::Added | DecoStatus::Untracked => theme.success,
        DecoStatus::Modified => theme.warning,
        DecoStatus::Renamed => theme.info,
        DecoStatus::Ignored => theme.muted_foreground.opacity(0.7),
    }
}

/// Display precedence for rolling a directory up to one status: the worst of
/// everything beneath it wins.
///
/// This is `DecoStatus`'s own `Ord` spelled out rather than a second opinion —
/// `StatusIndex` already rolls directories up with `max()`, so a rank that
/// disagreed would make a folder and the file inside it contradict each other.
/// Written out longhand so reordering the enum trips the test below instead of
/// silently reshuffling the UI.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn status_rank(s: DecoStatus) -> u8 {
    match s {
        DecoStatus::Ignored => 0,
        DecoStatus::Untracked => 1,
        DecoStatus::Added => 2,
        DecoStatus::Modified => 3,
        DecoStatus::Renamed => 4,
        DecoStatus::Deleted => 5,
        DecoStatus::Conflict => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [DecoStatus; 7] = [
        DecoStatus::Ignored,
        DecoStatus::Untracked,
        DecoStatus::Added,
        DecoStatus::Modified,
        DecoStatus::Renamed,
        DecoStatus::Deleted,
        DecoStatus::Conflict,
    ];

    #[test]
    fn every_status_has_its_own_letter() {
        let mut seen: Vec<&str> = Vec::new();
        for s in ALL {
            let glyph = status_glyph(s);
            if s == DecoStatus::Ignored {
                assert_eq!(glyph, "", "ignored files carry no letter");
                continue;
            }
            assert_eq!(glyph.chars().count(), 1, "{s:?} should be one character");
            assert!(!seen.contains(&glyph), "{glyph} is used twice");
            seen.push(glyph);
        }
        assert_eq!(status_glyph(DecoStatus::Conflict), "U");
        assert_eq!(status_glyph(DecoStatus::Untracked), "?");
    }

    #[test]
    fn rank_orders_conflict_above_everything_and_ignored_below() {
        let worst_first = [
            DecoStatus::Conflict,
            DecoStatus::Deleted,
            DecoStatus::Renamed,
            DecoStatus::Modified,
            DecoStatus::Added,
            DecoStatus::Untracked,
            DecoStatus::Ignored,
        ];
        for pair in worst_first.windows(2) {
            assert!(
                status_rank(pair[0]) > status_rank(pair[1]),
                "{:?} should outrank {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn rank_agrees_with_the_enums_own_ordering() {
        // The data layer sorts by `Ord`; the UI sorts by `status_rank`. If the
        // two ever disagree a directory rollup and a file row can disagree too.
        for a in ALL {
            for b in ALL {
                assert_eq!(
                    status_rank(a).cmp(&status_rank(b)),
                    a.cmp(&b),
                    "{a:?} vs {b:?}"
                );
            }
        }
    }
}

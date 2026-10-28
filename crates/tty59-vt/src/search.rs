//! The engine-owned search session (docs/libghostty-migration.md,
//! decision 6).
//!
//! [`VtSearch`] owns everything about a search except the terminal itself:
//! the query, the matches, which one is selected, and the bookkeeping that
//! keeps matches naming the text they were found in while output scrolls
//! the grid under them. Callers feed it terminal state and read results
//! back; they never hold a match list of their own to keep honest.
//!
//! The shape follows the engine arriving in Phase 3, whose search is also
//! a session bound to a terminal that tracks its matches internally across
//! scrollback pruning, reflow and resets: set a query, feed, read the
//! selected match and the matches in the viewport, step through with a
//! scroll policy. This side of the seam, the work is the engine being
//! replaced's regex walker plus the drift correction the GUI used to do
//! for it — moved here wholesale, semantics unchanged.

use crate::alacritty::VtTermSession;
use crate::model::{
    GridPoint, Scroll, SearchDirection, SearchMatch, SearchQuery, SearchScroll, SearchStatus,
};

/// How many matches one scan stops at. A scrollback can hold more than any
/// find bar can make sense of; past this the count reads as a floor ("n+")
/// rather than a total, and the scan stops rather than walking it all.
const MAX_MATCHES: usize = 10_000;

/// The search session: query, matches, selection, drift bookkeeping.
///
/// Nothing here locks or owns a terminal — every method that needs one
/// takes a session borrowed for the call, the same discipline the rest of
/// the seam uses. Between calls the state is plain data, safe to hold for
/// as long as the bar is open.
///
/// Results are true as of the last [`feed`](Self::feed); terminal writes
/// since then are unseen until the next one. Read matches after a feed,
/// use them before the terminal changes again, and re-read rather than
/// cache.
pub struct VtSearch {
    query: Option<SearchQuery>,
    /// The compiled-dialect pattern the last `set_query` built: the text
    /// escaped unless the query is a regex, prefixed case-sensitive when
    /// it asks. The engine matcher's own spelling of the query.
    pattern: Option<String>,
    status: SearchStatus,
    matches: Vec<SearchMatch>,
    index: Option<usize>,
    capped: bool,
    /// Scrollback depth the `matches` lines are counted against. Match
    /// points are grid-relative, so this is what turns one back into the
    /// absolute row it named — the anchor that survives output scrolling
    /// the grid under the highlights (see [`Self::rebase`]).
    history: usize,
}

impl Default for VtSearch {
    fn default() -> Self {
        Self::new()
    }
}

impl VtSearch {
    /// An idle search: no query, no matches, nothing selected.
    pub fn new() -> Self {
        Self {
            query: None,
            pattern: None,
            status: SearchStatus::Idle,
            matches: Vec::new(),
            index: None,
            capped: false,
            history: 0,
        }
    }

    /// Set what to look for. A changed query restarts the search from
    /// scratch: matches, selection and anchors are dropped, and the next
    /// [`feed`](Self::feed) picks the match nearest the viewport the way a
    /// fresh find does. Empty text returns the search to idle.
    pub fn set_query(&mut self, query: SearchQuery) {
        let pattern = build_pattern(&query);
        self.status = match (&query.text[..], pattern.as_deref()) {
            ("", _) => SearchStatus::Idle,
            (_, None) => SearchStatus::InvalidPattern,
            (_, Some(_)) => SearchStatus::Pending,
        };
        self.query = Some(query);
        self.pattern = pattern;
        self.matches.clear();
        self.index = None;
        self.capped = false;
        self.history = 0;
    }

    /// Where the search stands.
    pub fn status(&self) -> SearchStatus {
        self.status
    }

    /// Matches found as of the last feed, oldest first (grid order).
    pub fn matches(&self) -> &[SearchMatch] {
        &self.matches
    }

    /// How many matches the last feed found. A capped count is a floor,
    /// not a total — see [`Self::is_capped`].
    pub fn count(&self) -> usize {
        self.matches.len()
    }

    /// Whether the scan stopped at its cap rather than the end of the
    /// grid: there may be more matches than [`Self::count`] reports.
    pub fn is_capped(&self) -> bool {
        self.capped
    }

    /// Which match is selected, by index into [`Self::matches`].
    pub fn selected_index(&self) -> Option<usize> {
        self.index
    }

    /// The selected match itself.
    pub fn selected(&self) -> Option<SearchMatch> {
        self.index.and_then(|i| self.matches.get(i)).copied()
    }

    /// The matches a viewport shows, in *viewport-relative* coordinates —
    /// `line` counting from the top of the visible rows, for drawing
    /// highlight rectangles. `viewport_offset` is how far that viewport
    /// sits scrolled up into scrollback (0 at the live screen) and `rows`
    /// its height; a match is included when any row of it intersects the
    /// viewport, and its span is shifted, not clipped — the caller paints
    /// the rows it can see, as it always did.
    pub fn matches_in_viewport(&self, viewport_offset: usize, rows: usize) -> Vec<SearchMatch> {
        let offset = viewport_offset as i32;
        self.matches
            .iter()
            .filter(|m| m.end.line + offset >= 0 && m.start.line + offset < rows as i32)
            .map(|m| SearchMatch {
                start: GridPoint { line: m.start.line + offset, col: m.start.col },
                end: GridPoint { line: m.end.line + offset, col: m.end.col },
            })
            .collect()
    }

    /// Catch up with the terminal: rescan the whole grid against the
    /// query, keeping the selected match where the text it named went.
    ///
    /// Staying on the match the user stepped to is decided by absolute
    /// row (scrollback depth plus grid line), not by line alone: a line is
    /// only meaningful with the depth it was counted against, and matching
    /// on the raw number would latch the selection onto whichever
    /// occurrence has taken that grid row over. When it is gone, the feed
    /// falls back to where a fresh query would land — the last match at or
    /// above the bottom of the viewport — rather than to a stale ordinal.
    ///
    /// This never scrolls and never touches the selection: output arriving
    /// under an open find bar must not yank the viewport or erase what the
    /// user was selecting.
    pub fn feed(&mut self, session: &mut VtTermSession) {
        let Some(pattern) = self.pattern.as_deref() else {
            // Idle or invalid: the last feed's answers are wrong either
            // way, and there is nothing to look for or no way to look.
            self.matches.clear();
            self.index = None;
            self.capped = false;
            self.history = 0;
            return;
        };
        let previous = self.selected().map(|m| (m.start, self.history));
        let (matches, capped) = session.search_all(pattern, MAX_MATCHES);
        let history = (-session.topmost_line()) as usize;
        let index = previous
            .and_then(|(start, history_of_start)| {
                matches.iter().position(|m| {
                    anchor_row(history, m.start) == anchor_row(history_of_start, start)
                        && m.start.col == start.col
                })
            })
            .or_else(|| Self::nearest_the_viewport(session, &matches));
        self.matches = matches;
        self.index = index;
        self.capped = capped;
        self.history = history;
        self.status = SearchStatus::Complete;
    }

    /// The cheap half of catching up, for every output wakeup: slide the
    /// stored match points along with the text they name, without reading
    /// the grid again.
    ///
    /// A match point is a grid line, and output moves the text under it:
    /// every line that scrolls off the top of the screen shifts the whole
    /// grid up one. Between two feeds that leaves each highlight washing
    /// the row its text has just left — one row off per line scrolled, for
    /// as long as printing keeps the debounced feed from firing, which is
    /// exactly when someone is watching the highlight. Absolute rows (see
    /// [`anchor_row`]) do not move, so re-deriving each line from its
    /// anchor puts the highlight back under its text.
    ///
    /// Matches carried off the top of the scrollback are dropped. A grid
    /// that *shrank* — a cleared scrollback, a swap to the alt screen —
    /// moved every anchor by an amount this cannot see, so that case feeds
    /// in full instead.
    ///
    /// Returns whether a full feed ran (the caller repaints then; the
    /// cheap slide needs no repaint of its own — the output that moved the
    /// grid is bringing one anyway).
    pub fn rebase(&mut self, session: &mut VtTermSession) -> bool {
        let history = (-session.topmost_line()) as usize;
        let drift = history as i64 - self.history as i64;
        if drift == 0 {
            return false;
        }
        if drift < 0 {
            self.feed(session);
            return true;
        }

        let drift = drift as i32;
        let topmost = -(history as i32);
        // The matches are in grid order, so the ones that fell off the top
        // are a prefix — and a match whose start survived kept its end too.
        let gone = self
            .matches
            .partition_point(|m| m.start.line - drift < topmost);
        self.matches.drain(..gone);
        for m in &mut self.matches {
            m.start.line -= drift;
            m.end.line -= drift;
        }
        self.index = self.index.and_then(|i| i.checked_sub(gone));
        self.history = history;
        false
    }

    /// Step the selection to the next match in `direction`, wrapping past
    /// the newest/oldest, and scroll it into view per `scroll`. Nothing is
    /// stepped when there are no matches.
    pub fn select(
        &mut self,
        session: &mut VtTermSession,
        direction: SearchDirection,
        scroll: SearchScroll,
    ) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len();
        let current = self.index.unwrap_or(0);
        self.index = Some(match direction {
            SearchDirection::Newer => (current + 1) % len,
            SearchDirection::Older => (current + len - 1) % len,
        });
        self.apply_scroll(session, scroll);
    }

    /// Apply `scroll` to the selected match without stepping — what a
    /// fresh query's first pick wants: the eye is where the viewport is,
    /// so the match nearest it has to be readable.
    pub fn reveal(&mut self, session: &mut VtTermSession, scroll: SearchScroll) {
        self.apply_scroll(session, scroll);
    }

    fn apply_scroll(&self, session: &mut VtTermSession, scroll: SearchScroll) {
        let SearchScroll::IfNeeded { hidden } = scroll else {
            return;
        };
        if let Some(m) = self.selected() {
            scroll_match_into_view(session, &m, hidden as i32);
        }
    }

    /// The match a fresh query starts on: the last one at or above the
    /// bottom of what is on screen, so a step forward walks from where the
    /// eye is.
    fn nearest_the_viewport(
        session: &VtTermSession,
        matches: &[SearchMatch],
    ) -> Option<usize> {
        if matches.is_empty() {
            return None;
        }
        let offset = session.viewport_offset() as i32;
        let bottom = GridPoint {
            line: session.screen_lines() as i32 - 1 - offset,
            col: (session.columns() as u16).saturating_sub(1),
        };
        matches.iter().rposition(|m| m.start <= bottom)
    }
}

/// A match point's row counted from the top of scrollback, which survives
/// the grid scrolling under it — the same anchor a command mark or a
/// placed image uses, with the same caveat once the scrollback is full and
/// the discard count stops being observable.
fn anchor_row(history: usize, point: GridPoint) -> i64 {
    history as i64 + point.line as i64
}

/// What moving a match into view takes.
#[derive(Debug, PartialEq, Eq)]
enum Reveal {
    /// Already clear of the bar and inside the viewport.
    Stay,
    /// Below the viewport — the engine's own minimal scroll lands it on
    /// the last row, which nothing covers.
    ToPoint,
    /// Above the readable area. A minimal scroll would park it on row 0,
    /// the row most likely to be behind the bar, so walk back this many
    /// lines and land it just clear instead.
    Back(i32),
}

fn reveal(line: i32, display_offset: i32, screen_lines: i32, hidden: i32) -> Reveal {
    let top = -display_offset + hidden;
    let bottom = screen_lines - 1 - display_offset;
    if line > bottom {
        Reveal::ToPoint
    } else if line < top {
        Reveal::Back(top - line)
    } else {
        Reveal::Stay
    }
}

fn scroll_match_into_view(session: &mut VtTermSession, m: &SearchMatch, hidden: i32) {
    let display_offset = session.viewport_offset() as i32;
    let screen_lines = session.screen_lines() as i32;
    match reveal(m.start.line, display_offset, screen_lines, hidden) {
        Reveal::Stay => {}
        Reveal::ToPoint => session.scroll_to(m.start),
        Reveal::Back(lines) => session.scroll(Scroll::Delta(lines)),
    }
}

/// The engine matcher's spelling of a query: literal text escaped unless
/// the query asks for a regex, and a case-sensitive anchor prefix when it
/// asks for that (the matcher defaults to case-insensitive). Empty text is
/// no pattern at all — nothing to hand the matcher.
fn build_pattern(query: &SearchQuery) -> Option<String> {
    if query.text.is_empty() {
        return None;
    }
    let base = match query.regex {
        true => query.text.clone(),
        false => regex_escape(&query.text),
    };
    let pattern = match query.case_sensitive {
        true => format!("(?-i){base}"),
        false => base,
    };
    // The engine matcher compiles its own dialect; a pattern it rejects is
    // the query's fault, and the caller has to be able to say so.
    crate::alacritty::pattern_compiles(&pattern).then_some(pattern)
}

fn regex_escape(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for c in query.chars() {
        if matches!(
            c,
            '\\' | '.'
                | '+'
                | '*'
                | '?'
                | '('
                | ')'
                | '|'
                | '['
                | ']'
                | '{'
                | '}'
                | '^'
                | '$'
                | '#'
                | '&'
                | '-'
                | '~'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::alacritty::VtTerm;
    use crate::model::SearchQuery as Query;

    fn fed(cols: u16, rows: u16, text: &str) -> VtTerm {
        let mut term = VtTerm::new(cols, rows, 10_000);
        term.feed(text.replace('\n', "\r\n").as_bytes());
        term
    }

    fn searched(term: &mut VtTerm, text: &str) -> VtSearch {
        let mut search = VtSearch::new();
        search.set_query(Query {
            text: text.to_string(),
            regex: false,
            case_sensitive: false,
        });
        search.feed(&mut term.session());
        search
    }

    #[test]
    fn an_empty_query_is_idle_and_finds_nothing() {
        let mut term = fed(20, 4, "hello");
        let mut search = VtSearch::new();
        search.set_query(Query {
            text: String::new(),
            regex: false,
            case_sensitive: false,
        });
        assert_eq!(search.status(), SearchStatus::Idle);
        search.feed(&mut term.session());
        assert_eq!(search.status(), SearchStatus::Idle);
        assert_eq!(search.count(), 0);
        assert_eq!(search.selected(), None);
    }

    #[test]
    fn a_broken_pattern_is_reported_not_swallowed() {
        let mut term = fed(20, 4, "hello");
        let mut search = VtSearch::new();
        search.set_query(Query {
            text: "(".to_string(),
            regex: true,
            case_sensitive: false,
        });
        assert_eq!(search.status(), SearchStatus::InvalidPattern);
        search.feed(&mut term.session());
        assert_eq!(search.status(), SearchStatus::InvalidPattern);
        assert_eq!(search.count(), 0);
        // The same text as a literal is fine.
        search.set_query(Query {
            text: "(".to_string(),
            regex: false,
            case_sensitive: false,
        });
        assert_eq!(search.status(), SearchStatus::Pending);
        search.feed(&mut term.session());
        assert_eq!(search.status(), SearchStatus::Complete);
        assert_eq!(search.count(), 0, "a literal paren matches nothing here");
    }

    #[test]
    fn matches_run_oldest_first_in_grid_order() {
        let mut term = fed(20, 6, "b\nworld a\nworld b\nplain\nworld c");
        let search = searched(&mut term, "world");
        assert_eq!(search.count(), 3);
        let lines: Vec<i32> = search.matches().iter().map(|m| m.start.line).collect();
        assert_eq!(lines, vec![1, 2, 4], "oldest first, top to bottom");
        assert_eq!(search.matches()[0].start.col, 0);
        assert_eq!(search.matches()[0].end.col, 4, "inclusive of the last letter");
    }

    #[test]
    fn case_and_regex_toggles_answer_as_two_queries() {
        let mut term = fed(30, 5, "Hello World\r\nhello world\r\nWORLD wide");
        let mut query = |regex: bool, case: bool, text: &str| {
            let mut search = VtSearch::new();
            search.set_query(Query {
                text: text.to_string(),
                regex,
                case_sensitive: case,
            });
            search.feed(&mut term.session());
            search.count()
        };
        assert_eq!(query(false, false, "world"), 3);
        assert_eq!(query(false, true, "world"), 1);
        assert_eq!(query(false, false, "wor.d"), 0);
        assert_eq!(query(true, false, "wor.d"), 3);
    }

    #[test]
    fn a_scan_stops_at_the_cap_and_says_so() {
        // One match per row: the cap is what stops the walk, and the count
        // has to read as a floor.
        let rows = MAX_MATCHES as u16 + 5;
        let mut term = VtTerm::new(10, rows, 10_000);
        let text = "x\n".repeat(rows as usize);
        term.feed(text.as_bytes());
        let search = searched(&mut term, "x");
        assert_eq!(search.count(), MAX_MATCHES);
        assert!(search.is_capped());
        // And below the cap it is the truth.
        let mut small = VtTerm::new(10, 5, 10_000);
        small.feed(b"x\r\ny\r\nx");
        let search = searched(&mut small, "x");
        assert_eq!(search.count(), 2);
        assert!(!search.is_capped());
    }

    #[test]
    fn a_fresh_feed_picks_the_match_nearest_the_viewport() {
        // Eight matches on a four-row screen, scrolled back two: the band
        // reads grid lines -2..=1, so the matches on 2 and 3 are below it
        // and the pick is the last one the eye can still see.
        let mut term = fed(20, 4, "hit\r\nhit\r\nhit\r\nhit\r\nhit\r\nhit\r\nhit\r\nhit");
        term.session().scroll(Scroll::Delta(2));
        let search = searched(&mut term, "hit");
        assert_eq!(search.selected_index(), Some(5), "the last one still readable");
        // At the live screen every match is at or above the bottom, so the
        // pick is simply the newest.
        let mut term = fed(20, 4, "hit\r\nhit\r\nhit\r\nhit");
        let search = searched(&mut term, "hit");
        assert_eq!(search.selected_index(), Some(3));
    }

    #[test]
    fn a_feed_keeps_the_selected_match_by_its_absolute_row() {
        let mut term = fed(20, 3, "world 1");
        let mut search = searched(&mut term, "world");
        assert_eq!(search.selected_index(), Some(0));

        // Output scrolls the grid (four lines through three rows): the
        // same text is now one row deeper in scrollback, and a NEW match
        // joins at the live end. The selection stays on the text it was
        // on, not on grid line 0 — which the new match now holds.
        term.feed(b"\r\nworld 2\r\nfiller\r\nmore");
        search.feed(&mut term.session());
        assert_eq!(search.count(), 2);
        let selected = search.selected().expect("still selected");
        assert_eq!(selected.start.line, -1, "the row the old text scrolled to");
        assert_eq!(search.selected_index(), Some(0));
    }

    #[test]
    fn a_lost_selection_falls_back_to_nearest_the_viewport() {
        let mut term = fed(20, 3, "hit a\r\nmid\r\nhit b");
        let mut search = searched(&mut term, "hit");
        assert_eq!(search.selected_index(), Some(1), "the newest match is on screen");

        // Rewrite the selected match's row in place: the absolute row is
        // still there but the text is gone, so the feed falls back to
        // where a fresh query would land.
        term.feed(b"\x1b[3;1H\x1b[2Knothing");
        search.feed(&mut term.session());
        assert_eq!(search.count(), 1);
        assert_eq!(search.selected_index(), Some(0));
    }

    #[test]
    fn rebase_slides_matches_with_the_text_without_rescanning() {
        let mut term = fed(20, 3, "world 1");
        let mut search = searched(&mut term, "world");
        assert_eq!(search.matches()[0].start.line, 0);

        // Seven filler lines scroll the grid up (eight lines through three
        // rows: five of history); no feed runs, so the stored point has to
        // move on its own.
        term.feed(b"\r\nfiller 1\r\nfiller 2\r\nfiller 3\r\nfiller 4\r\nfiller 5\r\nfiller 6\r\nfiller 7");
        assert!(!search.rebase(&mut term.session()), "the cheap slide, no feed");
        assert_eq!(search.matches()[0].start.line, -5);
        // And the text really is there.
        let start = search.matches()[0].start;
        assert_eq!(term.view().glyph(start).c, 'w');
    }

    #[test]
    fn rebase_drops_matches_that_scrolled_off_the_top() {
        // A scrollback of only four lines, so "old hit" can be carried out
        // of it without printing a small book first. The cheap slide's
        // arithmetic shifts by the *observable* history growth — once the
        // scrollback is full, discards stop being observable — so a match
        // carried out by the discarding survives the slide and is dropped
        // by the next feed's rescan instead.
        let mut term = VtTerm::new(10, 2, 4);
        term.feed(b"old hit\r\nnewer\r\nmore");
        let mut search = searched(&mut term, "hit");
        assert_eq!(search.count(), 1);

        for i in 0..12 {
            term.feed(format!("\r\nfiller {i}").as_bytes());
        }
        assert!(!search.rebase(&mut term.session()));
        assert_eq!(
            search.count(),
            1,
            "the slide cannot see past a full scrollback"
        );
        search.feed(&mut term.session());
        assert_eq!(search.count(), 0, "the rescan drops it");
        assert_eq!(search.selected_index(), None);
    }

    #[test]
    fn a_grid_that_shrank_feeds_in_full() {
        let mut term = fed(10, 4, "hit\r\nhit\r\nhit\r\nfiller\r\nfiller");
        let mut search = searched(&mut term, "hit");
        assert_eq!(search.count(), 3);
        // The alt screen swap empties the scrollback: history went down,
        // which the cheap slide cannot see through.
        term.feed(b"\x1b[?1049h\x1b[Halt hit");
        assert!(search.rebase(&mut term.session()), "a full feed ran");
        assert_eq!(search.status(), SearchStatus::Complete);
        assert_eq!(search.count(), 1, "the alt screen is what is scanned now");
    }

    #[test]
    fn select_steps_and_wraps_both_ways() {
        let mut term = fed(20, 5, "a hit\r\nanother hit\r\nlast hit");
        let mut search = searched(&mut term, "hit");
        assert_eq!(search.selected_index(), Some(2), "nearest the viewport: the newest");
        search.select(&mut term.session(), SearchDirection::Newer, SearchScroll::None);
        assert_eq!(search.selected_index(), Some(0), "wraps past the newest");
        search.select(&mut term.session(), SearchDirection::Older, SearchScroll::None);
        assert_eq!(search.selected_index(), Some(2), "wraps back past the oldest");
        search.select(&mut term.session(), SearchDirection::Older, SearchScroll::None);
        assert_eq!(search.selected_index(), Some(1));
        search.select(&mut term.session(), SearchDirection::Newer, SearchScroll::None);
        assert_eq!(search.selected_index(), Some(2));
    }

    #[test]
    fn select_scrolls_a_buried_match_into_view_and_past_the_bar() {
        // The word one row below the top of a 20-row scrollback: a select
        // has to walk the viewport almost all the way up, and land the
        // match clear of a one-row bar. (A word at the very topmost row
        // cannot clear a bar — the scroll clamps at topmost — which is why
        // the fixture starts with filler.)
        let mut term = fed(20, 4, "filler\r\nneedle");
        for i in 0..22 {
            term.feed(format!("\r\nfiller {i}").as_bytes());
        }
        let mut search = searched(&mut term, "needle");
        let buried = search.matches()[0].start.line;
        assert_eq!(buried, -19, "24 lines through 4 rows: 20 of history");

        assert_eq!(term.view().viewport_offset(), 0, "the viewport starts at live");
        search.select(
            &mut term.session(),
            SearchDirection::Older,
            SearchScroll::IfNeeded { hidden: 1 },
        );
        let offset = term.view().viewport_offset();
        let row = buried + offset as i32;
        assert_eq!(row, 1, "one row clear of the bar (offset {offset})");

        // A select of a match already readable leaves the viewport alone.
        let before = term.view().viewport_offset();
        search.select(
            &mut term.session(),
            SearchDirection::Newer,
            SearchScroll::IfNeeded { hidden: 1 },
        );
        assert_eq!(
            term.view().viewport_offset(),
            before,
            "a visible match does not move the viewport"
        );
    }

    #[test]
    fn reveal_brings_the_fresh_pick_under_the_bar() {
        // The word three rows below the top of the scrollback, a two-row
        // bar: the reveal has to walk far enough that the word lands under
        // the bar, not behind it.
        let mut term = fed(20, 4, "a\r\nb\r\nc\r\nneedle");
        for i in 0..20 {
            term.feed(format!("\r\nfiller {i}").as_bytes());
        }
        let mut search = VtSearch::new();
        search.set_query(Query {
            text: "needle".to_string(),
            regex: false,
            case_sensitive: false,
        });
        let mut session = term.session();
        search.feed(&mut session);
        assert_eq!(search.matches()[0].start.line, -17, "24 lines: history 20");
        search.reveal(&mut session, SearchScroll::IfNeeded { hidden: 2 });
        let row = search.matches()[0].start.line + term.view().viewport_offset() as i32;
        assert_eq!(row, 2, "the first row clear of a two-row bar");
    }

    #[test]
    fn viewport_matches_are_shifted_and_filtered() {
        // Three matches on a three-row screen, viewport at the live end.
        let mut term = fed(10, 3, "hit a\r\nhit b\r\nhit c");
        let search = searched(&mut term, "hit");
        assert_eq!(search.matches().len(), 3);
        let visible = search.matches_in_viewport(0, 3);
        assert_eq!(visible.len(), 3);
        assert_eq!(visible[0].start.line, 0, "viewport-relative");

        // Scrolled up two: the band shows grid lines -2..=0, so only the
        // match on the viewport's bottom row is left, one row lower down
        // the screen than it was.
        let visible = search.matches_in_viewport(2, 3);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].start.line, 2);

        // A match straddling the top edge of the band stays, span shifted
        // but not clipped: the caller paints the rows it can see.
        let mut term = fed(9, 3, "xxxxxxxxhit\ny");
        let search = searched(&mut term, "hit");
        assert_eq!(search.matches()[0].start.col, 8);
        assert_eq!(search.matches()[0].end.line, 1, "spans the wrap");
        let visible = search.matches_in_viewport(1, 3);
        assert_eq!(visible.len(), 1, "its end row is on screen");
        assert_eq!((visible[0].start.line, visible[0].end.line), (1, 2));
    }

    #[test]
    fn regex_escape_neutralizes_metacharacters() {
        assert_eq!(regex_escape("a.b*c"), r"a\.b\*c");
        assert_eq!(regex_escape("foo(bar)"), r"foo\(bar\)");
        assert_eq!(regex_escape("1+1=2"), r"1\+1=2");
        assert_eq!(regex_escape("hello"), "hello");
    }

    #[test]
    fn a_match_under_the_bar_counts_as_off_screen() {
        // 24 rows on screen, sitting at the live end, bar over the first 3.
        let at = |line| reveal(line, 0, 24, 3);
        assert_eq!(at(0), Reveal::Back(3), "row 0 is fully covered");
        assert_eq!(at(2), Reveal::Back(1), "row 2 is partly covered");
        assert_eq!(at(3), Reveal::Stay, "row 3 is the first readable one");
        assert_eq!(at(23), Reveal::Stay, "the last row is still on screen");
        assert_eq!(at(24), Reveal::ToPoint, "one past the end is not");
    }

    #[test]
    fn scrolled_back_into_history_the_readable_band_moves_with_it() {
        // 10 lines of history above the viewport: rows now run -10..=13.
        let at = |line| reveal(line, 10, 24, 3);
        assert_eq!(at(-10), Reveal::Back(3), "the top row is behind the bar");
        assert_eq!(at(-7), Reveal::Stay);
        assert_eq!(at(13), Reveal::Stay);
        assert_eq!(at(14), Reveal::ToPoint);
    }

    #[test]
    fn without_a_bar_nothing_on_screen_is_moved() {
        assert_eq!(reveal(0, 0, 24, 0), Reveal::Stay);
        assert_eq!(reveal(-1, 0, 24, 0), Reveal::Back(1))
    }
}

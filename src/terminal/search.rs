use std::path::{Path, PathBuf};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point, Side};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::search::{Match, RegexSearch};
use gpui::{Context, Entity, Subscription, Window, div, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, IconName, Selectable as _, Sizable as _, Size,
};

use super::view::TerminalView;
use crate::ui::i18n::{L10nKey, t};

const MAX_MATCHES: usize = 10_000;

/// How many readings of one token the candidate ladder may produce. Each one
/// costs a probe per root, so the cap is what keeps a separator-dense token
/// from turning a hover into a burst of filesystem calls.
const MAX_FILE_CANDIDATES: usize = 8;

/// The longest quoted span read as one path with spaces in it. Windows'
/// classic `MAX_PATH`: past this a quote is holding a sentence, not a path.
const MAX_QUOTED_PATH: usize = 260;

/// How long a printing pane has to stay quiet before an open search bar
/// rescans it. Short enough that a command's output is re-counted by the time
/// the eye gets back to the bar, long enough that a flood costs one scan per
/// pause rather than one per frame.
pub(super) const SCAN_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(120);

/// How many debounce windows a pane that never stops printing may push the
/// rescan out before it gets one anyway.
///
/// A debounce alone waits for a pause, and a coding agent streaming a reply
/// does not give one for a minute at a time — the count would sit frozen for
/// the whole flood and the highlights would sit on text that has since been
/// rewritten under them. Rebasing keeps a highlight under text that *scrolled*;
/// only a scan catches up with text that changed in place.
pub(super) const SCAN_LAG_LIMIT: u32 = 4;

/// The search bar floats over the top of the grid rather than pushing it down,
/// so these two decide how many rows it hides. Keep them next to the `.top()`
/// and `.h()` that use them — a match parked under the bar is on screen and
/// still unreadable, which is the one thing "next match" must never do.
const BAR_TOP: f32 = 8.;
const BAR_HEIGHT: f32 = 34.;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LinkTarget {
    Url(String),
    File {
        path: PathBuf,
        line: Option<u32>,
        column: Option<u32>,
        /// Set when the path is a directory, which is a link to a *place* —
        /// the file tree shows it, the editor cannot.
        is_dir: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LinkMatch {
    pub start: usize,
    pub end: usize,
    pub target: LinkTarget,
}

pub struct SearchState {
    pub input: Entity<InputState>,
    pub matches: Vec<Match>,
    pub current_index: Option<usize>,
    /// Scrollback depth the `matches` lines are counted against. Match points
    /// are grid-relative, so this is what turns one back into the absolute row
    /// it named — see [`TerminalView::refresh_matches_after_output`]. It is the
    /// depth at the last scan until output scrolls the grid, which moves the
    /// lines and this together (see
    /// [`TerminalView::rebase_matches_after_scroll`]) so the anchors hold.
    scanned_history: usize,
    _subs: Vec<Subscription>,
}

impl SearchState {
    pub fn current(&self) -> Option<&Match> {
        self.current_index.and_then(|i| self.matches.get(i))
    }
}

/// One read of the grid: every match in it, whether the pattern compiled, and
/// how deep the scrollback was at the time.
struct Scan {
    matches: Vec<Match>,
    regex_error: bool,
    history: usize,
}

impl Scan {
    fn empty(regex_error: bool) -> Self {
        Self {
            matches: Vec::new(),
            regex_error,
            history: 0,
        }
    }
}

/// A match point's row counted from the top of scrollback, which survives the
/// grid scrolling under it — the same anchor a command mark or a placed image
/// uses, with the same caveat once the scrollback is full and the discard count
/// stops being observable.
fn anchor_row(history: usize, point: &Point) -> i64 {
    history as i64 + point.line.0 as i64
}

impl TerminalView {
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fresh = self.search.is_none();
        // When the query is seeded from the grid selection, that selection is
        // the thing being searched for — not a casualty of opening the bar.
        // `recompute_matches` clears it because its other callers are the user
        // *changing* the query (typing, toggles), where the old selection no
        // longer names anything; here the query IS the selection, so it is
        // put back after the scan (#584).
        let mut seeded_selection = None;
        if fresh {
            let seed = self.selected_search_seed();
            if seed.is_some() {
                seeded_selection = self.terminal.term.lock().selection.clone();
            }
            let seed = seed.unwrap_or_else(|| self.search_last_query.clone());
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(t(L10nKey::SearchFind))
                    .default_value(seed)
            });
            let subs = vec![cx.subscribe_in(&input, window, Self::on_search_event)];
            self.search = Some(SearchState {
                input,
                matches: Vec::new(),
                current_index: None,
                scanned_history: 0,
                _subs: subs,
            });
        }
        if let Some(input) = self.search.as_ref().map(|s| s.input.clone()) {
            input.update(cx, |state, cx| state.focus(window, cx));
            // The box keeps the last query on purpose — reopening on the word
            // you just looked for is most of what a find bar is for — so the
            // caret must not treat it as text to type around.
            crate::ui::prefill::select_all_when_drawn(&input, window, cx);
        }
        if fresh {
            self.recompute_matches(cx);
            if let Some(selection) = seeded_selection {
                self.terminal.term.lock().selection = Some(selection);
            }
        }
        cx.notify();
    }

    fn selected_search_seed(&self) -> Option<String> {
        let text = self.terminal.term.lock().selection_to_string()?;
        let trimmed = text.trim_matches(['\n', '\r']);
        if trimmed.is_empty() || trimmed.contains('\n') {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    pub fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(s) = self.search.as_ref() {
            self.search_last_query = s.input.read(cx).value().to_string();
        }
        self.search = None;
        self.search_focused = false;
        self.search_regex_error = false;
        // The grid selection stays: it is the user's, not the bar's. The same
        // discipline `refresh_matches_after_output` states — the search bar
        // opening and closing around a selection must not erase it (#584).
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn on_search_event(
        &mut self,
        _input: &Entity<InputState>,
        event: &InputEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => self.recompute_matches(cx),
            InputEvent::PressEnter { shift, .. } => {
                let dir = if *shift {
                    Direction::Left
                } else {
                    Direction::Right
                };
                self.step_match(dir, cx);
            }
            InputEvent::Focus => {
                self.search_focused = true;
                cx.notify();
            }
            InputEvent::Blur => {
                self.search_focused = false;
                cx.notify();
            }
        }
    }

    /// Every match of the current query in the whole grid, and whether the
    /// pattern failed to compile.
    ///
    /// A match point is a line *of the grid it was read from*, and output
    /// scrolling the grid makes every one of them name a different line —
    /// which is what [`TerminalView::rebase_matches_after_scroll`] undoes
    /// between scans. Nothing here caches, and both callers re-read the grid.
    fn scan_matches(&self, query: &str) -> Scan {
        let mut matches: Vec<Match> = Vec::new();
        if query.is_empty() {
            return Scan::empty(false);
        }
        let pattern = self.effective_search_pattern(query);
        let Ok(mut regex) = RegexSearch::new(&pattern) else {
            return Scan::empty(true);
        };
        let term = self.terminal.term.lock();
        let grid = term.grid();
        let history = grid.history_size();
        let mut origin = Point::new(grid.topmost_line(), Column(0));

        while matches.len() < MAX_MATCHES {
            let Some(m) = term.search_next(&mut regex, origin, Direction::Right, Side::Left, None)
            else {
                break;
            };
            if matches.last().is_some_and(|last| m.start() <= last.start()) {
                break;
            }
            origin = m.end().add(grid, Boundary::None, 1);
            let wrapped = origin <= *m.end();
            matches.push(m);
            if wrapped {
                break;
            }
        }
        Scan {
            matches,
            regex_error: false,
            history,
        }
    }

    /// The match a fresh query starts on: the last one at or above the bottom
    /// of what is on screen, so Enter walks forward from where the eye is.
    fn match_nearest_the_viewport(&self, matches: &[Match]) -> Option<usize> {
        if matches.is_empty() {
            return None;
        }
        let term = self.terminal.term.lock();
        let grid = term.grid();
        let display_offset = grid.display_offset() as i32;
        let bottom = Point::new(
            Line(grid.screen_lines() as i32 - 1 - display_offset),
            grid.last_column(),
        );
        Some(
            matches
                .iter()
                .rposition(|m| *m.start() <= bottom)
                .unwrap_or(0),
        )
    }

    /// Note that output changed the grid an open search bar is describing.
    ///
    /// Two things happen here, because output breaks a scan in two ways.
    ///
    /// The cheap one runs every wakeup: text that scrolled took the highlights
    /// off their rows, and [`Self::rebase_matches_after_scroll`] puts them back
    /// without reading the grid.
    ///
    /// The rescan behind it is debounced rather than run per wakeup: it reads
    /// the whole grid, which is up to `MAX_SCROLLBACK` lines, and a pane
    /// mid-flood is repainting far faster than anyone can read a match count
    /// off it. One task waits for the printing to pause and then rescans once;
    /// further output while it waits pushes the deadline out, up to
    /// [`SCAN_LAG_LIMIT`] windows — a pane that never pauses still gets scanned.
    pub(super) fn note_output_under_search(&mut self, cx: &mut Context<Self>) {
        if self.search.is_none() {
            return;
        }
        self.rebase_matches_after_scroll(cx);
        self.search_scan_epoch = self.search_scan_epoch.wrapping_add(1);
        if self.search_scan_armed {
            return;
        }
        self.search_scan_armed = true;
        cx.spawn(async move |this, cx| {
            let mut waited = 0;
            loop {
                let Ok(epoch) = this.update(cx, |view, _| view.search_scan_epoch) else {
                    break;
                };
                cx.background_executor().timer(SCAN_DEBOUNCE).await;
                waited += 1;
                let settled = this.update(cx, |view, cx| {
                    if view.search_scan_epoch != epoch && waited < SCAN_LAG_LIMIT {
                        return false;
                    }
                    view.search_scan_armed = false;
                    view.refresh_matches_after_output(cx);
                    true
                });
                match settled {
                    Ok(true) | Err(_) => break,
                    Ok(false) => continue,
                }
            }
        })
        .detach();
    }

    /// Slide the stored match points along with the text they name.
    ///
    /// A match point is a grid line, and output moves the text under it: every
    /// line that scrolls off the top of the screen shifts the whole grid up
    /// one. Between two scans that leaves each highlight washing the row its
    /// text has just left — one row off per line scrolled, for as long as the
    /// printing keeps the debounced rescan from firing, which is exactly when
    /// someone is watching the highlight. Absolute rows (see [`anchor_row`]) do
    /// not move, so re-deriving each line from its anchor puts the highlight
    /// back under its text. Nothing here re-reads the grid, so it is cheap
    /// enough for every wakeup.
    ///
    /// Matches carried off the top of the scrollback are dropped. A grid that
    /// *shrank* — a cleared scrollback, a swap to the alt screen — moved every
    /// anchor by an amount this cannot see, so that case rescans instead.
    fn rebase_matches_after_scroll(&mut self, cx: &mut Context<Self>) {
        let history = self.terminal.term.lock().grid().history_size();
        let Some(s) = self.search.as_mut() else {
            return;
        };
        let drift = history as i64 - s.scanned_history as i64;
        if drift == 0 {
            return;
        }
        if drift < 0 {
            self.refresh_matches_after_output(cx);
            return;
        }

        let drift = drift as i32;
        let topmost = -(history as i32);
        let moved = |p: &Point| Point::new(Line(p.line.0 - drift), p.column);
        // The matches are in grid order, so the ones that fell off the top are
        // a prefix — and a match whose start survived kept its end too.
        let gone = s
            .matches
            .partition_point(|m| m.start().line.0 - drift < topmost);
        s.matches.drain(..gone);
        for m in &mut s.matches {
            let (start, end) = (moved(m.start()), moved(m.end()));
            *m = start..=end;
        }
        s.current_index = s.current_index.and_then(|i| i.checked_sub(gone));
        s.scanned_history = history;
    }

    /// Re-run the query against the grid as it now stands, keeping the count
    /// and the highlights honest while the pane is still printing.
    ///
    /// This is the same scan as `recompute_matches`, minus the two things that
    /// only a *user* action has the right to do: it does not drop the
    /// selection, and it does not scroll. Output arriving under an open search
    /// bar must not yank the viewport or erase what the user was selecting.
    pub(super) fn refresh_matches_after_output(&mut self, cx: &mut Context<Self>) {
        let Some(query) = self
            .search
            .as_ref()
            .map(|s| s.input.read(cx).value().to_string())
        else {
            return;
        };
        let scan = self.scan_matches(&query);
        // Stay on the match the user stepped to if it survived. Compare where
        // it was by absolute row, not by line: a line is only meaningful with
        // the scrollback depth it was counted against, and matching on the raw
        // number would latch the selection onto whichever occurrence has taken
        // that grid row over. When it is gone, fall back to where a fresh query
        // would land rather than to a stale ordinal.
        let previous = self
            .search
            .as_ref()
            .and_then(|s| Some((*s.current()?.start(), s.scanned_history)))
            .map(|(p, history)| (anchor_row(history, &p), p.column));
        let current_index = previous
            .and_then(|(row, column)| {
                scan.matches.iter().position(|m| {
                    anchor_row(scan.history, m.start()) == row && m.start().column == column
                })
            })
            .or_else(|| self.match_nearest_the_viewport(&scan.matches));

        if let Some(s) = self.search.as_mut() {
            s.matches = scan.matches;
            s.current_index = current_index;
            s.scanned_history = scan.history;
        }
        self.search_regex_error = scan.regex_error;
        cx.notify();
    }

    pub(super) fn recompute_matches(&mut self, cx: &mut Context<Self>) {
        let Some(query) = self
            .search
            .as_ref()
            .map(|s| s.input.read(cx).value().to_string())
        else {
            return;
        };

        let scan = self.scan_matches(&query);
        let current_index = self.match_nearest_the_viewport(&scan.matches);

        if let Some(s) = self.search.as_mut() {
            s.matches = scan.matches;
            s.current_index = current_index;
            s.scanned_history = scan.history;
        }
        self.search_regex_error = scan.regex_error;

        let current = self.search.as_ref().and_then(|s| s.current().cloned());
        let hidden = self.rows_behind_the_search_bar();
        let mut term = self.terminal.term.lock();
        term.selection = None;
        if let Some(m) = current {
            scroll_match_into_view(&mut term, &m, hidden);
        }
        drop(term);
        cx.notify();
    }

    pub(super) fn step_match(&mut self, direction: Direction, cx: &mut Context<Self>) {
        let current = {
            let Some(s) = self.search.as_mut() else {
                return;
            };
            if s.matches.is_empty() {
                return;
            }
            let len = s.matches.len();
            let cur = s.current_index.unwrap_or(0);
            let next = match direction {
                Direction::Right => (cur + 1) % len,
                Direction::Left => (cur + len - 1) % len,
            };
            s.current_index = Some(next);
            s.matches[next].clone()
        };
        let hidden = self.rows_behind_the_search_bar();
        scroll_match_into_view(&mut self.terminal.term.lock(), &current, hidden);
        cx.notify();
    }

    fn rows_behind_the_search_bar(&self) -> i32 {
        rows_under_the_bar(self.line_height.as_f32())
    }

    fn toggle_search_case(&mut self, cx: &mut Context<Self>) {
        if self.search.is_none() {
            return;
        }
        self.search_case_sensitive = !self.search_case_sensitive;
        self.recompute_matches(cx);
    }

    fn toggle_search_regex(&mut self, cx: &mut Context<Self>) {
        if self.search.is_none() {
            return;
        }
        self.search_regex = !self.search_regex;
        self.recompute_matches(cx);
    }

    fn effective_search_pattern(&self, query: &str) -> String {
        let base = if self.search_regex {
            query.to_string()
        } else {
            regex_escape(query)
        };
        if self.search_case_sensitive {
            format!("(?-i){base}")
        } else {
            base
        }
    }

    pub(super) fn render_search_bar(
        &self,
        state: &SearchState,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let border = theme.border;
        let popover = theme.popover;
        let danger = theme.red;

        let total = state.matches.len();
        let has_query = !state.input.read(cx).value().is_empty();
        let has_matches = !state.matches.is_empty();
        let regex_error = self.search_regex_error;
        let case_on = self.search_case_sensitive;
        let regex_on = self.search_regex;

        let field = Input::new(&state.input)
            .appearance(false)
            .with_size(Size::Small);

        let count = has_query.then(|| {
            let current = if has_matches {
                state.current_index.map(|i| i + 1).unwrap_or(0)
            } else {
                0
            };
            // The scan stops at MAX_MATCHES. Without the mark, a query that hit
            // the ceiling reads as if the scrollback held exactly that many.
            let more = match total >= MAX_MATCHES {
                true => "+",
                false => "",
            };
            div()
                .flex_none()
                .text_xs()
                .text_color(muted)
                .child(format!("{current}/{total}{more}"))
        });

        let case_toggle = Button::new("search-case")
            .label("Aa")
            .ghost()
            .small()
            .selected(case_on)
            .tooltip(t(L10nKey::SearchMatchCase))
            .on_click(cx.listener(|this, _, _window, cx| {
                this.toggle_search_case(cx);
            }));
        let regex_toggle = Button::new("search-regex")
            .label(".*")
            .ghost()
            .small()
            .selected(regex_on)
            .tooltip(t(L10nKey::SearchUseRegex))
            .on_click(cx.listener(|this, _, _window, cx| {
                this.toggle_search_regex(cx);
            }));

        let divider = div().flex_none().w(px(1.)).h(px(16.)).bg(border);

        let prev = Button::new("search-prev")
            .icon(IconName::ChevronUp)
            .ghost()
            .small()
            .tooltip(t(L10nKey::AppMenuFindPrevious))
            .disabled(!has_matches)
            .on_click(cx.listener(|this, _, _window, cx| {
                this.step_match(Direction::Left, cx);
            }));
        let next = Button::new("search-next")
            .icon(IconName::ChevronDown)
            .ghost()
            .small()
            .tooltip(t(L10nKey::AppMenuFindNext))
            .disabled(!has_matches)
            .on_click(cx.listener(|this, _, _window, cx| {
                this.step_match(Direction::Right, cx);
            }));
        let close = Button::new("search-close")
            .icon(IconName::Close)
            .ghost()
            .small()
            .tooltip(t(L10nKey::Close))
            .on_click(cx.listener(|this, _, window, cx| {
                this.close_search(window, cx);
            }));

        div()
            .absolute()
            .top(px(BAR_TOP))
            .right_4()
            .occlude()
            .flex()
            .items_center()
            .gap_1p5()
            .w(px(400.))
            .h(px(BAR_HEIGHT))
            .pl_3()
            .pr_1()
            .rounded_lg()
            .border_1()
            // Focus is shown by the caret, not by the frame: an accent border
            // over the bar's drop shadow read as a glow.
            .border_color(if regex_error { danger } else { border })
            .bg(popover)
            .shadow_md()
            .child(div().flex_1().min_w_0().child(field))
            .children(count)
            .child(case_toggle)
            .child(regex_toggle)
            .child(divider)
            .child(prev)
            .child(next)
            .child(close)
    }
}

/// How many grid rows the floating bar covers, at this line height.
fn rows_under_the_bar(line_height: f32) -> i32 {
    if line_height <= 0. {
        return 0;
    }
    ((BAR_TOP + BAR_HEIGHT) / line_height).ceil() as i32
}

#[derive(Debug, PartialEq, Eq)]
enum Reveal {
    /// Already clear of the bar and inside the viewport.
    Stay,
    /// Below the viewport — alacritty's own minimal scroll lands it on the
    /// last row, which nothing covers.
    ToPoint,
    /// Above the readable area. `scroll_to_point` would park it on row 0, the
    /// row most likely to be behind the bar, so walk back this many lines and
    /// land it just clear instead.
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

fn scroll_match_into_view<T: EventListener>(term: &mut Term<T>, m: &Match, hidden: i32) {
    let grid = term.grid();
    let display_offset = grid.display_offset() as i32;
    let screen_lines = grid.screen_lines() as i32;
    match reveal(m.start().line.0, display_offset, screen_lines, hidden) {
        Reveal::Stay => {}
        Reveal::ToPoint => term.scroll_to_point(*m.start()),
        Reveal::Back(lines) => term.scroll_display(Scroll::Delta(lines)),
    }
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
pub(super) fn url_at(text: &str, col: usize) -> Option<String> {
    url_span_at(text, col).map(|(_, _, url)| url)
}

/// What checking one candidate path came back with.
///
/// The third arm is what makes remote panes work at all. A local pane answers
/// out of the filesystem in microseconds, but a pane whose paths live on
/// another machine has to ask that machine, and the answer arrives frames
/// later — long after the mouse event that wanted it. [`Probe::Unknown`] is
/// that gap: it means "nobody has asked yet", and the prober is expected to go
/// ask so the *next* look finds a real answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Probe {
    /// Something is there. Whether it is a directory travels with the answer:
    /// a directory link belongs in the file tree, not in the editor.
    Hit {
        is_dir: bool,
    },
    Miss,
    Unknown,
}

/// Answers for paths on the machine tty7 itself is running on.
pub(super) fn local_probe(path: &Path, require_file: bool) -> Probe {
    if path.is_file() {
        return Probe::Hit { is_dir: false };
    }
    match !require_file && path.is_dir() {
        true => Probe::Hit { is_dir: true },
        false => Probe::Miss,
    }
}

/// Which language a pane's paths are written in.
///
/// The machine tty7 runs on and the machine a pane's paths live on need not
/// agree, and `std::path` only ever speaks the first one's dialect. On a
/// Windows client that is the whole difference between a link and nothing: a
/// leading `/` is not absolute to `Path` there, so `/etc/hosts` printed by a
/// Linux pane used to be measured from that pane's directory instead of
/// standing alone, and a relative `src/lib.rs` was joined onto it with a
/// backslash the far side has never heard of.
///
/// The two arms are the two dialects, not the two operating systems: a WSL
/// distro and an SSH host both speak [`PathStyle::Posix`] whatever the client
/// is, and a pane on this machine speaks [`PathStyle::NATIVE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PathStyle {
    /// `/`-rooted and `/`-joined. Everything a Unix host, a WSL distro or a
    /// Git-Bash-style shell prints.
    Posix,
    /// Rooted by a drive letter or a UNC share, joined with `\`.
    Windows,
}

impl Default for PathStyle {
    fn default() -> Self {
        Self::NATIVE
    }
}

impl PathStyle {
    /// The dialect the machine tty7 is running on speaks.
    pub const NATIVE: PathStyle = match cfg!(windows) {
        true => PathStyle::Windows,
        false => PathStyle::Posix,
    };

    /// The dialect a host that called `sample` one of its own directories
    /// speaks: a drive letter or a UNC share is a Windows host's, and anything
    /// else is read as POSIX.
    ///
    /// Only a directory that names a *Windows* root counts as one, rather than
    /// every directory that does not name a POSIX one. A cwd that is neither —
    /// a relative one, or an empty one from a host that has not settled yet —
    /// is the same "nothing to go on" as no cwd at all, and has to fall the
    /// same way; reading it as Windows would spell a Linux host's paths with
    /// backslashes on the strength of a directory it never really reported.
    ///
    /// This is inference from what a host says about itself in passing. It is
    /// sound in the direction that matters: nothing but a POSIX host reports a
    /// `/`-rooted cwd.
    pub fn of_dir(sample: &Path) -> Self {
        match PathStyle::Windows.is_absolute(&sample.to_string_lossy()) {
            true => PathStyle::Windows,
            false => PathStyle::Posix,
        }
    }

    /// Whether a token says for itself which filesystem root it hangs off.
    ///
    /// Deliberately textual rather than [`Path::is_absolute`], which answers
    /// for *this* machine: the same string has to be read the pane's way on
    /// every client, or a link works on a Mac and not on the Windows box next
    /// to it.
    pub fn is_absolute(self, path: &str) -> bool {
        match self {
            PathStyle::Posix => path.starts_with('/'),
            // A UNC share, or a drive letter with a separator behind it.
            // `C:foo` is drive-*relative* and deliberately not included, which
            // is what `Path::is_absolute` says on Windows too.
            PathStyle::Windows => path.starts_with("\\\\") || drive_root(path).is_some(),
        }
    }

    /// The one path a token that names its own root stands for, spelled the
    /// way the pane's host can look it up — `None` for a token that has to be
    /// measured from somewhere.
    ///
    /// A Windows pane gets its path back with every separator a `\` and the
    /// drive letter upper-cased. `c:/Users/me/a.png` is what Node prints on
    /// Windows, and while the filesystem takes either slash, Explorer does
    /// not: handed a forward-slashed path to open or `/select,` it shows
    /// Documents and reports success.
    ///
    /// A POSIX pane has no drives, but it may well be a WSL distro looking at
    /// the Windows machine it runs on — a Windows tool run through interop, a
    /// `wslpath -w`, a build log from the other side. There `C:\Users` is
    /// `/mnt/c/Users`, and that is what the host is asked about. Anywhere else
    /// the mount is not there and the probe simply misses, so nothing
    /// underlines; that beats measuring `C:\Users` from the pane's directory,
    /// which could only ever be nonsense.
    pub fn rooted(self, path: &str) -> Option<String> {
        match self {
            PathStyle::Posix if path.starts_with('/') => Some(path.to_string()),
            PathStyle::Posix => {
                let (drive, rest) = drive_root(path)?;
                let rest = rest.replace('\\', "/");
                Some(format!(
                    "/mnt/{}/{}",
                    drive.to_ascii_lowercase(),
                    rest.trim_start_matches('/')
                ))
            }
            PathStyle::Windows => self.is_absolute(path).then(|| windows_spelling(path)),
        }
    }

    /// Whether a token only means something against the current directory of
    /// one particular drive: `C:notes.txt`, or `C:` on its own.
    ///
    /// There is no such thing as "the current directory of drive C" for a
    /// pane — only for a process — so the one answer that would exist is tty7's
    /// own, which has nothing to do with what the pane printed. And a bare
    /// letter and a colon is far more often prose than a path: `a:b`, `x:y`.
    fn is_drive_relative(self, path: &str) -> bool {
        let mut chars = path.chars();
        self == PathStyle::Windows
            && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.next() == Some(':')
            && !matches!(chars.next(), Some('\\' | '/'))
    }

    /// `rel` measured from `root`, spelled the way the pane's host spells it.
    ///
    /// Both arms join textually because `Path::join` would reach for this
    /// machine's separator: on Windows it turns `/home/u` and `src/lib.rs`
    /// into `/home/u\src/lib.rs`, which the Linux box on the other end of the
    /// probe cannot stat, and on a Mac it turns `C:\proj` and `src\lib.rs`
    /// into `C:\proj/src\lib.rs` for a Windows host.
    ///
    /// A Windows `rel` that starts with a separator is rooted on the root's
    /// own drive (or share), the way `cd \etc` is in a `cmd.exe` sitting on C:.
    pub fn join(self, root: &Path, rel: &str) -> PathBuf {
        let root = root.to_string_lossy();
        match self {
            PathStyle::Posix => PathBuf::from(format!("{}/{rel}", root.trim_end_matches('/'))),
            PathStyle::Windows => {
                let joined = match rel.starts_with(['\\', '/']) {
                    true => format!("{}{rel}", windows_prefix(&root)),
                    false => format!("{}\\{rel}", root.trim_end_matches(['\\', '/'])),
                };
                PathBuf::from(windows_spelling(&joined))
            }
        }
    }
}

/// The drive letter a path starts with, and everything after the colon — only
/// when a separator follows it, which is what makes it absolute. `C:\x` and
/// `c:/x` are; `C:x`, `C:` and `12:30` are not.
fn drive_root(path: &str) -> Option<(char, &str)> {
    let mut chars = path.chars();
    let drive = chars.next().filter(char::is_ascii_alphabetic)?;
    let rest = path.strip_prefix(drive)?.strip_prefix(':')?;
    rest.starts_with(['\\', '/']).then_some((drive, rest))
}

/// The part of a Windows path that names its drive or share: `C:` out of
/// `C:\proj`, `\\server\share` out of `\\server\share\proj`. Empty when there
/// is none to take.
fn windows_prefix(path: &str) -> &str {
    if drive_root(path).is_some() || path.len() == 2 && path.ends_with(':') {
        return &path[..2];
    }
    if path.starts_with("\\\\") || path.starts_with("//") {
        // Past the two leading separators, the server, and the share.
        let mut seps = path
            .char_indices()
            .skip(2)
            .filter(|&(_, c)| matches!(c, '\\' | '/'));
        seps.next();
        return match seps.next() {
            Some((end, _)) => &path[..end],
            None => path,
        };
    }
    ""
}

/// `path` spelled the way `style`'s own tools want it — for handing a path to
/// something outside tty7, like the OS opener. Only a Windows spelling
/// changes, and only a path that is valid UTF-8: rewriting anything else
/// through a lossy string would name a different file.
pub(super) fn spelled_for(style: PathStyle, path: &Path) -> PathBuf {
    match (style, path.to_str()) {
        (PathStyle::Windows, Some(text)) => PathBuf::from(windows_spelling(text)),
        _ => path.to_path_buf(),
    }
}

/// A Windows path the way Windows itself writes one: `\` throughout and an
/// upper-case drive letter. Every Windows API takes `/` too, but Explorer —
/// the system opener and the "Reveal" menu item — does not.
fn windows_spelling(path: &str) -> String {
    let mut out = path.replace('/', "\\");
    if let Some((drive, _)) = drive_root(&out)
        && drive.is_ascii_lowercase()
    {
        out.replace_range(..1, &drive.to_ascii_uppercase().to_string());
    }
    out
}

/// Where a relative path printed by a pane is measured from.
///
/// `local_home` is the part that is easy to miss. `~` has to become a real
/// directory before anything can be looked up, and the only clue a pane
/// usually offers is its own cwd; when that does not reveal a home, this
/// machine's `$HOME` is the last resort. Sound for a pane on this machine, and
/// a fabrication for one whose paths live elsewhere — `/Users/me/.zshrc` is
/// not what `~/.zshrc` means on a Linux box, and asking that box about it is
/// at best a miss and at worst somebody else's file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct LinkRoots {
    /// Directories a relative path is tried against, nearest first.
    pub dirs: Vec<PathBuf>,
    /// Whether this machine's `$HOME` may stand in for a `~` the roots cannot
    /// explain.
    pub local_home: bool,
    /// How the pane's host spells the paths it prints.
    pub style: PathStyle,
}

impl LinkRoots {
    /// Roots on the machine tty7 is running on, where `$HOME` means what it
    /// says and paths are spelled this OS's way.
    pub fn local(dirs: Vec<PathBuf>) -> Self {
        Self {
            dirs,
            local_home: true,
            style: PathStyle::NATIVE,
        }
    }

    /// The directory the pane is in — the first root, and the one a `~` is
    /// read out of.
    pub fn cwd(&self) -> Option<&Path> {
        self.dirs.first().map(PathBuf::as_path)
    }
}

pub(super) fn link_at(
    text: &str,
    col: usize,
    roots: &LinkRoots,
    include_files: bool,
    probe: &mut dyn FnMut(&Path, bool) -> Probe,
) -> Option<LinkMatch> {
    if let Some((start, end, url)) = url_span_at(text, col) {
        return Some(LinkMatch {
            start,
            end,
            target: LinkTarget::Url(url),
        });
    }
    if !include_files {
        return None;
    }
    for candidate in file_candidates_at(text, col) {
        let Some((path, is_dir)) = resolve_candidate(&candidate, roots, probe) else {
            continue;
        };
        // A path can carry its line number instead of wearing it: only once
        // something has answered for the file is it worth reading the prose
        // beside it, and a directory has no line to land on.
        let line = match (candidate.line, is_dir) {
            (Some(line), _) => Some(line),
            (None, true) => None,
            (None, false) => location_after(text, candidate.end + 1),
        };
        return Some(LinkMatch {
            start: candidate.start,
            end: candidate.end,
            target: LinkTarget::File {
                path,
                line,
                column: candidate.column,
                is_dir,
            },
        });
    }
    None
}

/// The first of `candidate`'s possible paths that something answers for.
///
/// Roots are tried in order and the first [`Probe::Hit`] wins, which is what
/// makes the pane's own directory beat the repository root when both hold a
/// file by that name. A [`Probe::Unknown`] does not stop the walk — a later
/// root may already be cached — it just leaves the token unresolved for now.
pub(super) fn resolve_candidate(
    candidate: &FileCandidate,
    roots: &LinkRoots,
    probe: &mut dyn FnMut(&Path, bool) -> Probe,
) -> Option<(PathBuf, bool)> {
    let require_file = candidate.require_file();
    candidate
        .paths(roots)
        .into_iter()
        .find_map(|path| match probe(&path, require_file) {
            Probe::Hit { is_dir } => Some((path, is_dir)),
            Probe::Miss | Probe::Unknown => None,
        })
}

/// A path-shaped token lifted out of the grid, before anything has checked
/// whether it points at something that exists. Splitting this out from the
/// lookup is what lets one pane answer from the filesystem and another answer
/// from a cache filled by a remote host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileCandidate {
    /// Column of the token's first character in the logical line.
    pub start: usize,
    /// Column of its last, inclusive.
    pub end: usize,
    /// The path exactly as it was written — relative, `~`-prefixed, absolute.
    pub path: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

impl FileCandidate {
    /// A line number is a claim about a file's *contents*, so a directory that
    /// happens to carry the name cannot answer for it. Without this,
    /// `localhost:8080` resolves the moment a `localhost/` directory exists.
    pub fn require_file(&self) -> bool {
        self.line.is_some()
    }

    /// Every path this token could mean, best guess first: absolute paths
    /// stand alone, relative ones are joined onto each root in turn.
    pub fn paths(&self, roots: &LinkRoots) -> Vec<PathBuf> {
        let Some(expanded) = expand_home(&self.path, roots) else {
            return Vec::new();
        };
        if expanded.is_empty() || roots.style.is_drive_relative(&expanded) {
            return Vec::new();
        }
        if let Some(rooted) = roots.style.rooted(&expanded) {
            return vec![PathBuf::from(rooted)];
        }
        let mut out: Vec<PathBuf> = Vec::new();
        for root in &roots.dirs {
            let joined = roots.style.join(root, &expanded);
            if !out.contains(&joined) {
                out.push(joined);
            }
        }
        out
    }

    /// Whether the token says for itself where it starts, rather than being
    /// measured from anywhere. [`Self::paths`] ignores the roots entirely for
    /// these, so a report about one must not name a directory as the place it
    /// was looked for.
    ///
    /// Takes the pane's own dialect for the same reason `paths` does: on a
    /// Windows client `/etc/hosts` printed by a Linux pane is rooted and
    /// `/etc/hosts` printed by a `cmd.exe` pane is not, and `Path` alone
    /// cannot tell those apart.
    pub fn is_rooted(&self, style: PathStyle) -> bool {
        self.path.starts_with('~') || style.rooted(&self.path).is_some()
    }

    /// Whether the token is written enough like a path to be worth telling the
    /// user about when nothing answers for it. A bare word is not — every
    /// modifier-click on ordinary output would raise a notification saying so.
    ///
    /// A backslash counts only where it separates directories. In a POSIX
    /// pane it is an escape or an ordinary filename character, so `foo\ bar`
    /// there is a word, not a path. A drive letter is one in any pane.
    pub fn looks_like_a_path(&self, style: PathStyle) -> bool {
        self.path.starts_with('~')
            || self.path.contains('/')
            || (style == PathStyle::Windows && self.path.contains('\\'))
            || drive_root(&self.path).is_some()
    }
}

/// Every path-shaped reading of the token under `col`, best first.
///
/// The syntactic half of file detection: everything that can be decided from
/// the text alone, with no filesystem behind it. One token can be read more
/// than one way because terminal output wraps paths in a small set of known
/// prefixes and suffixes — `--file=src/main.rs`, `+++ b/src/main.rs`,
/// `src@` from `ls -F` — and which reading is the real one is a question only
/// the filesystem can settle. So the ladder hands them all to the caller in
/// the order they are worth asking about, longest path first.
///
/// Deliberately a short ladder rather than every substring between every
/// separator: naming the prefixes costs a handful of readings where
/// enumerating pairs costs a probe per pair, on every hover.
pub(super) fn file_candidates_at(text: &str, col: usize) -> Vec<FileCandidate> {
    let quoted = quoted_candidates_at(text, col);
    let Some((base, _, token)) = non_ws_token_at(text, col) else {
        return quoted;
    };
    let chars: Vec<char> = token.chars().collect();
    let mut out: Vec<FileCandidate> = Vec::new();
    for cut in left_cuts(&chars) {
        push_candidates(base, &chars, cut, col, &mut out);
    }
    // Longest path first: `/srv/app/log` beats the `/srv/app` inside it when
    // both exist, and a peeled reading only gets its turn once the one that
    // was actually written has missed.
    out.sort_by(|a, b| {
        b.path
            .chars()
            .count()
            .cmp(&a.path.chars().count())
            .then(a.start.cmp(&b.start))
    });
    // A quote is the one place a path is allowed a space, and the writer put
    // it there to say where the path starts and stops — so it is asked first.
    for (i, candidate) in quoted.into_iter().enumerate() {
        out.retain(|c| *c != candidate);
        out.insert(i, candidate);
    }
    out.truncate(MAX_FILE_CANDIDATES);
    out
}

/// The path a pair of quotes holds around `col`, when it has a space in it.
///
/// Whitespace ends a path everywhere else, and guessing where a path with
/// spaces stops in unquoted prose is how `see C:\Program Files for more`
/// would become a link to a folder that happens to exist. A quote is the
/// writer saying where it stops — `"C:\Program Files\app\app.exe"`,
/// `'/Users/me/My Docs/a.txt'` — so that is the only place a space is read as
/// part of a path.
///
/// What is inside still has to be written like a path: no space at either
/// end, a separator somewhere, and short enough to be one. A quoted sentence
/// that clears all that and still names nothing costs one reading, the same
/// as a single unquoted word — never one per word inside it.
///
/// A token with no space in it is left to the ordinary rules, which already
/// peel quotes off a path. A location may be written inside the quotes
/// (`"my file.rs:10:2"`) or straight after the closing one
/// (`"my file.rs":10:2`, `"my file.cs"(10,2)`).
fn quoted_candidates_at(text: &str, col: usize) -> Vec<FileCandidate> {
    let chars: Vec<char> = text.chars().collect();
    if col >= chars.len() {
        return Vec::new();
    }
    let Some((open, close)) = ['"', '\'', '`']
        .into_iter()
        .filter_map(|q| {
            let open = chars[..col].iter().rposition(|&c| c == q)?;
            let close = col + chars[col..].iter().position(|&c| c == q)?;
            (close > col).then_some((open, close))
        })
        // The innermost pair: `"it's '/a b/c'"` means the single-quoted one.
        .min_by_key(|&(open, close)| close - open)
    else {
        return Vec::new();
    };
    let inner = &chars[open + 1..close];
    let path_shaped = inner.len() <= MAX_QUOTED_PATH
        && inner.iter().any(|c| c.is_whitespace())
        && inner.first().is_some_and(|c| !c.is_whitespace())
        && inner.last().is_some_and(|c| !c.is_whitespace())
        && inner.iter().any(|&c| matches!(c, '/' | '\\'))
        && !inner.iter().any(|c| c.is_control());
    if !path_shaped {
        return Vec::new();
    }
    let written: String = inner.iter().collect();
    let mut location = split_file_location(&written);
    if location.line.is_none() {
        let after: String = chars[close + 1..]
            .iter()
            .take_while(|c| !c.is_whitespace())
            .collect();
        let after = after.trim_end_matches([':', ',', ';']);
        if !after.is_empty() {
            let outside = split_file_location(&format!("{written}{after}"));
            if outside.line.is_some() && outside.path == written {
                location = outside;
            }
        }
    }
    let mut readings = Vec::with_capacity(2);
    if location.literal_too && location.line.is_some() && location.path != written {
        readings.push((written.clone(), None, None));
    }
    readings.push((location.path, location.line, location.column));
    readings
        .into_iter()
        .map(|(path, line, column)| FileCandidate {
            start: open + 1,
            end: close - 1,
            path,
            line,
            column,
        })
        .collect()
}

/// The best reading of the token under `col`, which is what most of the
/// carving tests are about.
#[cfg(test)]
fn file_candidate_at(text: &str, col: usize) -> Option<FileCandidate> {
    file_candidates_at(text, col).into_iter().next()
}

/// The reading a click that resolved nothing should name.
///
/// The shortest one still written like a path, which is the part the user was
/// pointing at rather than the `--flag=` or `label:` wrapped around it. A
/// token that is not written like a path at all still comes back, so the
/// caller can decide to say nothing at all about it.
pub(super) fn unresolved_candidate(
    text: &str,
    col: usize,
    style: PathStyle,
) -> Option<FileCandidate> {
    let candidates = file_candidates_at(text, col);
    candidates
        .iter()
        .rev()
        .find(|c| c.looks_like_a_path(style))
        .or_else(|| candidates.first())
        .cloned()
}

/// Where inside a token a path could start, besides the token's own start.
///
/// Each rule is applied to every cut it produces, so the prefixes stack the
/// way they do in real output: `git diff`'s `a/` behind a `--file=`, a label
/// behind that. The cap is what stops a separator-dense token from walking.
fn left_cuts(chars: &[char]) -> Vec<usize> {
    let mut cuts = vec![0usize];
    let mut next = 0;
    while next < cuts.len() && cuts.len() < MAX_FILE_CANDIDATES {
        let base = cuts[next];
        next += 1;
        let rest = &chars[base..];
        let mut offsets: Vec<usize> = Vec::new();
        // `--file=src/main.rs`, `PATH=/usr/bin`. The last `=` wins, so a value
        // that is itself an assignment keeps only its tail.
        if let Some(i) = rest.iter().rposition(|&c| c == '=') {
            offsets.push(i + 1);
        }
        // `note:src/main.rs`. Only a plain word may sit in front: one letter
        // is a Windows drive (`C:\src`), and a prefix carrying a `/` or a `.`
        // is more likely a path with a line number written onto it. What
        // follows has to be spelled like a path too, or `branch:main` and
        // `remote:origin` become links the moment the pane's directory holds
        // a `main/` or an `origin/`.
        if let Some(i) = rest.iter().position(|&c| c == ':')
            && i >= 2
            && rest[0].is_ascii_alphabetic()
            && rest[..i]
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'))
            && rest[i + 1..]
                .iter()
                .any(|c| matches!(c, '/' | '\\' | '.' | '~'))
        {
            offsets.push(i + 1);
        }
        // What `git diff` calls the two sides of a change.
        if matches!(rest.first(), Some('a' | 'b')) && rest.get(1) == Some(&'/') {
            offsets.push(2);
        }
        // A Windows drive glued onto whatever came before it: `保存到：C:\out`,
        // `图片已生成c:/Users/me/a.png`. CJK prose puts no space in front of a
        // path, and the label rule above cannot see these — the only ASCII
        // colon in the token is the drive's own. The letter must not be the
        // tail of an ASCII word, or `abc:/x` would read as drive `c:`.
        if let Some(i) = (1..rest.len()).find(|&i| {
            !rest[i - 1].is_ascii_alphanumeric()
                && rest[i].is_ascii_alphabetic()
                && rest.get(i + 1) == Some(&':')
                && matches!(rest.get(i + 2), Some('\\' | '/'))
        }) {
            offsets.push(i);
        }
        // A Markdown link's target, which is what an agent prints a file as:
        // `![chart](c:/out/chart.png)`, `[notes](docs/notes.md)`.
        if let Some(i) = rest.windows(2).position(|w| w == [']', '(']) {
            offsets.push(i + 2);
        }
        for offset in offsets {
            let cut = base + offset;
            if cut < chars.len() && !cuts.contains(&cut) {
                cuts.push(cut);
            }
        }
    }
    cuts
}

/// Turns one left cut into the readings it supports and files them in `out`.
fn push_candidates(
    base: usize,
    chars: &[char],
    cut: usize,
    col: usize,
    out: &mut Vec<FileCandidate>,
) {
    let mut start = cut;
    while chars.get(start).is_some_and(|&c| is_left_wrapper(c)) {
        start += 1;
    }
    let end = start + trim_token_end(&chars[start..]);
    if end <= start {
        return;
    }

    let mut ends = vec![end];
    // `ls -F` marks what it lists: `src@` is a symlink, `run*` an executable,
    // `sock=` a socket, `fifo|` a FIFO. The mark is not part of the name.
    if end - 1 > start && matches!(chars[end - 1], '@' | '*' | '=' | '|') {
        ends.push(end - 1);
    }

    for end in ends {
        let mut token: String = chars[start..end].iter().collect();
        let mut end = end - 1;
        let mut location = split_file_location(&token);
        // `foo.sh:` before `line 12`, and ripgrep's `path:` before a match.
        if location.line.is_none() && token.ends_with(':') {
            token.pop();
            end = end.saturating_sub(1);
            location = split_file_location(&token);
        }
        if location.path.is_empty() {
            continue;
        }
        if !(base + start..=base + end).contains(&col) {
            continue;
        }
        let mut readings = Vec::with_capacity(2);
        if location.literal_too {
            readings.push((token, None, None));
        }
        readings.push((location.path, location.line, location.column));
        for (path, line, column) in readings {
            let candidate = FileCandidate {
                start: base + start,
                end: base + end,
                path,
                line,
                column,
            };
            if !out.contains(&candidate) {
                out.push(candidate);
            }
        }
    }
}

pub(super) fn url_span_at(text: &str, col: usize) -> Option<(usize, usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    if col >= chars.len() {
        return None;
    }
    if chars[col].is_whitespace() {
        return None;
    }
    let mut start = col;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < chars.len() && !chars[end + 1].is_whitespace() {
        end += 1;
    }
    let mut token: String = chars[start..=end].iter().collect();
    trim_trailing_punct(&mut token);

    const SCHEMES: [&str; 4] = ["https://", "http://", "file://", "ftp://"];
    if let Some(off) = SCHEMES.iter().filter_map(|s| token.find(s)).min() {
        start += token[..off].chars().count();
        token.drain(..off);
        if let Some(bad) = token.find(|c| !is_url_char(c)) {
            token.truncate(bad);
        }
        truncate_at_unbalanced_close(&mut token);
        trim_trailing_punct(&mut token);
        let end = start + token.chars().count() - 1;
        return (start..=end).contains(&col).then_some((start, end, token));
    }

    while token
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '(' | '[' | '<' | '\'' | '"' | '{'))
    {
        token.remove(0);
        start += 1;
    }
    trim_trailing_punct(&mut token);
    if token.starts_with("www.") && token.contains('.') {
        let end = start + token.chars().count() - 1;
        (start..=end)
            .contains(&col)
            .then(|| (start, end, format!("https://{token}")))
    } else {
        None
    }
}

fn non_ws_token_at(text: &str, col: usize) -> Option<(usize, usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    if col >= chars.len() || chars[col].is_whitespace() {
        return None;
    }

    let mut start = col;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < chars.len() && !chars[end + 1].is_whitespace() {
        end += 1;
    }
    Some((start, end, chars[start..=end].iter().collect()))
}

/// What can open a path without being part of it: the brackets and quotes
/// prose puts around one, and the box-drawing glyphs `tree` and friends draw
/// in front of one.
fn is_left_wrapper(c: char) -> bool {
    matches!(
        c,
        '(' | '[' | '<' | '{' | '\'' | '"' | '`' | '（' | '［' | '｛' | '《' | '「' | '『' | '【'
    ) || ('\u{2500}'..='\u{257F}').contains(&c)
}

/// Where the path stops and the sentence around it starts again, as an
/// exclusive char index.
fn trim_token_end(chars: &[char]) -> usize {
    let mut end = chars.len();
    while end > 0 {
        let strip = match chars[end - 1] {
            // A close that has an open of its own inside the token belongs to
            // the path: `report(1).pdf` is a filename, `(src/lib.rs)` is not.
            ')' => count_chars(&chars[..end], ')') > count_chars(&chars[..end], '('),
            ']' => count_chars(&chars[..end], ']') > count_chars(&chars[..end], '['),
            '}' => count_chars(&chars[..end], '}') > count_chars(&chars[..end], '{'),
            // `.`, `..` and `foo/.` are directories, not a sentence that ran
            // into the path.
            '.' => !matches!(
                end.checked_sub(2).and_then(|i| chars.get(i)),
                None | Some('.' | '/' | '\\')
            ),
            ',' | ';' | '\'' | '"' | '>' | '`' | '）' | '］' | '】' | '》' | '」' | '』' | '。'
            | '，' | '、' | '；' | '：' => true,
            _ => false,
        };
        if !strip {
            break;
        }
        end -= 1;
    }
    end
}

fn count_chars(chars: &[char], needle: char) -> usize {
    chars.iter().filter(|&&c| c == needle).count()
}

struct FileLocation {
    path: String,
    line: Option<u32>,
    column: Option<u32>,
    /// Whether the whole token is also worth reading as a filename.
    ///
    /// `backup(1)` and `main.ts(10)` are spelled identically, and only the
    /// filesystem knows which is which, so both readings go on the ladder.
    /// A `:10:2` suffix gets no such courtesy: a colon is illegal in a
    /// Windows filename and vanishingly rare in a POSIX one, and doubling
    /// the probes for the spelling every compiler uses is not worth it.
    literal_too: bool,
}

/// The path, and the `line:column` written onto the end of it.
///
/// Three spellings cover what build tools actually print: `main.rs:10:2`
/// (rustc, clang, ripgrep, eslint), `main.ts(10,2)` (tsc, MSVC, and every
/// compiler that grew up on Windows) and `main.rs#L10` (a permalink pasted
/// into a shell).
fn split_file_location(token: &str) -> FileLocation {
    if let Some(location) = paren_location(token) {
        return location;
    }
    if let Some(location) = anchor_location(token) {
        return location;
    }
    colon_location(token)
}

/// `main.ts(10,2)` and `main.ts(10)`.
fn paren_location(token: &str) -> Option<FileLocation> {
    let inner = token.strip_suffix(')')?;
    let open = inner.rfind('(')?;
    let (path, digits) = inner.split_at(open);
    let digits = &digits['('.len_utf8()..];
    if path.is_empty() || digits.is_empty() {
        return None;
    }
    let mut parts = digits.split(',').map(str::trim);
    let line = parts.next()?.parse().ok()?;
    let column = match parts.next() {
        Some(column) => Some(column.parse().ok()?),
        None => None,
    };
    parts.next().is_none().then_some(FileLocation {
        path: path.to_string(),
        line: Some(line),
        column,
        literal_too: true,
    })
}

/// `main.rs#L10`, and the `#L10-L20` a GitHub range permalink carries.
fn anchor_location(token: &str) -> Option<FileLocation> {
    let (path, anchor) = token.rsplit_once("#L")?;
    if path.is_empty() {
        return None;
    }
    let line = anchor.split('-').next()?.parse().ok()?;
    Some(FileLocation {
        path: path.to_string(),
        line: Some(line),
        column: None,
        literal_too: true,
    })
}

/// `main.rs:10:2` and `main.rs:10`.
fn colon_location(token: &str) -> FileLocation {
    let Some((prefix, last)) = strip_numeric_suffix(token) else {
        return FileLocation {
            path: token.to_string(),
            line: None,
            column: None,
            literal_too: false,
        };
    };
    if let Some((path, line)) = strip_numeric_suffix(prefix) {
        FileLocation {
            path: path.to_string(),
            line: Some(line),
            column: Some(last),
            literal_too: false,
        }
    } else {
        FileLocation {
            path: prefix.to_string(),
            line: Some(last),
            column: None,
            literal_too: false,
        }
    }
}

fn strip_numeric_suffix(token: &str) -> Option<(&str, u32)> {
    let (prefix, suffix) = token.rsplit_once(':')?;
    if prefix.is_empty() || suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let value = suffix.parse().ok()?;
    Some((prefix, value))
}

/// The line number written *next to* a path rather than onto it.
///
/// Python's traceback is why this exists: `File "handlers.py", line 214` puts
/// the number two tokens away, and landing on the file but not the line is
/// most of the way to useless in the one place where clicking a path is worth
/// the most. `bash`, `make` and `pytest` spell it the same way.
fn location_after(text: &str, from: usize) -> Option<u32> {
    /// Far enough to clear `", line ` and the number, and short enough that a
    /// `line` belonging to the next sentence is out of reach.
    const WINDOW: usize = 24;

    let chars: Vec<char> = text.chars().skip(from).take(WINDOW).collect();
    let at = |i: usize| chars.get(i).copied();
    let mut i = 0;
    // Whatever closed the path: the quote around it, the comma after it, the
    // colon `bash` puts there.
    while at(i)
        .is_some_and(|c| matches!(c, '"' | '\'' | ',' | ':' | ')' | ']') || c.is_whitespace())
    {
        i += 1;
    }
    let word: String = chars.iter().skip(i).take(4).collect();
    if !word.eq_ignore_ascii_case("line") {
        return None;
    }
    i += 4;
    // `lineage 5` is not a location.
    if !at(i).is_some_and(char::is_whitespace) {
        return None;
    }
    while at(i).is_some_and(char::is_whitespace) {
        i += 1;
    }
    let digits: String = chars
        .iter()
        .skip(i)
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// The token with a leading `~` turned into a real directory, still spelled
/// the pane's way — a string rather than a `PathBuf`, because deciding what is
/// absolute and how to join is the [`PathStyle`]'s job from here on and
/// `Path` would answer for the wrong machine.
fn expand_home(path: &str, roots: &LinkRoots) -> Option<String> {
    let home = || {
        home_dir(roots.cwd(), roots.local_home, roots.style)
            .map(|home| home.to_string_lossy().into_owned())
    };
    if path == "~" {
        return home();
    }
    let rest = match roots.style {
        PathStyle::Posix => path.strip_prefix("~/"),
        PathStyle::Windows => path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")),
    };
    if let Some(rest) = rest {
        return home().map(|home| match roots.style {
            PathStyle::Posix => format!("{}/{rest}", home.trim_end_matches('/')),
            PathStyle::Windows => Path::new(&home).join(rest).to_string_lossy().into_owned(),
        });
    }
    Some(path.to_string())
}

/// The home `~` stands for, read out of the cwd where it can be and out of the
/// environment where it cannot.
///
/// The environment half only applies to a pane on this machine. Anywhere else
/// it would be guessing with our own answer: a cwd of `/srv/app` on a Linux
/// box says nothing about that box's home, and turning `~/.zshrc` into
/// `/Users/me/.zshrc` and asking the far side about it is how a link ends up
/// pointing at a file nobody meant.
fn home_dir(cwd: Option<&Path>, local_home: bool, style: PathStyle) -> Option<PathBuf> {
    if let Some(home) = cwd.and_then(|cwd| home_from_cwd(cwd, style)) {
        return Some(home);
    }
    if !local_home {
        return None;
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// The `/home/<user>` or `/Users/<user>` a POSIX cwd sits under.
///
/// Read off the string rather than `Path::components`, and keyed off the
/// pane's dialect rather than `cfg!(unix)`. The old spelling was gated to Unix
/// clients, which meant a Windows tty7 could not say what `~` meant in *any*
/// pane it was looking at — `~/.zshrc` printed by a Linux host resolved on a
/// Mac and silently did not on a Windows box beside it.
///
/// A Windows-dialect cwd still gets no answer: nothing about `D:\Users\team`
/// says whose home it is, and the environment fallback in [`home_dir`] is both
/// available and right for the panes that spell paths that way.
fn home_from_cwd(cwd: &Path, style: PathStyle) -> Option<PathBuf> {
    if style != PathStyle::Posix {
        return None;
    }
    let cwd = cwd.to_str()?;
    let mut parts = cwd.strip_prefix('/')?.split('/').filter(|p| !p.is_empty());
    let base = parts.next()?;
    let user = parts.next()?;
    matches!(base, "Users" | "home").then(|| PathBuf::from(format!("/{base}/{user}")))
}

fn trim_trailing_punct(token: &mut String) {
    loop {
        let strip = match token.chars().next_back() {
            Some(')') => count_char(token, ')') > count_char(token, '('),
            Some(']') => count_char(token, ']') > count_char(token, '['),
            Some(
                '.' | ',' | ';' | ':' | '\'' | '"' | '>' | '）' | '］' | '】' | '》' | '」' | '。'
                | '，' | '；' | '：',
            ) => true,
            _ => false,
        };
        if !strip {
            return;
        }
        token.pop();
    }
}

fn count_char(s: &str, needle: char) -> usize {
    s.chars().filter(|&c| c == needle).count()
}

fn truncate_at_unbalanced_close(token: &mut String) {
    let mut parens = 0usize;
    let mut brackets = 0usize;
    for (i, c) in token.char_indices() {
        match c {
            '(' => parens += 1,
            '[' => brackets += 1,
            ')' if parens == 0 => {
                token.truncate(i);
                return;
            }
            ']' if brackets == 0 => {
                token.truncate(i);
                return;
            }
            ')' => parens -= 1,
            ']' => brackets -= 1,
            _ => {}
        }
    }
}

pub(super) fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | ':'
                | '/'
                | '?'
                | '#'
                | '['
                | ']'
                | '@'
                | '!'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '='
                | '%'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capped_match_count_says_it_is_capped() {
        // Same rule the count uses. Below the ceiling the number is the truth;
        // at the ceiling it is a floor, and has to read like one.
        let mark = |total: usize| match total >= MAX_MATCHES {
            true => "+",
            false => "",
        };
        assert_eq!(mark(0), "");
        assert_eq!(mark(MAX_MATCHES - 1), "");
        assert_eq!(mark(MAX_MATCHES), "+");
    }

    #[test]
    fn the_bar_hides_whole_rows_however_tall_they_are() {
        // 8px inset + 34px tall, so at the default 17px line height the bar
        // sits over rows 0, 1 and part of 2 — all three count as hidden.
        assert_eq!(rows_under_the_bar(17.), 3);
        assert_eq!(rows_under_the_bar(42.), 1);
        assert_eq!(rows_under_the_bar(43.), 1);
        assert_eq!(rows_under_the_bar(10.), 5);
        assert_eq!(rows_under_the_bar(0.), 0, "a zero height must not divide");
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

    #[test]
    fn regex_escape_neutralizes_metacharacters() {
        assert_eq!(regex_escape("a.b*c"), r"a\.b\*c");
        assert_eq!(regex_escape("foo(bar)"), r"foo\(bar\)");
        assert_eq!(regex_escape("1+1=2"), r"1\+1=2");
        assert_eq!(regex_escape("hello"), "hello");
    }

    #[test]
    fn url_at_detects_http_and_strips_trailing_punct() {
        let line = "go https://example.com, now";
        assert_eq!(url_at(line, 6).as_deref(), Some("https://example.com"));
    }

    #[test]
    fn url_at_promotes_bare_www_and_ignores_plain_words() {
        assert_eq!(
            url_at("visit www.rust-lang.org now", 8).as_deref(),
            Some("https://www.rust-lang.org")
        );
        assert_eq!(url_at("just a word", 6), None);
        assert_eq!(url_at("word ", 4), None);
        assert_eq!(url_at("word", 99), None);
    }

    #[test]
    fn url_span_at_reports_inclusive_columns_without_trailing_punct() {
        let line = "go https://example.com, now";
        assert_eq!(
            url_span_at(line, 10),
            Some((3, 21, "https://example.com".to_string()))
        );
        assert_eq!(&line[3..=21], "https://example.com");
    }

    #[test]
    fn url_span_at_accepts_file_and_ftp_schemes() {
        assert_eq!(
            url_at("open file:///etc/hosts here", 5).as_deref(),
            Some("file:///etc/hosts")
        );
        assert_eq!(
            url_at("get ftp://host/pub done", 4).as_deref(),
            Some("ftp://host/pub")
        );
    }

    #[test]
    fn url_span_at_strips_various_trailing_punctuation() {
        assert_eq!(
            url_at("open https://a.com] done", 7).as_deref(),
            Some("https://a.com")
        );
        assert_eq!(
            url_at("open https://a.com> done", 7).as_deref(),
            Some("https://a.com")
        );
        assert_eq!(
            url_at("open https://a.com';: done", 7).as_deref(),
            Some("https://a.com")
        );
    }

    #[test]
    fn url_span_at_strips_leading_wrappers() {
        assert_eq!(
            url_at("see (https://a.com) ok", 8).as_deref(),
            Some("https://a.com")
        );
        assert_eq!(
            url_at("see [https://a.com] ok", 8).as_deref(),
            Some("https://a.com")
        );
        assert_eq!(
            url_at("see <https://a.com> ok", 8).as_deref(),
            Some("https://a.com")
        );
        assert_eq!(
            url_at("say \"https://a.com\" ok", 8).as_deref(),
            Some("https://a.com")
        );
        assert_eq!(
            url_at("(www.rust-lang.org)", 5).as_deref(),
            Some("https://www.rust-lang.org")
        );
    }

    #[test]
    fn url_span_at_reports_trimmed_span_after_stripping_both_ends() {
        let line = "log [https://a.com] end";
        let (start, end, url) = url_span_at(line, 8).expect("URL inside the brackets");
        assert_eq!(url, "https://a.com");
        assert_eq!(&line[start..=end], "https://a.com");
        assert_eq!(&line[start - 1..start], "[");
        assert_eq!(&line[end + 1..end + 2], "]");
    }

    #[test]
    fn url_at_detects_url_glued_to_cjk_prefix() {
        let url = "https://github.com/acme/app/pull/42";
        let line = format!("已创建：{url}");
        let scheme_col = 4;
        assert_eq!(url_at(&line, scheme_col).as_deref(), Some(url));
        assert_eq!(url_at(&line, 12).as_deref(), Some(url));
        let (start, end, got) = url_span_at(&line, scheme_col).expect("URL after prefix");
        assert_eq!(start, scheme_col);
        assert_eq!(got, url);
        assert_eq!(end, line.chars().count() - 1);

        let row = format!("PR 已创建:{url} 🎉收尾:删除临时");
        let h = row.chars().position(|c| c == 'h').expect("scheme start");
        assert_eq!(url_at(&row, h).as_deref(), Some(url));
        assert_eq!(url_at(&row, 0), None);
    }

    #[test]
    fn url_at_ignores_hover_on_cjk_prefix_before_url() {
        let line = "已创建：https://a.com";
        assert_eq!(url_at(line, 0), None);
        assert_eq!(url_at(line, 3), None);
        assert_eq!(url_at(line, 4).as_deref(), Some("https://a.com"));
    }

    #[test]
    fn url_at_strips_full_width_trailing_punctuation() {
        assert_eq!(
            url_at("见（https://a.com）", 3).as_deref(),
            Some("https://a.com")
        );
        assert_eq!(
            url_at("详见 https://a.com。", 5).as_deref(),
            Some("https://a.com")
        );
    }

    #[test]
    fn url_at_stops_at_full_width_open_bracket_glued_after_url() {
        let url = "https://github.com/acme/app/pull/343";
        let line = format!("PR 已创建：{url}（fix/cache-write-tokens → dev）");
        let h = line.chars().position(|c| c == 'h').expect("scheme start");
        assert_eq!(url_at(&line, h).as_deref(), Some(url));
        let (start, end, got) = url_span_at(&line, h + 10).expect("URL before bracket");
        assert_eq!(got, url);
        assert_eq!(start, h);
        assert_eq!(line.chars().nth(end + 1), Some('（'));
        let f = line.chars().position(|c| c == 'f').expect("`fix` start");
        assert_eq!(url_at(&line, f), None);
    }

    #[test]
    fn url_at_keeps_ascii_parens_inside_a_url() {
        let url = "https://en.wikipedia.org/wiki/Rust_(programming_language)/history";
        assert_eq!(url_at(url, 40).as_deref(), Some(url));
        let url = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
        assert_eq!(url_at(url, 40).as_deref(), Some(url));
        let line = format!("see ({url}) ok");
        assert_eq!(url_at(&line, 8).as_deref(), Some(url));
        let url = "http://[::1]:8080/status";
        let line = format!("probe [{url}] done");
        assert_eq!(url_at(&line, 10).as_deref(), Some(url));
    }

    #[test]
    fn url_at_stops_at_unbalanced_close_paren_glued_after_url() {
        let url = "https://github.com/l0ng-ai/tty7/pull/43";
        let line = format!("PR 已开:#43 ({url})(Fixes #42),分支 fix-x。");
        let h = line.chars().position(|c| c == 'h').expect("scheme start");
        assert_eq!(url_at(&line, h).as_deref(), Some(url));
        let (start, end, got) = url_span_at(&line, h + 10).expect("URL inside parens");
        assert_eq!(got, url);
        assert_eq!(line.chars().nth(start - 1), Some('('));
        assert_eq!(line.chars().nth(end + 1), Some(')'));
        let f = line.chars().position(|c| c == 'F').expect("`Fixes` start");
        assert_eq!(url_at(&line, f), None);
        assert_eq!(
            url_at("read https://a.com/x]next now", 8).as_deref(),
            Some("https://a.com/x")
        );
    }

    #[test]
    fn url_span_at_rejects_www_without_a_dot_and_empty_tokens() {
        assert_eq!(url_at("www near text", 1), None);
        assert_eq!(url_at("...", 1), None);
        assert_eq!(url_at("httpsomething", 3), None);
    }

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-link-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temporary link-test dir");
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create temporary parent dir");
        }
        std::fs::write(&path, b"").expect("create temporary file");
        path
    }

    /// `link_at` against the real filesystem, the way a local pane resolves.
    fn local_link_at(line: &str, col: usize, roots: &LinkRoots, files: bool) -> Option<LinkMatch> {
        link_at(line, col, roots, files, &mut local_probe)
    }

    fn one_root(cwd: &Path) -> LinkRoots {
        LinkRoots::local(vec![cwd.to_path_buf()])
    }

    fn assert_file_link(
        line: &str,
        col: usize,
        cwd: &Path,
        expected_path: &Path,
        expected_line: Option<u32>,
        expected_column: Option<u32>,
    ) {
        let link = local_link_at(line, col, &one_root(cwd), true).expect("file link under cursor");
        match link.target {
            LinkTarget::File {
                path, line, column, ..
            } => {
                assert_eq!(path, expected_path);
                assert_eq!(line, expected_line);
                assert_eq!(column, expected_column);
            }
            LinkTarget::Url(url) => panic!("expected file link, got URL {url}"),
        }
    }

    #[test]
    fn link_at_detects_relative_file_paths_from_cwd() {
        let path = temp_file("src/main.rs");
        let cwd = path.parent().and_then(Path::parent).unwrap();

        assert_file_link(
            "error src/main.rs:10:2 failed",
            8,
            cwd,
            &path,
            Some(10),
            Some(2),
        );

        let link = local_link_at("error src/main.rs:10:2 failed", 8, &one_root(cwd), true)
            .expect("file link under cursor");
        assert_eq!((link.start, link.end), (6, 21));
    }

    /// Ungated with the rest: what a `~` in a POSIX pane stands for is that
    /// pane's business and not its client's, and the `#[cfg(unix)]` this used
    /// to carry described the wrong machine.
    #[test]
    fn tilde_expansion_prefers_home_inferred_from_the_pane_cwd() {
        let roots = LinkRoots {
            dirs: vec![PathBuf::from("/Users/alice/clone/tty7")],
            local_home: true,
            style: PathStyle::Posix,
        };
        assert_eq!(
            expand_home("~/clone/tty7/src/main.rs", &roots),
            Some("/Users/alice/clone/tty7/src/main.rs".to_string())
        );
    }

    /// A cwd that reveals no home is the end of the road for a pane on another
    /// machine: this machine's `$HOME` describes nobody there, and a path built
    /// out of it would be asked about — and possibly answered — on the far side.
    #[test]
    fn tilde_expansion_does_not_borrow_this_machines_home_for_another_one() {
        let elsewhere = |cwd: &str| LinkRoots {
            dirs: vec![PathBuf::from(cwd)],
            local_home: false,
            style: PathStyle::Posix,
        };
        assert_eq!(expand_home("~/.zshrc", &elsewhere("/srv/app")), None);
        assert_eq!(
            expand_home("~/.zshrc", &elsewhere("/home/deploy/app")),
            Some("/home/deploy/.zshrc".to_string()),
            "a cwd that does reveal the home needs nothing from us"
        );
        assert!(
            expand_home(
                "~/.zshrc",
                &LinkRoots::local(vec![PathBuf::from("/srv/app")])
            )
            .is_some(),
            "a local pane still falls back to the environment"
        );
    }

    #[test]
    fn link_at_detects_absolute_file_paths_and_single_line_suffix() {
        let path = temp_file("absolute.log");
        let line = format!("open {}:42 now", path.display());
        let col = line.chars().position(|c| c == '/').unwrap_or(5);

        assert_file_link(&line, col, Path::new("/"), &path, Some(42), None);
    }

    #[test]
    fn link_at_trims_wrappers_and_trailing_punctuation_around_file_paths() {
        let path = temp_file("wrapped/src/lib.rs");
        let cwd = path.parent().and_then(Path::parent).unwrap();
        let line = "see (src/lib.rs:7), now";

        let link = local_link_at(line, 7, &one_root(cwd), true).expect("wrapped file link");
        assert_eq!((link.start, link.end), (5, 16));
        match link.target {
            LinkTarget::File {
                path: got,
                line,
                column,
                is_dir,
            } => {
                assert_eq!(got, path);
                assert_eq!(line, Some(7));
                assert_eq!(column, None);
                assert!(!is_dir);
            }
            LinkTarget::Url(url) => panic!("expected file link, got URL {url}"),
        }
    }

    #[test]
    fn link_at_rejects_missing_files_and_file_detection_can_be_disabled() {
        let cwd = std::env::temp_dir();

        assert_eq!(
            local_link_at("missing src/nope.rs:1", 9, &one_root(&cwd), true),
            None
        );
        assert_eq!(
            local_link_at("missing src/nope.rs:1", 9, &one_root(&cwd), false),
            None
        );

        let path = temp_file("disabled.rs");
        let line = format!("open {}", path.display());
        assert!(local_link_at(&line, 6, &one_root(&cwd), false).is_none());
    }

    #[test]
    fn link_at_keeps_url_detection_ahead_of_file_detection() {
        let url = "https://example.com/src/main.rs";
        let link = local_link_at(url, 10, &one_root(Path::new("/")), true).expect("URL link");
        assert_eq!(
            link,
            LinkMatch {
                start: 0,
                end: url.len() - 1,
                target: LinkTarget::Url(url.to_string()),
            }
        );
    }

    #[test]
    fn link_at_detects_directory_paths() {
        let file = temp_file("dircase/nested/inner.txt");
        let dir = file.parent().unwrap();
        let cwd = dir.parent().and_then(Path::parent).unwrap();

        let link = local_link_at("artifacts in dircase/nested here", 14, &one_root(cwd), true)
            .expect("directory link");
        assert_eq!((link.start, link.end), (13, 26));
        match link.target {
            LinkTarget::File {
                path,
                line,
                column,
                is_dir,
            } => {
                assert_eq!(path, dir);
                assert_eq!(line, None);
                assert_eq!(column, None);
                assert!(is_dir, "a directory link says so, so the tree can take it");
            }
            LinkTarget::Url(url) => panic!("expected directory link, got URL {url}"),
        }

        assert!(local_link_at("ls dircase/nested/ done", 5, &one_root(cwd), true).is_some());
        assert!(
            local_link_at(
                "artifacts in dircase/nested here",
                14,
                &one_root(cwd),
                false
            )
            .is_none()
        );
    }

    #[test]
    fn a_relative_path_falls_back_to_the_repository_root() {
        // What `cargo` prints from a member directory: the path is measured
        // from the workspace root, not from where the shell happens to be.
        let path = temp_file("multiroot/crates/inner/src/lib.rs");
        let root = path
            .ancestors()
            .nth(4)
            .expect("multiroot/ above crates/inner/src")
            .to_path_buf();
        let cwd = root.join("crates/inner");
        std::fs::create_dir_all(cwd.join("empty")).expect("create a cwd with no src/");

        let line = "warning: unused import crates/inner/src/lib.rs:3";
        let col = line.find("crates").expect("path start");
        assert_eq!(local_link_at(line, col, &one_root(&cwd), true), None);

        let roots = LinkRoots::local(vec![cwd.clone(), root.clone()]);
        let link = local_link_at(line, col, &roots, true).expect("resolved from the repo root");
        match link.target {
            LinkTarget::File { path: got, .. } => assert_eq!(got, path),
            LinkTarget::Url(url) => panic!("expected file link, got URL {url}"),
        }
    }

    #[test]
    fn the_nearer_root_wins_when_both_hold_the_name() {
        let near = temp_file("ambiguous/crate/src/main.rs");
        let far = temp_file("ambiguous/src/main.rs");
        let cwd = near
            .ancestors()
            .nth(2)
            .expect("ambiguous/crate above src/")
            .to_path_buf();
        let root = far.ancestors().nth(2).expect("ambiguous/").to_path_buf();

        let roots = LinkRoots::local(vec![cwd, root]);
        let link = local_link_at("at src/main.rs:1", 3, &roots, true).expect("file link");
        match link.target {
            LinkTarget::File { path, .. } => assert_eq!(
                path, near,
                "the pane's own directory is tried before the repository around it"
            ),
            LinkTarget::Url(url) => panic!("expected file link, got URL {url}"),
        }
    }

    #[test]
    fn an_unanswered_probe_resolves_to_nothing_and_asks_about_every_root() {
        let mut asked: Vec<PathBuf> = Vec::new();
        let roots = LinkRoots::local(vec![PathBuf::from("/work/crate"), PathBuf::from("/work")]);
        let link = link_at("see src/lib.rs:9 there", 5, &roots, true, &mut |path, _| {
            asked.push(path.to_path_buf());
            Probe::Unknown
        });

        assert_eq!(link, None, "nothing underlines until the host has answered");
        assert_eq!(
            asked,
            vec![
                PathBuf::from("/work/crate/src/lib.rs"),
                PathBuf::from("/work/src/lib.rs"),
            ],
            "every root is asked about, so one round trip covers them all"
        );
    }

    #[test]
    fn a_path_shaped_token_is_kept_apart_from_a_bare_word() {
        let path_shaped = file_candidate_at("wrote scratchpad/notes.md now", 8).expect("candidate");
        assert_eq!(path_shaped.path, "scratchpad/notes.md");
        assert!(path_shaped.looks_like_a_path(PathStyle::NATIVE));

        let word = file_candidate_at("wrote notes now", 8).expect("candidate");
        assert_eq!(word.path, "notes");
        assert!(
            !word.looks_like_a_path(PathStyle::NATIVE),
            "a bare word must not raise a notification on every modifier-click"
        );

        let escaped = file_candidate_at(r"wrote a\b now", 6).expect("candidate");
        assert_eq!(escaped.path, r"a\b");
        assert!(
            escaped.looks_like_a_path(PathStyle::Windows),
            "a backslash separates directories in a Windows pane"
        );
        assert!(
            !escaped.looks_like_a_path(PathStyle::Posix),
            "and escapes a space in a POSIX one, whatever this client runs"
        );
    }

    #[test]
    fn a_relative_candidate_has_no_paths_without_a_root() {
        let candidate = file_candidate_at("see src/lib.rs here", 5).expect("candidate");
        assert!(
            candidate.paths(&LinkRoots::default()).is_empty(),
            "a pane that never said where it is cannot measure a relative path"
        );
        assert_eq!(
            candidate.paths(&LinkRoots::local(vec![
                PathBuf::from("/w"),
                PathBuf::from("/w")
            ])),
            vec![PathBuf::from("/w/src/lib.rs")],
            "a repo root equal to the cwd is one root, not two probes"
        );
    }

    #[test]
    fn an_absolute_candidate_ignores_the_roots_entirely() {
        let candidate = file_candidate_at("open /etc/hosts now", 6).expect("candidate");
        assert_eq!(
            candidate.paths(&LinkRoots::local(vec![PathBuf::from("/w")])),
            vec![PathBuf::from("/etc/hosts")]
        );
    }

    /// `is_rooted` decides whether a report about an unresolved token may name
    /// a directory it was "looked for under", so it has to agree with
    /// [`FileCandidate::paths`] about when the roots are consulted at all.
    ///
    /// Both used to ask `Path::is_absolute`, which answers for the machine
    /// tty7 runs on rather than the one the pane's paths are on — so this only
    /// held on Unix and was gated to it. Both now ask the pane's own dialect,
    /// and the agreement holds on every client.
    #[test]
    fn a_rooted_candidate_is_told_apart_from_one_measured_from_a_root() {
        let posix_roots = LinkRoots {
            dirs: vec![PathBuf::from("/home/u/proj")],
            local_home: false,
            style: PathStyle::Posix,
        };
        for line in ["open /etc/hosts now", "open ~/.zshrc now"] {
            let candidate = file_candidate_at(line, 6).expect("candidate");
            assert!(
                candidate.is_rooted(PathStyle::Posix),
                "{line} says for itself where it starts"
            );
            let paths = candidate.paths(&posix_roots);
            assert!(
                !paths.iter().any(|p| p.starts_with("/home/u/proj")),
                "{line} was not measured from the pane's directory: {paths:?}"
            );
        }
        for line in [r"open C:\Windows\win.ini now", "open ~/.gitconfig now"] {
            assert!(
                file_candidate_at(line, 6)
                    .expect("candidate")
                    .is_rooted(PathStyle::Windows),
                "{line} says for itself where it starts"
            );
        }
        for style in [PathStyle::Posix, PathStyle::Windows] {
            assert!(
                !file_candidate_at("see src/lib.rs here", 5)
                    .expect("candidate")
                    .is_rooted(style),
                "a relative path is only ever found by measuring from somewhere"
            );
        }
    }

    /// The bug this whole [`PathStyle`] exists for: a Windows tty7 looking at
    /// a Linux pane — a remote workspace, a native-SSH pane, or a WSL distro
    /// reached as a host — used to read `/etc/hosts` as a *relative* path,
    /// because `Path::is_absolute` speaks for the client and a leading `/` is
    /// not absolute on Windows. The pane's paths were then measured from its
    /// own directory and the far side was asked about something it never
    /// printed, so nothing ever underlined.
    #[test]
    fn a_posix_pane_roots_its_own_paths_on_every_client() {
        let roots = LinkRoots {
            dirs: vec![PathBuf::from("/home/u/proj")],
            local_home: false,
            style: PathStyle::Posix,
        };
        let candidate = file_candidate_at("open /etc/hosts now", 6).expect("candidate");
        assert_eq!(
            candidate.paths(&roots),
            vec![PathBuf::from("/etc/hosts")],
            "the pane said where the path starts; the roots have nothing to add"
        );

        // And with no cwd reported at all there is still exactly one thing it
        // can mean. This is the case that failed outright: no roots meant no
        // paths, so the host was never even asked.
        let roots = LinkRoots {
            dirs: Vec::new(),
            local_home: false,
            style: PathStyle::Posix,
        };
        assert_eq!(candidate.paths(&roots), vec![PathBuf::from("/etc/hosts")]);
    }

    /// A relative path is the other half, and it fails more quietly: the join
    /// used to reach for the *client's* separator, so a Windows tty7 asked a
    /// Linux host about `/home/u/proj\src/lib.rs`.
    #[test]
    fn a_posix_pane_joins_a_relative_path_with_its_own_separator() {
        let roots = LinkRoots {
            dirs: vec![PathBuf::from("/home/u/proj"), PathBuf::from("/home/u")],
            local_home: false,
            style: PathStyle::Posix,
        };
        let paths = file_candidate_at("see src/lib.rs here", 5)
            .expect("candidate")
            .paths(&roots);
        assert_eq!(
            paths
                .iter()
                .map(|p| p.to_string_lossy())
                .collect::<Vec<_>>(),
            vec!["/home/u/proj/src/lib.rs", "/home/u/src/lib.rs"],
            "the string the far side is asked about has to be one it can stat"
        );
    }

    /// `~` in a POSIX pane is read out of the pane's own cwd — on every
    /// client. The rule was gated to Unix ones, so a Windows tty7 could not
    /// resolve `~/.zshrc` in any pane it was looking at.
    #[test]
    fn a_posix_pane_reads_a_tilde_out_of_its_own_cwd() {
        let roots = LinkRoots {
            dirs: vec![PathBuf::from("/home/u/proj")],
            local_home: false,
            style: PathStyle::Posix,
        };
        let paths = file_candidate_at("open ~/.zshrc now", 6)
            .expect("candidate")
            .paths(&roots);
        assert_eq!(
            paths
                .iter()
                .map(|p| p.to_string_lossy())
                .collect::<Vec<_>>(),
            vec!["/home/u/.zshrc"]
        );

        let no_home = LinkRoots {
            dirs: vec![PathBuf::from("/srv/app")],
            local_home: false,
            style: PathStyle::Posix,
        };
        assert!(
            file_candidate_at("open ~/.zshrc now", 6)
                .expect("candidate")
                .paths(&no_home)
                .is_empty(),
            "a cwd that reveals no home may not borrow this machine's (#568)"
        );
    }

    /// The deliberate other half: a pane *on this machine* keeps this OS's
    /// reading of its own output. `/etc` in a `cmd.exe` pane sitting on `C:`
    /// means `C:\etc`, the way `cd /etc` does there — so it is measured from
    /// the pane's root and not turned into a link to a file Windows has not
    /// got. Pinned because the temptation is to make every `/`-rooted token
    /// stand alone, and that would underline `/etc/hosts` in a PowerShell pane
    /// with nothing behind it.
    #[test]
    fn a_local_pane_reads_its_own_output_the_way_its_own_os_does() {
        let candidate = file_candidate_at("open /etc/hosts now", 6).expect("candidate");
        assert_eq!(
            candidate.is_rooted(PathStyle::NATIVE),
            cfg!(unix),
            "a leading slash roots a path on Unix and names a drive-relative \
             directory on Windows"
        );

        let roots = LinkRoots::local(vec![PathBuf::from("/w")]);
        assert_eq!(
            roots.style,
            PathStyle::NATIVE,
            "a pane on this machine spells paths this machine's way"
        );
        assert_eq!(
            candidate.paths(&roots),
            vec![PathBuf::from("/etc/hosts")],
            "which comes to the same thing under a root with no drive letter"
        );
    }

    /// The Windows half of the rule above, spelled out where it can be: the
    /// drive the pane is on is what a leading `/` there is measured from.
    /// Cannot be asserted on a Unix client, where `PathBuf` has no notion of a
    /// drive at all — the *rule* is pinned ungated above, this is the reading.
    #[test]
    #[cfg(windows)]
    fn a_local_windows_pane_measures_a_leading_slash_from_its_own_drive() {
        let roots = LinkRoots::local(vec![PathBuf::from(r"C:\proj")]);
        assert_eq!(
            file_candidate_at("open /etc/hosts now", 6)
                .expect("candidate")
                .paths(&roots),
            vec![PathBuf::from(r"C:\etc\hosts")],
            "`/etc` in a cmd.exe pane on C: is C:\\etc, the way `cd /etc` is"
        );
    }

    #[test]
    fn a_windows_pane_roots_a_drive_letter_and_a_share() {
        let style = PathStyle::Windows;
        for rooted in [r"C:\Windows", "C:/Windows", r"\\server\share\x"] {
            assert!(style.is_absolute(rooted), "{rooted} names its own root");
        }
        for measured in ["/etc/hosts", "C:notes.txt", r"src\lib.rs", "", "C:"] {
            assert!(
                !style.is_absolute(measured),
                "{measured} has to be measured from somewhere"
            );
        }
        for measured in [r"C:\Windows", r"src\lib.rs", ""] {
            assert!(
                !PathStyle::Posix.is_absolute(measured),
                "{measured} is not a POSIX root"
            );
        }
    }

    #[test]
    fn a_hosts_dialect_is_read_off_the_directory_it_reports() {
        assert_eq!(
            PathStyle::of_dir(Path::new("/home/u/proj")),
            PathStyle::Posix
        );
        assert_eq!(
            PathStyle::of_dir(Path::new(r"C:\Users\u\proj")),
            PathStyle::Windows
        );
        assert_eq!(
            PathStyle::of_dir(Path::new(r"\\wsl$\Ubuntu\home\u")),
            PathStyle::Windows
        );
        // A directory that names no root at all says nothing about the host,
        // so it has to fall the way no directory does — POSIX. Reading it as
        // Windows would hand a Linux host `\`-joined paths on the strength of
        // a cwd it never really reported.
        for nothing_to_go_on in ["", "proj", "~/proj", "C:notes"] {
            assert_eq!(
                PathStyle::of_dir(Path::new(nothing_to_go_on)),
                PathStyle::Posix,
                "{nothing_to_go_on:?} names no Windows root"
            );
        }
    }

    #[test]
    fn link_at_requires_a_file_when_a_line_suffix_is_present() {
        let file = temp_file("localhost/keep.txt");
        let cwd = file.parent().and_then(Path::parent).unwrap();

        assert_eq!(
            local_link_at("listening on localhost:8080", 15, &one_root(cwd), true),
            None
        );
        assert!(local_link_at("listening on localhost", 15, &one_root(cwd), true).is_some());
    }

    /// The span a link underlines, as `(start, end)` inclusive columns.
    fn link_span(line: &str, col: usize, cwd: &Path) -> (usize, usize) {
        let link = local_link_at(line, col, &one_root(cwd), true).expect("file link under cursor");
        (link.start, link.end)
    }

    /// A path is rarely written on its own: build tools glue a flag, a label
    /// or a diff side onto the front of it, and none of that is the path.
    #[test]
    fn a_path_is_found_inside_the_prefix_glued_onto_it() {
        let path = temp_file("prefixed/src/main.rs");
        let cwd = path.parent().and_then(Path::parent).unwrap();
        let at = |line: &str| {
            let byte = line.find("src/main.rs").expect("path start");
            let col = line[..byte].chars().count();
            (link_span(line, col, cwd), col)
        };

        for line in [
            "rustc --emit=metadata --file=src/main.rs",
            "note:src/main.rs",
            "--- a/src/main.rs",
            "+++ b/src/main.rs",
            "├──src/main.rs",
        ] {
            let ((start, end), col) = at(line);
            assert_eq!(
                (start, end),
                (col, col + "src/main.rs".chars().count() - 1),
                "{line:?} must underline the path and nothing around it"
            );
            assert_file_link(line, col, cwd, &path, None, None);
        }
    }

    /// `ls -F` says what it is listing by writing the type onto the name.
    #[test]
    fn an_ls_type_marker_is_not_part_of_the_name() {
        let path = temp_file("marked/run.sh");
        let cwd = path.parent().unwrap();

        for marker in ['@', '*', '=', '|'] {
            let line = format!("run.sh{marker}  other");
            assert_file_link(&line, 2, cwd, &path, None, None);
            assert_eq!(link_span(&line, 2, cwd), (0, 5), "{marker} is not a name");
        }
    }

    /// What every compiler that grew up on Windows prints, `tsc` included.
    #[test]
    fn a_parenthesised_line_and_column_is_read_as_a_location() {
        let path = temp_file("msvc/app.ts");
        let cwd = path.parent().and_then(Path::parent).unwrap();

        assert_file_link(
            "msvc/app.ts(10,2): error TS2304",
            2,
            cwd,
            &path,
            Some(10),
            Some(2),
        );
        assert_file_link("msvc/app.ts(10): warning", 2, cwd, &path, Some(10), None);
        assert_file_link("msvc/app.ts#L7 pasted", 2, cwd, &path, Some(7), None);
    }

    /// The same spelling names a file on a machine that has downloaded
    /// something twice, so the written-out reading is tried first.
    #[test]
    fn a_filename_that_looks_like_a_location_wins_over_the_location() {
        let path = temp_file("dupes/backup(1)");
        let cwd = path.parent().and_then(Path::parent).unwrap();

        assert_file_link("kept dupes/backup(1) here", 5, cwd, &path, None, None);
    }

    /// Python puts the line number two tokens away from the file, and that is
    /// the single place where clicking a path is worth the most.
    #[test]
    fn a_line_number_written_beside_a_path_is_still_a_location() {
        let path = temp_file("beside/handlers.py");
        let cwd = path.parent().and_then(Path::parent).unwrap();
        // Found by the whole path rather than by its first separator: a
        // Windows temp path keeps the `/` this one was built with, and looking
        // for a separator lands three quarters of the way along it.
        let shown = path.display().to_string();
        let quoted = format!("  File \"{shown}\", line 214, in dispatch");
        let byte = quoted.find(&shown).expect("the path in the line");
        let col = quoted[..byte].chars().count();

        assert_file_link(&quoted, col, Path::new("/"), &path, Some(214), None);
        assert_eq!(
            link_span(&quoted, col, Path::new("/")),
            (col, col + path.to_string_lossy().chars().count() - 1),
            "the underline stays on the path, not on the prose after it"
        );

        assert_file_link(
            "beside/handlers.py: line 12: bad substitution",
            2,
            cwd,
            &path,
            Some(12),
            None,
        );
        assert_file_link("beside/handlers.py lineage 12", 2, cwd, &path, None, None);
    }

    /// A directory has no line to land on, so nothing beside it is read as
    /// one.
    #[test]
    fn nothing_beside_a_directory_is_read_as_a_line_number() {
        let file = temp_file("nolines/inner/keep.txt");
        let dir = file.parent().unwrap();
        let cwd = dir.parent().and_then(Path::parent).unwrap();

        assert_file_link("nolines/inner, line 12", 2, cwd, dir, None, None);
    }

    /// A trailing period ends a sentence, except where it is the directory.
    #[test]
    fn a_dot_that_names_a_directory_survives_the_trailing_trim() {
        let file = temp_file("dotdir/inner/keep.txt");
        let inner = file.parent().unwrap();
        let cwd = inner.parent().unwrap();

        let link = local_link_at("copied to inner/. now", 10, &one_root(cwd), true)
            .expect("a dot directory is a directory");
        match link.target {
            LinkTarget::File { path, is_dir, .. } => {
                assert_eq!(path, cwd.join("inner/."));
                assert!(is_dir);
            }
            LinkTarget::Url(url) => panic!("expected directory link, got URL {url}"),
        }
    }

    /// A label in front of a colon only peels off when what follows is
    /// written like a path. Otherwise a prompt segment turns into a link the
    /// moment the directory happens to hold a folder by that name.
    #[test]
    fn a_label_in_front_of_a_bare_word_is_not_a_path() {
        let file = temp_file("labelled/main/keep.txt");
        // `<tmp>/labelled`, the one directory that holds a `main/`.
        let labelled = file.parent().and_then(Path::parent).unwrap();

        assert!(
            local_link_at("on main/keep.txt", 4, &one_root(labelled), true).is_some(),
            "the directory itself is still reachable"
        );
        assert!(
            local_link_at("branch:main", 8, &one_root(labelled), true).is_none(),
            "but the branch a prompt is reporting is not a link"
        );
        assert!(
            local_link_at("note:main/keep.txt", 6, &one_root(labelled), true).is_some(),
            "a label in front of something written like a path still peels"
        );
    }

    /// Every reading of one token is worth at most a handful of questions.
    #[test]
    fn the_candidate_ladder_stays_short() {
        let dense = "a/b=note:d/e/f.rs:10:2";
        let candidates = file_candidates_at(dense, 12);
        assert!(
            candidates.len() <= MAX_FILE_CANDIDATES,
            "{} readings of {dense:?}",
            candidates.len()
        );
        assert!(
            candidates.iter().any(|c| c.path == "d/e/f.rs"),
            "the reading behind the `note:` label is still on the ladder"
        );
    }

    /// A Windows pane sitting in `C:\proj`, the way the reporter of #965 saw it.
    fn windows_roots() -> LinkRoots {
        LinkRoots {
            dirs: vec![PathBuf::from(r"C:\proj")],
            local_home: false,
            style: PathStyle::Windows,
        }
    }

    /// `link_at` over `line` with the pointer on the first char of `needle`,
    /// against a filesystem holding exactly `exists` — and every path it was
    /// asked about, so a test can say what was *not* looked up.
    fn link_with(
        line: &str,
        needle: &str,
        roots: &LinkRoots,
        exists: &str,
    ) -> (Option<LinkMatch>, Vec<String>) {
        let byte = line.find(needle).expect("needle in line");
        let col = line[..byte].chars().count();
        let mut asked = Vec::new();
        let mut probe = |path: &Path, _: bool| {
            let path = path.to_string_lossy().into_owned();
            let hit = path == exists;
            asked.push(path);
            match hit {
                true => Probe::Hit { is_dir: false },
                false => Probe::Miss,
            }
        };
        (link_at(line, col, roots, true, &mut probe), asked)
    }

    /// The path and span a Windows pane underlines under `needle`.
    fn windows_link(line: &str, needle: &str, exists: &str) -> (String, usize, usize) {
        let (link, asked) = link_with(line, needle, &windows_roots(), exists);
        let link = link.unwrap_or_else(|| panic!("{line:?}: no link, asked {asked:?}"));
        match link.target {
            LinkTarget::File { path, .. } => {
                (path.to_string_lossy().into_owned(), link.start, link.end)
            }
            LinkTarget::Url(url) => panic!("{line:?}: expected a file link, got {url}"),
        }
    }

    /// The columns `needle` occupies in `line`, inclusive.
    fn span_of(line: &str, needle: &str) -> (usize, usize) {
        let byte = line.find(needle).expect("needle in line");
        let start = line[..byte].chars().count();
        (start, start + needle.chars().count() - 1)
    }

    /// #965: the path exactly as the reporter pasted it, and its backslashed
    /// twin. Either separator names the same file, and the file is handed on
    /// spelled the way Explorer will take it.
    #[test]
    fn a_drive_letter_path_is_a_link_with_either_separator() {
        let want = r"C:\Users\xxxxx\images\gen_gptimage2_gen_20260925_220725.png";
        for written in [
            "c:/Users/xxxxx/images/gen_gptimage2_gen_20260925_220725.png",
            r"C:\Users\xxxxx\images\gen_gptimage2_gen_20260925_220725.png",
            r"C:/Users\xxxxx/images\gen_gptimage2_gen_20260925_220725.png",
        ] {
            let line = format!("saved {written} ok");
            let (path, start, end) = windows_link(&line, "images", want);
            assert_eq!(path, want, "{line:?}");
            assert_eq!((start, end), span_of(&line, written), "{line:?}");
        }
    }

    /// Prose and markup around a Windows path is not part of it — the same
    /// trimming a POSIX path gets.
    #[test]
    fn a_drive_letter_path_sheds_the_punctuation_around_it() {
        let want = r"C:\out\a.png";
        for (line, path) in [
            ("wrote c:/out/a.png.", "c:/out/a.png"),
            (r"wrote C:\out\a.png, then quit", r"C:\out\a.png"),
            (r"(see C:\out\a.png)", r"C:\out\a.png"),
            (r#"open "C:\out\a.png";"#, r"C:\out\a.png"),
            (r"`C:\out\a.png`", r"C:\out\a.png"),
            ("「c:/out/a.png」。", "c:/out/a.png"),
            ("<c:/out/a.png>", "c:/out/a.png"),
            ("│ C:/out/a.png │", "C:/out/a.png"),
            (r"C:\out\a.png:", r"C:\out\a.png"),
        ] {
            let (got, start, end) = windows_link(line, "out", want);
            assert_eq!(got, want, "{line:?}");
            assert_eq!((start, end), span_of(line, path), "{line:?}");
        }
    }

    /// Compilers on Windows write the location onto the path; the drive's own
    /// colon must not be mistaken for one.
    #[test]
    fn a_drive_letter_path_keeps_its_line_and_column() {
        let want = r"C:\src\main.rs";
        for (line, loc) in [
            (r"C:\src\main.rs:10:2: error", (Some(10), Some(2))),
            ("c:/src/main.rs:10", (Some(10), None)),
            (r"C:\src\main.rs(10,2): error C2065", (Some(10), Some(2))),
            ("c:/src/main.rs#L10", (Some(10), None)),
            (r#"File "C:\src\main.rs", line 7"#, (Some(7), None)),
        ] {
            let (link, asked) = link_with(line, "src", &windows_roots(), want);
            match link.map(|l| l.target) {
                Some(LinkTarget::File {
                    path,
                    line: l,
                    column,
                    ..
                }) => {
                    assert_eq!(path, PathBuf::from(want), "{line:?}");
                    assert_eq!((l, column), loc, "{line:?}");
                }
                other => panic!("{line:?}: {other:?}, asked {asked:?}"),
            }
        }
    }

    /// Chinese output glues a path straight onto the sentence in front of it,
    /// often behind a full-width colon — which is how an agent reports the
    /// file it just wrote in #965. The sentence is not part of the path.
    #[test]
    fn a_drive_letter_path_is_found_inside_the_prose_glued_onto_it() {
        let want = r"C:\Users\me\a.png";
        for (line, path) in [
            ("图片已保存到：c:/Users/me/a.png", "c:/Users/me/a.png"),
            ("图片已生成c:/Users/me/a.png", "c:/Users/me/a.png"),
            (r"路径:C:\Users\me\a.png", r"C:\Users\me\a.png"),
            ("--out=c:/Users/me/a.png", "c:/Users/me/a.png"),
            ("output:c:/Users/me/a.png", "c:/Users/me/a.png"),
        ] {
            let (got, start, end) = windows_link(line, "Users", want);
            assert_eq!(got, want, "{line:?}");
            assert_eq!((start, end), span_of(line, path), "{line:?}");
        }
    }

    /// An agent prints the image it made as Markdown. The target is the link;
    /// the brackets and alt text are not.
    #[test]
    fn a_markdown_link_target_is_the_path() {
        let want = r"C:\out\chart.png";
        for line in [
            "![chart](c:/out/chart.png)",
            "see [the chart](c:/out/chart.png).",
        ] {
            let (got, start, end) = windows_link(line, "out", want);
            assert_eq!(got, want, "{line:?}");
            assert_eq!((start, end), span_of(line, "c:/out/chart.png"), "{line:?}");
        }
        let posix = LinkRoots {
            dirs: vec![PathBuf::from("/home/u/proj")],
            local_home: false,
            style: PathStyle::Posix,
        };
        let line = "[notes](docs/notes.md)";
        let (link, asked) = link_with(line, "docs", &posix, "/home/u/proj/docs/notes.md");
        let link = link.unwrap_or_else(|| panic!("no link, asked {asked:?}"));
        assert_eq!((link.start, link.end), span_of(line, "docs/notes.md"));
    }

    #[test]
    fn a_unc_share_is_a_link_that_stands_alone() {
        let want = r"\\server\share\team\a.png";
        let line = r"copied to \\server\share\team\a.png";
        let (got, start, end) = windows_link(line, "server", want);
        assert_eq!(got, want);
        assert_eq!((start, end), span_of(line, want));
    }

    /// Text that merely has a letter and a colon in it is not a drive path,
    /// and must never be looked up as one.
    #[test]
    fn a_letter_and_a_colon_is_not_a_drive_path() {
        for (line, needle) in [
            // Drive-relative, or a bare drive: nothing a pane can resolve.
            ("a:b", "a:b"),
            ("x:y and more", "x:y"),
            ("C:notes.txt", "C:notes"),
            ("see C: now", "C:"),
            // A time, a ratio, a port.
            ("at 12:30 today", "12:30"),
            ("ratio 16:9", "16:9"),
            ("localhost:8080", "localhost"),
            // A word that ends in a letter is not a drive behind it.
            ("abc:/x/y", "abc"),
            ("http:/x", "http"),
        ] {
            let (link, asked) = link_with(line, needle, &windows_roots(), "");
            assert_eq!(link, None, "{line:?}");
            for path in &asked {
                assert!(
                    path.starts_with(r"C:\proj\"),
                    "{line:?} asked about {path:?}, which is not under the pane's directory"
                );
            }
        }
        // The drive-relative ones are not asked about at all.
        for line in ["a:b", "C:notes.txt"] {
            let (_, asked) = link_with(line, line, &windows_roots(), "");
            assert!(asked.is_empty(), "{line:?} asked about {asked:?}");
        }
        // And nothing in `abc:/x/y` is read as drive `c:`.
        assert!(
            file_candidates_at("abc:/x/y", 0)
                .iter()
                .all(|c| !c.path.starts_with("c:")),
            "{:?}",
            file_candidates_at("abc:/x/y", 0)
        );
    }

    /// A Windows path that does not exist is not underlined: the probe is
    /// the gate, exactly as for a POSIX one.
    #[test]
    fn a_missing_drive_letter_path_is_not_a_link() {
        let (link, asked) = link_with("wrote c:/out/gone.png", "out", &windows_roots(), "");
        assert_eq!(link, None);
        assert_eq!(asked, vec![r"C:\out\gone.png".to_string()]);
        let candidate =
            unresolved_candidate("wrote c:/out/gone.png", 8, PathStyle::Windows).unwrap();
        assert!(candidate.looks_like_a_path(PathStyle::Windows));
        assert!(candidate.is_rooted(PathStyle::Windows));
    }

    /// A POSIX pane has no drives, but a WSL distro can reach the Windows
    /// machine it runs on through `/mnt/<drive>`, so that is what it is asked.
    #[test]
    fn a_posix_pane_reads_a_drive_letter_through_the_wsl_mount() {
        let roots = LinkRoots {
            dirs: vec![PathBuf::from("/home/u/proj")],
            local_home: false,
            style: PathStyle::Posix,
        };
        let want = "/mnt/c/Users/me/a.png";
        for written in ["c:/Users/me/a.png", r"C:\Users\me\a.png"] {
            let line = format!("wrote {written}");
            let (link, asked) = link_with(&line, "Users", &roots, want);
            assert_eq!(asked.first().map(String::as_str), Some(want), "{line:?}");
            let link = link.unwrap_or_else(|| panic!("{line:?}: no link"));
            assert_eq!((link.start, link.end), span_of(&line, written));
            let candidate = file_candidate_at(&line, 8).unwrap();
            assert!(candidate.is_rooted(PathStyle::Posix), "{line:?}");
            assert!(candidate.looks_like_a_path(PathStyle::Posix), "{line:?}");
        }
        // Where there is no such mount the probe misses and nothing underlines.
        let (link, _) = link_with("wrote c:/Users/me/a.png", "Users", &roots, "");
        assert_eq!(link, None);
    }

    #[test]
    fn a_rooted_token_is_spelled_the_way_its_host_looks_it_up() {
        let w = PathStyle::Windows;
        assert_eq!(w.rooted("c:/Users/me").as_deref(), Some(r"C:\Users\me"));
        assert_eq!(w.rooted(r"D:\x/y").as_deref(), Some(r"D:\x\y"));
        assert_eq!(
            w.rooted(r"\\server\share\x").as_deref(),
            Some(r"\\server\share\x")
        );
        for relative in ["src/lib.rs", r"src\lib.rs", "C:x", "C:", "/etc/hosts", ""] {
            assert_eq!(w.rooted(relative), None, "{relative:?}");
        }

        let p = PathStyle::Posix;
        assert_eq!(p.rooted("/etc/hosts").as_deref(), Some("/etc/hosts"));
        assert_eq!(p.rooted(r"C:\Users\me").as_deref(), Some("/mnt/c/Users/me"));
        assert_eq!(p.rooted("d:/").as_deref(), Some("/mnt/d/"));
        for relative in ["src/lib.rs", "C:x", r"\\server\share", "12:30"] {
            assert_eq!(p.rooted(relative), None, "{relative:?}");
        }
    }

    /// The Windows join is textual so a Mac looking at a Windows host asks it
    /// about a path it can stat, and so it can be pinned here at all.
    #[test]
    fn a_windows_join_spells_the_whole_path_the_windows_way() {
        let w = PathStyle::Windows;
        let join =
            |root: &str, rel: &str| w.join(Path::new(root), rel).to_string_lossy().into_owned();
        assert_eq!(join(r"C:\proj", "src/lib.rs"), r"C:\proj\src\lib.rs");
        assert_eq!(join(r"C:\proj\", r"src\lib.rs"), r"C:\proj\src\lib.rs");
        assert_eq!(join("c:/proj", "a.txt"), r"C:\proj\a.txt");
        assert_eq!(join(r"C:\", "a.txt"), r"C:\a.txt");
        // A leading separator is rooted on the root's own drive or share.
        assert_eq!(join(r"C:\proj", "/etc/hosts"), r"C:\etc\hosts");
        assert_eq!(join(r"D:\proj", r"\tmp\x"), r"D:\tmp\x");
        assert_eq!(join(r"\\srv\share\proj", r"\x"), r"\\srv\share\x");
        assert_eq!(join(r"\\srv\share\proj", "x"), r"\\srv\share\proj\x");
        assert_eq!(windows_prefix(r"\\srv\share"), r"\\srv\share");
        assert_eq!(windows_prefix("C:"), "C:");
        assert_eq!(windows_prefix("proj"), "");
    }

    #[test]
    fn a_path_handed_to_the_os_is_spelled_its_way() {
        assert_eq!(
            spelled_for(PathStyle::Windows, Path::new("c:/Users/me/a b.png")),
            PathBuf::from(r"C:\Users\me\a b.png")
        );
        assert_eq!(
            spelled_for(PathStyle::Windows, Path::new(r"\\srv/share/x")),
            PathBuf::from(r"\\srv\share\x")
        );
        assert_eq!(
            spelled_for(PathStyle::Posix, Path::new("/Users/me/a\\b.png")),
            PathBuf::from("/Users/me/a\\b.png"),
            "a POSIX backslash is a filename character, not a separator"
        );
    }

    fn posix_roots() -> LinkRoots {
        LinkRoots {
            dirs: vec![PathBuf::from("/home/u/proj")],
            local_home: false,
            style: PathStyle::Posix,
        }
    }

    /// The file link under the first char of `needle`, as (path, line,
    /// column, start, end) — or `None`, with what was asked about.
    fn file_link_with(
        line: &str,
        needle: &str,
        roots: &LinkRoots,
        exists: &str,
    ) -> Result<(String, Option<u32>, Option<u32>, usize, usize), Vec<String>> {
        let (link, asked) = link_with(line, needle, roots, exists);
        match link {
            Some(LinkMatch {
                start,
                end,
                target:
                    LinkTarget::File {
                        path, line, column, ..
                    },
            }) => Ok((
                path.to_string_lossy().into_owned(),
                line,
                column,
                start,
                end,
            )),
            _ => Err(asked),
        }
    }

    /// A quote is where a path is allowed its spaces: every word of it is the
    /// same link, and the quotes are not part of it.
    #[test]
    fn a_quoted_windows_path_keeps_its_spaces() {
        let want = r"C:\Program Files\foo\bar.exe";
        for line in [
            r#"run "C:\Program Files\foo\bar.exe" now"#,
            r"run 'C:\Program Files\foo\bar.exe' now",
            r"run `C:\Program Files\foo\bar.exe` now",
            r#"run "c:/Program Files/foo/bar.exe" now"#,
        ] {
            let written = &line[5..line.len() - 5];
            for needle in ["Program", "Files", "bar.exe"] {
                let got = file_link_with(line, needle, &windows_roots(), want);
                assert_eq!(
                    got,
                    Ok((
                        want.to_string(),
                        None,
                        None,
                        span_of(line, written).0,
                        span_of(line, written).1
                    )),
                    "{line:?} at {needle:?}"
                );
            }
        }
    }

    #[test]
    fn a_quoted_posix_path_keeps_its_spaces_and_its_location() {
        let roots = posix_roots();
        let docs = "/Users/me/My Docs/a.txt";
        let line = "open '/Users/me/My Docs/a.txt' please";
        let (start, end) = span_of(line, docs);
        assert_eq!(
            file_link_with(line, "Docs", &roots, docs),
            Ok((docs.to_string(), None, None, start, end))
        );

        let file = "/home/u/proj/src/my file.rs";
        for (line, loc) in [
            (
                r#"error: "src/my file.rs:10:2" failed"#,
                (Some(10), Some(2)),
            ),
            (
                r#"error: "src/my file.rs":10:2: failed"#,
                (Some(10), Some(2)),
            ),
            (
                r#"error: "src/my file.rs"(10,2): failed"#,
                (Some(10), Some(2)),
            ),
            (
                r#"File "src/my file.rs", line 7, in <module>"#,
                (Some(7), None),
            ),
            (r#"see "src/my file.rs"."#, (None, None)),
        ] {
            let got = file_link_with(line, "file", &roots, file)
                .unwrap_or_else(|asked| panic!("{line:?}: no link, asked {asked:?}"));
            assert_eq!(got.0, file, "{line:?}");
            assert_eq!((got.1, got.2), loc, "{line:?}");
            let byte = line.find("src/").unwrap();
            assert_eq!(
                got.3,
                line[..byte].chars().count(),
                "{line:?} starts inside the quote"
            );
        }
    }

    /// Outside a quote a space still ends a path — the part after it is not
    /// guessed onto the part before.
    #[test]
    fn an_unquoted_space_still_ends_a_path() {
        let want = r"C:\Program Files\foo\bar.exe";
        let line = r"run C:\Program Files\foo\bar.exe now";
        for needle in ["Program", "Files", "bar.exe"] {
            assert!(
                file_link_with(line, needle, &windows_roots(), want).is_err(),
                "{needle:?}"
            );
        }
        let docs = "/Users/me/My Docs/a.txt";
        assert!(
            file_link_with("open /Users/me/My Docs/a.txt", "Docs", &posix_roots(), docs).is_err()
        );
    }

    /// Quotes around ordinary prose are not a path, and one that looks enough
    /// like one to be asked about is asked about once.
    #[test]
    fn a_quote_around_prose_is_not_a_path() {
        let spaced = |line: &str, needle: &str| {
            let byte = line.find(needle).unwrap();
            file_candidates_at(line, line[..byte].chars().count())
                .into_iter()
                .filter(|c| c.path.contains(' '))
                .collect::<Vec<_>>()
        };
        for (line, needle) in [
            (r#"say "hello world" twice"#, "world"),
            (r#"say " /a b/c " twice"#, "b/c"),
            (r#"mismatched "/a b/c' here"#, "b/c"),
            (r#"a "x" and "y/z w""#, "and"),
            ("it's a /tmp/x dir", "tmp"),
            ("tab\"/a\tb/c\"", "b/c"),
        ] {
            assert_eq!(spaced(line, needle), Vec::new(), "{line:?}");
        }
        let long = format!("\"/{} x/y\"", "a".repeat(MAX_QUOTED_PATH));
        assert_eq!(spaced(&long, "x/y"), Vec::new(), "past the cap");
        let at_cap = format!("\"/{} x/y\"", "a".repeat(MAX_QUOTED_PATH - 6));
        assert_eq!(spaced(&at_cap, "x/y").len(), 1, "at the cap");

        // A quoted sentence that clears the shape check and names nothing
        // is one question, not one per word.
        let line = r#"note "see the docs/ folder and the src/ tree" ok"#;
        for needle in ["see", "folder", "tree"] {
            let (link, asked) = link_with(line, needle, &posix_roots(), "");
            assert_eq!(link, None);
            let spaced: Vec<_> = asked.iter().filter(|p| p.contains(' ')).collect();
            assert_eq!(spaced.len(), 1, "{needle:?} asked {asked:?}");
        }
    }
}

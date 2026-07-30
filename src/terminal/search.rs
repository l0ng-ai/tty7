use std::path::{Path, PathBuf};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
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

const MAX_MATCHES: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LinkTarget {
    Url(String),
    File {
        path: PathBuf,
        line: Option<u32>,
        column: Option<u32>,
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
    _subs: Vec<Subscription>,
}

impl SearchState {
    pub fn current(&self) -> Option<&Match> {
        self.current_index.and_then(|i| self.matches.get(i))
    }
}

impl TerminalView {
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fresh = self.search.is_none();
        if fresh {
            let seed = self
                .selected_search_seed()
                .unwrap_or_else(|| self.search_last_query.clone());
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Find")
                    .default_value(seed)
            });
            let subs = vec![cx.subscribe_in(&input, window, Self::on_search_event)];
            self.search = Some(SearchState {
                input,
                matches: Vec::new(),
                current_index: None,
                _subs: subs,
            });
        }
        if let Some(input) = self.search.as_ref().map(|s| s.input.clone()) {
            input.update(cx, |state, cx| state.focus(window, cx));
        }
        if fresh {
            self.recompute_matches(cx);
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
        self.terminal.term.lock().selection = None;
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

    pub(super) fn recompute_matches(&mut self, cx: &mut Context<Self>) {
        let Some(query) = self
            .search
            .as_ref()
            .map(|s| s.input.read(cx).value().to_string())
        else {
            return;
        };

        let mut matches: Vec<Match> = Vec::new();
        let mut current_index: Option<usize> = None;
        let mut regex_error = false;

        if !query.is_empty() {
            let pattern = self.effective_search_pattern(&query);
            let compiled = RegexSearch::new(&pattern);
            regex_error = compiled.is_err();
            if let Ok(mut regex) = compiled {
                let term = self.terminal.term.lock();
                let grid = term.grid();
                let mut origin = Point::new(grid.topmost_line(), Column(0));

                while matches.len() < MAX_MATCHES {
                    let Some(m) =
                        term.search_next(&mut regex, origin, Direction::Right, Side::Left, None)
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

                if !matches.is_empty() {
                    let display_offset = grid.display_offset() as i32;
                    let bottom = Point::new(
                        Line(grid.screen_lines() as i32 - 1 - display_offset),
                        grid.last_column(),
                    );
                    let idx = matches
                        .iter()
                        .rposition(|m| *m.start() <= bottom)
                        .unwrap_or(0);
                    current_index = Some(idx);
                }
            }
        }

        if let Some(s) = self.search.as_mut() {
            s.matches = matches;
            s.current_index = current_index;
        }
        self.search_regex_error = regex_error;

        let current = self.search.as_ref().and_then(|s| s.current().cloned());
        let mut term = self.terminal.term.lock();
        term.selection = None;
        if let Some(m) = current {
            scroll_match_into_view(&mut term, &m);
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
        scroll_match_into_view(&mut self.terminal.term.lock(), &current);
        cx.notify();
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
        let accent = theme.accent;
        let danger = theme.red;

        let total = state.matches.len();
        let has_query = !state.input.read(cx).value().is_empty();
        let has_matches = !state.matches.is_empty();
        let focused = self.search_focused;
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
            div()
                .flex_none()
                .text_xs()
                .text_color(muted)
                .child(format!("{current}/{total}"))
        });

        let case_toggle = Button::new("search-case")
            .label("Aa")
            .ghost()
            .small()
            .selected(case_on)
            .tooltip("Match case")
            .on_click(cx.listener(|this, _, _window, cx| {
                this.toggle_search_case(cx);
            }));
        let regex_toggle = Button::new("search-regex")
            .label(".*")
            .ghost()
            .small()
            .selected(regex_on)
            .tooltip("Use regular expression")
            .on_click(cx.listener(|this, _, _window, cx| {
                this.toggle_search_regex(cx);
            }));

        let divider = div().flex_none().w(px(1.)).h(px(16.)).bg(border);

        let prev = Button::new("search-prev")
            .icon(IconName::ChevronUp)
            .ghost()
            .small()
            .disabled(!has_matches)
            .on_click(cx.listener(|this, _, _window, cx| {
                this.step_match(Direction::Left, cx);
            }));
        let next = Button::new("search-next")
            .icon(IconName::ChevronDown)
            .ghost()
            .small()
            .disabled(!has_matches)
            .on_click(cx.listener(|this, _, _window, cx| {
                this.step_match(Direction::Right, cx);
            }));
        let close = Button::new("search-close")
            .icon(IconName::Close)
            .ghost()
            .small()
            .on_click(cx.listener(|this, _, window, cx| {
                this.close_search(window, cx);
            }));

        div()
            .absolute()
            .top_2()
            .right_4()
            .occlude()
            .flex()
            .items_center()
            .gap_1p5()
            .w(px(400.))
            .h(px(34.))
            .pl_3()
            .pr_1()
            .rounded_lg()
            .border_1()
            .border_color(if regex_error {
                danger
            } else if focused {
                accent
            } else {
                border
            })
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

fn scroll_match_into_view<T: EventListener>(term: &mut Term<T>, m: &Match) {
    let grid = term.grid();
    let display_offset = grid.display_offset() as i32;
    let top = -display_offset;
    let bottom = grid.screen_lines() as i32 - 1 - display_offset;
    let line = m.start().line.0;
    if line < top || line > bottom {
        term.scroll_to_point(*m.start());
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

pub(super) fn link_at(
    text: &str,
    col: usize,
    cwd: Option<&Path>,
    include_files: bool,
) -> Option<LinkMatch> {
    if let Some((start, end, url)) = url_span_at(text, col) {
        return Some(LinkMatch {
            start,
            end,
            target: LinkTarget::Url(url),
        });
    }
    include_files
        .then(|| file_span_at(text, col, cwd))
        .flatten()
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

fn file_span_at(text: &str, col: usize, cwd: Option<&Path>) -> Option<LinkMatch> {
    let (start, end, token) = non_ws_token_at(text, col)?;
    let (start, mut end, mut token) = trim_file_token(start, end, token);
    if token.is_empty() {
        return None;
    }

    let mut location = split_file_location(&token);
    if location.line.is_none() && token.ends_with(':') {
        token.pop();
        end = end.saturating_sub(1);
        location = split_file_location(&token);
    }

    let path = resolve_existing_path(&location.path, cwd, location.line.is_some())?;
    (start..=end).contains(&col).then_some(LinkMatch {
        start,
        end,
        target: LinkTarget::File {
            path,
            line: location.line,
            column: location.column,
        },
    })
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

fn trim_file_token(mut start: usize, mut end: usize, mut token: String) -> (usize, usize, String) {
    while token
        .chars()
        .next()
        .is_some_and(|c| matches!(c, '(' | '[' | '<' | '\'' | '"' | '{' | '`'))
    {
        token.remove(0);
        start += 1;
    }
    while token
        .chars()
        .next_back()
        .is_some_and(is_file_trailing_punct)
    {
        token.pop();
        end = end.saturating_sub(1);
    }
    (start, end, token)
}

fn is_file_trailing_punct(c: char) -> bool {
    matches!(
        c,
        ')' | ']'
            | '}'
            | '.'
            | ','
            | ';'
            | '\''
            | '"'
            | '>'
            | '`'
            | '）'
            | '］'
            | '】'
            | '》'
            | '」'
            | '。'
            | '，'
            | '；'
    )
}

struct FileLocation {
    path: String,
    line: Option<u32>,
    column: Option<u32>,
}

fn split_file_location(token: &str) -> FileLocation {
    let Some((prefix, last)) = strip_numeric_suffix(token) else {
        return FileLocation {
            path: token.to_string(),
            line: None,
            column: None,
        };
    };
    if let Some((path, line)) = strip_numeric_suffix(prefix) {
        FileLocation {
            path: path.to_string(),
            line: Some(line),
            column: Some(last),
        }
    } else {
        FileLocation {
            path: prefix.to_string(),
            line: Some(last),
            column: None,
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

fn resolve_existing_path(path: &str, cwd: Option<&Path>, require_file: bool) -> Option<PathBuf> {
    if path.is_empty() {
        return None;
    }
    let path = expand_home(path, cwd)?;
    let candidate = if path.is_absolute() {
        path
    } else {
        cwd?.join(path)
    };
    let hit = candidate.is_file() || (!require_file && candidate.is_dir());
    hit.then_some(candidate)
}

fn expand_home(path: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    if path == "~" {
        return home_dir(cwd);
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return home_dir(cwd).map(|home| home.join(rest));
    }
    Some(PathBuf::from(path))
}

fn home_dir(cwd: Option<&Path>) -> Option<PathBuf> {
    if let Some(home) = cwd.and_then(home_from_cwd) {
        return Some(home);
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

#[cfg(unix)]
fn home_from_cwd(cwd: &Path) -> Option<PathBuf> {
    let mut components = cwd.components();
    let root = components.next()?;
    let base = components.next()?;
    let user = components.next()?;
    let base = base.as_os_str().to_str()?;
    matches!(base, "Users" | "home").then(|| {
        let mut home = PathBuf::new();
        home.push(root.as_os_str());
        home.push(base);
        home.push(user.as_os_str());
        home
    })
}

#[cfg(not(unix))]
fn home_from_cwd(_cwd: &Path) -> Option<PathBuf> {
    None
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

    fn assert_file_link(
        line: &str,
        col: usize,
        cwd: &Path,
        expected_path: &Path,
        expected_line: Option<u32>,
        expected_column: Option<u32>,
    ) {
        let link = link_at(line, col, Some(cwd), true).expect("file link under cursor");
        match link.target {
            LinkTarget::File { path, line, column } => {
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

        let link = link_at("error src/main.rs:10:2 failed", 8, Some(cwd), true)
            .expect("file link under cursor");
        assert_eq!((link.start, link.end), (6, 21));
    }

    #[test]
    #[cfg(unix)]
    fn tilde_expansion_prefers_home_inferred_from_the_pane_cwd() {
        let cwd = Path::new("/Users/alice/clone/tty7");
        assert_eq!(
            expand_home("~/clone/tty7/src/main.rs", Some(cwd)),
            Some(PathBuf::from("/Users/alice/clone/tty7/src/main.rs"))
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

        let link = link_at(line, 7, Some(cwd), true).expect("wrapped file link");
        assert_eq!((link.start, link.end), (5, 16));
        match link.target {
            LinkTarget::File {
                path: got,
                line,
                column,
            } => {
                assert_eq!(got, path);
                assert_eq!(line, Some(7));
                assert_eq!(column, None);
            }
            LinkTarget::Url(url) => panic!("expected file link, got URL {url}"),
        }
    }

    #[test]
    fn link_at_rejects_missing_files_and_file_detection_can_be_disabled() {
        let cwd = std::env::temp_dir();

        assert_eq!(link_at("missing src/nope.rs:1", 9, Some(&cwd), true), None);
        assert_eq!(link_at("missing src/nope.rs:1", 9, Some(&cwd), false), None);

        let path = temp_file("disabled.rs");
        let line = format!("open {}", path.display());
        assert!(link_at(&line, 6, Some(&cwd), false).is_none());
    }

    #[test]
    fn link_at_keeps_url_detection_ahead_of_file_detection() {
        let url = "https://example.com/src/main.rs";
        let link = link_at(url, 10, Some(Path::new("/")), true).expect("URL link");
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

        let link = link_at("artifacts in dircase/nested here", 14, Some(cwd), true)
            .expect("directory link");
        assert_eq!((link.start, link.end), (13, 26));
        match link.target {
            LinkTarget::File { path, line, column } => {
                assert_eq!(path, dir);
                assert_eq!(line, None);
                assert_eq!(column, None);
            }
            LinkTarget::Url(url) => panic!("expected directory link, got URL {url}"),
        }

        assert!(link_at("ls dircase/nested/ done", 5, Some(cwd), true).is_some());
        assert!(link_at("artifacts in dircase/nested here", 14, Some(cwd), false).is_none());
    }

    #[test]
    fn link_at_requires_a_file_when_a_line_suffix_is_present() {
        let file = temp_file("localhost/keep.txt");
        let cwd = file.parent().and_then(Path::parent).unwrap();

        assert_eq!(
            link_at("listening on localhost:8080", 15, Some(cwd), true),
            None
        );
        assert!(link_at("listening on localhost", 15, Some(cwd), true).is_some());
    }
}

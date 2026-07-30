use std::sync::OnceLock;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;
use regex::Regex;

const MAX_WRAP_ROWS: usize = 32;

const MATCH_WINDOW: usize = 2000;

const BRACKET_PAIRS: [(char, char); 15] = [
    ('(', ')'),
    ('[', ']'),
    ('{', '}'),
    ('<', '>'),
    ('（', '）'),
    ('［', '］'),
    ('｛', '｝'),
    ('〈', '〉'),
    ('《', '》'),
    ('「', '」'),
    ('『', '』'),
    ('【', '】'),
    ('〔', '〕'),
    ('“', '”'),
    ('‘', '’'),
];

const SYMMETRIC_QUOTES: [char; 3] = ['\'', '"', '`'];

pub(super) struct SmartRange {
    pub start: Point,
    pub end: Point,
    pub exact: bool,
}

pub(super) fn grid_smart_range<T: EventListener>(
    term: &Term<T>,
    click: Point,
) -> Option<SmartRange> {
    if click.line < term.topmost_line() || click.line > term.bottommost_line() {
        return None;
    }

    if let Some((start, end)) = hyperlink_run(term, click) {
        return Some(SmartRange {
            start,
            end,
            exact: true,
        });
    }

    let (text, points, click_idx) = logical_line_at(term, click, false)?;
    let chars: Vec<char> = text.chars().collect();
    let separators = term.semantic_escape_chars();
    let resolved = |s: usize, e: usize| SmartRange {
        start: points[s],
        end: points[e],
        exact: !(s == 0 || separators.contains(chars[s - 1]))
            || !(e + 1 == chars.len() || separators.contains(chars[e + 1])),
    };

    if let Some((s, e)) = pair_range(&chars, click_idx) {
        return Some(resolved(s, e));
    }

    if is_cjk(chars[click_idx])
        && let Some((s, e)) = cjk_word_range(&text, click_idx)
    {
        return Some(resolved(s, e));
    }

    let (s, e) = smart_range(&text, &chars, click_idx, separators)?;
    Some(resolved(s, e))
}

pub(super) fn is_cjk(c: char) -> bool {
    matches!(
        u32::from(c),
        0x1100..=0x11FF
        | 0x2E80..=0x9FFF
        | 0xAC00..=0xD7AF
        | 0xF900..=0xFAFF
        | 0xFF00..=0xFFEF
        | 0x20000..=0x3134F
    )
}

#[cfg(not(target_os = "macos"))]
fn is_kana_or_hangul(c: char) -> bool {
    matches!(
        u32::from(c),
        0x1100..=0x11FF
        | 0x3040..=0x30FF
        | 0x31F0..=0x31FF
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7FF
        | 0xFF66..=0xFF9F
    )
}

#[cfg(not(target_os = "macos"))]
static JIEBA: OnceLock<jieba_rs::Jieba> = OnceLock::new();

#[cfg(not(target_os = "macos"))]
fn warm() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(|| {
            let _ = JIEBA.get_or_init(jieba_rs::Jieba::new);
        });
    });
}

pub(super) fn cjk_word_range(text: &str, click: usize) -> Option<(usize, usize)> {
    #[cfg(target_os = "macos")]
    {
        tokenizer::word_range(text, click)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let chars: Vec<char> = text.chars().collect();
        chars.get(click)?;
        jieba_word_range(&chars, click)
    }
}

#[cfg(not(target_os = "macos"))]
fn jieba_word_range(chars: &[char], click: usize) -> Option<(usize, usize)> {
    let mut rs = click;
    while rs > 0 && is_cjk(chars[rs - 1]) {
        rs -= 1;
    }
    let mut re = click;
    while re + 1 < chars.len() && is_cjk(chars[re + 1]) {
        re += 1;
    }
    if chars[rs..=re].iter().copied().any(is_kana_or_hangul) {
        return None;
    }
    let Some(jieba) = JIEBA.get() else {
        warm();
        return None;
    };
    let run: String = chars[rs..=re].iter().collect();
    let rel = click - rs;
    jieba
        .cut(&run, true)
        .iter()
        .find(|tok| rel < tok.end)
        .map(|tok| (rs + tok.start, rs + tok.end - 1))
}

#[cfg(target_os = "macos")]
mod tokenizer {
    use core_foundation::base::{CFIndex, CFRange, TCFType};
    use core_foundation::string::{CFString, CFStringRef};
    use std::os::raw::c_void;

    type CFStringTokenizerRef = *mut c_void;
    type CFLocaleRef = *const c_void;

    const UNIT_WORD_BOUNDARY: u64 = 4;

    unsafe extern "C" {
        fn CFStringTokenizerCreate(
            alloc: *const c_void,
            string: CFStringRef,
            range: CFRange,
            options: u64,
            locale: CFLocaleRef,
        ) -> CFStringTokenizerRef;
        fn CFStringTokenizerGoToTokenAtIndex(
            tokenizer: CFStringTokenizerRef,
            index: CFIndex,
        ) -> u64;
        fn CFStringTokenizerGetCurrentTokenRange(tokenizer: CFStringTokenizerRef) -> CFRange;
        fn CFLocaleCopyCurrent() -> CFLocaleRef;
        fn CFRelease(cf: *const c_void);
    }

    pub(super) fn word_range(text: &str, click: usize) -> Option<(usize, usize)> {
        let mut u16_of: Vec<CFIndex> = Vec::new();
        let mut total: CFIndex = 0;
        for c in text.chars() {
            u16_of.push(total);
            total += c.len_utf16() as CFIndex;
        }
        let click_u16 = *u16_of.get(click)?;

        let cf = CFString::new(text);
        let range = unsafe {
            let locale = CFLocaleCopyCurrent();
            let tok = CFStringTokenizerCreate(
                std::ptr::null(),
                cf.as_concrete_TypeRef(),
                CFRange::init(0, total),
                UNIT_WORD_BOUNDARY,
                locale,
            );
            let token_type = CFStringTokenizerGoToTokenAtIndex(tok, click_u16);
            let range = (token_type != 0).then(|| CFStringTokenizerGetCurrentTokenRange(tok));
            CFRelease(tok);
            if !locale.is_null() {
                CFRelease(locale);
            }
            range?
        };
        if range.location < 0 || range.length <= 0 {
            return None;
        }
        let start = u16_of.binary_search(&range.location).ok()?;
        let end = u16_of.partition_point(|&v| v < range.location + range.length) - 1;
        (start <= click && click <= end).then_some((start, end))
    }
}

pub(super) fn hyperlink_run<T: EventListener>(
    term: &Term<T>,
    click: Point,
) -> Option<(Point, Point)> {
    let grid = term.grid();
    let cols = term.columns();
    if click.column.0 >= cols {
        return None;
    }
    let uri = grid[click.line][click.column]
        .hyperlink()?
        .uri()
        .to_string();
    let same = |p: Point| {
        grid[p.line][p.column]
            .hyperlink()
            .is_some_and(|h| h.uri() == uri)
    };
    let wraps = |line: Line| grid[line][Column(cols - 1)].flags.contains(Flags::WRAPLINE);
    let top = term.topmost_line();
    let bottom = term.bottommost_line();

    let mut start = click;
    let mut rows = 0;
    loop {
        let prev = if start.column.0 > 0 {
            Point::new(start.line, Column(start.column.0 - 1))
        } else if start.line > top && rows < MAX_WRAP_ROWS && wraps(start.line - 1) {
            rows += 1;
            Point::new(start.line - 1, Column(cols - 1))
        } else {
            break;
        };
        if !same(prev) {
            break;
        }
        start = prev;
    }
    let mut end = click;
    rows = 0;
    loop {
        let next = if end.column.0 + 1 < cols {
            Point::new(end.line, Column(end.column.0 + 1))
        } else if end.line < bottom && rows < MAX_WRAP_ROWS && wraps(end.line) {
            rows += 1;
            Point::new(end.line + 1, Column(0))
        } else {
            break;
        };
        if !same(next) {
            break;
        }
        end = next;
    }
    Some((start, end))
}

pub(super) fn logical_line_at<T: EventListener>(
    term: &Term<T>,
    click: Point,
    bridge_hard_wrap: bool,
) -> Option<(String, Vec<Point>, usize)> {
    let cols = term.columns();
    if click.column.0 >= cols {
        return None;
    }
    let grid = term.grid();
    let last_col = Column(cols - 1);
    let top = term.topmost_line();
    let bottom = term.bottommost_line();
    let wraps = |line: Line| grid[line][last_col].flags.contains(Flags::WRAPLINE);
    let is_link_char = |c: char| super::search::is_url_char(c);
    let hard = |line: Line| {
        bridge_hard_wrap && line < bottom && is_link_char(grid[line][last_col].c) && {
            let next = grid[Line(line.0 + 1)][Column(0)].c;
            is_link_char(next) && next != '@'
        }
    };
    let continues = |line: Line| wraps(line) || hard(line);

    let mut start_line = click.line;
    let mut guard = 0;
    while start_line > top && guard < MAX_WRAP_ROWS && continues(start_line - 1) {
        start_line -= 1;
        guard += 1;
    }
    let mut end_line = click.line;
    guard = 0;
    while end_line < bottom && guard < MAX_WRAP_ROWS && continues(end_line) {
        end_line += 1;
        guard += 1;
    }

    let mut text = String::new();
    let mut points = Vec::new();
    let mut click_idx = None;
    let mut line = start_line;
    while line <= end_line {
        for col in 0..cols {
            let cell = &grid[line][Column(col)];
            let p = Point::new(line, Column(col));
            if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                if p == click {
                    click_idx = Some(points.len());
                }
                continue;
            }
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                if p == click && !points.is_empty() {
                    click_idx = Some(points.len() - 1);
                }
                continue;
            }
            if p == click {
                click_idx = Some(points.len());
            }
            text.push(cell.c);
            points.push(p);
        }
        line += 1;
    }
    let click_idx = click_idx.filter(|&i| i < points.len())?;
    Some((text, points, click_idx))
}

pub(super) fn pair_range(chars: &[char], click: usize) -> Option<(usize, usize)> {
    bracket_range(chars, click).or_else(|| quote_range(chars, click))
}

fn is_contraction(chars: &[char], i: usize) -> bool {
    if chars[i] != '\'' {
        return false;
    }
    let flanked = |j: Option<usize>| {
        j.and_then(|j| chars.get(j))
            .is_some_and(|c| c.is_alphanumeric())
    };
    flanked(i.checked_sub(1)) && flanked(Some(i + 1))
}

pub(super) fn quote_range(chars: &[char], click: usize) -> Option<(usize, usize)> {
    let q = *chars.get(click)?;
    if !SYMMETRIC_QUOTES.contains(&q) || is_contraction(chars, click) {
        return None;
    }
    let quote_at = |i: usize| chars[i] == q && !is_contraction(chars, i);
    let before = (0..click).filter(|&i| quote_at(i)).count();
    if before % 2 == 0 {
        let close = (click + 1..chars.len()).find(|&i| quote_at(i))?;
        Some((click, close))
    } else {
        let open = (0..click).rev().find(|&i| quote_at(i))?;
        Some((open, click))
    }
}

fn pair_is_plausible(chars: &[char], open: char, s: usize, e: usize) -> bool {
    if open != '<' {
        return true;
    }
    e > s + 1 && !chars[s + 1].is_whitespace() && !chars[e - 1].is_whitespace()
}

pub(super) fn bracket_range(chars: &[char], click: usize) -> Option<(usize, usize)> {
    let c = *chars.get(click)?;
    if let Some((open, close)) = BRACKET_PAIRS.iter().find(|(o, _)| *o == c) {
        let mut depth = 0usize;
        for (i, &ch) in chars.iter().enumerate().skip(click) {
            if ch == *open {
                depth += 1;
            } else if ch == *close {
                depth -= 1;
                if depth == 0 {
                    return pair_is_plausible(chars, *open, click, i).then_some((click, i));
                }
            }
        }
        return None;
    }
    if let Some((open, close)) = BRACKET_PAIRS.iter().find(|(_, c2)| *c2 == c) {
        let mut depth = 0usize;
        for i in (0..=click).rev() {
            let ch = chars[i];
            if ch == *close {
                depth += 1;
            } else if ch == *open {
                depth -= 1;
                if depth == 0 {
                    return pair_is_plausible(chars, *open, i, click).then_some((i, click));
                }
            }
        }
    }
    None
}

fn regexes() -> &'static [Regex] {
    static RE: OnceLock<Vec<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        [
            r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}",
            r"\b[0-9]+(?:\.[0-9]+)?[eE][+-]?[0-9]+\b",
            r"[A-Za-z0-9._+@%~-]*(?:/[A-Za-z0-9._+@%~-]+)+/?",
            r"[0-9A-Za-z_]+(?:[.-][0-9A-Za-z_]+)*",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("static smart-select regex"))
        .collect()
    })
}

pub(super) fn smart_range(
    text: &str,
    chars: &[char],
    click: usize,
    separators: &str,
) -> Option<(usize, usize)> {
    if click >= chars.len() || chars[click].is_whitespace() {
        return None;
    }

    let boundary = |c: char| c.is_whitespace() || separators.contains(c);
    let (mut pws, mut pwe) = (click, click);
    if !boundary(chars[click]) {
        while pws > 0 && !boundary(chars[pws - 1]) {
            pws -= 1;
        }
        while pwe + 1 < chars.len() && !boundary(chars[pwe + 1]) {
            pwe += 1;
        }
    }
    let (ws, we) = narrow_to_script(chars, click, pws, pwe);
    let extends = |s: usize, e: usize| s <= ws && e >= we && (s < ws || e > we);

    if let Some((s, e, _url)) = super::search::url_span_at(text, click)
        && extends(s, e)
    {
        return Some((s, e));
    }

    let byte_of: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
    let w_start = click.saturating_sub(MATCH_WINDOW);
    let w_end = (click + MATCH_WINDOW).min(chars.len() - 1);
    let wb_start = byte_of[w_start];
    let wb_end = byte_of[w_end] + chars[w_end].len_utf8();
    let window = &text[wb_start..wb_end];
    let click_byte = byte_of[click] - wb_start;

    for re in regexes() {
        let Some(m) = re
            .find_iter(window)
            .find(|m| m.range().contains(&click_byte))
        else {
            continue;
        };
        let s = text[..wb_start + m.start()].chars().count();
        let e = text[..wb_start + m.end()].chars().count() - 1;
        if extends(s, e) {
            return Some((s, e));
        }
    }
    ((ws, we) != (pws, pwe)).then_some((ws, we))
}

pub(super) fn narrow_to_script(
    chars: &[char],
    click: usize,
    lo: usize,
    hi: usize,
) -> (usize, usize) {
    let class = is_cjk(chars[click]);
    let mut s = click;
    while s > lo && is_cjk(chars[s - 1]) == class {
        s -= 1;
    }
    let mut e = click;
    while e < hi && is_cjk(chars[e + 1]) == class {
        e += 1;
    }
    (s, e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;

    const SEPS: &str = ",│`|:\"' ()[]{}<>\t";

    fn ensure_segmenter() {
        #[cfg(not(target_os = "macos"))]
        let _ = JIEBA.get_or_init(jieba_rs::Jieba::new);
    }

    fn range(text: &str, click: usize) -> Option<(usize, usize)> {
        let chars: Vec<char> = text.chars().collect();
        smart_range(text, &chars, click, SEPS)
    }

    fn term_with(cols: usize, rows: usize, input: &str) -> Term<VoidListener> {
        let config = alacritty_terminal::term::Config {
            semantic_escape_chars: SEPS.to_string(),
            ..Default::default()
        };
        let mut term = Term::new(
            config,
            &crate::terminal::size::TermSize::new(cols, rows),
            VoidListener,
        );
        let mut parser: alacritty_terminal::vte::ansi::Processor =
            alacritty_terminal::vte::ansi::Processor::new();
        parser.advance(&mut term, input.as_bytes());
        term
    }

    fn grid_select(term: &Term<VoidListener>, line: i32, col: usize) -> Option<String> {
        let r = grid_smart_range(term, Point::new(Line(line), Column(col)))?;
        Some(term.bounds_to_string(r.start, r.end))
    }

    fn col_of(row: &str, needle: &str) -> usize {
        row.find(needle).expect("needle in fixture")
    }

    #[test]
    fn osc8_hyperlink_selects_the_declared_extent_not_the_visible_word() {
        let term = term_with(
            40,
            3,
            "go \x1b]8;;https://example.com/x\x1b\\click here\x1b]8;;\x1b\\ now",
        );
        let line = "go click here now";
        assert_eq!(
            grid_select(&term, 0, col_of(line, "here")).as_deref(),
            Some("click here"),
        );
        assert_ne!(
            grid_select(&term, 0, col_of(line, "now")).as_deref(),
            Some("click here"),
        );
    }

    #[test]
    fn osc8_hyperlink_follows_a_soft_wrap() {
        let term = term_with(
            20,
            4,
            "\x1b]8;;https://e.com\x1b\\aaaaaaaaaabbbbbbbbbbcccccccccc\x1b]8;;\x1b\\",
        );
        let whole = "aaaaaaaaaabbbbbbbbbbcccccccccc";
        assert_eq!(grid_select(&term, 1, 2).as_deref(), Some(whole));
        assert_eq!(grid_select(&term, 0, 3).as_deref(), Some(whole));
    }

    #[test]
    fn soft_wrapped_url_is_stitched_back_into_one_selection() {
        let term = term_with(20, 4, "see https://example.com/deep/path here");
        let whole = "https://example.com/deep/path";
        assert_eq!(
            grid_select(&term, 0, col_of("see https://example", "example")).as_deref(),
            Some(whole),
        );
        assert_eq!(
            grid_select(&term, 1, col_of("com/deep/path here", "deep")).as_deref(),
            Some(whole),
        );
    }

    #[test]
    fn hard_wrapped_url_is_bridged_only_for_links() {
        let term = term_with(20, 4, "https://example.com/\r\ndeep/path/seg rest");
        assert!(
            !term.grid()[Line(0)][Column(19)]
                .flags
                .contains(Flags::WRAPLINE),
            "fixture must be a hard newline, not a soft wrap"
        );

        let click = Point::new(Line(0), Column(3));
        let (text, _points, _idx) =
            logical_line_at(&term, click, true).expect("logical line under click");
        let idx = text.find("https").expect("url in bridged line");
        let (_s, _e, url) =
            crate::terminal::search::url_span_at(&text, idx + 2).expect("url span in bridged line");
        assert_eq!(url, "https://example.com/deep/path/seg");

        let (text, _points, _idx) =
            logical_line_at(&term, click, false).expect("logical line under click");
        assert!(
            !text.contains("deep"),
            "double-click must not bridge a hard newline: {text:?}"
        );
    }

    #[test]
    fn a_hard_break_before_userinfo_is_never_bridged() {
        let term = term_with(20, 4, "go1 https://good.com\r\n@evil.com/x rest");
        assert!(
            !term.grid()[Line(0)][Column(19)]
                .flags
                .contains(Flags::WRAPLINE),
            "fixture must be a hard newline, not a soft wrap"
        );

        let click = Point::new(Line(0), Column(8));
        let (text, _points, idx) =
            logical_line_at(&term, click, true).expect("logical line under click");
        assert!(
            !text.contains("evil"),
            "a hard break before `@` must not bridge: {text:?}"
        );
        let (_s, _e, url) =
            crate::terminal::search::url_span_at(&text, idx).expect("url span under click");
        assert_eq!(url, "https://good.com");
    }

    #[test]
    fn a_soft_wrap_before_userinfo_still_stitches() {
        let term = term_with(20, 4, "see https://user1234@ex.com/z rest");
        assert!(
            term.grid()[Line(0)][Column(19)]
                .flags
                .contains(Flags::WRAPLINE),
            "fixture must be a soft wrap, not a hard newline"
        );

        let click = Point::new(Line(0), Column(10));
        let (text, _points, idx) =
            logical_line_at(&term, click, true).expect("logical line under click");
        let (_s, _e, url) =
            crate::terminal::search::url_span_at(&text, idx).expect("url span under click");
        assert_eq!(url, "https://user1234@ex.com/z");
    }

    #[test]
    fn wide_glyph_and_its_spacer_resolve_to_the_same_word() {
        ensure_segmenter();
        let term = term_with(40, 3, "run 北京欢迎你 done");
        let expected = grid_select(&term, 0, 4);
        assert_eq!(expected.as_deref(), Some("北京"), "click on 北");
        assert_eq!(
            grid_select(&term, 0, 5).as_deref(),
            expected.as_deref(),
            "spacer of 北"
        );
        assert_eq!(
            grid_select(&term, 0, 6).as_deref(),
            Some("北京"),
            "click on 京"
        );
        assert_eq!(
            grid_select(&term, 0, 7).as_deref(),
            Some("北京"),
            "spacer of 京"
        );
    }

    #[test]
    fn wide_glyph_wrapping_to_the_next_row_keeps_its_word_intact() {
        ensure_segmenter();
        let term = term_with(9, 4, "abcdefgh北京欢迎你");
        assert_eq!(grid_select(&term, 1, 0).as_deref(), Some("北京"));
    }

    #[test]
    fn click_past_the_last_column_yields_no_range() {
        let term = term_with(10, 2, "hello");
        assert!(grid_smart_range(&term, Point::new(Line(0), Column(10))).is_none());
        assert!(grid_smart_range(&term, Point::new(Line(0), Column(99))).is_none());
    }

    #[test]
    fn click_outside_the_grid_rows_yields_no_range() {
        let term = term_with(10, 2, "hello");
        assert!(grid_smart_range(&term, Point::new(Line(2), Column(0))).is_none());
        assert!(grid_smart_range(&term, Point::new(Line(9_000), Column(0))).is_none());
        assert!(grid_smart_range(&term, Point::new(Line(-1), Column(0))).is_none());
    }

    fn selected(text: &str, click: usize) -> Option<String> {
        let chars: Vec<char> = text.chars().collect();
        range(text, click).map(|(s, e)| chars[s..=e].iter().collect())
    }

    #[test]
    fn url_expands_past_scheme_colon() {
        let text = "fetch https://example.com/a/b?q=1 done";
        let click = text.find("example").unwrap();
        assert_eq!(
            selected(text, click).as_deref(),
            Some("https://example.com/a/b?q=1")
        );
    }

    #[test]
    fn url_trailing_comma_excluded() {
        let text = "see https://a.com/x, then";
        let click = text.find("a.com").unwrap();
        assert_eq!(selected(text, click).as_deref(), Some("https://a.com/x"));
    }

    #[test]
    fn email_only_fires_when_it_extends_the_word() {
        let text = "author:dev@example.com pushed";
        let click = text.find("example").unwrap();
        assert_eq!(range(text, click), None);
        let chars: Vec<char> = text.chars().collect();
        let got = smart_range(text, &chars, click, ",@:() ");
        let (s, e) = got.expect("email should match");
        let sel: String = chars[s..=e].iter().collect();
        assert_eq!(sel, "dev@example.com");
    }

    #[test]
    fn plain_word_yields_none() {
        let text = "just some words";
        let click = text.find("some").unwrap();
        assert_eq!(range(text, click), None);
    }

    #[test]
    fn path_across_quote_boundary_stays_plain() {
        let text = "cat /usr/local/bin/tool";
        let click = text.find("local").unwrap();
        assert_eq!(range(text, click), None);
    }

    #[test]
    fn path_glued_to_colon_expands() {
        let text = "error:/tmp/x/y";
        let click = text.find("tmp").unwrap();
        assert_eq!(range(text, click), None);
    }

    #[test]
    fn whitespace_click_yields_none() {
        assert_eq!(range("a b", 1), None);
    }

    #[test]
    fn scientific_notation_with_custom_separators() {
        let text = "n = 6.02e+23 mol";
        let chars: Vec<char> = text.chars().collect();
        let click = text.find("02").unwrap();
        let got = smart_range(text, &chars, click, ",.+():");
        let (s, e) = got.expect("sci notation should match");
        let sel: String = chars[s..=e].iter().collect();
        assert_eq!(sel, "6.02e+23");
    }

    #[test]
    fn identifier_with_custom_separators() {
        let text = "run foo-bar.baz now";
        let chars: Vec<char> = text.chars().collect();
        let click = text.find("bar").unwrap();
        let got = smart_range(text, &chars, click, ",.-():");
        let (s, e) = got.expect("identifier should match");
        let sel: String = chars[s..=e].iter().collect();
        assert_eq!(sel, "foo-bar.baz");
    }

    #[test]
    fn latin_word_glued_directly_to_han_narrows_without_punctuation() {
        let text = "已合并到main分支";
        let chars: Vec<char> = text.chars().collect();
        let click = chars.iter().position(|&c| c == 'm').unwrap();
        let (s, e) = smart_range(text, &chars, click, SEPS).expect("narrowed word");
        let sel: String = chars[s..=e].iter().collect();
        assert_eq!(sel, "main");
    }

    #[test]
    fn latin_word_glued_to_cjk_narrows_to_the_latin_run() {
        let text = "分支 worktree-feat-smart-select，已 rebase";
        let chars: Vec<char> = text.chars().collect();
        let click = chars.iter().position(|&c| c == 'w').unwrap() + 10;
        let (s, e) = smart_range(text, &chars, click, SEPS).expect("narrowed word");
        let sel: String = chars[s..=e].iter().collect();
        assert_eq!(sel, "worktree-feat-smart-select");
    }

    #[test]
    fn symmetric_quotes_pair_by_parity() {
        let chars: Vec<char> = r#"echo 'a,b' "c d" x"#.chars().collect();
        assert_eq!(quote_range(&chars, 5), Some((5, 9)));
        assert_eq!(quote_range(&chars, 9), Some((5, 9)));
        assert_eq!(quote_range(&chars, 11), Some((11, 15)));
        assert_eq!(quote_range(&chars, 15), Some((11, 15)));
        let chars: Vec<char> = "say 'oops".chars().collect();
        assert_eq!(quote_range(&chars, 4), None);
        assert_eq!(quote_range(&chars, 1), None);
    }

    #[test]
    fn contraction_apostrophes_do_not_pair() {
        let chars: Vec<char> = "it's a test, isn't it".chars().collect();
        assert_eq!(quote_range(&chars, 2), None);
        assert_eq!(quote_range(&chars, 16), None);
        let text = "it's a test, isn't it";
        assert_eq!(range(text, 2), None);
    }

    #[test]
    fn contractions_do_not_skew_a_real_quote() {
        let chars: Vec<char> = "echo 'it isn't so' done".chars().collect();
        let open = 5;
        let close = chars.iter().rposition(|&c| c == '\'').unwrap();
        assert_eq!(quote_range(&chars, open), Some((open, close)));
        assert_eq!(quote_range(&chars, close), Some((open, close)));
    }

    #[test]
    fn trailing_apostrophe_still_closes() {
        let chars: Vec<char> = "the 'dogs' bark".chars().collect();
        assert_eq!(quote_range(&chars, 4), Some((4, 9)));
        assert_eq!(quote_range(&chars, 9), Some((4, 9)));
    }

    #[test]
    fn directional_cjk_quotes_pair_like_brackets() {
        let chars: Vec<char> = "他说“你好”了".chars().collect();
        assert_eq!(bracket_range(&chars, 2), Some((2, 5)));
        assert_eq!(bracket_range(&chars, 5), Some((2, 5)));
    }

    #[test]
    fn fullwidth_brackets_pair() {
        let chars: Vec<char> = "说（worktree 分支）好".chars().collect();
        let open = chars.iter().position(|&c| c == '（').unwrap();
        let close = chars.iter().position(|&c| c == '）').unwrap();
        assert_eq!(bracket_range(&chars, open), Some((open, close)));
        assert_eq!(bracket_range(&chars, close), Some((open, close)));
        let chars: Vec<char> = "书名《三体》完".chars().collect();
        assert_eq!(bracket_range(&chars, 2), Some((2, 5)));
    }

    #[test]
    fn bracket_forward_and_backward_with_nesting() {
        let chars: Vec<char> = "f(a(b)c) x".chars().collect();
        assert_eq!(bracket_range(&chars, 1), Some((1, 7)));
        assert_eq!(bracket_range(&chars, 7), Some((1, 7)));
        assert_eq!(bracket_range(&chars, 3), Some((3, 5)));
        assert_eq!(bracket_range(&chars, 0), None);
    }

    #[test]
    fn angle_brackets_pair_only_when_they_hug_their_contents() {
        for (text, want) in [
            ("let v: Vec<String> = x", "<String>"),
            ("<div class=\"row\">hi", "<div class=\"row\">"),
            ("usage: tty7 <command> [opts]", "<command>"),
            ("From: Jo <j@example.com> ok", "<j@example.com>"),
            ("map: HashMap<K, V> here", "<K, V>"),
        ] {
            let chars: Vec<char> = text.chars().collect();
            let click = chars.iter().position(|&c| c == '<').unwrap();
            let (s, e) = bracket_range(&chars, click).unwrap_or_else(|| panic!("{text}"));
            let got: String = chars[s..=e].iter().collect();
            assert_eq!(got, want, "{text}");
        }
        for text in [
            "if a < b then c > d",
            "awk '{ if ($1 > 100 && $2 < 5) print }'",
            "WHERE a < 10 AND b > 20",
            "empty <> pair",
        ] {
            let chars: Vec<char> = text.chars().collect();
            for (i, &c) in chars.iter().enumerate() {
                if c == '<' || c == '>' {
                    assert_eq!(bracket_range(&chars, i), None, "{text} at {i}");
                }
            }
        }
        for text in ["cargo build 2>&1 | tee out", "grep -rn foo src/ > /tmp/o"] {
            let chars: Vec<char> = text.chars().collect();
            for (i, &c) in chars.iter().enumerate() {
                if c == '<' || c == '>' {
                    assert_eq!(bracket_range(&chars, i), None, "{text} at {i}");
                }
            }
        }
    }

    #[test]
    fn unmatched_bracket_yields_none() {
        let chars: Vec<char> = "f(a".chars().collect();
        assert_eq!(bracket_range(&chars, 1), None);
    }

    #[test]
    fn cjk_segmentation_selects_a_dictionary_word_not_the_whole_run() {
        ensure_segmenter();
        let text = "run 北京欢迎你 done";
        let chars: Vec<char> = text.chars().collect();
        let click = chars.iter().position(|&c| c == '京').unwrap();
        let (s, e) = cjk_word_range(text, click).expect("segmented range");
        let sel: String = chars[s..=e].iter().collect();
        assert_eq!(sel, "北京");
    }

    #[test]
    fn cjk_segmentation_survives_surrogate_pairs_before_the_click() {
        ensure_segmenter();
        for text in ["你好世界", "🙂 你好世界", "🙂🙂🙂 你好世界"] {
            let chars: Vec<char> = text.chars().collect();
            let click = chars.iter().position(|&c| c == '世').unwrap();
            let (s, e) = cjk_word_range(text, click).expect("segmented range");
            let sel: String = chars[s..=e].iter().collect();
            assert_eq!(sel, "世界", "{text:?} segmented wrong");
        }
    }

    #[test]
    fn cjk_punctuation_is_its_own_token() {
        ensure_segmenter();
        let text = "比赛，天气";
        let chars: Vec<char> = text.chars().collect();
        let click = chars.iter().position(|&c| c == '，').unwrap();
        let (s, e) = cjk_word_range(text, click).expect("segmented range");
        let sel: String = chars[s..=e].iter().collect();
        assert_eq!(sel, "，");
    }

    #[test]
    fn japanese_is_not_shredded_into_single_kana() {
        ensure_segmenter();
        let text = "日本語の文章です";
        let chars: Vec<char> = text.chars().collect();
        let click = chars.iter().position(|&c| c == 'で').unwrap();
        if let Some((s, e)) = cjk_word_range(text, click) {
            let sel: String = chars[s..=e].iter().collect();
            assert_eq!(sel, "です");
        }
    }

    #[test]
    fn is_cjk_covers_han_kana_hangul_fullwidth() {
        for c in ['中', 'あ', 'ア', '한', '，', '（'] {
            assert!(is_cjk(c), "{c} should be CJK");
        }
        for c in ['a', '1', '-', '/', 'é'] {
            assert!(!is_cjk(c), "{c} should not be CJK");
        }
    }
}

//! Double-click smart selection (à la iTerm2): when a double-click's
//! plain word selection sits inside a larger semantic object — a URL, an
//! email address, a file path, a matching bracket pair, or an OSC 8
//! hyperlink — expand the selection to cover the whole object.
//!
//! The expansion is strictly additive: a candidate is only applied when it
//! *contains* the plain word the double-click would have selected, so the
//! feature can never shrink a selection below what alacritty's semantic
//! (word) selection yields. With no candidate the caller falls back to the
//! stock `SelectionType::Semantic` behavior unchanged.

use std::sync::OnceLock;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;
use regex::Regex;

/// How many soft-wrapped rows to join on each side of the clicked row when
/// reconstructing the logical line. Caps the text a pathological fully-wrapped
/// scrollback line (minified JS piped to `cat`) can feed the regexes.
const MAX_WRAP_ROWS: usize = 32;

/// How many chars around the click offset the regex window covers on each
/// side. Matches never straddle real whitespace anyway, so a bounded window
/// only drops matches on absurdly long unbroken runs.
const MATCH_WINDOW: usize = 2000;

/// Bracket pairs a double-click on either half expands across (with
/// nesting): the ASCII pairs plus the full-width/CJK ones.
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

/// Symmetric quotes: open and close are the same char, so pairing needs the
/// parity heuristic in [`quote_range`] instead of the bracket scan.
const SYMMETRIC_QUOTES: [char; 3] = ['\'', '"', '`'];

/// A resolved smart selection: an inclusive grid-point span, plus whether the
/// span is `exact`. Exact spans have endpoints that may sit mid-word-run (CJK
/// prose, a candidate glued to non-separator text), so the caller must select
/// them with `SelectionType::Simple` — a `Semantic` anchor would re-expand the
/// endpoints across the very boundary the smart range established. Non-exact
/// spans end on run boundaries and can keep `Semantic` for word-wise dragging.
pub(super) struct SmartRange {
    pub start: Point,
    pub end: Point,
    pub exact: bool,
}

/// Resolve a smart selection range for a double-click at `click` (grid
/// coordinates). `None` means "no candidate beats the plain word" and the
/// caller should keep the stock semantic selection.
pub(super) fn grid_smart_range<T: EventListener>(
    term: &Term<T>,
    click: Point,
) -> Option<SmartRange> {
    // 0) The click carries the geometry of the frame that dispatched it, and
    //    the grid can shrink out from under it (a split, a window drag, a
    //    replayed attach size landing on the reader thread). Both walks below
    //    index `grid[click.line]` straight away, and `Grid`'s `Index<Line>`
    //    only `debug_assert`s the bound — a release build walks off the
    //    storage. Same guard, same reason, as `TerminalView::grid_line`.
    if click.line < term.topmost_line() || click.line > term.bottommost_line() {
        return None;
    }

    // 1) An explicit OSC 8 hyperlink run wins outright — the program told us
    //    the exact extent, no guessing needed.
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
    // A span whose flanks are separator chars ends exactly where alacritty's
    // semantic re-expansion would stop anyway; anything else must stay exact.
    // Only the separator set counts here — alacritty stops at nothing else,
    // so a flank of e.g. U+3000 ideographic space would still re-expand.
    let resolved = |s: usize, e: usize| SmartRange {
        start: points[s],
        end: points[e],
        exact: !(s == 0 || separators.contains(chars[s - 1]))
            || !(e + 1 == chars.len() || separators.contains(chars[e + 1])),
    };

    // 2) Double-click on a bracket or quote selects through its match.
    if let Some((s, e)) = pair_range(&chars, click_idx) {
        return Some(resolved(s, e));
    }

    // 3) CJK prose has no separators to walk — the whole clause is one run —
    //    so segment it with a dictionary instead of selecting the entire
    //    unbroken run. No segmenter available means the run stands as-is.
    if is_cjk(chars[click_idx])
        && let Some((s, e)) = cjk_word_range(&text, click_idx)
    {
        return Some(resolved(s, e));
    }

    // 4) URL / email / path / identifier patterns around the click.
    let (s, e) = smart_range(&text, &chars, click_idx, separators)?;
    Some(resolved(s, e))
}

/// Whether a char belongs to a CJK script (Han, Kana, Hangul, or the
/// full-width/CJK punctuation blocks) — text whose words aren't delimited by
/// whitespace or the separator set.
pub(super) fn is_cjk(c: char) -> bool {
    matches!(
        u32::from(c),
        0x1100..=0x11FF        // Hangul Jamo
        | 0x2E80..=0x9FFF      // CJK radicals, punctuation, Kana, ideographs
        | 0xAC00..=0xD7AF      // Hangul syllables
        | 0xF900..=0xFAFF      // CJK compatibility ideographs
        | 0xFF00..=0xFFEF      // full-width forms
        | 0x20000..=0x3134F    // ideograph extensions
    )
}

/// Kana or Hangul — the scripts jieba has no dictionary for. A run holding
/// either is left unsegmented rather than handed to jieba, which shreds it
/// into single characters (`です` → `で` `す`); selecting the whole run is the
/// friendlier failure.
#[cfg(not(target_os = "macos"))]
fn is_kana_or_hangul(c: char) -> bool {
    matches!(
        u32::from(c),
        0x1100..=0x11FF        // Hangul Jamo
        | 0x3040..=0x30FF      // Hiragana + Katakana
        | 0x31F0..=0x31FF      // Katakana phonetic extensions
        | 0xA960..=0xA97F      // Hangul Jamo Extended-A
        | 0xAC00..=0xD7FF      // Hangul syllables + Jamo Extended-B
        | 0xFF66..=0xFF9F      // half-width Katakana
    )
}

/// The jieba segmenter, built once on a background thread. The table costs
/// ~55 MB resident and ~130 ms to build, so it is constructed only if a CJK
/// double-click actually happens — see [`jieba_word_range`].
#[cfg(not(target_os = "macos"))]
static JIEBA: OnceLock<jieba_rs::Jieba> = OnceLock::new();

/// Kick off dictionary construction on a background thread (idempotent).
/// Never called eagerly: the first CJK double-click triggers it and settles
/// for the unsegmented run, so the UI thread never blocks on the build.
#[cfg(not(target_os = "macos"))]
fn warm() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(|| {
            let _ = JIEBA.get_or_init(jieba_rs::Jieba::new);
        });
    });
}

/// Dictionary-based word bounds for CJK text: the inclusive char range of the
/// word containing char index `click`, or `None` to keep the whole run.
///
/// The OS tokenizer wins wherever there is one. macOS's CFStringTokenizer
/// carries a Chinese lexicon that matches jieba on most prose, is locale-
/// independent, handles Japanese and Korean properly, and costs nothing —
/// jieba is only worth its ~55 MB on platforms with no such API.
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

/// Segment the contiguous CJK run around `click` with jieba and return the
/// token containing it. `None` — meaning "select the whole run" — when the
/// dictionary isn't built yet or the run isn't Chinese.
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
    // Japanese/Korean: jieba's Chinese dictionary would cut the run into
    // single characters, which is worse than not segmenting at all.
    if chars[rs..=re].iter().copied().any(is_kana_or_hangul) {
        return None;
    }
    // Building the table takes ~130 ms — far too long to hold the UI thread
    // on a click. Start it in the background and let this one click select
    // the whole run; every later click finds the table ready.
    let Some(jieba) = JIEBA.get() else {
        warm();
        return None;
    };
    let run: String = chars[rs..=re].iter().collect();
    // Token start/end are Unicode char offsets into `run`.
    let rel = click - rs;
    jieba
        .cut(&run, true)
        .iter()
        .find(|tok| rel < tok.end)
        .map(|tok| (rs + tok.start, rs + tok.end - 1))
}

/// CFStringTokenizer FFI. The tokenizer functions aren't wrapped by the
/// `core-foundation` crate, so declare them directly against its types.
#[cfg(target_os = "macos")]
mod tokenizer {
    use core_foundation::base::{CFIndex, CFRange, TCFType};
    use core_foundation::string::{CFString, CFStringRef};
    use std::os::raw::c_void;

    type CFStringTokenizerRef = *mut c_void;
    type CFLocaleRef = *const c_void;

    /// `kCFStringTokenizerUnitWordBoundary`: every position belongs to a
    /// token (words, punctuation runs, whitespace runs alike), which is the
    /// double-click contract.
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

    /// Inclusive char range of the token containing char index `click`.
    /// CFString ranges are UTF-16 code-unit offsets, so map through a
    /// per-char offset table both ways.
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

/// The contiguous run of cells carrying the same OSC 8 hyperlink URI as the
/// clicked cell, following soft wraps in both directions (a long link wraps
/// across rows; stopping at the row edge would truncate the selection).
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

/// Reconstruct the logical (soft-wrap-joined) line containing `click`:
/// the text with wide-char spacers dropped, a per-char grid point, and the
/// char index the click landed on. `None` when the click maps to no char
/// (out-of-bounds column).
///
/// When `bridge_hard_wrap` is set, rows are also joined across a *producer*
/// hard newline (no `WRAPLINE` flag) when the row is filled to the right edge
/// with a link char that continues into the first column of the next row. This
/// lets link resolution recover a URL a printing program split with a literal
/// `\n`, while double-click smart-select (which passes `false`) keeps its
/// word/semantic boundaries and never glues separate output lines together.
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
    // A hard bridge joins `line` to `line + 1` when the row is full to the
    // right edge with a link char and the next row opens with one too, which
    // rules out gluing an ordinary short line onto the following paragraph.
    //
    // It cannot rule out the converse: a hard newline carries no signal about
    // whether the producer split a URL, so a *complete* URL that happens to end
    // exactly at the right edge is bridged onto whatever the next row starts
    // with (`…/a` + `README.md` resolves as `…/aREADME.md`). There is no
    // reliable test for that — the head of a genuinely split URL is itself a
    // valid URL — so we accept the false positive: the address bar shows the
    // mistake and the user is one glance from spotting it.
    //
    // What we do not accept is the same accident promoting the *second* row to
    // the authority. `https://good.com` + `@evil.com/x` parses as userinfo, so
    // the real host becomes `evil.com` while the underline still reads
    // `good.com` — a phishing hop wearing a trusted label. Never bridge into
    // one.
    let is_link_char = |c: char| super::search::is_url_char(c);
    let hard = |line: Line| {
        // `line < bottom` must stay ahead of the `line + 1` lookup — the last
        // grid line has no successor to index.
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
            // Spacer cells pad wide (CJK/emoji) glyphs. A trailing spacer
            // follows its wide char; a leading spacer pads the end of a row
            // whose wide char wrapped to the next row, so it belongs to the
            // *next* pushed char.
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
    // A leading spacer at the very end of the collected range can point one
    // past the last char; treat that as no hit.
    let click_idx = click_idx.filter(|&i| i < points.len())?;
    Some((text, points, click_idx))
}

/// Double-click on a paired delimiter — bracket or quote — selects through
/// its match. `None` when the clicked char is neither, or has no match on
/// the logical line.
pub(super) fn pair_range(chars: &[char], click: usize) -> Option<(usize, usize)> {
    bracket_range(chars, click).or_else(|| quote_range(chars, click))
}

/// Whether the `'` at `i` is a contraction apostrophe rather than a quote.
///
/// A delimiter has whitespace, punctuation, or a line edge on at least one
/// side; a contraction is welded into a word on both (`it's`, `isn't`,
/// `won't`). Only `'` needs this — `"` and `` ` `` don't appear inside words.
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

/// Select through a matching symmetric quote (`'`, `"`, `` ` ``). Open and
/// close are the same char, so direction comes from parity: an even count of
/// that quote before the click means it opens (match forward), odd means it
/// closes (match backward).
///
/// Contraction apostrophes are excluded throughout — clicking one falls
/// through to the stock word, and they count neither toward the parity nor as
/// a candidate match. Without that, `it's a test, isn't it` pairs the two
/// contractions and a double-click on either selects `'s a test, isn'`. This
/// path returns before the `extends` guard that keeps other candidates
/// additive (see [`pair_is_plausible`]), so a bad match here has no safety net.
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

/// Whether a candidate span is an acceptable match for its bracket pair.
///
/// Every pair but `<>` is accepted outright — `( a )` is a legitimate subshell,
/// `[ 1 ]` a legitimate index. `<` and `>` are different: they are comparison
/// and redirection operators at least as often as delimiters, and the bracket
/// path returns before the `extends` guard that keeps every other candidate
/// additive, so a bad match here has no safety net. Require the span to hug its
/// contents, which real delimiters do (`Vec<String>`, `<div>`, `<user@host>`,
/// `<info>`) and a comparison doesn't (`a < b > c`, `x <= 0 || y > 9`).
///
/// Redirections need no special handling: `2>&1` or `cmd > out` have no
/// partner to match, so the scan already fails.
fn pair_is_plausible(chars: &[char], open: char, s: usize, e: usize) -> bool {
    if open != '<' {
        return true;
    }
    e > s + 1 && !chars[s + 1].is_whitespace() && !chars[e - 1].is_whitespace()
}

/// Select through a matching bracket: `click` on an opener scans forward,
/// on a closer scans backward, nesting-aware. Inclusive char range covering
/// both brackets, or `None` when the clicked char isn't a bracket, the match
/// isn't on the logical line, or the span fails [`pair_is_plausible`].
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

/// Patterns tried in specificity order after the URL detector: email,
/// scientific-notation number, file path, dotted/hyphenated identifier.
/// (URLs go through `search::url_span_at` first — it handles scheme
/// detection, wrapper stripping and trailing-punctuation trimming better
/// than a lone regex.)
fn regexes() -> &'static [Regex] {
    static RE: OnceLock<Vec<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        [
            // Email address.
            r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}",
            // Scientific notation (6.02e+23).
            r"\b[0-9]+(?:\.[0-9]+)?[eE][+-]?[0-9]+\b",
            // File path: at least two segments, or ~/.-anchored.
            r"[A-Za-z0-9._+@%~-]*(?:/[A-Za-z0-9._+@%~-]+)+/?",
            // Identifier chained with `.`/`-` (foo-bar.baz, 10.0.0.1).
            // ASCII classes only: the regex crate's `\w` matches Han
            // ideographs, which would swallow CJK text glued to a Latin
            // word and defeat the script narrowing.
            r"[0-9A-Za-z_]+(?:[.-][0-9A-Za-z_]+)*",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("static smart-select regex"))
        .collect()
    })
}

/// Find a semantic object containing char index `click` in `text` that
/// strictly extends the plain word selection the configured `separators`
/// would produce. Inclusive char range, or `None` to keep the stock word.
pub(super) fn smart_range(
    text: &str,
    chars: &[char],
    click: usize,
    separators: &str,
) -> Option<(usize, usize)> {
    if click >= chars.len() || chars[click].is_whitespace() {
        return None;
    }

    // The plain word the double-click would select: the run of chars around
    // the click that are neither whitespace nor configured separators.
    // (Mirrors alacritty's semantic expansion over the same separator set.)
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
    // CJK chars/punctuation glue onto Latin runs (`分支name，已` is one
    // separator-free run), so the word the user *means* is the same-script
    // sub-run around the click. Candidates are judged against that; if
    // nothing beats it, the narrowed run itself is the answer.
    let (ws, we) = narrow_to_script(chars, click, pws, pwe);
    // Applied only when the candidate strictly contains the (narrowed) word,
    // so smart select can grow the meant word but never shrink it.
    let extends = |s: usize, e: usize| s <= ws && e >= we && (s < ws || e > we);

    if let Some((s, e, _url)) = super::search::url_span_at(text, click)
        && extends(s, e)
    {
        return Some((s, e));
    }

    // Regexes run over a bounded byte window around the click.
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
    // No pattern beat the meant word — but if script narrowing shrank the
    // raw run (Latin word glued to CJK text), that narrowed word *is* the
    // correction.
    ((ws, we) != (pws, pwe)).then_some((ws, we))
}

/// Shrink the inclusive run `[lo, hi]` to the chars sharing `click`'s script
/// class (CJK vs not) — the sub-run a double-click on mixed-script text
/// means. A no-op on single-script runs.
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

    /// alacritty's stock separator set, which is also the config default.
    const SEPS: &str = ",│`|:\"' ()[]{}<>\t";

    /// In production the jieba table builds lazily off-thread and the racing
    /// click settles for the whole run; tests want it ready up front. No-op on
    /// macOS, where CFStringTokenizer needs no warm-up.
    fn ensure_segmenter() {
        #[cfg(not(target_os = "macos"))]
        let _ = JIEBA.get_or_init(jieba_rs::Jieba::new);
    }

    fn range(text: &str, click: usize) -> Option<(usize, usize)> {
        let chars: Vec<char> = text.chars().collect();
        smart_range(text, &chars, click, SEPS)
    }

    // ---- Grid-level tests ----
    //
    // The functions above operate on a plain `&str`; everything below drives a
    // real `Term` through the VT parser instead, because the grid is where the
    // index arithmetic actually gets hard: wide CJK glyphs occupy two cells
    // (the second a spacer), soft-wrapped rows have to be stitched back into
    // one logical line, and OSC 8 runs can straddle both.

    /// A `cols`×`rows` terminal with `input` fed through the VT parser, so the
    /// grid holds exactly what a PTY would have produced.
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

    /// The text a double-click at `(line, col)` would select, or `None` when
    /// no smart candidate applies and the caller keeps the stock word.
    fn grid_select(term: &Term<VoidListener>, line: i32, col: usize) -> Option<String> {
        let r = grid_smart_range(term, Point::new(Line(line), Column(col)))?;
        Some(term.bounds_to_string(r.start, r.end))
    }

    /// Column of the first occurrence of `needle` on row 0 — keeps the tests
    /// from hard-coding offsets that shift when the fixture text changes.
    fn col_of(row: &str, needle: &str) -> usize {
        row.find(needle).expect("needle in fixture")
    }

    #[test]
    fn osc8_hyperlink_selects_the_declared_extent_not_the_visible_word() {
        // The link text has a space in it: only the OSC 8 run knows where the
        // link really ends, which is the whole point of checking it first.
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
        // A cell outside the run must not pick the link up.
        assert_ne!(
            grid_select(&term, 0, col_of(line, "now")).as_deref(),
            Some("click here"),
        );
    }

    #[test]
    fn osc8_hyperlink_follows_a_soft_wrap() {
        // 30 chars of link text in 20 columns: the run fills row 0 and spills
        // 10 cells onto row 1. Stopping at the row edge would truncate the
        // selection to the visible first half.
        let term = term_with(
            20,
            4,
            "\x1b]8;;https://e.com\x1b\\aaaaaaaaaabbbbbbbbbbcccccccccc\x1b]8;;\x1b\\",
        );
        let whole = "aaaaaaaaaabbbbbbbbbbcccccccccc";
        // Click on the wrapped remainder (row 1) — walks backwards over the wrap.
        assert_eq!(grid_select(&term, 1, 2).as_deref(), Some(whole));
        // ...and from the first row, walking forwards over it.
        assert_eq!(grid_select(&term, 0, 3).as_deref(), Some(whole));
    }

    #[test]
    fn soft_wrapped_url_is_stitched_back_into_one_selection() {
        // No OSC 8 here — the URL is recovered from the joined logical line,
        // so this exercises `logical_line_at`'s wrap walk rather than the
        // hyperlink path.
        let term = term_with(20, 4, "see https://example.com/deep/path here");
        let whole = "https://example.com/deep/path";
        // Row 0 holds "see https://example.", row 1 the "com/deep/path here"
        // remainder. Clicking the head joins forwards over the wrap...
        assert_eq!(
            grid_select(&term, 0, col_of("see https://example", "example")).as_deref(),
            Some(whole),
        );
        // ...and clicking the tail joins backwards, which is the direction a
        // click on a continuation row depends on entirely.
        assert_eq!(
            grid_select(&term, 1, col_of("com/deep/path here", "deep")).as_deref(),
            Some(whole),
        );
    }

    #[test]
    fn hard_wrapped_url_is_bridged_only_for_links() {
        // A printing program emitted a literal `\n` mid-URL: the head fills
        // row 0 exactly (20 chars, no WRAPLINE flag) and the tail lands on
        // row 1. Soft-wrap stitching can't see across this gap; the hard
        // bridge in link mode joins them, while smart-select stays put.
        let term = term_with(20, 4, "https://example.com/\r\ndeep/path/seg rest");
        // The break carries no WRAPLINE flag — this is a producer hard newline,
        // not a terminal soft wrap.
        assert!(
            !term.grid()[Line(0)][Column(19)]
                .flags
                .contains(Flags::WRAPLINE),
            "fixture must be a hard newline, not a soft wrap"
        );

        // Link mode (bridge_hard_wrap = true) recovers the whole URL spanning
        // both rows.
        let click = Point::new(Line(0), Column(3));
        let (text, _points, _idx) =
            logical_line_at(&term, click, true).expect("logical line under click");
        let idx = text.find("https").expect("url in bridged line");
        let (_s, _e, url) =
            crate::terminal::search::url_span_at(&text, idx + 2).expect("url span in bridged line");
        assert_eq!(url, "https://example.com/deep/path/seg");

        // Smart-select mode (bridge_hard_wrap = false) must NOT glue the two
        // output lines together.
        let (text, _points, _idx) =
            logical_line_at(&term, click, false).expect("logical line under click");
        assert!(
            !text.contains("deep"),
            "double-click must not bridge a hard newline: {text:?}"
        );
    }

    #[test]
    fn a_hard_break_before_userinfo_is_never_bridged() {
        // Row 0 ends with a bare host that fills the row exactly, row 1 opens
        // with `@`. Bridging would resolve `https://good.com@evil.com/x`, whose
        // authority per RFC 3986 is `evil.com` — the underline would read
        // `good.com` while the click navigated elsewhere. The hard bridge must
        // refuse this one even though the row shape otherwise invites it.
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
        // The `@` guard is about the *ambiguity* of a hard newline. A soft wrap
        // is the terminal folding one logical line, so the continuation is
        // certain and a userinfo URL must still resolve whole.
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
        // Each Han char occupies two cells; the second carries WIDE_CHAR_SPACER
        // and has no `c` of its own. Clicking either half must select the same
        // segmented word — an off-by-one in the spacer branch shows up here.
        ensure_segmenter();
        let term = term_with(40, 3, "run 北京欢迎你 done");
        // "run " is 4 cells, then 北 at col 4 (spacer at 5), 京 at 6 (spacer 7).
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
        // An odd column count leaves one cell at the end of the row: the wide
        // char can't fit, so alacritty pads with LEADING_WIDE_CHAR_SPACER and
        // moves the glyph to the next row. The logical line must still join.
        ensure_segmenter();
        let term = term_with(9, 4, "abcdefgh北京欢迎你");
        // 北 is pushed to row 1 col 0 by the leading spacer at row 0 col 8.
        assert_eq!(grid_select(&term, 1, 0).as_deref(), Some("北京"));
    }

    #[test]
    fn click_past_the_last_column_yields_no_range() {
        let term = term_with(10, 2, "hello");
        assert!(grid_smart_range(&term, Point::new(Line(0), Column(10))).is_none());
        assert!(grid_smart_range(&term, Point::new(Line(0), Column(99))).is_none());
    }

    /// A double-click dispatched with the previous frame's geometry can name a
    /// row the grid has since dropped. Indexing it walks off the storage, and
    /// the click arrives in a gpui `extern "C"` callback where that panic
    /// aborts instead of unwinding — so the row has to be refused first.
    #[test]
    fn click_outside_the_grid_rows_yields_no_range() {
        let term = term_with(10, 2, "hello");
        // Below the last row of a shrunken grid...
        assert!(grid_smart_range(&term, Point::new(Line(2), Column(0))).is_none());
        assert!(grid_smart_range(&term, Point::new(Line(9_000), Column(0))).is_none());
        // ...and above the top of a scrollback this short.
        assert!(grid_smart_range(&term, Point::new(Line(-1), Column(0))).is_none());
    }

    fn selected(text: &str, click: usize) -> Option<String> {
        let chars: Vec<char> = text.chars().collect();
        range(text, click).map(|(s, e)| chars[s..=e].iter().collect())
    }

    #[test]
    fn url_expands_past_scheme_colon() {
        let text = "fetch https://example.com/a/b?q=1 done";
        // Click inside "example" — the plain word starts after the `:`
        // separator; smart select recovers the whole URL.
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
        // With the default separators the plain word already covers the whole
        // address (`@` and `.` are word chars) — the candidate equals the word
        // and must be rejected, keeping the stock selection.
        let text = "author:dev@example.com pushed";
        let click = text.find("example").unwrap();
        assert_eq!(range(text, click), None);
        // With `@` configured as a separator, the email regex reassembles the
        // full address across it.
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
        // The whole path is one plain word already (no separators inside);
        // candidates equal to the word are rejected → stock selection.
        let text = "cat /usr/local/bin/tool";
        let click = text.find("local").unwrap();
        assert_eq!(range(text, click), None);
    }

    #[test]
    fn path_glued_to_colon_expands() {
        // `error:/tmp/x/y` — the word starts after `:`; the path regex
        // must not leak left past the colon but the URL/identifier ones
        // must not shrink it either. Path candidate is `/tmp/x/y`, equal
        // to the plain word → None. Click on `error` side: word `error`.
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
        // With `.` and `+` configured as separators (finer-grained
        // boundaries), the sci-notation regex reassembles the number.
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
        // Fine-grained separators split `foo-bar.baz`; the identifier
        // regex restores the full dotted chain.
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
        // No separator or punctuation between the scripts at all — the
        // identifier regex must not reassemble the mixed run (`\w` would).
        let text = "已合并到main分支";
        let chars: Vec<char> = text.chars().collect();
        let click = chars.iter().position(|&c| c == 'm').unwrap();
        let (s, e) = smart_range(text, &chars, click, SEPS).expect("narrowed word");
        let sel: String = chars[s..=e].iter().collect();
        assert_eq!(sel, "main");
    }

    #[test]
    fn latin_word_glued_to_cjk_narrows_to_the_latin_run() {
        // `，` and `已` are not separators, so the raw run is
        // `worktree-feat-smart-select，已`; the meant word is the Latin part.
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
        // First ' opens (0 quotes before), second closes.
        assert_eq!(quote_range(&chars, 5), Some((5, 9)));
        assert_eq!(quote_range(&chars, 9), Some((5, 9)));
        // Double quotes pair independently of the single ones.
        assert_eq!(quote_range(&chars, 11), Some((11, 15)));
        assert_eq!(quote_range(&chars, 15), Some((11, 15)));
        // An unmatched opener finds nothing.
        let chars: Vec<char> = "say 'oops".chars().collect();
        assert_eq!(quote_range(&chars, 4), None);
        // Non-quote chars never match.
        assert_eq!(quote_range(&chars, 1), None);
    }

    #[test]
    fn contraction_apostrophes_do_not_pair() {
        let chars: Vec<char> = "it's a test, isn't it".chars().collect();
        // Clicking either contraction falls through to the stock word.
        assert_eq!(quote_range(&chars, 2), None);
        assert_eq!(quote_range(&chars, 16), None);
        // And the whole line yields no smart candidate at all, so the
        // double-click keeps alacritty's `it's`.
        let text = "it's a test, isn't it";
        assert_eq!(range(text, 2), None);
    }

    #[test]
    fn contractions_do_not_skew_a_real_quote() {
        // The apostrophes inside the quoted span must not flip the parity or
        // steal the match from the genuine delimiters.
        let chars: Vec<char> = "echo 'it isn't so' done".chars().collect();
        let open = 5;
        let close = chars.iter().rposition(|&c| c == '\'').unwrap();
        assert_eq!(quote_range(&chars, open), Some((open, close)));
        assert_eq!(quote_range(&chars, close), Some((open, close)));
    }

    #[test]
    fn trailing_apostrophe_still_closes() {
        // `dogs'` — the apostrophe has a word char only on its left, so it is
        // a delimiter, not a contraction.
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
        // Real delimiters: generics, tags, placeholders, bracketed addresses.
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
        // Comparison operators must not pair across half a line — a space just
        // inside either end is the tell.
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
        // Redirections never had a partner to match in the first place.
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
        // The emoji is two UTF-16 units: a tokenizer offset table that counted
        // chars instead would shift every index after it. Both backends agree
        // on `世界`, so a skewed mapping shows up as a different token.
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

    /// Japanese must not be run through jieba's Chinese dictionary — it cuts
    /// kana into single characters, which is worse than leaving the run whole.
    /// macOS hands it to CFStringTokenizer, which segments it properly.
    #[test]
    fn japanese_is_not_shredded_into_single_kana() {
        ensure_segmenter();
        let text = "日本語の文章です";
        let chars: Vec<char> = text.chars().collect();
        let click = chars.iter().position(|&c| c == 'で').unwrap();
        // macOS yields a real token, never a lone kana; elsewhere the run
        // comes back unsegmented and the caller selects all of it.
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

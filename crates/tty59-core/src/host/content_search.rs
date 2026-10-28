//! The engine behind [`Host::search_content`](super::Host::search_content):
//! "find in files" over one machine's filesystem.
//!
//! The local host runs it directly and a remote `tty59-server` runs the very
//! same code on its side of the wire, so what a query matches, which files it
//! skips and where each cap bites cannot differ between a local and a remote
//! pane.
//!
//! The walk agrees with the name search and the file tree about what is in
//! the project: a directory's `.gitignore` applies whether or not there is a
//! repository around it, dot-entries are skipped, and `show_hidden` turns
//! both off. Parent directories' ignore files, `.git/info/exclude` and the
//! user's global excludes are *not* consulted — the tree does not dim what
//! they name either, and a file that is on screen in the Files tab but never
//! found by Search would be the harder surprise.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use regex::{Regex, RegexBuilder};

use super::{ContentHit, ContentLimits, ContentQuery, ContentRange, ContentResults};

/// How much of a file is inspected for a NUL byte before deciding it is
/// binary — the same sniff git and ripgrep use.
const BINARY_SNIFF: usize = 8000;

/// The longest excerpt a hit carries, in characters, before the `…` marks.
pub const EXCERPT_CHARS: usize = 200;

/// How much of the line before its first match an excerpt keeps when the line
/// has to be cut, in characters.
const EXCERPT_LEAD: usize = 40;

/// The mark put where an excerpt was cut.
pub const ELLIPSIS: &str = "…";

/// Compile a query into the one regex every file is matched with.
///
/// A literal query is escaped, so `a.b` finds `a.b` and not `axb`; whole-word
/// wraps whatever came out in `\b…\b`. An expression that does not compile is
/// `InvalidInput`, carrying the parser's own explanation.
pub fn compile(query: &ContentQuery) -> io::Result<Regex> {
    let body = if query.regex {
        query.pattern.clone()
    } else {
        regex::escape(&query.pattern)
    };
    let pattern = if query.whole_word {
        format!(r"\b(?:{body})\b")
    } else {
        body
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(!query.case_sensitive)
        // A line is searched on its own, so `^`/`$` mean its ends either way;
        // multi-line keeps them meaning that when the whole file is scanned
        // for the first candidate line.
        .multi_line(true)
        .size_limit(8 * (1 << 20))
        .build()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))
}

/// Search `roots` on this machine's filesystem.
pub fn search(
    roots: &[PathBuf],
    query: &ContentQuery,
    limits: &ContentLimits,
) -> io::Result<ContentResults> {
    let mut out = ContentResults::default();
    if query.pattern.is_empty() {
        return Ok(out);
    }
    let re = compile(query)?;
    let roots = distinct_roots(roots);
    for root in &roots {
        // A missing root is the one failure worth reporting: every other
        // unreadable thing below it is a file the walk steps past.
        std::fs::metadata(root)?;
    }

    // The limits come off the wire on a server; a budget too large to add to
    // a clock is no budget, not a panic.
    let deadline = Instant::now().checked_add(Duration::from_millis(limits.max_millis));
    let max_hits = limits.max_hits as usize;
    let per_file = limits.max_hits_per_file.max(1) as usize;

    'roots: for root in &roots {
        let walk = walker(root, query.show_hidden);
        for entry in walk {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if out.files_searched >= limits.max_files
                || deadline.is_some_and(|d| Instant::now() >= d)
            {
                out.truncated = true;
                break 'roots;
            }
            let path = entry.path();
            let Some(text) = read_text(path, limits.max_file_bytes) else {
                continue;
            };
            out.files_searched += 1;
            match search_text(&re, &text, path, per_file, max_hits, &mut out.hits) {
                Scan::Done => {}
                Scan::FileCapped => out.truncated = true,
                Scan::TotalCapped => {
                    out.truncated = true;
                    break 'roots;
                }
            }
        }
    }
    Ok(out)
}

/// The roots with duplicates and roots nested inside another root dropped, so
/// one file is never walked — and listed — twice.
fn distinct_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for root in roots {
        if roots
            .iter()
            .any(|other| other != root && root.starts_with(other))
            || out.contains(root)
        {
            continue;
        }
        out.push(root.clone());
    }
    out
}

fn walker(root: &Path, show_hidden: bool) -> ignore::Walk {
    let mut b = ignore::WalkBuilder::new(root);
    b.standard_filters(false)
        .hidden(!show_hidden)
        .git_ignore(!show_hidden)
        // A plain folder's `.gitignore` counts too, as it does in the tree.
        .require_git(false)
        .parents(false)
        .follow_links(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        // `.git` is never the project's content, even with hidden files on:
        // the name search marks it ignored on the same grounds.
        .filter_entry(|e| e.file_name() != ".git");
    b.build()
}

/// The file as text, or `None` for a file to step past: unreadable, over the
/// size cap, or binary. Bytes that are not UTF-8 are replaced rather than
/// losing the whole file to one Latin-1 comment.
fn read_text(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > max_bytes {
        return None;
    }
    // Read through the cap rather than trusting the size just seen: a log
    // that grows between the two would otherwise be read whole.
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > max_bytes {
        return None;
    }
    if is_binary(&bytes) {
        return None;
    }
    Some(match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    })
}

pub fn is_binary(bytes: &[u8]) -> bool {
    memchr::memchr(0, &bytes[..bytes.len().min(BINARY_SNIFF)]).is_some()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Scan {
    /// Every matching line in the file was taken.
    Done,
    /// The file had more matching lines than the per-file cap.
    FileCapped,
    /// The search as a whole is full; nothing more should be read.
    TotalCapped,
}

/// Collect the matching lines of one file's text into `out`.
///
/// Rather than run the regex over every line, it asks the whole text for the
/// next match and only then looks at the line that match starts on — most of
/// a file has nothing to find, and one pass over it is the cheap way to learn
/// that. A match that runs across a newline (`\s+` can) finds its line, but
/// the line is still matched on its own, so no hit ever spans two lines.
pub fn search_text(
    re: &Regex,
    text: &str,
    path: &Path,
    per_file: usize,
    max_hits: usize,
    out: &mut Vec<ContentHit>,
) -> Scan {
    let mut taken = 0usize;
    let mut pos = 0usize;
    let mut line_no = 1u32;
    let mut counted_to = 0usize;
    while pos <= text.len() {
        let Some(m) = re.find_at(text, pos) else {
            break;
        };
        let line_start = text[..m.start()].rfind('\n').map_or(0, |i| i + 1);
        let line_end = text[m.start()..]
            .find('\n')
            .map_or(text.len(), |i| m.start() + i);
        line_no +=
            memchr::memchr_iter(b'\n', &text.as_bytes()[counted_to..line_start]).count() as u32;
        counted_to = line_start;

        let line = text[line_start..line_end]
            .strip_suffix('\r')
            .unwrap_or(&text[line_start..line_end]);
        let matches: Vec<(usize, usize)> = re
            .find_iter(line)
            .filter(|m| !m.is_empty())
            .map(|m| (m.start(), m.end()))
            .collect();
        if !matches.is_empty() {
            if taken == per_file {
                return Scan::FileCapped;
            }
            if out.len() >= max_hits {
                return Scan::TotalCapped;
            }
            let column = line[..matches[0].0].chars().count() as u32 + 1;
            let (excerpt, ranges) = excerpt(line, &matches);
            out.push(ContentHit {
                path: path.to_path_buf(),
                line: line_no,
                column,
                text: excerpt,
                ranges,
            });
            taken += 1;
        }
        pos = line_end + 1;
    }
    Scan::Done
}

/// The part of `line` a hit shows, and where its matches fall within it.
///
/// Leading indentation goes, unless a match is in it. A line longer than
/// [`EXCERPT_CHARS`] is cut to a window that opens [`EXCERPT_LEAD`]
/// characters before the first match, with [`ELLIPSIS`] at each cut; matches
/// outside the window are dropped and one straddling its edge is clipped.
pub fn excerpt(line: &str, matches: &[(usize, usize)]) -> (String, Vec<ContentRange>) {
    let first = matches.first().map_or(0, |m| m.0);
    let indent = line.len() - line.trim_start().len();
    let trim = indent.min(first);
    let body = &line[trim..];
    let shifted = matches.iter().map(|&(s, e)| (s - trim, e - trim));

    let (from, to) = if body.chars().count() <= EXCERPT_CHARS {
        (0, body.len())
    } else {
        let first = first - trim;
        let from = body[..first]
            .char_indices()
            .rev()
            .nth(EXCERPT_LEAD - 1)
            .map_or(0, |(i, _)| i);
        let to = body[from..]
            .char_indices()
            .nth(EXCERPT_CHARS)
            .map_or(body.len(), |(i, _)| from + i);
        (from, to)
    };

    let lead = if from > 0 { ELLIPSIS } else { "" };
    let tail = if to < body.len() { ELLIPSIS } else { "" };
    let text = format!("{lead}{}{tail}", &body[from..to]);
    let ranges = shifted
        .filter(|&(s, e)| e > from && s < to)
        .map(|(s, e)| ContentRange {
            start: (s.max(from) - from + lead.len()) as u32,
            end: (e.min(to) - from + lead.len()) as u32,
        })
        .collect();
    (text, ranges)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(pattern: &str) -> ContentQuery {
        ContentQuery {
            pattern: pattern.into(),
            ..ContentQuery::default()
        }
    }

    fn lines(text: &str, query: &ContentQuery) -> Vec<(u32, u32, String)> {
        let re = compile(query).unwrap();
        let mut out = Vec::new();
        search_text(&re, text, Path::new("f"), 100, 100, &mut out);
        out.into_iter()
            .map(|h| (h.line, h.column, h.text))
            .collect()
    }

    fn marked(hit: &ContentHit) -> String {
        let mut s = String::new();
        let mut at = 0;
        for r in &hit.ranges {
            s.push_str(&hit.text[at..r.start as usize]);
            s.push('[');
            s.push_str(&hit.text[r.start as usize..r.end as usize]);
            s.push(']');
            at = r.end as usize;
        }
        s.push_str(&hit.text[at..]);
        s
    }

    #[test]
    fn a_literal_query_is_not_an_expression() {
        let text = "a.b\naxb\n";
        assert_eq!(lines(text, &q("a.b")), vec![(1, 1, "a.b".into())]);
    }

    #[test]
    fn case_is_ignored_unless_asked_for() {
        let text = "Needle\nneedle\nNEEDLE\n";
        assert_eq!(lines(text, &q("needle")).len(), 3);
        let exact = ContentQuery {
            case_sensitive: true,
            ..q("needle")
        };
        assert_eq!(lines(text, &exact), vec![(2, 1, "needle".into())]);
    }

    #[test]
    fn whole_word_needs_a_boundary_on_both_sides() {
        let text = "foo\nfoobar\nbar_foo\n(foo)\n";
        let word = ContentQuery {
            whole_word: true,
            ..q("foo")
        };
        let found: Vec<u32> = lines(text, &word).into_iter().map(|l| l.0).collect();
        assert_eq!(found, vec![1, 4]);
    }

    #[test]
    fn a_regex_query_is_an_expression_and_a_bad_one_says_why() {
        let re = ContentQuery {
            regex: true,
            ..q(r"fn \w+\(")
        };
        assert_eq!(lines("fn main() {}\nlet f = 1;\n", &re).len(), 1);

        let bad = ContentQuery {
            regex: true,
            ..q("(unclosed")
        };
        let err = compile(&bad).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("unclosed"), "{err}");
    }

    #[test]
    fn line_and_column_are_one_based_and_count_characters() {
        let text = "first\n\n  héllo wörld needle\r\nlast needle";
        let got = lines(text, &q("needle"));
        assert_eq!(got[0].0, 3);
        assert_eq!(got[0].1, 15, "counted in chars, indent included");
        assert_eq!(got[0].2, "héllo wörld needle", "indent and \\r dropped");
        assert_eq!((got[1].0, got[1].1), (4, 6));
    }

    #[test]
    fn every_match_on_a_line_is_one_hit_with_several_ranges() {
        let re = compile(&q("ab")).unwrap();
        let mut out = Vec::new();
        search_text(&re, "  ab ab x AB\n", Path::new("f"), 10, 10, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(marked(&out[0]), "[ab] [ab] x [AB]");
    }

    #[test]
    fn an_empty_match_is_not_a_hit() {
        let re = ContentQuery {
            regex: true,
            ..q("x*")
        };
        assert_eq!(lines("abc\nxx\n", &re), vec![(2, 1, "xx".into())]);
    }

    #[test]
    fn a_match_across_a_newline_does_not_make_a_two_line_hit() {
        let re = ContentQuery {
            regex: true,
            ..q(r"a\s+b")
        };
        assert!(lines("a\nb\n", &re).is_empty());
        assert_eq!(lines("x\na  b\n", &re), vec![(2, 1, "a  b".into())]);
    }

    #[test]
    fn a_long_line_is_cut_around_its_first_match() {
        let line = format!("{}needle{}", "a".repeat(500), "b".repeat(500));
        let (text, ranges) = excerpt(&line, &[(500, 506)]);
        assert!(text.starts_with(ELLIPSIS) && text.ends_with(ELLIPSIS));
        assert_eq!(text.chars().count(), EXCERPT_CHARS + 2);
        let r = ranges[0];
        assert_eq!(&text[r.start as usize..r.end as usize], "needle");
        assert_eq!(text[..r.start as usize].chars().count(), EXCERPT_LEAD + 1);
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        let line = format!("{}針{}", "字".repeat(300), "字".repeat(300));
        let at = line.find('針').unwrap();
        let (text, ranges) = excerpt(&line, &[(at, at + '針'.len_utf8())]);
        let r = ranges[0];
        assert_eq!(&text[r.start as usize..r.end as usize], "針");
    }

    #[test]
    fn a_match_in_the_indent_keeps_the_indent() {
        let (text, ranges) = excerpt("\tx", &[(0, 1)]);
        assert_eq!(text, "\tx");
        assert_eq!(ranges, vec![ContentRange { start: 0, end: 1 }]);
    }

    #[test]
    fn caps_say_which_one_was_hit() {
        let re = compile(&q("x")).unwrap();
        let text = "x\nx\nx\n";
        let mut out = Vec::new();
        assert_eq!(
            search_text(&re, text, Path::new("f"), 2, 10, &mut out),
            Scan::FileCapped
        );
        assert_eq!(out.len(), 2);

        let mut out = Vec::new();
        assert_eq!(
            search_text(&re, text, Path::new("f"), 10, 3, &mut out),
            Scan::Done,
            "exactly full is not truncated"
        );
        assert_eq!(
            search_text(&re, text, Path::new("g"), 10, 3, &mut out),
            Scan::TotalCapped
        );
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn nested_and_repeated_roots_are_walked_once() {
        let roots = [
            PathBuf::from("/r"),
            PathBuf::from("/r/sub"),
            PathBuf::from("/r"),
            PathBuf::from("/other"),
        ];
        assert_eq!(
            distinct_roots(&roots),
            vec![PathBuf::from("/r"), PathBuf::from("/other")]
        );
    }

    #[test]
    fn an_unbounded_time_budget_is_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("tty59-cs-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "needle\n").unwrap();
        let limits = ContentLimits {
            max_millis: u64::MAX,
            ..ContentLimits::default()
        };
        let found = search(&[dir.clone()], &q("needle"), &limits).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(found.hits.len(), 1);
    }

    #[test]
    fn nul_in_the_head_means_binary() {
        assert!(is_binary(b"abc\0def"));
        assert!(!is_binary(b"plain text"));
        let mut late = vec![b'a'; BINARY_SNIFF + 10];
        late.push(0);
        assert!(!is_binary(&late), "only the head is sniffed");
    }
}

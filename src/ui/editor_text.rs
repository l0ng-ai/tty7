//! The code editor's view of a file's bytes: how they decode into editable
//! text, how that text is written back, how the file is indented, and what an
//! `.editorconfig` says about it.
//!
//! Everything here is pure — no I/O, no gpui — so the editor can read files
//! through whatever host abstraction it uses (local disk, SFTP, a remote
//! `tty59-server`) and hand the bytes over. The one promise the module makes is
//! that a file the editor opens and saves unchanged comes back byte-for-byte:
//! the encoding, the byte-order mark and the line endings are remembered in a
//! [`TextFormat`] on the way in and restored by [`encode`] on the way out.

use std::collections::HashMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use encoding_rs::{EncoderResult, Encoding, GB18030, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252};

// ---------------------------------------------------------------------------
// Decoding and encoding
// ---------------------------------------------------------------------------

/// How much of the file is searched for a NUL byte before deciding it is
/// binary. The same window git uses: large enough that a text file's header
/// is covered, small enough that a multi-gigabyte blob costs nothing.
const BINARY_SNIFF_LEN: usize = 8 * 1024;

const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
const UTF16LE_BOM: &[u8] = &[0xFF, 0xFE];
const UTF16BE_BOM: &[u8] = &[0xFE, 0xFF];

/// The line break a file uses. A lone `\r` (classic Mac OS) has no variant:
/// such files are rare enough that they are left exactly as they are rather
/// than converted, which keeps them round-tripping untouched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    /// The status-bar label.
    pub(crate) fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::CrLf => "CRLF",
        }
    }
}

/// Everything about a file's bytes that the editable text does not carry, so
/// that [`encode`] can put it back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TextFormat {
    pub(crate) encoding: &'static Encoding,
    /// Whether the file started with a byte-order mark. Always true for UTF-16,
    /// which is only ever recognised by its BOM.
    pub(crate) bom: bool,
    pub(crate) line_ending: LineEnding,
}

impl Default for TextFormat {
    /// What a brand-new file is written as: plain UTF-8, no BOM, LF.
    fn default() -> Self {
        TextFormat {
            encoding: UTF_8,
            bom: false,
            line_ending: LineEnding::Lf,
        }
    }
}

impl TextFormat {
    /// The status-bar label, e.g. "UTF-8", "UTF-8 BOM", "UTF-16 LE", "GB18030".
    pub(crate) fn encoding_label(&self) -> &'static str {
        if self.encoding == UTF_8 && self.bom {
            "UTF-8 BOM"
        } else {
            encoding_name(self.encoding)
        }
    }
}

/// A human name for an encoding. encoding_rs's own names are the WHATWG
/// canonical ones ("UTF-16LE", "windows-1252"), which read like identifiers in
/// a status bar, so the handful the editor can produce get friendlier ones.
fn encoding_name(encoding: &'static Encoding) -> &'static str {
    if encoding == UTF_8 {
        "UTF-8"
    } else if encoding == UTF_16LE {
        "UTF-16 LE"
    } else if encoding == UTF_16BE {
        "UTF-16 BE"
    } else if encoding == GB18030 {
        "GB18030"
    } else if encoding == WINDOWS_1252 {
        "Windows-1252"
    } else {
        encoding.name()
    }
}

/// A file's bytes as editable text, plus what is needed to write it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Decoded {
    /// The text with any BOM removed and, when the file is CRLF, every `\r\n`
    /// turned into `\n`.
    pub(crate) text: String,
    pub(crate) format: TextFormat,
}

/// Decodes a file for editing, or returns `None` when it looks binary.
///
/// The order of the checks matters:
///
/// 1. A byte-order mark is the file stating its encoding, so it wins outright:
///    UTF-8, UTF-16 LE or UTF-16 BE. UTF-16 is full of NUL bytes, which is why
///    this runs before the binary check. (A UTF-32 LE BOM starts with the
///    UTF-16 LE one and is read as UTF-16; UTF-32 text files are too rare to
///    carry a separate path.)
/// 2. A NUL byte in the first 8 KiB means binary. Text in every encoding this
///    function can otherwise produce never contains one.
/// 3. Valid UTF-8 is UTF-8. Plain ASCII lands here too.
/// 4. Anything else is legacy-encoded text; see [`decode_legacy`].
///
/// A BOM-less file that is neither UTF-8 nor binary is never refused: the
/// Windows-1252 fallback maps every byte, so the worst case is mojibake that
/// still saves back to the original bytes.
pub(crate) fn decode(bytes: &[u8]) -> Option<Decoded> {
    let (text, encoding, bom) = if let Some(rest) = bytes.strip_prefix(UTF8_BOM) {
        // A file that declares UTF-8 and then breaks it is damaged rather than
        // secretly in another encoding, so it is shown with replacement
        // characters instead of being second-guessed.
        (String::from_utf8_lossy(rest).into_owned(), UTF_8, true)
    } else if let Some(rest) = bytes.strip_prefix(UTF16LE_BOM) {
        (decode_utf16(UTF_16LE, rest), UTF_16LE, true)
    } else if let Some(rest) = bytes.strip_prefix(UTF16BE_BOM) {
        (decode_utf16(UTF_16BE, rest), UTF_16BE, true)
    } else if bytes[..bytes.len().min(BINARY_SNIFF_LEN)].contains(&0) {
        return None;
    } else if let Ok(text) = std::str::from_utf8(bytes) {
        (text.to_owned(), UTF_8, false)
    } else {
        let (text, encoding) = decode_legacy(bytes);
        (text, encoding, false)
    };

    let line_ending = detect_line_ending(&text);
    let text = match line_ending {
        LineEnding::CrLf => text.replace("\r\n", "\n"),
        LineEnding::Lf => text,
    };
    Some(Decoded {
        text,
        format: TextFormat {
            encoding,
            bom,
            line_ending,
        },
    })
}

/// Decodes the body of a UTF-16 file. An odd trailing byte or an unpaired
/// surrogate is replaced rather than refused: the BOM already told us this is
/// UTF-16 text, and showing it damaged beats not showing it. Such a file will
/// not save back byte-for-byte, which is the price of it being malformed.
fn decode_utf16(encoding: &'static Encoding, body: &[u8]) -> String {
    let (text, _had_errors) = encoding.decode_without_bom_handling(body);
    text.into_owned()
}

/// Decodes text that is neither UTF-8 nor marked with a BOM.
///
/// Only two candidates are tried, GB18030 and Windows-1252, and the reasoning
/// is about which mistakes are cheap:
///
/// - GB18030 (the superset of GBK and GB2312) is accepted only when all of
///   these hold: the bytes decode with no malformed sequences at all; at least
///   80% of the multi-byte characters sit in the GB2312 zone, where both bytes
///   are `0xA1..=0xFE` (with a lead of at most `0xF7`) — the zone real
///   simplified-Chinese text lives in, covering the common hanzi and full-width
///   punctuation; and re-encoding the decoded text reproduces the input
///   exactly. Latin text in a single-byte code page fails the first test
///   almost immediately, because an accented letter followed by a space, a
///   digit or punctuation is not a valid GB18030 pair. When an accented letter
///   is followed by an ASCII letter the pair *is* valid GBK, but its trail
///   byte is below `0xA1`, so the zone test rejects it. The round-trip test
///   guarantees that choosing GB18030 never costs a byte on save.
/// - Windows-1252 is the fallback because it can decode every byte and encode
///   every result back, so a misjudged file is displayed wrongly but never
///   corrupted by an unchanged save.
///
/// Shift_JIS, EUC-KR and Big5 are deliberately not guessed. EUC-KR occupies
/// the same byte zone as GB2312, and Shift_JIS/Big5 overlap GBK's pair ranges,
/// so telling them apart needs character-frequency models, not a range check.
/// A heuristic that is wrong half the time on those files is worse than a
/// predictable fallback: they open as Windows-1252 (or, for EUC-KR, as
/// GB18030 mojibake) and still save back unchanged.
fn decode_legacy(bytes: &[u8]) -> (String, &'static Encoding) {
    if let Some(text) = GB18030.decode_without_bom_handling_and_without_replacement(bytes)
        && looks_like_gb2312_text(bytes)
        && encode_legacy(&text, GB18030).is_ok_and(|encoded| encoded == bytes)
    {
        return (text.into_owned(), GB18030);
    }
    let (text, _had_errors) = WINDOWS_1252.decode_without_bom_handling(bytes);
    (text.into_owned(), WINDOWS_1252)
}

/// The GB2312-zone test described on [`decode_legacy`]. The input must
/// already be known to be well-formed GB18030, which is what makes it safe to
/// walk it by lead byte alone.
fn looks_like_gb2312_text(bytes: &[u8]) -> bool {
    let mut in_zone = 0usize;
    let mut other = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        let lead = bytes[i];
        if lead < 0x80 {
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            // `0x80` is a one-byte euro sign in encoding_rs's decoder, and
            // neither it nor `0xFF` can lead a pair.
            _ if lead == 0x80 || lead == 0xFF => {
                other += 1;
                i += 1;
            }
            // A four-byte sequence: GB18030's mapping of the rest of Unicode.
            Some(0x30..=0x39) => {
                other += 1;
                i += 4;
            }
            Some(&trail) => {
                if (0xA1..=0xF7).contains(&lead) && (0xA1..=0xFE).contains(&trail) {
                    in_zone += 1;
                } else {
                    other += 1;
                }
                i += 2;
            }
            None => {
                other += 1;
                i += 1;
            }
        }
    }
    in_zone > 0 && in_zone * 10 >= (in_zone + other) * 8
}

/// CRLF when `\r\n` breaks outnumber lone `\n` breaks; LF on a tie or when the
/// text has no breaks at all, since LF is what a new line should get by
/// default. A file with mixed endings is normalised to its majority, so an
/// unchanged save of such a file makes it consistent — the one case where
/// bytes change without an edit, and arguably the right one.
fn detect_line_ending(text: &str) -> LineEnding {
    let bytes = text.as_bytes();
    let mut crlf = 0usize;
    let mut lf = 0usize;
    for (i, _) in bytes.iter().enumerate().filter(|(_, b)| **b == b'\n') {
        if i > 0 && bytes[i - 1] == b'\r' {
            crlf += 1;
        } else {
            lf += 1;
        }
    }
    if crlf > lf {
        LineEnding::CrLf
    } else {
        LineEnding::Lf
    }
}

/// Why text could not be written in its file's encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EncodeError {
    /// The human name of the encoding, as [`TextFormat::encoding_label`]
    /// spells it without the BOM suffix.
    pub(crate) encoding: &'static str,
    /// The first character the encoding has no bytes for.
    pub(crate) unmappable: char,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "can't save as {}: contains '{}'",
            self.encoding, self.unmappable
        )
    }
}

impl std::error::Error for EncodeError {}

/// Writes editor text back in the file's original format: `\n` becomes `\r\n`
/// for a CRLF file, the BOM is restored, and the text is encoded in the
/// original encoding.
///
/// The CRLF expansion is a plain replacement. A CRLF file's text never holds a
/// `\r\n` after [`decode`], so this is exact for anything decode produced; a
/// `\r` the user typed before a newline would be written as `\r\r\n`, which is
/// also what the file would have to contain to decode to that text.
///
/// Fails, naming the first offending character, when the text holds something
/// the encoding cannot represent — an emoji typed into a Windows-1252 file,
/// say. Nothing is silently replaced, so the caller can tell the user and let
/// them choose another encoding instead.
pub(crate) fn encode(text: &str, format: &TextFormat) -> Result<Vec<u8>, EncodeError> {
    let expanded;
    let text = match format.line_ending {
        LineEnding::CrLf => {
            expanded = text.replace('\n', "\r\n");
            expanded.as_str()
        }
        LineEnding::Lf => text,
    };

    let encoding = format.encoding;
    // encoding_rs has no UTF-16 encoder (the WHATWG standard it implements only
    // ever emits UTF-8 for UTF-16 pages), so UTF-16 is written by hand. Every
    // `char` is representable, so neither UTF-16 nor UTF-8 can fail.
    if encoding == UTF_16LE || encoding == UTF_16BE {
        let little = encoding == UTF_16LE;
        let mut out = Vec::with_capacity(2 + text.len() * 2);
        if format.bom {
            out.extend_from_slice(if little { UTF16LE_BOM } else { UTF16BE_BOM });
        }
        for unit in text.encode_utf16() {
            out.extend_from_slice(&if little {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        return Ok(out);
    }
    if encoding == UTF_8 {
        let mut out = Vec::with_capacity(3 + text.len());
        if format.bom {
            out.extend_from_slice(UTF8_BOM);
        }
        out.extend_from_slice(text.as_bytes());
        return Ok(out);
    }
    encode_legacy(text, encoding).map_err(|unmappable| EncodeError {
        encoding: encoding_name(encoding),
        unmappable,
    })
}

/// Encodes with encoding_rs, refusing rather than substituting: encoding_rs's
/// convenience `encode` would write an unmappable character as an HTML numeric
/// reference like `&#128512;`, which is right for a web form and silently
/// corrupts a source file.
fn encode_legacy(text: &str, encoding: &'static Encoding) -> Result<Vec<u8>, char> {
    let mut encoder = encoding.new_encoder();
    let mut out = Vec::with_capacity(text.len());
    let mut buf = [0u8; 8 * 1024];
    let mut rest = text;
    loop {
        let (result, read, written) =
            encoder.encode_from_utf8_without_replacement(rest, &mut buf, true);
        out.extend_from_slice(&buf[..written]);
        rest = &rest[read..];
        match result {
            EncoderResult::InputEmpty => return Ok(out),
            EncoderResult::OutputFull => {}
            EncoderResult::Unmappable(c) => return Err(c),
        }
    }
}

// ---------------------------------------------------------------------------
// Indentation
// ---------------------------------------------------------------------------

/// How many lines [`detect_indent`] looks at. The top of a file is as
/// representative as the rest of it, and a bound keeps opening a huge
/// generated file instant.
const INDENT_SCAN_LINES: usize = 1000;

/// The display width of a hard tab when nothing says otherwise.
const DEFAULT_TAB_WIDTH: usize = 4;

/// How the editor indents: with hard tabs of display width `size`, or with
/// `size` spaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Indent {
    pub(crate) hard_tabs: bool,
    pub(crate) size: usize,
}

/// The indentation to use when a file offers no evidence — a new or flat file.
/// Kept conservative: only languages whose tooling *requires* a style get one.
/// Makefiles need tab-led recipes and gofmt writes tabs; YAML is written in
/// two spaces nearly universally. Everything else gets four spaces, the most
/// common single choice, rather than a per-language table of house styles
/// that would be wrong for many projects anyway — those projects say what they
/// want through `.editorconfig` or the file's existing lines.
fn default_indent(language: &str) -> Indent {
    match language {
        "make" | "go" => Indent {
            hard_tabs: true,
            size: DEFAULT_TAB_WIDTH,
        },
        "yaml" => Indent {
            hard_tabs: false,
            size: 2,
        },
        _ => Indent {
            hard_tabs: false,
            size: 4,
        },
    }
}

/// Infers a file's indentation from its first lines, falling back to
/// [`default_indent`] for `language` (a name from `language_for_path`).
///
/// Tabs versus spaces is a vote of lines by their first character. Lines led
/// by exactly one space do not vote: they are nearly always the ` * ` of a
/// block comment, which a tab-indented C file has at column zero.
///
/// The width of space indentation is the classic heuristic: every time a line
/// is indented deeper than the previous non-blank line, the increase is
/// tallied, and the most common of 2, 4 and 8 wins (ties prefer 4, then 2).
/// Only increases count, because a dedent often closes several levels at
/// once and would vote for a multiple of the real width. Other widths — the
/// odd steps of aligned continuation lines, a comment's single space — carry
/// no vote at all.
pub(crate) fn detect_indent(text: &str, language: &str) -> Indent {
    let default = default_indent(language);
    let mut tab_lines = 0usize;
    let mut space_lines = 0usize;
    // Tallies of indent increases of 2, 4 and 8 spaces.
    let mut steps = [0usize; 3];
    let mut previous = 0usize;

    for line in text.lines().take(INDENT_SCAN_LINES) {
        let content = line.trim_start_matches([' ', '\t']);
        // Blank lines say nothing, and must not reset `previous`: a block that
        // resumes after an empty line has not been re-indented from zero.
        if content.is_empty() {
            continue;
        }
        if line.starts_with('\t') {
            tab_lines += 1;
            continue;
        }
        let spaces = line.len() - line.trim_start_matches(' ').len();
        if spaces >= 2 {
            space_lines += 1;
        }
        if spaces > previous {
            match spaces - previous {
                2 => steps[0] += 1,
                4 => steps[1] += 1,
                8 => steps[2] += 1,
                _ => {}
            }
        }
        previous = spaces;
    }

    if tab_lines == 0 && space_lines == 0 {
        return default;
    }
    let hard_tabs = match tab_lines.cmp(&space_lines) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => default.hard_tabs,
    };
    if hard_tabs {
        return Indent {
            hard_tabs: true,
            size: DEFAULT_TAB_WIDTH,
        };
    }
    // In preference order, so `max_by_key` (which keeps the last maximum)
    // walks it reversed and the earliest preferred width wins a tie.
    let size = [(4, steps[1]), (2, steps[0]), (8, steps[2])]
        .into_iter()
        .rev()
        .max_by_key(|&(_, count)| count)
        .filter(|&(_, count)| count > 0)
        .map(|(size, _)| size)
        .unwrap_or(if default.hard_tabs { 4 } else { default.size });
    Indent {
        hard_tabs: false,
        size,
    }
}

// ---------------------------------------------------------------------------
// EditorConfig
// ---------------------------------------------------------------------------

/// The file name the caller looks for in each directory from the file's own
/// upward.
pub(crate) const EDITORCONFIG: &str = ".editorconfig";

/// The properties of <https://spec.editorconfig.org> the editor acts on, as
/// they apply to one file. `None` means no config said anything (or said
/// `unset`), so the editor's own detection stands.
///
/// `charset` is not read: the editor keeps the encoding a file already has,
/// and new files are UTF-8, so the property would only matter for converting
/// files, which the editor does not do on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EditorConfig {
    /// `indent_style`: `Some(true)` for `tab`, `Some(false)` for `space`.
    pub(crate) hard_tabs: Option<bool>,
    /// `indent_size`, with `tab` already resolved to the tab width.
    pub(crate) indent_size: Option<usize>,
    /// `tab_width`, defaulting to a numeric `indent_size` as the spec says.
    pub(crate) tab_width: Option<usize>,
    /// `end_of_line`. `cr` is ignored: the editor cannot write lone-CR files.
    pub(crate) end_of_line: Option<LineEnding>,
    pub(crate) insert_final_newline: Option<bool>,
    pub(crate) trim_trailing_whitespace: Option<bool>,
}

impl EditorConfig {
    /// The indentation to edit with: whatever the config sets, and the
    /// detected indentation for whatever it leaves open. For hard tabs the size
    /// is the tab's display width, so `tab_width` is preferred there.
    pub(crate) fn indent(&self, detected: Indent) -> Indent {
        let hard_tabs = self.hard_tabs.unwrap_or(detected.hard_tabs);
        let size = if hard_tabs {
            self.tab_width.or(self.indent_size)
        } else {
            self.indent_size
        };
        Indent {
            hard_tabs,
            size: size.unwrap_or(detected.size),
        }
    }
}

/// Whether an `.editorconfig` declares `root = true` in its preamble, which
/// tells the caller to stop walking up the directory tree.
pub(crate) fn is_root(contents: &str) -> bool {
    parse_editorconfig(contents).root
}

/// Resolves the properties that apply to `file`.
///
/// `configs` pairs each `.editorconfig`'s directory with its contents, ordered
/// from the file's own directory upward. Anything past the first root config
/// is ignored even if the caller passed it. Following the spec, farther
/// configs are applied first so nearer ones override them, and within one file
/// later sections override earlier ones.
///
/// Section globs are matched with globset (with `*` and `?` not crossing `/`),
/// which covers `*`, `**`, `?`, `[abc]`, `[!abc]` and `{a,b}`. A glob without
/// a `/` matches the file's name at any depth below the config's directory; a
/// glob with one is anchored to that directory. Numeric ranges `{1..3}` are
/// expanded into alternatives when they are short and not nested inside
/// another brace group. Globs globset rejects — nested braces are the usual
/// case — match nothing.
pub(crate) fn editorconfig_for(file: &Path, configs: &[(PathBuf, String)]) -> EditorConfig {
    let parsed: Vec<(&Path, ParsedConfig)> = {
        let mut parsed = Vec::new();
        for (dir, contents) in configs {
            let config = parse_editorconfig(contents);
            let root = config.root;
            parsed.push((dir.as_path(), config));
            if root {
                break;
            }
        }
        parsed
    };

    let mut properties: HashMap<String, String> = HashMap::new();
    for (dir, config) in parsed.iter().rev() {
        let Some(relative) = relative_glob_path(file, dir) else {
            continue;
        };
        for section in &config.sections {
            if section_matches(&section.glob, &relative) {
                for (key, value) in &section.properties {
                    properties.insert(key.clone(), value.clone());
                }
            }
        }
    }
    resolve_properties(&properties)
}

struct ParsedConfig {
    root: bool,
    sections: Vec<Section>,
}

struct Section {
    glob: String,
    /// Keys lowercased; values lowercased too, since every value the editor
    /// reads is case-insensitive.
    properties: Vec<(String, String)>,
}

/// A forgiving INI reader: comment lines start with `#` or `;`, a `#` later
/// in a line is part of the value (as the current spec requires), and a line
/// that is neither a section header nor `key = value` is skipped.
fn parse_editorconfig(contents: &str) -> ParsedConfig {
    let mut root = false;
    let mut sections: Vec<Section> = Vec::new();
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(glob) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.push(Section {
                glob: glob.to_owned(),
                properties: Vec::new(),
            });
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_ascii_lowercase();
        match sections.last_mut() {
            Some(section) => section.properties.push((key, value)),
            None if key == "root" => root = value == "true",
            None => {}
        }
    }
    ParsedConfig { root, sections }
}

/// `file` relative to `dir`, `/`-separated whatever the platform, or `None`
/// when the file is not below that directory.
fn relative_glob_path(file: &Path, dir: &Path) -> Option<String> {
    let relative = file.strip_prefix(dir).ok()?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn section_matches(glob: &str, relative: &str) -> bool {
    let glob = expand_numeric_ranges(glob);
    let pattern = if glob.contains('/') {
        glob.strip_prefix('/').unwrap_or(&glob).to_owned()
    } else {
        format!("**/{glob}")
    };
    globset::GlobBuilder::new(&pattern)
        .literal_separator(true)
        // Explicit because globset's default differs by platform, and
        // editorconfig globs use `\` as an escape everywhere.
        .backslash_escape(true)
        .build()
        .is_ok_and(|glob| glob.compile_matcher().is_match(relative))
}

/// The most alternatives a `{m..n}` range is expanded into. Larger ranges are
/// left alone (and so match nothing literal), which is fine for the small
/// ranges configs actually use.
const MAX_RANGE_EXPANSION: i64 = 1000;

/// Rewrites `{m..n}` as `{m,m+1,…,n}` so globset can match it. This matches the
/// spec for the integers written in their plain form, which is how file names
/// that such ranges target are spelled.
fn expand_numeric_ranges(glob: &str) -> String {
    let mut out = String::with_capacity(glob.len());
    let mut rest = glob;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let expanded = after.find('}').and_then(|close| {
            let (lo, hi) = after[..close].split_once("..")?;
            let (lo, hi): (i64, i64) = (lo.parse().ok()?, hi.parse().ok()?);
            let (lo, hi) = (lo.min(hi), lo.max(hi));
            (hi - lo < MAX_RANGE_EXPANSION).then(|| {
                let numbers: Vec<String> = (lo..=hi).map(|n| n.to_string()).collect();
                (format!("{{{}}}", numbers.join(",")), close)
            })
        });
        match expanded {
            Some((replacement, close)) => {
                out.push_str(&replacement);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn resolve_properties(properties: &HashMap<String, String>) -> EditorConfig {
    // `unset` is stored like any other value so that it overrides an earlier
    // setting, and only here does it turn back into "not set".
    let get = |key: &str| {
        properties
            .get(key)
            .map(String::as_str)
            .filter(|value| *value != "unset")
    };
    let positive = |value: &str| value.parse::<usize>().ok().filter(|n| *n > 0);
    let boolean = |key: &str| match get(key) {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    };

    let hard_tabs = match get("indent_style") {
        Some("tab") => Some(true),
        Some("space") => Some(false),
        _ => None,
    };
    let numeric_indent = get("indent_size").and_then(positive);
    let tab_width = get("tab_width").and_then(positive).or(numeric_indent);
    let indent_size = match get("indent_size") {
        Some("tab") => tab_width,
        Some(_) => numeric_indent,
        // The spec: with tab indentation and no size, the size is the tab.
        None if hard_tabs == Some(true) => tab_width,
        None => None,
    };
    let end_of_line = match get("end_of_line") {
        Some("lf") => Some(LineEnding::Lf),
        Some("crlf") => Some(LineEnding::CrLf),
        _ => None,
    };

    EditorConfig {
        hard_tabs,
        indent_size,
        tab_width,
        end_of_line,
        insert_final_newline: boolean("insert_final_newline"),
        trim_trailing_whitespace: boolean("trim_trailing_whitespace"),
    }
}

/// Applies the config's save-time rewrites to editor text, returning the new
/// text, or `None` when nothing changes (so the caller can skip touching the
/// buffer and its undo history).
///
/// Trailing whitespace means spaces and tabs only: stripping other Unicode
/// spaces (a no-break space, say) could remove something deliberate. A `\r`
/// at a line's end is kept, since it is a line break the editor preserves in
/// an LF file, not whitespace to trim.
///
/// `insert_final_newline = true` adds a `\n` to non-empty text that lacks
/// one; an empty file stays empty. `false` never removes an existing one —
/// the spec leaves that to taste and removing it would surprise more people.
pub(crate) fn apply_save_rules(text: &str, cfg: &EditorConfig) -> Option<String> {
    let mut out = if cfg.trim_trailing_whitespace == Some(true) {
        let mut trimmed = String::with_capacity(text.len());
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                trimmed.push('\n');
            }
            let (body, cr) = match line.strip_suffix('\r') {
                Some(body) => (body, "\r"),
                None => (line, ""),
            };
            trimmed.push_str(body.trim_end_matches([' ', '\t']));
            trimmed.push_str(cr);
        }
        trimmed
    } else {
        text.to_owned()
    };
    if cfg.insert_final_newline == Some(true) && !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    (out != text).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trips(bytes: &[u8]) -> Decoded {
        let decoded = decode(bytes).expect("text");
        let encoded = encode(&decoded.text, &decoded.format).expect("encodable");
        assert_eq!(encoded, bytes, "round trip of {decoded:?}");
        decoded
    }

    // --- decoding ---

    #[test]
    fn plain_utf8_is_utf8_without_bom() {
        let decoded = round_trips("héllo 世界\n".as_bytes());
        assert_eq!(decoded.text, "héllo 世界\n");
        assert_eq!(decoded.format, TextFormat::default());
        assert_eq!(decoded.format.encoding_label(), "UTF-8");
    }

    #[test]
    fn empty_file_is_utf8() {
        let decoded = round_trips(b"");
        assert_eq!(decoded.text, "");
        assert_eq!(decoded.format, TextFormat::default());
    }

    #[test]
    fn utf8_bom_is_stripped_and_restored() {
        let decoded = round_trips(b"\xEF\xBB\xBFfn main() {}\n");
        assert_eq!(decoded.text, "fn main() {}\n");
        assert_eq!(decoded.format.encoding, UTF_8);
        assert!(decoded.format.bom);
        assert_eq!(decoded.format.encoding_label(), "UTF-8 BOM");
    }

    #[test]
    fn utf16_le_and_be_boms_decode() {
        let text = "a\u{00e9}\u{4e2d}\u{1F600}\n";
        let mut le = vec![0xFF, 0xFE];
        let mut be = vec![0xFE, 0xFF];
        for unit in text.encode_utf16() {
            le.extend_from_slice(&unit.to_le_bytes());
            be.extend_from_slice(&unit.to_be_bytes());
        }
        let decoded = round_trips(&le);
        assert_eq!(decoded.text, text);
        assert_eq!(decoded.format.encoding, UTF_16LE);
        assert!(decoded.format.bom);
        assert_eq!(decoded.format.encoding_label(), "UTF-16 LE");

        let decoded = round_trips(&be);
        assert_eq!(decoded.text, text);
        assert_eq!(decoded.format.encoding, UTF_16BE);
        assert_eq!(decoded.format.encoding_label(), "UTF-16 BE");
    }

    #[test]
    fn utf16_crlf_is_normalised_and_restored() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "one\r\ntwo\r\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let decoded = round_trips(&bytes);
        assert_eq!(decoded.text, "one\ntwo\n");
        assert_eq!(decoded.format.line_ending, LineEnding::CrLf);
    }

    #[test]
    fn nul_in_the_first_8k_is_binary() {
        assert_eq!(decode(b"\x7FELF\x02\x01\x01\x00\x00"), None);
        let mut late = vec![b'a'; BINARY_SNIFF_LEN - 1];
        late.push(0);
        assert_eq!(decode(&late), None);
        // Past the sniff window a NUL no longer decides anything.
        let mut later = vec![b'a'; BINARY_SNIFF_LEN];
        later.push(0);
        assert!(decode(&later).is_some());
    }

    #[test]
    fn gb18030_chinese_text_is_recognised() {
        let text = "// 中文注释：你好，世界。\nfn main() {}\n";
        let (bytes, _, unmappable) = GB18030.encode(text);
        assert!(!unmappable);
        assert!(std::str::from_utf8(&bytes).is_err());
        let decoded = round_trips(&bytes);
        assert_eq!(decoded.text, text);
        assert_eq!(decoded.format.encoding, GB18030);
        assert_eq!(decoded.format.encoding_label(), "GB18030");
    }

    #[test]
    fn latin_text_falls_back_to_windows_1252() {
        let text = "café au lait, naïve — “quoted” €5\n";
        let (bytes, _, _) = WINDOWS_1252.encode(text);
        let decoded = round_trips(&bytes);
        assert_eq!(decoded.text, text);
        assert_eq!(decoded.format.encoding, WINDOWS_1252);
        assert_eq!(decoded.format.encoding_label(), "Windows-1252");
    }

    #[test]
    fn latin_text_that_is_valid_gbk_is_still_windows_1252() {
        // `é` followed by a letter is a well-formed GBK pair, so GB18030
        // decodes this without error; the zone test must reject it.
        let (bytes, _, _) = WINDOWS_1252.encode("résumés");
        assert!(
            GB18030
                .decode_without_bom_handling_and_without_replacement(&bytes)
                .is_some()
        );
        let decoded = round_trips(&bytes);
        assert_eq!(decoded.text, "résumés");
        assert_eq!(decoded.format.encoding, WINDOWS_1252);
    }

    #[test]
    fn every_byte_survives_the_windows_1252_fallback() {
        // Includes 0x81, 0x8D, 0x8F, 0x90 and 0x9D, which Windows-1252 leaves
        // undefined and encoding_rs maps to C1 controls.
        let bytes: Vec<u8> = (1..=255u8).collect();
        let decoded = round_trips(&bytes);
        assert_eq!(decoded.format.encoding, WINDOWS_1252);
    }

    // --- line endings ---

    #[test]
    fn crlf_is_normalised_and_restored() {
        let decoded = round_trips(b"a\r\nb\r\n\r\nc");
        assert_eq!(decoded.text, "a\nb\n\nc");
        assert_eq!(decoded.format.line_ending, LineEnding::CrLf);
        assert_eq!(decoded.format.line_ending.label(), "CRLF");
    }

    #[test]
    fn mixed_endings_follow_the_majority() {
        let decoded = decode(b"a\r\nb\r\nc\nd").unwrap();
        assert_eq!(decoded.format.line_ending, LineEnding::CrLf);
        assert_eq!(decoded.text, "a\nb\nc\nd");
        // Saving makes the minority line consistent.
        assert_eq!(
            encode(&decoded.text, &decoded.format).unwrap(),
            b"a\r\nb\r\nc\r\nd"
        );

        let decoded = round_trips(b"a\nb\nc\r\nd");
        assert_eq!(decoded.format.line_ending, LineEnding::Lf);
        assert_eq!(decoded.text, "a\nb\nc\r\nd");
    }

    #[test]
    fn ties_and_no_breaks_are_lf() {
        assert_eq!(round_trips(b"a\r\nb\n").format.line_ending, LineEnding::Lf);
        assert_eq!(round_trips(b"one line").format.line_ending, LineEnding::Lf);
    }

    #[test]
    fn lone_cr_is_left_alone() {
        let decoded = round_trips(b"old\rmac\r");
        assert_eq!(decoded.text, "old\rmac\r");
        assert_eq!(decoded.format.line_ending, LineEnding::Lf);
        // A stray CR before a CRLF survives a CRLF round trip.
        let decoded = round_trips(b"a\r\r\nb\r\n");
        assert_eq!(decoded.text, "a\r\nb\n");
    }

    #[test]
    fn crlf_gb18030_round_trips() {
        let (bytes, _, _) = GB18030.encode("第一行\r\n第二行\r\n");
        let decoded = round_trips(&bytes);
        assert_eq!(decoded.text, "第一行\n第二行\n");
        assert_eq!(decoded.format.encoding, GB18030);
        assert_eq!(decoded.format.line_ending, LineEnding::CrLf);
    }

    // --- encoding ---

    #[test]
    fn unrepresentable_character_is_named() {
        let format = TextFormat {
            encoding: WINDOWS_1252,
            ..TextFormat::default()
        };
        let err = encode("ok é then 中 and 😀", &format).unwrap_err();
        assert_eq!(
            err,
            EncodeError {
                encoding: "Windows-1252",
                unmappable: '中'
            }
        );
        assert_eq!(err.to_string(), "can't save as Windows-1252: contains '中'");
    }

    #[test]
    fn gb18030_encodes_all_of_unicode() {
        let format = TextFormat {
            encoding: GB18030,
            ..TextFormat::default()
        };
        // Mostly hanzi, so the decode heuristic still recognises it with a
        // four-byte emoji in the mix.
        let bytes = encode("中文测试 😀 é", &format).unwrap();
        assert_eq!(decode(&bytes).unwrap().text, "中文测试 😀 é");
    }

    #[test]
    fn new_text_encodes_with_the_chosen_format() {
        let format = TextFormat {
            encoding: UTF_8,
            bom: true,
            line_ending: LineEnding::CrLf,
        };
        assert_eq!(encode("a\nb", &format).unwrap(), b"\xEF\xBB\xBFa\r\nb");
    }

    // --- indentation ---

    #[test]
    fn tab_indented_file_uses_tabs() {
        let text = "fn a() {\n\tif x {\n\t\ty();\n\t}\n}\n";
        assert_eq!(
            detect_indent(text, "rust"),
            Indent {
                hard_tabs: true,
                size: 4
            }
        );
    }

    #[test]
    fn two_space_file() {
        let text = "a:\n  b:\n    c: 1\n    d: 2\n  e:\n    f: 3\n";
        assert_eq!(
            detect_indent(text, "rust"),
            Indent {
                hard_tabs: false,
                size: 2
            }
        );
    }

    #[test]
    fn four_space_file_with_deep_dedents_and_blank_lines() {
        let text = "def f():\n    if x:\n        if y:\n\n            z()\n    return 1\n\ndef g():\n    pass\n";
        assert_eq!(
            detect_indent(text, "python"),
            Indent {
                hard_tabs: false,
                size: 4
            }
        );
    }

    #[test]
    fn block_comment_spaces_do_not_outvote_tabs() {
        let text = "/**\n * doc\n * more\n */\nint f() {\n\treturn 0;\n}\n";
        assert!(detect_indent(text, "c").hard_tabs);
    }

    #[test]
    fn no_evidence_uses_the_language_default() {
        let flat = "a\nb\n\nc\n";
        let tabs = Indent {
            hard_tabs: true,
            size: 4,
        };
        assert_eq!(detect_indent(flat, "make"), tabs);
        assert_eq!(detect_indent(flat, "go"), tabs);
        assert_eq!(
            detect_indent(flat, "yaml"),
            Indent {
                hard_tabs: false,
                size: 2
            }
        );
        assert_eq!(
            detect_indent("", "rust"),
            Indent {
                hard_tabs: false,
                size: 4
            }
        );
    }

    #[test]
    fn spaces_without_a_recognised_step_fall_back_to_four() {
        // Aligned continuation lines only: a step of 3 votes for nothing.
        let text = "foo(a,\n    b)\n".replace("    ", "   ");
        assert_eq!(
            detect_indent(&text, "go"),
            Indent {
                hard_tabs: false,
                size: 4
            }
        );
    }

    // --- editorconfig ---

    fn configs(list: &[(&str, &str)]) -> Vec<(PathBuf, String)> {
        list.iter()
            .map(|(dir, contents)| (PathBuf::from(dir), contents.to_string()))
            .collect()
    }

    #[test]
    fn basename_glob_matches_at_any_depth() {
        let cfgs = configs(&[("/p", "[*.rs]\nindent_style = space\nindent_size = 2\n")]);
        let cfg = editorconfig_for(Path::new("/p/src/deep/main.rs"), &cfgs);
        assert_eq!(cfg.hard_tabs, Some(false));
        assert_eq!(cfg.indent_size, Some(2));
        assert_eq!(cfg.tab_width, Some(2));
        let other = editorconfig_for(Path::new("/p/src/main.go"), &cfgs);
        assert_eq!(other, EditorConfig::default());
        // Not below the config's directory at all.
        let outside = editorconfig_for(Path::new("/q/main.rs"), &cfgs);
        assert_eq!(outside, EditorConfig::default());
    }

    #[test]
    fn glob_with_a_slash_is_anchored() {
        let cfgs = configs(&[(
            "/p",
            "[src/*.rs]\nindent_size = 3\n[/lib/**]\nend_of_line = crlf\n",
        )]);
        assert_eq!(
            editorconfig_for(Path::new("/p/src/a.rs"), &cfgs).indent_size,
            Some(3)
        );
        // `*` does not cross a separator, and the anchor is the config's dir.
        assert_eq!(
            editorconfig_for(Path::new("/p/src/x/a.rs"), &cfgs).indent_size,
            None
        );
        assert_eq!(
            editorconfig_for(Path::new("/p/sub/src/a.rs"), &cfgs).indent_size,
            None
        );
        assert_eq!(
            editorconfig_for(Path::new("/p/lib/a/b/c.txt"), &cfgs).end_of_line,
            Some(LineEnding::CrLf)
        );
        assert_eq!(
            editorconfig_for(Path::new("/p/x/lib/c.txt"), &cfgs).end_of_line,
            None
        );
    }

    #[test]
    fn braces_classes_and_ranges() {
        let cfgs = configs(&[(
            "/p",
            "[*.{js,ts}]\nindent_size = 2\n[file[0-9].c]\ntab_width = 8\n[v{1..3}.txt]\ninsert_final_newline = true\n[?.md]\ntrim_trailing_whitespace = false\n",
        )]);
        let at = |p: &str| editorconfig_for(Path::new(p), &cfgs);
        assert_eq!(at("/p/a.ts").indent_size, Some(2));
        assert_eq!(at("/p/web/a.js").indent_size, Some(2));
        assert_eq!(at("/p/a.rs").indent_size, None);
        assert_eq!(at("/p/file7.c").tab_width, Some(8));
        assert_eq!(at("/p/fileX.c").tab_width, None);
        assert_eq!(at("/p/v2.txt").insert_final_newline, Some(true));
        assert_eq!(at("/p/v4.txt").insert_final_newline, None);
        assert_eq!(at("/p/a.md").trim_trailing_whitespace, Some(false));
        assert_eq!(at("/p/ab.md").trim_trailing_whitespace, None);
    }

    #[test]
    fn double_star_crosses_directories() {
        let cfgs = configs(&[("/p", "[docs/**/*.md]\nindent_size = 2\n")]);
        let at = |p: &str| editorconfig_for(Path::new(p), &cfgs).indent_size;
        assert_eq!(at("/p/docs/a.md"), Some(2));
        assert_eq!(at("/p/docs/x/y/a.md"), Some(2));
        assert_eq!(at("/p/other/a.md"), None);
    }

    #[test]
    fn nearer_configs_override_and_root_stops_the_walk() {
        let cfgs = configs(&[
            ("/p/sub", "[*]\nindent_size = 8\n"),
            (
                "/p",
                "root = true\n[*]\nindent_style = space\nindent_size = 2\n",
            ),
            (
                "/",
                "[*]\nend_of_line = crlf\ninsert_final_newline = true\n",
            ),
        ]);
        let cfg = editorconfig_for(Path::new("/p/sub/a.txt"), &cfgs);
        assert_eq!(cfg.indent_size, Some(8));
        assert_eq!(cfg.hard_tabs, Some(false));
        // The config above the root one is ignored.
        assert_eq!(cfg.end_of_line, None);
        assert_eq!(cfg.insert_final_newline, None);
    }

    #[test]
    fn later_sections_override_earlier_ones() {
        let cfgs = configs(&[("/p", "[*]\nindent_size = 4\n[*.py]\nindent_size = 2\n")]);
        assert_eq!(
            editorconfig_for(Path::new("/p/a.py"), &cfgs).indent_size,
            Some(2)
        );
    }

    #[test]
    fn is_root_reads_only_the_preamble() {
        assert!(is_root("# top\nROOT = TRUE\n[*]\nindent_size = 2\n"));
        assert!(!is_root("root = false\n"));
        assert!(!is_root("[*]\nroot = true\n"));
        assert!(!is_root(""));
    }

    #[test]
    fn indent_size_tab_uses_tab_width() {
        let cfgs = configs(&[(
            "/p",
            "[*]\nindent_style = Tab\nindent_size = tab\ntab_width = 6\n",
        )]);
        let cfg = editorconfig_for(Path::new("/p/Makefile"), &cfgs);
        assert_eq!(cfg.hard_tabs, Some(true));
        assert_eq!(cfg.indent_size, Some(6));
        assert_eq!(cfg.tab_width, Some(6));

        // With tab indentation and no size, the size is the tab width.
        let cfgs = configs(&[("/p", "[*]\nindent_style = tab\ntab_width = 3\n")]);
        let cfg = editorconfig_for(Path::new("/p/a.c"), &cfgs);
        assert_eq!(cfg.indent_size, Some(3));
    }

    #[test]
    fn unset_clears_an_inherited_value() {
        let cfgs = configs(&[
            (
                "/p/sub",
                "[*.rs]\nindent_size = unset\nend_of_line = UNSET\n",
            ),
            ("/p", "[*]\nindent_size = 2\nend_of_line = crlf\n"),
        ]);
        let cfg = editorconfig_for(Path::new("/p/sub/a.rs"), &cfgs);
        assert_eq!(cfg.indent_size, None);
        assert_eq!(cfg.end_of_line, None);
    }

    #[test]
    fn unknown_keys_and_bad_values_are_ignored() {
        let cfgs = configs(&[(
            "/p",
            "; comment\n[*]\nmax_line_length = 80\nindent_size = zero\nend_of_line = cr\ninsert_final_newline = maybe\n",
        )]);
        assert_eq!(
            editorconfig_for(Path::new("/p/a"), &cfgs),
            EditorConfig::default()
        );
    }

    #[test]
    fn config_overrides_detected_indent() {
        let detected = Indent {
            hard_tabs: false,
            size: 2,
        };
        assert_eq!(EditorConfig::default().indent(detected), detected);
        let tabs = EditorConfig {
            hard_tabs: Some(true),
            tab_width: Some(8),
            indent_size: Some(4),
            ..EditorConfig::default()
        };
        assert_eq!(
            tabs.indent(detected),
            Indent {
                hard_tabs: true,
                size: 8
            }
        );
        let size_only = EditorConfig {
            indent_size: Some(4),
            ..EditorConfig::default()
        };
        assert_eq!(
            size_only.indent(detected),
            Indent {
                hard_tabs: false,
                size: 4
            }
        );
    }

    // --- save rules ---

    #[test]
    fn save_rules_trim_and_add_final_newline() {
        let both = EditorConfig {
            trim_trailing_whitespace: Some(true),
            insert_final_newline: Some(true),
            ..EditorConfig::default()
        };
        assert_eq!(
            apply_save_rules("a  \nb\t\n  c   ", &both).as_deref(),
            Some("a\nb\n  c\n")
        );
        // A CR kept by an LF file stays put; whitespace before it goes.
        assert_eq!(
            apply_save_rules("a \r\nb\n", &both).as_deref(),
            Some("a\r\nb\n")
        );
        assert_eq!(apply_save_rules("clean\n", &both), None);
        assert_eq!(apply_save_rules("", &both), None);
    }

    #[test]
    fn save_rules_respect_false_and_unset() {
        let off = EditorConfig {
            trim_trailing_whitespace: Some(false),
            insert_final_newline: Some(false),
            ..EditorConfig::default()
        };
        assert_eq!(apply_save_rules("a  \nb", &off), None);
        assert_eq!(apply_save_rules("a  \n", &EditorConfig::default()), None);
        let add_only = EditorConfig {
            insert_final_newline: Some(true),
            ..EditorConfig::default()
        };
        assert_eq!(apply_save_rules("a  ", &add_only).as_deref(), Some("a  \n"));
    }
}

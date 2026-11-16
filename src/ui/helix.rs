//! Shared Helix modal editing state machine, selection-first motions, text
//! objects, surround operations, register store, and `:command` parser.
//!
//! Used by the Code Editor (`ui::code_editor`), Inline Prompt Editor
//! (`terminal::cmd_editor`), Agent Composer (`terminal::composer`), and
//! read-only views (`ui::diff_overlay`, Markdown Preview).

use std::collections::HashMap;
use std::ops::Range;

use gpui::Keystroke;
#[cfg(test)]
use gpui::Modifiers;

/// Primary modal state of a Helix-enabled surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HelixMode {
    #[default]
    Normal,
    Insert,
    Select,
}

impl HelixMode {
    /// Three-letter badge shown in the status bar and composer pill.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Normal => "NOR",
            Self::Insert => "INS",
            Self::Select => "SEL",
        }
    }

    /// Whether the caret is drawn as a solid block (`Normal` / `Select`)
    /// rather than a vertical bar (`Insert`).
    pub fn is_block_cursor(self) -> bool {
        matches!(self, Self::Normal | Self::Select)
    }
}

/// Byte range `[head..next]` covered by the Helix block cursor in a `Rope`
/// buffer. Clamps `raw_head` to a valid UTF-8 character boundary and advances
/// over at most one non-newline code point without slicing fixed byte windows.
pub fn block_cursor_byte_range(
    text: &gpui_component::input::Rope,
    raw_head: usize,
) -> Range<usize> {
    use gpui_component::input::RopeExt as _;
    let len = text.len();
    let head = text.floor_char_boundary(raw_head.min(len));
    let next = match (head < len).then(|| text.char_at(head)).flatten() {
        Some(ch) if ch != '\n' && ch != '\r' => (head + ch.len_utf8()).min(len),
        _ => head,
    };
    head..next
}

/// Pending prefix mode entered from `Normal` or `Select` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinorMode {
    /// `g` — Goto locations, buffers, or LSP symbols.
    Goto,
    /// `m` — Match brackets, text objects (`mi`/`ma`), or surround (`ms`/`md`/`mr`).
    Match,
    /// `mi` — Select inside text object (`w`, `W`, `()`, `[]`, `{}`, `<>`, quotes, `p`).
    MatchInside,
    /// `ma` — Select around text object (`w`, `W`, `()`, `[]`, `{}`, `<>`, quotes, `p`).
    MatchAround,
    /// `ms` — Wrap selection with surrounding character pair.
    SurroundAdd,
    /// `md` — Delete surrounding character pair.
    SurroundDelete,
    /// `mr` — Replace surrounding character pair (`None` awaiting `<from>`, `Some(ch)` awaiting `<to>`).
    SurroundReplace(Option<char>),
    /// `z` — View scroll alignment (`zz`, `zt`, `zb`).
    View,
    /// `Space` — Workspace pickers, LSP actions, and system clipboard shortcuts.
    Space,
    /// `Ctrl-w` / `Space w` — Window / split navigation and management.
    Window,
    /// `"` — Select register for the next yank, delete, change, or paste.
    Register,
    /// `f` / `t` / `F` / `T` — Find or till character on or across lines.
    FindChar { forward: bool, till: bool },
    /// `r` — Replace every character in selection with the next typed character.
    ReplaceChar,
    /// `[` — Previous navigation (`[c` git hunk, `[b` buffer).
    BracketLeft,
    /// `]` — Next navigation (`]c` git hunk, `]b` buffer).
    BracketRight,
}

impl MinorMode {
    /// Header title for the bottom-right Minor Mode Key Hints Popup.
    pub fn title(self) -> &'static str {
        match self {
            Self::Goto => "Goto (g)",
            Self::Match => "Match (m)",
            Self::MatchInside => "Match Inside (mi)",
            Self::MatchAround => "Match Around (ma)",
            Self::SurroundAdd => "Surround Add (ms)",
            Self::SurroundDelete => "Surround Delete (md)",
            Self::SurroundReplace(None) => "Surround Replace — from (mr)",
            Self::SurroundReplace(Some(_)) => "Surround Replace — to (mr)",
            Self::View => "View (z)",
            Self::Space => "Space",
            Self::Window => "Window (Ctrl-w)",
            Self::Register => "Register (\")",
            Self::FindChar {
                forward: true,
                till: false,
            } => "Find Char (f)",
            Self::FindChar {
                forward: true,
                till: true,
            } => "Till Char (t)",
            Self::FindChar {
                forward: false,
                till: false,
            } => "Find Char Backward (F)",
            Self::FindChar {
                forward: false,
                till: true,
            } => "Till Char Backward (T)",
            Self::ReplaceChar => "Replace Char (r)",
            Self::BracketLeft => "Previous ([)",
            Self::BracketRight => "Next (])",
        }
    }

    /// Compact key prefix displayed in the status bar while waiting for the next key.
    pub fn key_prefix(self) -> String {
        match self {
            Self::Goto => "g".to_string(),
            Self::Match => "m".to_string(),
            Self::MatchInside => "mi".to_string(),
            Self::MatchAround => "ma".to_string(),
            Self::SurroundAdd => "ms".to_string(),
            Self::SurroundDelete => "md".to_string(),
            Self::SurroundReplace(None) => "mr".to_string(),
            Self::SurroundReplace(Some(ch)) => format!("mr{ch}"),
            Self::View => "z".to_string(),
            Self::Space => "SPC".to_string(),
            Self::Window => "C-w".to_string(),
            Self::Register => "\"".to_string(),
            Self::FindChar {
                forward: true,
                till: false,
            } => "f".to_string(),
            Self::FindChar {
                forward: true,
                till: true,
            } => "t".to_string(),
            Self::FindChar {
                forward: false,
                till: false,
            } => "F".to_string(),
            Self::FindChar {
                forward: false,
                till: true,
            } => "T".to_string(),
            Self::ReplaceChar => "r".to_string(),
            Self::BracketLeft => "[".to_string(),
            Self::BracketRight => "]".to_string(),
        }
    }

    /// Key-description rows for the floating bottom-right Minor Mode Key Hints Popup.
    pub fn hints(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Goto => &[
                ("g", "Go to file start / line N"),
                ("e", "Go to file end"),
                ("h", "Go to line start"),
                ("l", "Go to line end"),
                ("s", "First non-whitespace char"),
                ("d", "Go to definition (LSP)"),
                ("r", "Find references (LSP)"),
                ("n / p", "Next / previous buffer"),
                ("c", "Toggle line comment"),
            ],
            Self::Match => &[
                ("m", "Go to matching bracket"),
                ("i", "Select inside object…"),
                ("a", "Select around object…"),
                ("s", "Surround selection with pair…"),
                ("d", "Delete surrounding pair…"),
                ("r", "Replace surrounding pair…"),
            ],
            Self::MatchInside | Self::MatchAround => &[
                ("w / W", "Word / WORD"),
                ("( ) / b", "Parentheses"),
                ("[ ]", "Square brackets"),
                ("{ } / B", "Curly braces"),
                ("< >", "Angle brackets"),
                ("\" ' `", "Quotes / backticks"),
                ("p", "Paragraph"),
            ],
            Self::SurroundAdd => &[
                ("( ) [ ] { } < >", "Wrap with bracket pair"),
                ("\" ' ` <char>", "Wrap with delimiter"),
            ],
            Self::SurroundDelete => &[
                ("( ) [ ] { } < >", "Remove enclosing brackets"),
                ("\" ' ` <char>", "Remove enclosing delimiter"),
            ],
            Self::SurroundReplace(None) => &[("<from>", "Delimiter to replace")],
            Self::SurroundReplace(Some(_)) => &[("<to>", "New replacement delimiter")],
            Self::View => &[
                ("z", "Center cursor line in viewport"),
                ("t", "Align cursor line to top"),
                ("b", "Align cursor line to bottom"),
            ],
            Self::Space => &[
                ("f", "Open file picker"),
                ("/", "Global project search"),
                ("s", "Go to symbol in file"),
                ("r", "Rename symbol (LSP)"),
                ("a", "Code actions (LSP)"),
                ("d", "Toggle diagnostics panel"),
                ("y / p", "Yank / paste system clipboard"),
                ("w", "Window / split mode"),
            ],
            Self::Window => &[
                ("w / h / l", "Focus other editor split / pane"),
                ("v / s", "Split editor right"),
                ("q", "Close active buffer / split"),
            ],
            Self::Register => &[
                ("a-z", "Named register (A-Z appends)"),
                ("\"", "Default yank register"),
                ("+ / *", "System clipboard"),
                ("_", "Blackhole register (no yank)"),
            ],
            Self::FindChar { .. } => &[("<char>", "Target character")],
            Self::ReplaceChar => &[("<char>", "Replacement character")],
            Self::BracketLeft => &[("c", "Previous git change"), ("b", "Previous buffer")],
            Self::BracketRight => &[("c", "Next git change"), ("b", "Next buffer")],
        }
    }
}

/// Shared register store across Helix surfaces.
#[derive(Debug, Clone, Default)]
pub struct RegisterStore {
    pub default: String,
    pub named: HashMap<char, String>,
}

#[cfg(not(test))]
fn global_registers() -> &'static std::sync::Mutex<RegisterStore> {
    static GLOBAL: std::sync::OnceLock<std::sync::Mutex<RegisterStore>> =
        std::sync::OnceLock::new();
    GLOBAL.get_or_init(|| std::sync::Mutex::new(RegisterStore::default()))
}

impl RegisterStore {
    /// Read the contents of `reg` (defaulting to `"`), using `system_clipboard`
    /// when `reg` is `'+'`, `'*'`, or `'"'`.
    pub fn read(&self, reg: Option<char>, system_clipboard: Option<&str>) -> String {
        match reg.unwrap_or('"') {
            '_' => String::new(),
            '+' | '*' | '"' => {
                if let Some(clip) = system_clipboard.filter(|s| !s.is_empty()) {
                    return clip.to_owned();
                }
                if !self.default.is_empty() {
                    return self.default.clone();
                }
                #[cfg(not(test))]
                if let Ok(g) = global_registers().lock()
                    && !g.default.is_empty()
                {
                    return g.default.clone();
                }
                String::new()
            }
            c if c.is_ascii_alphabetic() => {
                let key = c.to_ascii_lowercase();
                if let Some(val) = self.named.get(&key) {
                    return val.clone();
                }
                #[cfg(not(test))]
                if let Ok(g) = global_registers().lock()
                    && let Some(val) = g.named.get(&key)
                {
                    return val.clone();
                }
                String::new()
            }
            _ => self.default.clone(),
        }
    }

    /// Write `text` into `reg` (defaulting to `"`).
    ///
    /// Returns `Some(text)` when the caller should also write the string to the
    /// OS clipboard (default `"`, `'+'`, and `'*'` registers).
    pub fn write(&mut self, reg: Option<char>, text: String) -> Option<String> {
        if text.is_empty() {
            return None;
        }
        match reg.unwrap_or('"') {
            '_' => None,
            '+' | '*' | '"' => {
                self.default = text.clone();
                #[cfg(not(test))]
                if let Ok(mut g) = global_registers().lock() {
                    g.default = text.clone();
                }
                Some(text)
            }
            c if c.is_ascii_lowercase() => {
                self.named.insert(c, text.clone());
                #[cfg(not(test))]
                if let Ok(mut g) = global_registers().lock() {
                    g.named.insert(c, text);
                }
                None
            }
            c if c.is_ascii_uppercase() => {
                let key = c.to_ascii_lowercase();
                self.named.entry(key).or_default().push_str(&text);
                #[cfg(not(test))]
                if let Ok(mut g) = global_registers().lock() {
                    g.named.entry(key).or_default().push_str(&text);
                }
                None
            }
            _ => {
                self.default = text.clone();
                #[cfg(not(test))]
                if let Ok(mut g) = global_registers().lock() {
                    g.default = text.clone();
                }
                Some(text)
            }
        }
    }
}

/// Per-surface Helix modal state.
#[derive(Debug, Clone)]
pub struct HelixState {
    pub mode: HelixMode,
    pub count: Option<usize>,
    pub minor: Option<MinorMode>,
    pub selected_register: Option<char>,
    /// Selection anchor in UTF-8 byte offset.
    pub anchor: usize,
    /// Selection head (cursor) in UTF-8 byte offset.
    pub head: usize,
    /// Additional selections `(anchor, head)` for multi-cursor operations (`s`, `C`).
    pub extra_selections: Vec<(usize, usize)>,
    /// Current `/` or `*` search query.
    pub search_query: String,
    pub registers: RegisterStore,
}

impl Default for HelixState {
    fn default() -> Self {
        Self::new(HelixMode::Normal)
    }
}

impl HelixState {
    pub fn new(initial_mode: HelixMode) -> Self {
        Self {
            mode: initial_mode,
            count: None,
            minor: None,
            selected_register: None,
            anchor: 0,
            head: 0,
            extra_selections: Vec::new(),
            search_query: String::new(),
            registers: RegisterStore::default(),
        }
    }

    /// Clear transient minor mode, count, and register selection.
    pub fn clear_pending(&mut self) {
        self.count = None;
        self.minor = None;
        self.selected_register = None;
    }

    /// Reset modal state to `mode` and clear selection / pending prefixes.
    pub fn reset(&mut self, mode: HelixMode) {
        self.mode = mode;
        self.clear_pending();
        self.anchor = 0;
        self.head = 0;
        self.extra_selections.clear();
    }

    /// Set `(anchor, head)` byte offsets directly.
    pub fn set_selection(&mut self, anchor: usize, head: usize) {
        self.anchor = anchor;
        self.head = head;
    }

    /// Whether a count, register, or minor mode is waiting for another key.
    pub fn has_pending(&self) -> bool {
        self.count.is_some() || self.minor.is_some() || self.selected_register.is_some()
    }

    /// Pending sequence string for status bar display (e.g. `"3g"`, `"\"a"`, `"mi"`).
    pub fn pending_status(&self) -> String {
        let mut out = String::new();
        if let Some(c) = self.count {
            out.push_str(&c.to_string());
        }
        if let Some(reg) = self.selected_register {
            out.push('"');
            out.push(reg);
        }
        if let Some(minor) = self.minor {
            out.push_str(&minor.key_prefix());
        }
        out
    }

    /// Pending sequence string as `Option<String>` when non-empty.
    pub fn pending_display(&self) -> Option<String> {
        let s = self.pending_status();
        (!s.is_empty()).then_some(s)
    }

    /// Consume the accumulated count, defaulting to 1.
    pub fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1).max(1)
    }

    /// Sync `anchor` and `head` from the underlying editor's byte cursor and selection.
    pub fn sync_from_editor(&mut self, cursor: usize, selected: Range<usize>, text: &str) {
        let len = text.len();
        let start = clamp_char_boundary(text, selected.start.min(len));
        let end = clamp_char_boundary(text, selected.end.min(len));
        let cur = clamp_char_boundary(text, cursor.min(len));
        if start == end {
            self.anchor = cur;
            self.head = cur;
        } else if cur == start {
            self.anchor = end;
            self.head = start;
        } else {
            self.anchor = start;
            self.head = end;
        }
    }

    /// Ordered byte range of `anchor..head` (empty when `anchor == head`).
    pub fn selection_range(&self, text: &str) -> Range<usize> {
        let a = clamp_char_boundary(text, self.anchor);
        let h = clamp_char_boundary(text, self.head);
        a.min(h)..a.max(h)
    }

    /// Effective byte range operated on by Helix actions (`d`, `c`, `y`, `r`, `~`, etc.).
    ///
    /// In Helix, a bare cursor (`anchor == head`) selects the single character
    /// under `head` (or is empty at EOF).
    pub fn effective_range(&self, text: &str) -> Range<usize> {
        let a = clamp_char_boundary(text, self.anchor);
        let h = clamp_char_boundary(text, self.head);
        if a != h {
            a.min(h)..a.max(h)
        } else {
            h..next_char_offset(text, h)
        }
    }

    /// Move `head` to `new_head`, collapsing `anchor` in `Normal` mode or
    /// preserving `anchor` in `Select` mode.
    pub fn move_cursor(&mut self, text: &str, new_head: usize) {
        let pos = clamp_char_boundary(text, new_head);
        self.head = pos;
        if self.mode != HelixMode::Select {
            self.anchor = pos;
        }
    }

    /// Set selection span from `new_anchor` to `new_head` (used by word/find motions
    /// which select the traversed span in `Normal` mode and extend in `Select` mode).
    pub fn select_span(&mut self, text: &str, new_anchor: usize, new_head: usize) {
        let a = clamp_char_boundary(text, new_anchor);
        let h = clamp_char_boundary(text, new_head);
        if self.mode == HelixMode::Select {
            self.head = h;
        } else {
            self.anchor = a;
            self.head = h;
        }
    }
}

/// High-level outcome of processing a keystroke in Helix mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelixOutcome {
    /// Key was not handled by Helix (e.g. in `Insert` mode, let normal input process it).
    Ignored,
    /// Key updated internal Helix state / selection only; sync cursor/selection to view.
    Handled,
    /// Replace `range` (UTF-8 byte offsets) in the buffer with `new_text`, and
    /// place selection at `(new_anchor, new_head)`.
    Edit {
        range: Range<usize>,
        new_text: String,
        new_anchor: usize,
        new_head: usize,
        yank_to_clipboard: Option<String>,
    },
    /// Copy `String` to the OS clipboard (from `y` / `Space y`), without editing text.
    Yank { clipboard: Option<String> },
    /// Execute a host editor / workspace command.
    Host(HelixHostCommand),
}

/// Host-level commands triggered by Helix keys (`g`, `z`, `Space`, `Ctrl-w`, `:`, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelixHostCommand {
    Undo,
    Redo,
    ToggleComment,
    /// `zz` (Center), `zt` (Top), `zb` (Bottom).
    AlignView(ViewAlign),
    /// `Ctrl-d` / `Ctrl-u` / `Ctrl-f` / `Ctrl-b`: scroll + move cursor by `lines` (signed).
    ScrollPages {
        half_pages: isize,
    },
    /// `gd` — Go to definition.
    GoToDefinition,
    /// `gr` — Find references.
    FindReferences,
    /// `gn` / `gp` — Next / previous open buffer.
    CycleBuffer {
        next: bool,
    },
    /// `]c` / `[c` — Next / previous git change hunk.
    StepGitChange {
        next: bool,
    },
    /// `Ctrl-o` / `Ctrl-i` — Jump list back / forward.
    JumpHistory {
        forward: bool,
    },
    /// `Space f` — Quick open file picker.
    QuickOpenFile,
    /// `Space /` — Global project search (or reverse history search in `CmdEditor`).
    GlobalSearch,
    /// `Space s` — Symbol outline picker.
    GoToSymbol,
    /// `Space r` — LSP Rename symbol.
    RenameSymbol,
    /// `Space a` — LSP Code actions.
    CodeActions,
    /// `Space d` — Toggle diagnostics/problems panel.
    ToggleDiagnostics,
    /// `Ctrl-w w` / `Ctrl-w h` / `Ctrl-w l` — Switch split group or focus pane.
    WindowFocusOther,
    /// `Ctrl-w v` / `Ctrl-w s` — Split editor right.
    WindowSplitRight,
    /// `Ctrl-w q` — Close active buffer / split.
    WindowCloseActive,
    /// `:` — Open the Command Bar in the Code Editor.
    OpenCommandBar,
    /// `/` — Open the in-buffer search bar.
    OpenSearchBar,
    /// `s` — Open the split-selection regex prompt bar.
    OpenSplitSelectionBar,
    /// `Enter` in Normal mode of Prompt / Composer: submit the command/message.
    Submit,
    /// `Escape` in Normal mode of Composer / Read-only view when nothing is selected/pending.
    DismissSurface,
}

pub use HelixHostCommand as HostAction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewAlign {
    Center,
    Top,
    Bottom,
}

/// Parsed `:command` from the Code Editor's Command Bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandBarAction {
    Empty,
    Write(Option<String>),
    Quit { force: bool },
    WriteQuit { force: bool, path: Option<String> },
    WriteAll,
    QuitAll { force: bool },
    WriteQuitAll,
    Reload,
    Edit(String),
    VSplit(Option<String>),
    GoToLine(usize),
    Unknown(String),
}

/// Parse a `:command` string (with or without a leading `:`).
pub fn parse_command_bar(raw: &str) -> CommandBarAction {
    let trimmed = raw.trim().strip_prefix(':').unwrap_or(raw.trim()).trim();
    if trimmed.is_empty() {
        return CommandBarAction::Empty;
    }
    if let Ok(line) = trimmed.parse::<usize>() {
        return CommandBarAction::GoToLine(line.max(1));
    }
    let (cmd, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((c, r)) => (c, Some(r.trim().to_string()).filter(|s| !s.is_empty())),
        None => (trimmed, None),
    };
    match cmd {
        "w" | "write" => CommandBarAction::Write(rest),
        "q" | "quit" => CommandBarAction::Quit { force: false },
        "q!" | "quit!" => CommandBarAction::Quit { force: true },
        "wq" | "x" => CommandBarAction::WriteQuit {
            force: false,
            path: rest,
        },
        "wq!" | "x!" => CommandBarAction::WriteQuit {
            force: true,
            path: rest,
        },
        "wa" | "wall" => CommandBarAction::WriteAll,
        "qa" | "qall" => CommandBarAction::QuitAll { force: false },
        "qa!" | "qall!" => CommandBarAction::QuitAll { force: true },
        "wqa" | "wqall" | "xa" | "xall" | "wqa!" | "xa!" => CommandBarAction::WriteQuitAll,
        "e" | "edit" => match rest {
            Some(path) => CommandBarAction::Edit(path),
            None => CommandBarAction::Reload,
        },
        "reload" | "e!" => CommandBarAction::Reload,
        "vs" | "vsplit" => CommandBarAction::VSplit(rest),
        other => CommandBarAction::Unknown(other.to_string()),
    }
}

// ---- UTF-8 & Text Motion Primitives ---------------------------------------

pub fn clamp_char_boundary(text: &str, offset: usize) -> usize {
    let mut o = offset.min(text.len());
    while o > 0 && !text.is_char_boundary(o) {
        o -= 1;
    }
    o
}

pub fn prev_char_offset(text: &str, offset: usize) -> usize {
    let o = clamp_char_boundary(text, offset);
    text[..o]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

pub fn next_char_offset(text: &str, offset: usize) -> usize {
    let o = clamp_char_boundary(text, offset);
    text[o..]
        .chars()
        .next()
        .map(|c| o + c.len_utf8())
        .unwrap_or(text.len())
}

pub fn move_left(text: &str, offset: usize, count: usize) -> usize {
    let mut cur = clamp_char_boundary(text, offset);
    for _ in 0..count {
        cur = prev_char_offset(text, cur);
    }
    cur
}

pub fn move_right(text: &str, offset: usize, count: usize) -> usize {
    let mut cur = clamp_char_boundary(text, offset);
    for _ in 0..count {
        cur = next_char_offset(text, cur);
    }
    cur
}

pub fn line_start_offset(text: &str, offset: usize) -> usize {
    let o = clamp_char_boundary(text, offset);
    text[..o].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

pub fn line_end_offset(text: &str, offset: usize) -> usize {
    let o = clamp_char_boundary(text, offset);
    let end = text[o..].find('\n').map(|i| o + i).unwrap_or(text.len());
    if end > 0 && text.as_bytes().get(end - 1) == Some(&b'\r') {
        end - 1
    } else {
        end
    }
}

pub fn line_end_including_newline(text: &str, offset: usize) -> usize {
    let o = clamp_char_boundary(text, offset);
    text[o..]
        .find('\n')
        .map(|i| o + i + 1)
        .unwrap_or(text.len())
}

pub fn line_first_non_blank_offset(text: &str, offset: usize) -> usize {
    let start = line_start_offset(text, offset);
    let end = line_end_offset(text, offset);
    for (rel, ch) in text[start..end].char_indices() {
        if !ch.is_whitespace() {
            return start + rel;
        }
    }
    end
}

/// Convert 1-based line number to byte offset at the start of that line.
pub fn line_number_to_offset(text: &str, line_1_based: usize) -> usize {
    let target = line_1_based.saturating_sub(1);
    if target == 0 {
        return 0;
    }
    let mut current_line = 0;
    for (idx, b) in text.bytes().enumerate() {
        if b == b'\n' {
            current_line += 1;
            if current_line == target {
                return idx + 1;
            }
        }
    }
    line_start_offset(text, text.len())
}

/// Move `delta_lines` vertically while preserving the character column on the line.
pub fn move_vertical(text: &str, offset: usize, delta_lines: isize) -> usize {
    if delta_lines == 0 || text.is_empty() {
        return clamp_char_boundary(text, offset);
    }
    let o = clamp_char_boundary(text, offset);
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let cur_line = line_starts
        .partition_point(|&start| start <= o)
        .saturating_sub(1);
    let cur_start = line_starts[cur_line];
    let col_chars = text[cur_start..o].chars().count();

    let target_line = (cur_line as isize + delta_lines)
        .clamp(0, line_starts.len().saturating_sub(1) as isize) as usize;
    let target_start = line_starts[target_line];
    let target_end = line_end_offset(text, target_start);
    text[target_start..target_end]
        .char_indices()
        .nth(col_chars)
        .map(|(rel, _)| target_start + rel)
        .unwrap_or(target_end)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Whitespace,
    Word,
    Punct,
}

fn classify_char(c: char, big_word: bool) -> CharClass {
    if c.is_whitespace() {
        CharClass::Whitespace
    } else if big_word || c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

/// Move to the start of the next word (`w` / `W`).
pub fn word_next_start(text: &str, offset: usize, big_word: bool) -> usize {
    let o = clamp_char_boundary(text, offset);
    let mut iter = text[o..].char_indices().peekable();
    let Some((_, first)) = iter.next() else {
        return text.len();
    };
    let first_class = classify_char(first, big_word);
    if first_class != CharClass::Whitespace {
        while let Some(&(_, ch)) = iter.peek() {
            if classify_char(ch, big_word) != first_class {
                break;
            }
            iter.next();
        }
    }
    while let Some(&(rel, ch)) = iter.peek() {
        if !ch.is_whitespace() {
            return o + rel;
        }
        iter.next();
    }
    text.len()
}

/// Move to the end of the current or next word (`e` / `E`).
pub fn word_next_end(text: &str, offset: usize, big_word: bool) -> usize {
    let mut o = clamp_char_boundary(text, offset);
    if o >= text.len() {
        return text.len();
    }
    // Step at least one char if sitting at the end of a word.
    let next = next_char_offset(text, o);
    if next < text.len() {
        let cur_ch = text[o..].chars().next().unwrap();
        let next_ch = text[next..].chars().next().unwrap();
        if classify_char(cur_ch, big_word) != classify_char(next_ch, big_word)
            || cur_ch.is_whitespace()
        {
            o = next;
        }
    }
    let mut iter = text[o..].char_indices().peekable();
    // Skip leading whitespace.
    while let Some(&(_, ch)) = iter.peek() {
        if !ch.is_whitespace() {
            break;
        }
        iter.next();
    }
    let Some(&(start_rel, first)) = iter.peek() else {
        return text.len();
    };
    let target_class = classify_char(first, big_word);
    let mut end_rel = start_rel + first.len_utf8();
    iter.next();
    for (rel, ch) in iter {
        if classify_char(ch, big_word) != target_class {
            break;
        }
        end_rel = rel + ch.len_utf8();
    }
    o + end_rel
}

/// Move to the start of the previous word (`b` / `B`).
pub fn word_prev_start(text: &str, offset: usize, big_word: bool) -> usize {
    let o = clamp_char_boundary(text, offset);
    if o == 0 {
        return 0;
    }
    let mut chars: Vec<(usize, char)> = text[..o].char_indices().collect();
    while let Some(&(_, ch)) = chars.last() {
        if !ch.is_whitespace() {
            break;
        }
        chars.pop();
    }
    let Some(&(last_idx, last_ch)) = chars.last() else {
        return 0;
    };
    let target_class = classify_char(last_ch, big_word);
    let mut start_idx = last_idx;
    while let Some(&(idx, ch)) = chars.last() {
        if classify_char(ch, big_word) != target_class {
            break;
        }
        start_idx = idx;
        chars.pop();
    }
    start_idx
}

/// Find the `count`-th occurrence of `target` forward or backward (`f`, `t`, `F`, `T`).
pub fn find_char_motion(
    text: &str,
    offset: usize,
    target: char,
    forward: bool,
    till: bool,
    count: usize,
) -> Option<usize> {
    let o = clamp_char_boundary(text, offset);
    let mut remaining = count.max(1);
    if forward {
        let search_start = next_char_offset(text, o);
        for (rel, ch) in text[search_start..].char_indices() {
            if ch == target {
                remaining -= 1;
                if remaining == 0 {
                    let found = search_start + rel;
                    return Some(if till { found } else { found + ch.len_utf8() });
                }
            }
        }
        None
    } else {
        for (idx, ch) in text[..o].char_indices().rev() {
            if ch == target {
                remaining -= 1;
                if remaining == 0 {
                    return Some(if till { idx + ch.len_utf8() } else { idx });
                }
            }
        }
        None
    }
}

/// Select the current line, or extend an existing full-line selection by `count` lines (`x`).
pub fn extend_line_selection(
    text: &str,
    anchor: usize,
    head: usize,
    count: usize,
) -> (usize, usize) {
    let a = clamp_char_boundary(text, anchor);
    let h = clamp_char_boundary(text, head);
    let min = a.min(h);
    let max = a.max(h);
    let start_of_min = line_start_offset(text, min);
    let already_line_selected = a <= h
        && a == start_of_min
        && max > min
        && (max == text.len() || text.as_bytes().get(max - 1) == Some(&b'\n'));

    let mut cur_end = if already_line_selected {
        max
    } else {
        line_end_including_newline(text, h)
    };
    let extra = if already_line_selected {
        count
    } else {
        count.saturating_sub(1)
    };
    for _ in 0..extra {
        if cur_end < text.len() {
            cur_end = line_end_including_newline(text, cur_end);
        }
    }
    let new_anchor = if already_line_selected {
        start_of_min
    } else {
        line_start_offset(text, h)
    };
    (new_anchor, cur_end)
}

// ---- Brackets, Text Objects, and Surround ---------------------------------

pub fn bracket_pair(ch: char) -> Option<(char, char)> {
    match ch {
        '(' | ')' | 'b' => Some(('(', ')')),
        '[' | ']' => Some(('[', ']')),
        '{' | '}' | 'B' => Some(('{', '}')),
        '<' | '>' => Some(('<', '>')),
        _ => None,
    }
}

pub fn surround_pair(ch: char) -> (char, char) {
    bracket_pair(ch).unwrap_or((ch, ch))
}

/// Find the matching bracket for the character at (or nearest on the line to) `offset` (`mm`).
pub fn find_matching_bracket(text: &str, offset: usize) -> Option<usize> {
    let o = clamp_char_boundary(text, offset);
    let is_bracket = |c: char| matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>');
    let mut bracket_pos = None;
    if let Some(ch) = text[o..].chars().next()
        && is_bracket(ch)
    {
        bracket_pos = Some((o, ch));
    } else {
        let line_end = line_end_offset(text, o);
        for (rel, ch) in text[o..line_end].char_indices() {
            if is_bracket(ch) {
                bracket_pos = Some((o + rel, ch));
                break;
            }
        }
    }
    let (pos, ch) = bracket_pos?;
    let (open, close, forward) = match ch {
        '(' => ('(', ')', true),
        '[' => ('[', ']', true),
        '{' => ('{', '}', true),
        '<' => ('<', '>', true),
        ')' => ('(', ')', false),
        ']' => ('[', ']', false),
        '}' => ('{', '}', false),
        '>' => ('<', '>', false),
        _ => return None,
    };
    if forward {
        let mut depth = 0usize;
        for (rel, c) in text[pos..].char_indices() {
            if c == open {
                depth += 1;
            } else if c == close {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(pos + rel);
                }
            }
        }
        None
    } else {
        let mut depth = 0usize;
        for (idx, c) in text[..=pos].char_indices().rev() {
            if c == close {
                depth += 1;
            } else if c == open {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(idx);
                }
            }
        }
        None
    }
}

/// Find the innermost balanced `(open, close)` pair enclosing `offset`.
/// Returns `(outer_range, inner_range)`.
pub fn find_enclosing_pair(
    text: &str,
    offset: usize,
    open: char,
    close: char,
) -> Option<(Range<usize>, Range<usize>)> {
    let o = clamp_char_boundary(text, offset);
    if open == close {
        // Quote-like symmetric delimiter: search on the current line first, then whole text.
        let line_start = line_start_offset(text, o);
        let line_end = line_end_offset(text, o);
        if let Some(res) =
            find_symmetric_pair_in_slice(&text[line_start..line_end], o - line_start, open)
        {
            return Some((
                (line_start + res.0.start)..(line_start + res.0.end),
                (line_start + res.1.start)..(line_start + res.1.end),
            ));
        }
        return find_symmetric_pair_in_slice(text, o, open);
    }

    // Asymmetric bracket pair: scan backward for unmatched `open`, then forward for matching `close`.
    let mut depth = 0usize;
    let mut open_idx = None;
    // If cursor is directly on `open`, use it.
    if text[o..].chars().next() == Some(open) {
        open_idx = Some(o);
    } else {
        let scan_end = if text[o..].chars().next() == Some(close) {
            o
        } else {
            next_char_offset(text, o)
        };
        for (idx, ch) in text[..scan_end].char_indices().rev() {
            if ch == close && idx < o {
                depth += 1;
            } else if ch == open {
                if depth == 0 {
                    open_idx = Some(idx);
                    break;
                }
                depth -= 1;
            }
        }
    }
    let start = open_idx?;
    let mut fwd_depth = 0usize;
    for (rel, ch) in text[start..].char_indices() {
        if ch == open {
            fwd_depth += 1;
        } else if ch == close {
            fwd_depth = fwd_depth.saturating_sub(1);
            if fwd_depth == 0 {
                let close_start = start + rel;
                let close_end = close_start + ch.len_utf8();
                let inner_start = start + open.len_utf8();
                return Some((start..close_end, inner_start..close_start));
            }
        }
    }
    None
}

fn find_symmetric_pair_in_slice(
    slice: &str,
    rel_offset: usize,
    delim: char,
) -> Option<(Range<usize>, Range<usize>)> {
    let positions: Vec<usize> = slice
        .char_indices()
        .filter_map(|(i, c)| (c == delim).then_some(i))
        .collect();
    for pair in positions.chunks_exact(2) {
        let open_pos = pair[0];
        let close_pos = pair[1];
        let close_end = close_pos + delim.len_utf8();
        if rel_offset >= open_pos && rel_offset <= close_end {
            let inner_start = open_pos + delim.len_utf8();
            return Some((open_pos..close_end, inner_start..close_pos));
        }
    }
    None
}

/// Compute the byte range for a text object (`mi<obj>` when `!around`, `ma<obj>` when `around`).
pub fn text_object_range(
    text: &str,
    offset: usize,
    obj: char,
    around: bool,
) -> Option<Range<usize>> {
    let o = clamp_char_boundary(text, offset);
    if text.is_empty() {
        return None;
    }
    match obj {
        'w' | 'W' => {
            let big = obj == 'W';
            let ch = text[o..].chars().next()?;
            let class = classify_char(ch, big);
            if class == CharClass::Whitespace {
                return None;
            }
            let mut start = o;
            for (idx, c) in text[..o].char_indices().rev() {
                if classify_char(c, big) != class {
                    break;
                }
                start = idx;
            }
            let mut end = o;
            for (rel, c) in text[o..].char_indices() {
                if classify_char(c, big) != class {
                    break;
                }
                end = o + rel + c.len_utf8();
            }
            if around {
                let line_end = line_end_offset(text, end);
                let mut trailing_end = end;
                for (rel, c) in text[end..line_end].char_indices() {
                    if !c.is_whitespace() {
                        break;
                    }
                    trailing_end = end + rel + c.len_utf8();
                }
                if trailing_end > end {
                    end = trailing_end;
                } else {
                    let line_start = line_start_offset(text, start);
                    for (idx, c) in text[line_start..start].char_indices().rev() {
                        if !c.is_whitespace() {
                            break;
                        }
                        start = line_start + idx;
                    }
                }
            }
            Some(start..end)
        }
        'p' => {
            // Paragraph text object: contiguous non-blank lines.
            let line_starts: Vec<usize> = std::iter::once(0)
                .chain(text.match_indices('\n').map(|(i, _)| i + 1))
                .collect();
            let is_blank = |line_idx: usize| -> bool {
                let s = line_starts[line_idx];
                let e = line_end_offset(text, s);
                text[s..e].trim().is_empty()
            };
            let cur_line = line_starts.partition_point(|&s| s <= o).saturating_sub(1);
            if is_blank(cur_line) {
                return None;
            }
            let mut first = cur_line;
            while first > 0 && !is_blank(first - 1) {
                first -= 1;
            }
            let mut last = cur_line;
            while last + 1 < line_starts.len() && !is_blank(last + 1) {
                last += 1;
            }
            if around {
                while last + 1 < line_starts.len() && is_blank(last + 1) {
                    last += 1;
                }
            }
            let start = line_starts[first];
            let end = line_end_including_newline(text, line_starts[last]);
            Some(start..end)
        }
        _ => {
            let (open, close) = surround_pair(obj);
            let (outer, inner) = find_enclosing_pair(text, o, open, close)?;
            Some(if around { outer } else { inner })
        }
    }
}

// ---- Pure Text Transforms (`>`, `<`, `J`, `~`, `` ` ``, `ms`, `md`, `mr`) --

pub fn indent_lines(
    text: &str,
    range: Range<usize>,
    count: usize,
    indent_unit: &str,
) -> (Range<usize>, String) {
    let start_line = line_start_offset(text, range.start);
    let end_probe = if range.end > range.start && text.as_bytes().get(range.end - 1) == Some(&b'\n')
    {
        range.end - 1
    } else {
        range.end
    };
    let end_line = line_end_offset(text, end_probe);
    let prefix = indent_unit.repeat(count.max(1));
    let slice = &text[start_line..end_line];
    let mut out = String::new();
    for (i, line) in slice.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if !line.trim().is_empty() {
            out.push_str(&prefix);
        }
        out.push_str(line);
    }
    (start_line..end_line, out)
}

pub fn outdent_lines(
    text: &str,
    range: Range<usize>,
    count: usize,
    tab_width: usize,
) -> (Range<usize>, String) {
    let start_line = line_start_offset(text, range.start);
    let end_probe = if range.end > range.start && text.as_bytes().get(range.end - 1) == Some(&b'\n')
    {
        range.end - 1
    } else {
        range.end
    };
    let end_line = line_end_offset(text, end_probe);
    let max_spaces = count.max(1) * tab_width.max(1);
    let slice = &text[start_line..end_line];
    let mut out = String::new();
    for (i, line) in slice.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let mut removed = 0usize;
        let mut byte_cut = 0usize;
        for (rel, ch) in line.char_indices() {
            if removed >= max_spaces {
                break;
            }
            if ch == ' ' {
                removed += 1;
                byte_cut = rel + 1;
            } else if ch == '\t' {
                removed += tab_width.max(1);
                byte_cut = rel + 1;
            } else {
                break;
            }
        }
        out.push_str(&line[byte_cut..]);
    }
    (start_line..end_line, out)
}

pub fn join_lines_in_range(text: &str, range: Range<usize>) -> Option<(Range<usize>, String)> {
    let start_line = line_start_offset(text, range.start);
    let end_line_with_nl = line_end_including_newline(text, range.end.max(range.start));
    let target_end = if text[start_line..range.end.max(range.start)].contains('\n') {
        line_end_offset(text, range.end)
    } else if end_line_with_nl < text.len() {
        line_end_offset(text, end_line_with_nl)
    } else {
        return None;
    };
    let slice = &text[start_line..target_end];
    if !slice.contains('\n') {
        return None;
    }
    let mut out = String::new();
    for (i, line) in slice.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if i == 0 {
            out.push_str(line.trim_end());
        } else {
            let trimmed = line.trim_start();
            if !out.is_empty() && !trimmed.is_empty() {
                out.push(' ');
            }
            out.push_str(trimmed);
        }
    }
    Some((start_line..target_end, out))
}

pub fn toggle_case_str(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_uppercase() {
                c.to_lowercase().collect::<Vec<_>>()
            } else if c.is_lowercase() {
                c.to_uppercase().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

// ---- Search & Multi-Cursor Helpers (`/`, `*`, `n`, `N`, `s`, `C`) ----------

/// Find the next (`forward == true`) or previous (`forward == false`) match of
/// `query` in `text`, using smartcase (case-insensitive unless `query` contains
/// an uppercase letter) and wrapping around the buffer.
pub fn find_next_match(
    text: &str,
    query: &str,
    from_offset: usize,
    forward: bool,
) -> Option<Range<usize>> {
    if query.is_empty() || text.is_empty() {
        return None;
    }
    let case_sensitive = query.chars().any(char::is_uppercase);
    let matches: Vec<Range<usize>> = if case_sensitive {
        text.match_indices(query)
            .map(|(i, m)| i..(i + m.len()))
            .collect()
    } else {
        let pat = format!("(?i){}", regex::escape(query));
        let re = regex::Regex::new(&pat).ok()?;
        re.find_iter(text).map(|m| m.start()..m.end()).collect()
    };
    if matches.is_empty() {
        return None;
    }
    let o = clamp_char_boundary(text, from_offset);
    if forward {
        matches
            .iter()
            .find(|r| r.start > o)
            .or_else(|| matches.first())
            .cloned()
    } else {
        matches
            .iter()
            .rev()
            .find(|r| r.start < o)
            .or_else(|| matches.last())
            .cloned()
    }
}

/// Split `range` in `text` into sub-selections matching `pattern` (`s` command).
pub fn split_selection_on_pattern(
    text: &str,
    range: Range<usize>,
    pattern: &str,
) -> Vec<Range<usize>> {
    let start = clamp_char_boundary(text, range.start);
    let end = clamp_char_boundary(text, range.end);
    if pattern.is_empty() || start >= end {
        return Vec::new();
    }
    let slice = &text[start..end];
    if let Ok(re) = regex::Regex::new(pattern) {
        re.find_iter(slice)
            .filter(|m| !m.is_empty())
            .map(|m| (start + m.start())..(start + m.end()))
            .collect()
    } else {
        slice
            .match_indices(pattern)
            .map(|(i, m)| (start + i)..(start + i + m.len()))
            .collect()
    }
}

/// Compute the byte range for a new cursor on the line below the last selection (`C`).
pub fn add_cursor_below_range(text: &str, last_offset: usize) -> Option<usize> {
    let next = move_vertical(text, last_offset, 1);
    if line_start_offset(text, next) > line_start_offset(text, last_offset) {
        Some(next)
    } else {
        None
    }
}

// ---- Keystroke Dispatch Engine ---------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceKind {
    CodeEditor,
    InlinePrompt,
    Composer,
}

impl HelixState {
    /// Process a GPUI `Keystroke` for `text` on `surface`.
    ///
    /// Returns [`HelixOutcome::Ignored`] when the keystroke should pass through
    /// to standard input handling (e.g. printable typing in `Insert` mode, or
    /// platform shortcuts like `Cmd+C` / `Cmd+V`).
    pub fn handle_keystroke(
        &mut self,
        ks: &Keystroke,
        text: &str,
        surface: SurfaceKind,
        system_clipboard: Option<&str>,
    ) -> HelixOutcome {
        let key = ks.key.as_str();
        let m = &ks.modifiers;

        // Always let platform/secondary shortcuts (`Cmd+...` on macOS, `Ctrl+Shift+...`) pass through.
        if m.platform || (m.control && m.shift) {
            return HelixOutcome::Ignored;
        }

        // In Insert mode, only `Escape` transitions to `Normal` mode.
        if self.mode == HelixMode::Insert {
            if key == "escape" && !m.control && !m.alt && !m.shift {
                self.mode = HelixMode::Normal;
                self.clear_pending();
                // Collapse any selection at current head.
                self.anchor = self.head;
                return HelixOutcome::Handled;
            }
            return HelixOutcome::Ignored;
        }

        // Escape in Normal / Select / Minor mode:
        if key == "escape" {
            if self.has_pending() {
                self.clear_pending();
                return HelixOutcome::Handled;
            }
            if self.mode == HelixMode::Select {
                self.mode = HelixMode::Normal;
                return HelixOutcome::Handled;
            }
            if !self.extra_selections.is_empty() || self.anchor != self.head {
                self.extra_selections.clear();
                self.anchor = self.head;
                return HelixOutcome::Handled;
            }
            return match surface {
                SurfaceKind::Composer => HelixOutcome::Host(HelixHostCommand::DismissSurface),
                SurfaceKind::CodeEditor | SurfaceKind::InlinePrompt => HelixOutcome::Handled,
            };
        }

        // Handle active Minor Mode first.
        if let Some(minor) = self.minor.take() {
            return self.handle_minor_mode(minor, ks, text, surface, system_clipboard);
        }

        // Handle Control chords in Normal / Select mode (`Ctrl-d`, `Ctrl-u`, `Ctrl-f`, `Ctrl-b`, `Ctrl-o`, `Ctrl-i`, `Ctrl-w`, `Ctrl-c`).
        if m.control && !m.alt {
            return match key {
                "d" => {
                    let count = self.take_count() as isize;
                    HelixOutcome::Host(HelixHostCommand::ScrollPages { half_pages: count })
                }
                "u" => {
                    let count = self.take_count() as isize;
                    HelixOutcome::Host(HelixHostCommand::ScrollPages { half_pages: -count })
                }
                "f" => {
                    let count = self.take_count() as isize;
                    HelixOutcome::Host(HelixHostCommand::ScrollPages {
                        half_pages: count * 2,
                    })
                }
                "b" => {
                    let count = self.take_count() as isize;
                    HelixOutcome::Host(HelixHostCommand::ScrollPages {
                        half_pages: -count * 2,
                    })
                }
                "o" => {
                    self.clear_pending();
                    HelixOutcome::Host(HelixHostCommand::JumpHistory { forward: false })
                }
                "i" => {
                    self.clear_pending();
                    HelixOutcome::Host(HelixHostCommand::JumpHistory { forward: true })
                }
                "w" => {
                    self.minor = Some(MinorMode::Window);
                    HelixOutcome::Handled
                }
                "c" => {
                    self.clear_pending();
                    HelixOutcome::Host(HelixHostCommand::ToggleComment)
                }
                _ => HelixOutcome::Ignored,
            };
        }

        let effective_char = resolved_key_char(ks);

        // Handle Alt chords (`Alt-;` flip selection, `Alt-d` / `Alt-c` blackhole delete/change, `Alt-`` uppercase).
        if m.alt && !m.control {
            match effective_char.as_deref().unwrap_or(key) {
                ";" => {
                    std::mem::swap(&mut self.anchor, &mut self.head);
                    return HelixOutcome::Handled;
                }
                "d" => {
                    let range = self.effective_range(text);
                    self.clear_pending();
                    self.mode = HelixMode::Normal;
                    return HelixOutcome::Edit {
                        new_anchor: range.start,
                        new_head: range.start,
                        range,
                        new_text: String::new(),
                        yank_to_clipboard: None,
                    };
                }
                "c" => {
                    let range = self.effective_range(text);
                    self.clear_pending();
                    self.mode = HelixMode::Insert;
                    return HelixOutcome::Edit {
                        new_anchor: range.start,
                        new_head: range.start,
                        range,
                        new_text: String::new(),
                        yank_to_clipboard: None,
                    };
                }
                "`" | "~" => {
                    let range = self.effective_range(text);
                    self.clear_pending();
                    let upper = text[range.clone()].to_uppercase();
                    let new_end = range.start + upper.len();
                    return HelixOutcome::Edit {
                        range: range.clone(),
                        new_text: upper,
                        new_anchor: range.start,
                        new_head: new_end,
                        yank_to_clipboard: None,
                    };
                }
                _ => return HelixOutcome::Ignored,
            }
        }

        // Numeric count prefix (`1`..=`9`, or `0` when count is already accumulating).
        if let Some(s) = effective_char.as_deref()
            && s.len() == 1
        {
            let ch = s.chars().next().unwrap();
            if ch.is_ascii_digit() && (ch != '0' || self.count.is_some()) {
                let digit = (ch as u8 - b'0') as usize;
                let next = self
                    .count
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(digit)
                    .min(100_000);
                self.count = Some(next);
                return HelixOutcome::Handled;
            }
        }

        // Named keys (`left`, `right`, `up`, `down`, `home`, `end`, `enter`, `space`, `backspace`).
        match key {
            "left" => {
                let count = self.take_count();
                let next = move_left(text, self.head, count);
                self.move_cursor(text, next);
                return HelixOutcome::Handled;
            }
            "right" => {
                let count = self.take_count();
                let next = move_right(text, self.head, count);
                self.move_cursor(text, next);
                return HelixOutcome::Handled;
            }
            "up" => {
                let count = self.take_count() as isize;
                let next = move_vertical(text, self.head, -count);
                self.move_cursor(text, next);
                return HelixOutcome::Handled;
            }
            "down" => {
                let count = self.take_count() as isize;
                let next = move_vertical(text, self.head, count);
                self.move_cursor(text, next);
                return HelixOutcome::Handled;
            }
            "home" => {
                self.clear_pending();
                let next = line_start_offset(text, self.head);
                self.move_cursor(text, next);
                return HelixOutcome::Handled;
            }
            "end" => {
                self.clear_pending();
                let next = line_end_offset(text, self.head);
                self.move_cursor(text, next);
                return HelixOutcome::Handled;
            }
            "enter" => {
                self.clear_pending();
                if matches!(surface, SurfaceKind::InlinePrompt | SurfaceKind::Composer) {
                    return HelixOutcome::Host(HelixHostCommand::Submit);
                }
                let next = move_vertical(text, self.head, 1);
                self.move_cursor(text, next);
                return HelixOutcome::Handled;
            }
            "space" => {
                self.minor = Some(MinorMode::Space);
                return HelixOutcome::Handled;
            }
            "backspace" => {
                let count = self.take_count();
                let next = move_left(text, self.head, count);
                self.move_cursor(text, next);
                return HelixOutcome::Handled;
            }
            _ => {}
        }

        let Some(ch_str) = effective_char else {
            return HelixOutcome::Ignored;
        };
        let mut chars = ch_str.chars();
        let Some(ch) = chars.next() else {
            return HelixOutcome::Ignored;
        };
        if chars.next().is_some() {
            return HelixOutcome::Ignored;
        }

        match ch {
            // Basic character & line motions
            'h' => {
                let count = self.take_count();
                let next = move_left(text, self.head, count);
                self.move_cursor(text, next);
                HelixOutcome::Handled
            }
            'l' => {
                let count = self.take_count();
                let next = move_right(text, self.head, count);
                self.move_cursor(text, next);
                HelixOutcome::Handled
            }
            'j' => {
                let count = self.take_count() as isize;
                let next = move_vertical(text, self.head, count);
                self.move_cursor(text, next);
                HelixOutcome::Handled
            }
            'k' => {
                let count = self.take_count() as isize;
                let next = move_vertical(text, self.head, -count);
                self.move_cursor(text, next);
                HelixOutcome::Handled
            }
            '0' => {
                let next = line_start_offset(text, self.head);
                self.move_cursor(text, next);
                HelixOutcome::Handled
            }
            '$' => {
                let next = line_end_offset(text, self.head);
                self.move_cursor(text, next);
                HelixOutcome::Handled
            }
            '^' => {
                let next = line_first_non_blank_offset(text, self.head);
                self.move_cursor(text, next);
                HelixOutcome::Handled
            }
            'G' => {
                let target = match self.count.take() {
                    Some(line) => line_number_to_offset(text, line),
                    None => line_start_offset(text, text.len()),
                };
                self.move_cursor(text, target);
                HelixOutcome::Handled
            }

            // Word motions (select span in Normal mode, extend in Select mode)
            'w' | 'W' => {
                let big = ch == 'W';
                let count = self.take_count();
                let mut start = self.head;
                let mut end = self.head;
                for _ in 0..count {
                    start = end;
                    end = word_next_start(text, end, big);
                }
                self.select_span(text, start, end);
                HelixOutcome::Handled
            }
            'e' | 'E' => {
                let big = ch == 'E';
                let count = self.take_count();
                let mut start = self.head;
                let mut end = self.head;
                for _ in 0..count {
                    start = end;
                    end = word_next_end(text, end, big);
                }
                self.select_span(text, start, end);
                HelixOutcome::Handled
            }
            'b' | 'B' => {
                let big = ch == 'B';
                let count = self.take_count();
                let mut start = self.head;
                let mut end = self.head;
                for _ in 0..count {
                    start = end;
                    end = word_prev_start(text, end, big);
                }
                self.select_span(text, start, end);
                HelixOutcome::Handled
            }

            // Find / Till char minor modes
            'f' => {
                self.minor = Some(MinorMode::FindChar {
                    forward: true,
                    till: false,
                });
                HelixOutcome::Handled
            }
            't' => {
                self.minor = Some(MinorMode::FindChar {
                    forward: true,
                    till: true,
                });
                HelixOutcome::Handled
            }
            'F' => {
                self.minor = Some(MinorMode::FindChar {
                    forward: false,
                    till: false,
                });
                HelixOutcome::Handled
            }
            'T' => {
                self.minor = Some(MinorMode::FindChar {
                    forward: false,
                    till: true,
                });
                HelixOutcome::Handled
            }

            // Selection manipulation (`v`, `x`, `%`, `;`, `,`, `C`, `s`)
            'v' => {
                self.clear_pending();
                self.mode = match self.mode {
                    HelixMode::Select => HelixMode::Normal,
                    _ => HelixMode::Select,
                };
                HelixOutcome::Handled
            }
            'x' => {
                let count = self.take_count();
                let (new_a, new_h) = extend_line_selection(text, self.anchor, self.head, count);
                self.anchor = new_a;
                self.head = new_h;
                HelixOutcome::Handled
            }
            '%' => {
                self.clear_pending();
                self.anchor = 0;
                self.head = text.len();
                HelixOutcome::Handled
            }
            ';' => {
                self.clear_pending();
                self.anchor = self.head;
                HelixOutcome::Handled
            }
            ',' => {
                self.clear_pending();
                self.extra_selections.clear();
                HelixOutcome::Handled
            }
            'C' => {
                let count = self.take_count();
                let mut last = self
                    .extra_selections
                    .last()
                    .map(|&(_, h)| h)
                    .unwrap_or(self.head);
                for _ in 0..count {
                    if let Some(next_pos) = add_cursor_below_range(text, last) {
                        self.extra_selections.push((next_pos, next_pos));
                        last = next_pos;
                    }
                }
                HelixOutcome::Handled
            }
            's' => {
                self.clear_pending();
                HelixOutcome::Host(HelixHostCommand::OpenSplitSelectionBar)
            }

            // Mode transitions (`i`, `a`, `I`, `A`, `o`, `O`)
            'i' => {
                self.clear_pending();
                let pos = self.anchor.min(self.head);
                self.anchor = pos;
                self.head = pos;
                self.mode = HelixMode::Insert;
                HelixOutcome::Handled
            }
            'a' => {
                self.clear_pending();
                let pos = if self.anchor == self.head {
                    let line_end = line_end_offset(text, self.head);
                    next_char_offset(text, self.head).min(line_end)
                } else {
                    self.anchor.max(self.head)
                };
                self.anchor = pos;
                self.head = pos;
                self.mode = HelixMode::Insert;
                HelixOutcome::Handled
            }
            'I' => {
                self.clear_pending();
                let pos = line_first_non_blank_offset(text, self.head);
                self.anchor = pos;
                self.head = pos;
                self.mode = HelixMode::Insert;
                HelixOutcome::Handled
            }
            'A' => {
                self.clear_pending();
                let pos = line_end_offset(text, self.head);
                self.anchor = pos;
                self.head = pos;
                self.mode = HelixMode::Insert;
                HelixOutcome::Handled
            }
            'o' => {
                self.clear_pending();
                let line_start = line_start_offset(text, self.head);
                let non_blank = line_first_non_blank_offset(text, self.head);
                let indent = &text[line_start..non_blank];
                let insert_at = line_end_offset(text, self.head);
                let new_text = format!("\n{indent}");
                let new_pos = insert_at + new_text.len();
                self.mode = HelixMode::Insert;
                HelixOutcome::Edit {
                    range: insert_at..insert_at,
                    new_text,
                    new_anchor: new_pos,
                    new_head: new_pos,
                    yank_to_clipboard: None,
                }
            }
            'O' => {
                self.clear_pending();
                let line_start = line_start_offset(text, self.head);
                let non_blank = line_first_non_blank_offset(text, self.head);
                let indent = &text[line_start..non_blank];
                let new_text = format!("{indent}\n");
                let new_pos = line_start + indent.len();
                self.mode = HelixMode::Insert;
                HelixOutcome::Edit {
                    range: line_start..line_start,
                    new_text,
                    new_anchor: new_pos,
                    new_head: new_pos,
                    yank_to_clipboard: None,
                }
            }

            // Delete, Change, Yank, Paste, Replace
            'd' => {
                let range = self.effective_range(text);
                let deleted = text[range.clone()].to_string();
                let reg = self.selected_register.take();
                let yank_to_clipboard = self.registers.write(reg, deleted);
                self.clear_pending();
                self.mode = HelixMode::Normal;
                HelixOutcome::Edit {
                    new_anchor: range.start,
                    new_head: range.start,
                    range,
                    new_text: String::new(),
                    yank_to_clipboard,
                }
            }
            'c' => {
                let range = self.effective_range(text);
                let deleted = text[range.clone()].to_string();
                let reg = self.selected_register.take();
                let yank_to_clipboard = self.registers.write(reg, deleted);
                self.clear_pending();
                self.mode = HelixMode::Insert;
                HelixOutcome::Edit {
                    new_anchor: range.start,
                    new_head: range.start,
                    range,
                    new_text: String::new(),
                    yank_to_clipboard,
                }
            }
            'y' => {
                let range = self.effective_range(text);
                let yanked = text[range].to_string();
                let reg = self.selected_register.take();
                let clipboard = self.registers.write(reg, yanked);
                self.clear_pending();
                HelixOutcome::Yank { clipboard }
            }
            'p' | 'P' => {
                let after = ch == 'p';
                let reg = self.selected_register.take();
                let content = self.registers.read(reg, system_clipboard);
                self.clear_pending();
                if content.is_empty() {
                    return HelixOutcome::Handled;
                }
                let eff = self.effective_range(text);
                if content.ends_with('\n') {
                    if after {
                        let line_end = line_end_including_newline(text, eff.end.saturating_sub(1));
                        let needs_leading_nl =
                            line_end == text.len() && !text.is_empty() && !text.ends_with('\n');
                        let inserted = if needs_leading_nl {
                            format!("\n{}", content.strip_suffix('\n').unwrap_or(&content))
                        } else {
                            content
                        };
                        let start = if needs_leading_nl {
                            line_end + 1
                        } else {
                            line_end
                        };
                        let end = line_end + inserted.len();
                        HelixOutcome::Edit {
                            range: line_end..line_end,
                            new_text: inserted,
                            new_anchor: start,
                            new_head: end,
                            yank_to_clipboard: None,
                        }
                    } else {
                        let line_start = line_start_offset(text, eff.start);
                        let end = line_start + content.len();
                        HelixOutcome::Edit {
                            range: line_start..line_start,
                            new_text: content,
                            new_anchor: line_start,
                            new_head: end,
                            yank_to_clipboard: None,
                        }
                    }
                } else {
                    let insert_at = if after { eff.end } else { eff.start };
                    let end = insert_at + content.len();
                    HelixOutcome::Edit {
                        range: insert_at..insert_at,
                        new_text: content,
                        new_anchor: insert_at,
                        new_head: end,
                        yank_to_clipboard: None,
                    }
                }
            }
            'r' => {
                self.minor = Some(MinorMode::ReplaceChar);
                HelixOutcome::Handled
            }

            // Undo / Redo (`u`, `U`)
            'u' => {
                self.clear_pending();
                HelixOutcome::Host(HelixHostCommand::Undo)
            }
            'U' => {
                self.clear_pending();
                HelixOutcome::Host(HelixHostCommand::Redo)
            }

            // Indent, Outdent, Join, Case transforms (`>`, `<`, `J`, `~`, `` ` ``)
            '>' => {
                let count = self.take_count();
                let eff = self.effective_range(text);
                let (range, new_text) = indent_lines(text, eff, count, "    ");
                let new_end = range.start + new_text.len();
                HelixOutcome::Edit {
                    new_anchor: range.start,
                    new_head: new_end,
                    range,
                    new_text,
                    yank_to_clipboard: None,
                }
            }
            '<' => {
                let count = self.take_count();
                let eff = self.effective_range(text);
                let (range, new_text) = outdent_lines(text, eff, count, 4);
                let new_end = range.start + new_text.len();
                HelixOutcome::Edit {
                    new_anchor: range.start,
                    new_head: new_end,
                    range,
                    new_text,
                    yank_to_clipboard: None,
                }
            }
            'J' => {
                self.clear_pending();
                let eff = self.effective_range(text);
                if let Some((range, new_text)) = join_lines_in_range(text, eff) {
                    let new_end = range.start + new_text.len();
                    HelixOutcome::Edit {
                        new_anchor: range.start,
                        new_head: new_end,
                        range,
                        new_text,
                        yank_to_clipboard: None,
                    }
                } else {
                    HelixOutcome::Handled
                }
            }
            '~' => {
                self.clear_pending();
                let range = self.effective_range(text);
                if range.is_empty() {
                    return HelixOutcome::Handled;
                }
                let toggled = toggle_case_str(&text[range.clone()]);
                let new_end = range.start + toggled.len();
                HelixOutcome::Edit {
                    new_anchor: range.start,
                    new_head: new_end,
                    range,
                    new_text: toggled,
                    yank_to_clipboard: None,
                }
            }
            '`' => {
                self.clear_pending();
                let range = self.effective_range(text);
                if range.is_empty() {
                    return HelixOutcome::Handled;
                }
                let lower = text[range.clone()].to_lowercase();
                let new_end = range.start + lower.len();
                HelixOutcome::Edit {
                    new_anchor: range.start,
                    new_head: new_end,
                    range,
                    new_text: lower,
                    yank_to_clipboard: None,
                }
            }

            // Minor mode prefixes (`g`, `m`, `z`, `"`, `:`, `/`, `*`, `n`, `N`)
            'g' => {
                self.minor = Some(MinorMode::Goto);
                HelixOutcome::Handled
            }
            'm' => {
                self.minor = Some(MinorMode::Match);
                HelixOutcome::Handled
            }
            'z' => {
                self.minor = Some(MinorMode::View);
                HelixOutcome::Handled
            }
            '[' => {
                self.minor = Some(MinorMode::BracketLeft);
                HelixOutcome::Handled
            }
            ']' => {
                self.minor = Some(MinorMode::BracketRight);
                HelixOutcome::Handled
            }
            '"' => {
                self.minor = Some(MinorMode::Register);
                HelixOutcome::Handled
            }
            ':' => {
                self.clear_pending();
                if surface == SurfaceKind::CodeEditor {
                    HelixOutcome::Host(HelixHostCommand::OpenCommandBar)
                } else {
                    HelixOutcome::Handled
                }
            }
            '/' => {
                self.clear_pending();
                HelixOutcome::Host(HelixHostCommand::OpenSearchBar)
            }
            '*' => {
                self.clear_pending();
                let range = if self.anchor != self.head {
                    self.selection_range(text)
                } else {
                    text_object_range(text, self.head, 'w', false)
                        .unwrap_or_else(|| self.effective_range(text))
                };
                if !range.is_empty() {
                    self.search_query = text[range.clone()].to_string();
                    self.anchor = range.start;
                    self.head = range.end;
                }
                HelixOutcome::Handled
            }
            'n' | 'N' => {
                let forward = ch == 'n';
                let count = self.take_count();
                if self.search_query.is_empty() {
                    return HelixOutcome::Handled;
                }
                let mut cur = if forward {
                    self.anchor.max(self.head)
                } else {
                    self.anchor.min(self.head)
                };
                let mut matched = None;
                for _ in 0..count {
                    if let Some(r) = find_next_match(text, &self.search_query, cur, forward) {
                        cur = if forward { r.end } else { r.start };
                        matched = Some(r);
                    } else {
                        break;
                    }
                }
                if let Some(r) = matched {
                    self.select_span(text, r.start, r.end);
                }
                HelixOutcome::Handled
            }

            _ => HelixOutcome::Handled,
        }
    }

    fn handle_minor_mode(
        &mut self,
        minor: MinorMode,
        ks: &Keystroke,
        text: &str,
        _surface: SurfaceKind,
        system_clipboard: Option<&str>,
    ) -> HelixOutcome {
        let Some(ch_str) = resolved_key_char(ks) else {
            self.clear_pending();
            return HelixOutcome::Handled;
        };
        let mut chars = ch_str.chars();
        let Some(ch) = chars.next() else {
            self.clear_pending();
            return HelixOutcome::Handled;
        };

        match minor {
            MinorMode::Goto => match ch {
                'g' => {
                    let target = match self.count.take() {
                        Some(line) => line_number_to_offset(text, line),
                        None => 0,
                    };
                    self.move_cursor(text, target);
                    HelixOutcome::Handled
                }
                'e' => {
                    self.clear_pending();
                    self.move_cursor(text, text.len());
                    HelixOutcome::Handled
                }
                'h' => {
                    self.clear_pending();
                    let next = line_start_offset(text, self.head);
                    self.move_cursor(text, next);
                    HelixOutcome::Handled
                }
                'l' => {
                    self.clear_pending();
                    let next = line_end_offset(text, self.head);
                    self.move_cursor(text, next);
                    HelixOutcome::Handled
                }
                's' => {
                    self.clear_pending();
                    let next = line_first_non_blank_offset(text, self.head);
                    self.move_cursor(text, next);
                    HelixOutcome::Handled
                }
                'd' => {
                    self.clear_pending();
                    HelixOutcome::Host(HelixHostCommand::GoToDefinition)
                }
                'r' => {
                    self.clear_pending();
                    HelixOutcome::Host(HelixHostCommand::FindReferences)
                }
                'n' => {
                    self.clear_pending();
                    HelixOutcome::Host(HelixHostCommand::CycleBuffer { next: true })
                }
                'p' => {
                    self.clear_pending();
                    HelixOutcome::Host(HelixHostCommand::CycleBuffer { next: false })
                }
                'c' => {
                    self.clear_pending();
                    HelixOutcome::Host(HelixHostCommand::ToggleComment)
                }
                _ => {
                    self.clear_pending();
                    HelixOutcome::Handled
                }
            },
            MinorMode::Match => match ch {
                'm' => {
                    self.clear_pending();
                    if let Some(target) = find_matching_bracket(text, self.head) {
                        self.move_cursor(text, target);
                    }
                    HelixOutcome::Handled
                }
                'i' => {
                    self.minor = Some(MinorMode::MatchInside);
                    HelixOutcome::Handled
                }
                'a' => {
                    self.minor = Some(MinorMode::MatchAround);
                    HelixOutcome::Handled
                }
                's' => {
                    self.minor = Some(MinorMode::SurroundAdd);
                    HelixOutcome::Handled
                }
                'd' => {
                    self.minor = Some(MinorMode::SurroundDelete);
                    HelixOutcome::Handled
                }
                'r' => {
                    self.minor = Some(MinorMode::SurroundReplace(None));
                    HelixOutcome::Handled
                }
                _ => {
                    self.clear_pending();
                    HelixOutcome::Handled
                }
            },
            MinorMode::MatchInside | MinorMode::MatchAround => {
                let around = minor == MinorMode::MatchAround;
                self.clear_pending();
                if let Some(range) = text_object_range(text, self.head, ch, around) {
                    self.anchor = range.start;
                    self.head = range.end;
                }
                HelixOutcome::Handled
            }
            MinorMode::SurroundAdd => {
                self.clear_pending();
                let range = self.effective_range(text);
                let (open, close) = surround_pair(ch);
                let wrapped = format!("{open}{}{close}", &text[range.clone()]);
                let new_end = range.start + wrapped.len();
                HelixOutcome::Edit {
                    new_anchor: range.start,
                    new_head: new_end,
                    range,
                    new_text: wrapped,
                    yank_to_clipboard: None,
                }
            }
            MinorMode::SurroundDelete => {
                self.clear_pending();
                let (open, close) = surround_pair(ch);
                if let Some((outer, inner)) = find_enclosing_pair(text, self.head, open, close) {
                    let inner_text = text[inner].to_string();
                    let new_end = outer.start + inner_text.len();
                    HelixOutcome::Edit {
                        new_anchor: outer.start,
                        new_head: new_end,
                        range: outer,
                        new_text: inner_text,
                        yank_to_clipboard: None,
                    }
                } else {
                    HelixOutcome::Handled
                }
            }
            MinorMode::SurroundReplace(None) => {
                self.minor = Some(MinorMode::SurroundReplace(Some(ch)));
                HelixOutcome::Handled
            }
            MinorMode::SurroundReplace(Some(from_ch)) => {
                self.clear_pending();
                let (open_from, close_from) = surround_pair(from_ch);
                let (open_to, close_to) = surround_pair(ch);
                if let Some((outer, inner)) =
                    find_enclosing_pair(text, self.head, open_from, close_from)
                {
                    let replaced = format!("{open_to}{}{close_to}", &text[inner]);
                    let new_end = outer.start + replaced.len();
                    HelixOutcome::Edit {
                        new_anchor: outer.start,
                        new_head: new_end,
                        range: outer,
                        new_text: replaced,
                        yank_to_clipboard: None,
                    }
                } else {
                    HelixOutcome::Handled
                }
            }
            MinorMode::View => {
                self.clear_pending();
                match ch {
                    'z' | 'c' => HelixOutcome::Host(HelixHostCommand::AlignView(ViewAlign::Center)),
                    't' => HelixOutcome::Host(HelixHostCommand::AlignView(ViewAlign::Top)),
                    'b' => HelixOutcome::Host(HelixHostCommand::AlignView(ViewAlign::Bottom)),
                    _ => HelixOutcome::Handled,
                }
            }
            MinorMode::Space => {
                self.clear_pending();
                match ch {
                    'f' => HelixOutcome::Host(HelixHostCommand::QuickOpenFile),
                    '/' => HelixOutcome::Host(HelixHostCommand::GlobalSearch),
                    's' => HelixOutcome::Host(HelixHostCommand::GoToSymbol),
                    'r' => HelixOutcome::Host(HelixHostCommand::RenameSymbol),
                    'a' => HelixOutcome::Host(HelixHostCommand::CodeActions),
                    'd' => HelixOutcome::Host(HelixHostCommand::ToggleDiagnostics),
                    'w' => {
                        self.minor = Some(MinorMode::Window);
                        HelixOutcome::Handled
                    }
                    'y' => {
                        let range = self.effective_range(text);
                        let yanked = text[range].to_string();
                        let clipboard = self.registers.write(Some('+'), yanked);
                        HelixOutcome::Yank { clipboard }
                    }
                    'p' => {
                        let content = self.registers.read(Some('+'), system_clipboard);
                        if content.is_empty() {
                            return HelixOutcome::Handled;
                        }
                        let eff = self.effective_range(text);
                        let end = eff.end + content.len();
                        HelixOutcome::Edit {
                            range: eff.end..eff.end,
                            new_text: content,
                            new_anchor: eff.end,
                            new_head: end,
                            yank_to_clipboard: None,
                        }
                    }
                    _ => HelixOutcome::Handled,
                }
            }
            MinorMode::Window => {
                self.clear_pending();
                match ch {
                    'w' | 'h' | 'l' | 'j' | 'k' => {
                        HelixOutcome::Host(HelixHostCommand::WindowFocusOther)
                    }
                    'v' | 's' => HelixOutcome::Host(HelixHostCommand::WindowSplitRight),
                    'q' => HelixOutcome::Host(HelixHostCommand::WindowCloseActive),
                    _ => HelixOutcome::Handled,
                }
            }
            MinorMode::Register => {
                self.selected_register = Some(ch);
                HelixOutcome::Handled
            }
            MinorMode::FindChar { forward, till } => {
                let count = self.take_count();
                if let Some(target) = find_char_motion(text, self.head, ch, forward, till, count) {
                    self.select_span(text, self.head, target);
                }
                HelixOutcome::Handled
            }
            MinorMode::ReplaceChar => {
                self.clear_pending();
                let range = self.effective_range(text);
                if range.is_empty() {
                    return HelixOutcome::Handled;
                }
                let replaced: String = text[range.clone()]
                    .chars()
                    .map(|c| if c == '\n' || c == '\r' { c } else { ch })
                    .collect();
                let new_end = range.start + replaced.len();
                HelixOutcome::Edit {
                    new_anchor: range.start,
                    new_head: new_end,
                    range,
                    new_text: replaced,
                    yank_to_clipboard: None,
                }
            }
            MinorMode::BracketLeft | MinorMode::BracketRight => {
                let next = minor == MinorMode::BracketRight;
                self.clear_pending();
                match ch {
                    'c' | 'g' => HelixOutcome::Host(HelixHostCommand::StepGitChange { next }),
                    'b' => HelixOutcome::Host(HelixHostCommand::CycleBuffer { next }),
                    _ => HelixOutcome::Handled,
                }
            }
        }
    }
}

/// Extract the effective printable character string from a `Keystroke`,
/// respecting `key_char` or `shift` on ASCII letters.
pub fn resolved_key_char(ks: &Keystroke) -> Option<String> {
    if let Some(kc) = ks.key_char.as_deref()
        && !kc.is_empty()
        && !kc.chars().any(char::is_control)
    {
        return Some(kc.to_string());
    }
    if ks.key == "space" {
        return Some(" ".to_string());
    }
    if ks.key.len() == 1 {
        let ch = ks.key.chars().next().unwrap();
        if ks.modifiers.shift && ch.is_ascii_lowercase() {
            return Some(ch.to_ascii_uppercase().to_string());
        }
        return Some(ch.to_string());
    }
    None
}

/// Helper to construct a `Keystroke` in unit tests.
#[cfg(test)]
pub fn make_keystroke(key: &str, shift: bool, control: bool, alt: bool) -> Keystroke {
    Keystroke {
        modifiers: Modifiers {
            control,
            alt,
            shift,
            platform: false,
            function: false,
        },
        key: key.to_string(),
        key_char: if !control && !alt && key.len() == 1 {
            let c = key.chars().next().unwrap();
            Some((if shift { c.to_ascii_uppercase() } else { c }).to_string())
        } else {
            None
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(hx: &mut HelixState, text: &mut String, seq: &str) -> Option<HelixHostCommand> {
        let mut last_host = None;
        for ch in seq.chars() {
            let (key_str, shift) = if ch == ' ' {
                ("space".to_string(), false)
            } else if ch.is_ascii_uppercase() {
                (ch.to_ascii_lowercase().to_string(), true)
            } else {
                (ch.to_string(), false)
            };
            let ks = make_keystroke(&key_str, shift, false, false);
            match hx.handle_keystroke(&ks, text, SurfaceKind::CodeEditor, None) {
                HelixOutcome::Edit {
                    range,
                    new_text,
                    new_anchor,
                    new_head,
                    ..
                } => {
                    text.replace_range(range, &new_text);
                    hx.anchor = new_anchor;
                    hx.head = new_head;
                }
                HelixOutcome::Host(cmd) => {
                    last_host = Some(cmd);
                }
                _ => {}
            }
        }
        last_host
    }

    #[test]
    fn word_motions_select_traversed_span_and_delete_works() {
        let mut hx = HelixState::new(HelixMode::Normal);
        let mut text = "hello world_two three".to_string();
        press(&mut hx, &mut text, "w");
        assert_eq!(&text[hx.selection_range(&text)], "hello ");
        press(&mut hx, &mut text, "d");
        assert_eq!(text, "world_two three");
        assert_eq!(hx.registers.read(None, None), "hello ");
    }

    #[test]
    fn line_selection_x_selects_and_extends_full_lines() {
        let mut hx = HelixState::new(HelixMode::Normal);
        let mut text = "alpha\nbeta\ngamma\n".to_string();
        press(&mut hx, &mut text, "x");
        assert_eq!(&text[hx.selection_range(&text)], "alpha\n");
        press(&mut hx, &mut text, "x");
        assert_eq!(&text[hx.selection_range(&text)], "alpha\nbeta\n");
        press(&mut hx, &mut text, "d");
        assert_eq!(text, "gamma\n");
    }

    #[test]
    fn match_inside_and_around_text_objects_and_surround() {
        let mut hx = HelixState::new(HelixMode::Normal);
        let mut text = "fn call(foo_bar, baz) {}".to_string();
        hx.head = 9; // inside `foo_bar`
        hx.anchor = 9;
        press(&mut hx, &mut text, "mi(");
        assert_eq!(&text[hx.selection_range(&text)], "foo_bar, baz");

        // Surround add `[` around `foo_bar`
        hx.head = 9;
        hx.anchor = 9;
        press(&mut hx, &mut text, "miwms[");
        assert_eq!(text, "fn call([foo_bar], baz) {}");

        // Surround replace `[` -> `{`
        hx.head = 10;
        hx.anchor = 10;
        press(&mut hx, &mut text, "mr[{");
        assert_eq!(text, "fn call({foo_bar}, baz) {}");

        // Surround delete `{`
        hx.head = 10;
        hx.anchor = 10;
        press(&mut hx, &mut text, "md{");
        assert_eq!(text, "fn call(foo_bar, baz) {}");
    }

    #[test]
    fn named_and_blackhole_registers_preserve_default_register() {
        let mut hx = HelixState::new(HelixMode::Normal);
        let mut text = "first second".to_string();
        press(&mut hx, &mut text, "w\"ay");
        assert_eq!(hx.registers.read(Some('a'), None), "first ");
        assert_eq!(hx.registers.read(None, None), "");

        // Blackhole register `_` does not overwrite register `a` or default
        press(&mut hx, &mut text, "\"_d");
        assert_eq!(hx.registers.read(None, None), "");
        assert_eq!(hx.registers.read(Some('a'), None), "first ");
    }

    #[test]
    fn command_bar_parser_handles_standard_helix_commands() {
        assert_eq!(parse_command_bar(":w"), CommandBarAction::Write(None));
        assert_eq!(
            parse_command_bar(":w src/main.rs"),
            CommandBarAction::Write(Some("src/main.rs".to_string()))
        );
        assert_eq!(
            parse_command_bar(":q!"),
            CommandBarAction::Quit { force: true }
        );
        assert_eq!(
            parse_command_bar(":wq"),
            CommandBarAction::WriteQuit {
                force: false,
                path: None
            }
        );
        assert_eq!(parse_command_bar(":wqa"), CommandBarAction::WriteQuitAll);
        assert_eq!(parse_command_bar(":42"), CommandBarAction::GoToLine(42));
        assert_eq!(parse_command_bar(":vs"), CommandBarAction::VSplit(None));
    }

    #[test]
    fn goto_and_space_minor_modes_emit_host_commands() {
        let mut hx = HelixState::new(HelixMode::Normal);
        let mut text = "one\ntwo\nthree".to_string();
        assert_eq!(
            press(&mut hx, &mut text, "gd"),
            Some(HelixHostCommand::GoToDefinition)
        );
        assert_eq!(
            press(&mut hx, &mut text, " f"),
            Some(HelixHostCommand::QuickOpenFile)
        );
        assert_eq!(
            press(&mut hx, &mut text, "zz"),
            Some(HelixHostCommand::AlignView(ViewAlign::Center))
        );
    }

    #[test]
    fn split_selection_and_search_smartcase() {
        let text = "foo = 1, FOO = 2, bar = 3";
        let r1 = find_next_match(text, "foo", 0, true).unwrap();
        assert_eq!(&text[r1], "FOO");
        let r2 = find_next_match(text, "FOO", 0, true).unwrap();
        assert_eq!(&text[r2], "FOO");

        let splits = split_selection_on_pattern(text, 0..text.len(), r"\d+");
        assert_eq!(splits.len(), 3);
        assert_eq!(&text[splits[0].clone()], "1");
        assert_eq!(&text[splits[1].clone()], "2");
        assert_eq!(&text[splits[2].clone()], "3");
    }

    #[test]
    fn block_cursor_byte_range_handles_multibyte_utf8_and_mid_char_offsets() {
        let md = "1234567中\r\nCodex、WorkBuddy 和 🚀\n";
        let rope = gpui_component::input::Rope::from(md);

        // `head + 8` lands inside '中' (bytes 7..10): must still measure '1' (0..1).
        assert_eq!(block_cursor_byte_range(&rope, 0), 0..1);
        // Directly on 3-byte CJK character '中' (bytes 7..10).
        assert_eq!(block_cursor_byte_range(&rope, 7), 7..10);
        // Mid-character byte offsets inside '中' clamp safely to its start boundary.
        assert_eq!(block_cursor_byte_range(&rope, 8), 7..10);
        assert_eq!(block_cursor_byte_range(&rope, 9), 7..10);
        // Line endings ('\r' and '\n') collapse to zero-width caret range.
        assert_eq!(block_cursor_byte_range(&rope, 10), 10..10);
        assert_eq!(block_cursor_byte_range(&rope, 11), 11..11);

        // Every byte offset (including non-char boundaries and past-EOF) produces valid boundaries.
        for raw in 0..=(md.len() + 16) {
            let range = block_cursor_byte_range(&rope, raw);
            assert!(rope.is_char_boundary(range.start), "start at raw={raw}");
            assert!(rope.is_char_boundary(range.end), "end at raw={raw}");
            assert!(range.start <= range.end);
        }
    }
}

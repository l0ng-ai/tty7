//! The GPUI view that hosts a terminal: owns the backend, pumps PTY events into
//! redraws, translates keystrokes to bytes, and renders the terminal chrome.

use alacritty_terminal::event::Event as AlacEvent;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use gpui::{
    App, ClipboardEntry, ClipboardItem, Context, ExternalPaths, FocusHandle, Focusable, Font,
    KeyDownEvent, Modifiers, MouseButton, MouseDownEvent, Pixels, ScrollDelta, ScrollWheelEvent,
    Window, actions, div, prelude::*, px,
};
use gpui_component::kbd::Kbd;
use gpui_component::menu::ContextMenuExt;
use gpui_component::{ActiveTheme as _, Icon, IconName, Size, h_flex};

use super::TermSize;
use super::cmd_editor::CmdEditor;
use super::completion::{self, CandidateKind, CompletionSession};
use super::element::TerminalElement;
use super::highlight::{self, TokenKind};
use super::hold::{GapHold, Verdict};
use super::remote::RemoteTerminal;
use super::reverse_search::{self, ReverseSearch};
use super::search::SearchState;
use super::typeahead::{RawInput, Typeahead};
use crate::core::actions::{
    CloseActiveTab, NewTab, SendBackTab, SendTab, SplitDown, SplitRight, ToggleMaximizePane,
};
use crate::core::config::{Config, NotifyMode};
use crate::daemon::protocol::ShellSpec;

// Terminal-scoped actions dispatched by the right-click context menu. They route
// to this view via `.on_action` handlers on the terminal surface; tab/split
// actions in the same menu bubble up to `Tty7App` from the focused terminal.
actions!(
    terminal,
    [
        CopyText,
        PasteText,
        SelectAll,
        FindInTerminal,
        ClearScrollback
    ]
);

/// Emitted when the pane's child process has genuinely exited (`exit`,
/// Ctrl-D, a crashed shell) — as opposed to the daemon connection dropping,
/// which keeps the dead pane visible. `Tty7App` subscribes (see
/// `new_terminal`) and closes the pane in response: collapsing its split, or
/// closing the tab when it was the only pane.
pub struct ChildExited;

impl gpui::EventEmitter<ChildExited> for TerminalView {}

/// See `TerminalView::drag_scroll`.
#[derive(Clone, Copy)]
struct DragScroll {
    /// How far past the pane edge the pointer sits, in lines. Positive =
    /// above the top edge (scroll up into history), negative = below.
    overshoot: f32,
    /// Column to keep extending the selection with at the edge row.
    col: usize,
    /// Cell half to anchor the selection end on.
    side: Side,
}

pub struct TerminalView {
    pub terminal: RemoteTerminal,
    /// Daemon-assigned id of the pane this view mirrors. Persisted in the session
    /// so a restart can re-`attach` to the still-running pane (process + scrollback
    /// intact) instead of spawning a fresh shell.
    pub pane_id: u64,
    /// The shell this pane was spawned with when the user picked one from the
    /// new-tab dropdown; `None` for the default shell and for re-attached
    /// panes. In-memory only (not persisted) — held so splits of this pane
    /// inherit the same shell.
    shell_spec: Option<ShellSpec>,
    pub focus_handle: FocusHandle,
    pub font: Font,
    /// Optional distinct base face for bold cells (from `font_family_bold`), with
    /// the same fallback chain as `font`. `None` → synthesize bold from `font`.
    pub font_bold: Option<Font>,
    /// Optional distinct base face for italic cells (from `font_family_italic`).
    /// `None` → synthesize italic from `font`.
    pub font_italic: Option<Font>,
    /// User-configured OpenType features for terminal fonts. `None` preserves
    /// tty7's terminal-safe default (ligatures disabled); `Some` is opt-in.
    font_features: Option<gpui::FontFeatures>,
    pub font_size: Pixels,
    /// Line height as a multiple of `font_size`; the element turns it into the
    /// concrete row height each frame. Sourced from `Config::line_height`.
    pub line_height_mul: f32,
    pub cell_width: Pixels,
    line_height: Pixels,
    selecting: bool,
    /// Auto-scroll state for a selection drag that has crossed the pane's top
    /// or bottom edge; `None` while the pointer is inside. A repeating task
    /// (armed by `select_autoscroll`) keeps scrolling the scrollback and
    /// re-extending the selection while this is `Some`, so the scroll goes on
    /// even when the pointer holds still past the edge — mouse-move events
    /// alone stop the moment the hand does.
    drag_scroll: Option<DragScroll>,
    /// Generation counter for the auto-scroll task. Bumped every time a new
    /// task is armed so a stale task from a just-cancelled edge visit kills
    /// itself instead of doubling the scroll speed when the pointer leaves,
    /// re-enters, and leaves the pane again within one tick.
    drag_scroll_epoch: u64,
    pub title: String,
    /// IME pre-edit (composing) text, e.g. the pinyin shown before a Chinese
    /// candidate is committed. Empty when not composing.
    pub marked_text: String,
    /// Last cell reported to the PTY in mouse-tracking mode, used to suppress
    /// duplicate motion reports while dragging within a single cell.
    last_mouse_cell: Option<(usize, usize)>,
    /// Fractional line debt carried between wheel events on the quantized
    /// paths (mouse-tracking reports, alternate-scroll arrow keys), where the
    /// app consumes whole lines. Trackpads report pixel deltas well under a
    /// line per event; rounding each one separately discards them all and slow
    /// scrolling never moves. Accumulate instead and spend whole lines as they
    /// build up.
    scroll_debt: f32,
    /// Sub-line part of the scrollback position, in lines (`0.0..1.0`). The
    /// emulator's `display_offset` holds the whole lines; together they form a
    /// continuous, pixel-smooth scroll position. The element shifts the whole
    /// grid down by `scroll_frac * line_height` at paint and fills the strip
    /// above with the next older row, so trackpad scrolling moves every frame
    /// instead of snapping line by line. Reset to 0 whenever something jumps
    /// the view (typing, submit, clear).
    pub(super) scroll_frac: f32,
    /// In-progress incremental search (Cmd+F), if the search bar is open.
    pub search: Option<SearchState>,
    /// Whether the block cursor is in its "on" (drawn) phase. Toggled by the
    /// blink task while focused, and forced back to `true` on input / focus so
    /// the cursor never lingers in the hidden phase right after the user acts.
    pub cursor_visible: bool,
    /// Whether this terminal currently holds keyboard focus. Kept in sync via
    /// focus listeners so the blink task pauses while unfocused (where the
    /// cursor is drawn as a hollow box instead of blinking).
    pub focused: bool,
    /// Whether the search field currently holds focus. Kept in sync from the
    /// field's `Focus`/`Blur` events; lets Escape close the bar while focused and
    /// keeps Escape feeding the PTY when the terminal is focused.
    /// `pub(super)` so the search code in `terminal::search` can mirror focus.
    pub(super) search_focused: bool,
    /// True for a brief window after a bell event; drives a momentary visual
    /// flash painted in place of an audible beep.
    pub bell_flash: bool,
    /// Last observed "shell is idle at its prompt" state, tracked so a change can
    /// trigger a redraw (showing/hiding the line editor) even when the shell
    /// produced no output to repaint on its own.
    last_at_prompt: bool,
    /// When a foreground command is running, the instant it started and the tab
    /// title captured then — used to fire a "command finished" notification for
    /// long-running commands completed while the window is in the background.
    running_since: Option<std::time::Instant>,
    running_title: String,
    /// The inline command line editor. Live only while the shell sits idle
    /// at its prompt (`input_active`): there the terminal keeps keyboard focus and
    /// we run our own line editor (so we own Tab / ↑ / ↓ for completion and
    /// history, which a focused `InputState` would otherwise claim). On Enter the
    /// whole edited line is shipped to the PTY at once. While a command runs (or on
    /// the alternate screen) it's hidden and keys feed the PTY directly.
    cmd: CmdEditor,
    /// Reconstruction of input typed while the line editor is disengaged —
    /// shell startup (rc sourcing) and the gap while every command runs. Those
    /// keys bypass the editor, queue in the TTY, and zle consumes them at the
    /// next prompt as un-editable strays that the editor overlay then
    /// double-draws over. Drained (^U + editor seed) once the editor is live
    /// *and* zle is reading (`zle_reading`). See `typeahead` module docs.
    typeahead: Typeahead,
    /// Short client-side hold for reconstructable gap input: a fast command's
    /// typeahead goes straight to the editor without ever touching the PTY
    /// (no kernel echo, no wipe); a lapsed window (`HOLD_WINDOW`) releases the
    /// bytes for whatever reads stdin. See `hold` module docs.
    hold: GapHold,
    /// Commands submitted this session, oldest first — the source for ↑/↓ recall
    /// and Ctrl+R search (both of which want strict chronological order).
    history: Vec<String>,
    /// How many times each history line has been run (across the shell histories,
    /// tty7's own file, and this session). The frequency half of the frecency
    /// ranking; kept in step with `history` on submit.
    history_counts: std::collections::HashMap<String, u32>,
    /// For each history line, the set of directories it was run in — the
    /// current-directory half of the frecency ranking, so commands used *here*
    /// float up. Kept in step with `history` on submit.
    history_cwds: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// `history` re-ordered by frecency (frequency × recency + a current-directory
    /// bonus), most relevant first. Drives the ghost-text autosuggestion — the
    /// sole whole-line recall surface besides Ctrl+R (the Tab menu stays
    /// history-free). Recomputed when a command is run or the working directory
    /// changes.
    history_ranked: Vec<String>,
    /// The directory `history_ranked` was last computed for, so the polling loop
    /// only re-ranks when the working directory actually changes.
    ranked_cwd: Option<std::path::PathBuf>,
    /// Current position while navigating history with ↑/↓: `Some(i)` indexes
    /// `history`; `None` means we're editing a fresh line (past the newest entry).
    history_nav: Option<usize>,
    /// The in-progress line saved when history navigation starts, so pressing ↓
    /// past the newest entry restores what the user was typing.
    history_stash: String,
    /// Open Tab-completion menu, if any — a picker over the candidates gathered
    /// when it opened. Typing/Backspace re-filter it in place; it closes on
    /// accept, on Escape, or once the edited word no longer matches anything.
    completion: Option<CompletionSession>,
    /// Active Ctrl+R reverse-history search, if any. While set, the editor shows a
    /// `(reverse-i-search)` prompt instead of the line: typing edits the query,
    /// Ctrl+R steps to older matches, Enter accepts the match into the line, and
    /// Escape/Ctrl+G cancels.
    reverse_search: Option<ReverseSearch>,
    /// True while a left-drag that began on the command-editor line is in progress,
    /// so mouse-move extends the editor selection rather than the terminal's.
    editor_selecting: bool,
    /// The URL currently under the mouse (an OSC 8 hyperlink or a bare URL found
    /// in the row text), if any. Drives the hover underline and the pointing-hand
    /// cursor that mark a link as clickable. Stored in scroll-stable grid
    /// coordinates so it survives a scroll without a fresh mouse-move; see
    /// [`HoveredLink`].
    pub(super) hovered_link: Option<HoveredLink>,
    /// Focus listeners kept alive for the lifetime of the view.
    _focus_subs: Vec<gpui::Subscription>,
}

/// A link under the mouse, remembered so the grid can underline its cells. The
/// `line` is the alacritty grid line (display row minus the scroll offset), which
/// stays fixed as the viewport scrolls; `start..=end` are the inclusive columns
/// the link's text spans on that line.
#[derive(Clone, PartialEq)]
pub(super) struct HoveredLink {
    pub line: i32,
    pub start: usize,
    pub end: usize,
}

/// Outcome of a ⌘ shortcut at the terminal surface — the three control-flow
/// paths the key dispatcher needs. Splitting the ⌘ block into its own method
/// keeps `on_key_down` readable; the caller maps each variant back to the
/// stop-propagation / return / fall-through it originally inlined.
enum CmdKey {
    /// Handled here — stop propagation and return.
    Consumed,
    /// Not ours — return without stopping, so the app shell (new tab, split, …)
    /// gets it.
    Bubble,
    /// Recognized but not applicable (e.g. ⌘C with no selection) — fall through
    /// to the editor / PTY paths below.
    FallThrough,
}

/// A foreground command must run at least this long for its completion to be
/// worth a background notification.
const LONG_COMMAND: std::time::Duration = std::time::Duration::from_secs(10);

/// How long gap input may be held client-side before it must be released to
/// the PTY (see the `hold` module). Long enough for a fast command's full
/// round trip (`133;D` report back to this client — tens of ms), short enough
/// that typing into a program that reads stdin right after launch feels
/// instant once the window lapses.
const HOLD_WINDOW: std::time::Duration = std::time::Duration::from_millis(150);

/// Post a desktop notification that a command finished. Best-effort and
/// non-blocking: routed through [`super::remote::notify_desktop`] (the single
/// `notify-rust` entry point shared with the escape-sequence path), so there's no
/// `osascript` subprocess and every notification goes through one code path.
fn notify_command_finished(label: &str, elapsed: std::time::Duration) {
    let secs = elapsed.as_secs();
    let label = label.trim();
    let body = if label.is_empty() {
        format!("Command finished after {secs}s")
    } else {
        format!("{label} — finished after {secs}s")
    };
    super::remote::notify_desktop(Some("tty7"), &body);
}

/// Build the byte sequence written to the PTY for a paste. Under bracketed paste
/// the content is wrapped in the `ESC[200~` / `ESC[201~` markers, and every ESC
/// (`0x1b`) byte is stripped from the content first. Without that strip, clipboard
/// text carrying its own `ESC[201~` end-marker could terminate the paste early and
/// have whatever follows (e.g. a newline + command) run as ordinary typed input —
/// a "bracketed-paste escape" that defeats the very protection the markers give
/// the shell. Removing ESC makes an embedded `ESC[201~` unrepresentable, matching
/// alacritty's own paste filtering. `0x1b` is ASCII, so it never appears inside a
/// multi-byte UTF-8 char — filtering the byte stream can't split a codepoint.
/// Legitimate pasted text does not contain raw ESC, so this is a no-op for it.
///
/// Without bracketed paste, line breaks are normalized to `\r` — the byte the
/// Enter key sends — matching xterm/alacritty. A raw-mode app (the only
/// consumer of this path, since the prompt routes pastes into the editor)
/// reads keys, not lines, and many bind accept/submit to CR only; leaving `\n`
/// in would feed them a byte no keyboard produces.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend(text.bytes().filter(|&b| b != 0x1b));
        bytes.extend_from_slice(b"\x1b[201~");
        bytes
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

/// Strip trailing spaces/tabs from every line, preserving the line structure
/// (and any final newline). Used by copy when `clipboard_trim_trailing_spaces`
/// is on so selections don't carry cell-padding whitespace.
fn trim_trailing_spaces(text: &str) -> String {
    // `split('\n')` keeps empty segments, so a trailing newline round-trips (the
    // final empty segment re-joins into it) and a string without one gains none.
    text.split('\n')
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Backslash-escape the shell-significant characters in a filesystem path so a
/// pasted filename with spaces (or `$`, `'`, `(`, `&`…) reaches the shell as a
/// single argument instead of splitting. Mirrors how macOS Terminal.app and
/// Warp turn a dropped/pasted file into command-line text. An empty path
/// becomes `''`.
///
/// A newline/CR can't be backslash-escaped into a literal (`\<newline>` is a
/// shell line-continuation), so a pathological filename containing one is
/// single-quoted whole instead.
fn shell_escape_path(path: &str) -> String {
    if path.is_empty() {
        return "''".to_string();
    }
    if path.contains(['\n', '\r']) {
        // Close/re-open the single quote around each embedded `'`.
        return format!("'{}'", path.replace('\'', "'\\''"));
    }
    let mut out = String::with_capacity(path.len() + 8);
    for ch in path.chars() {
        if matches!(
            ch,
            ' ' | '\t'
                | '"'
                | '\''
                | '\\'
                | '$'
                | '`'
                | '#'
                | '='
                | '!'
                | '~'
                | '['
                | ']'
                | '{'
                | '}'
                | '('
                | ')'
                | '<'
                | '>'
                | '|'
                | ';'
                | '*'
                | '?'
                | '&'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Decide what text a paste should insert for a clipboard item.
///
/// When the clipboard holds file references — a Finder "Copy" carries
/// `ExternalPaths` and (usually) no string rep — we shell-escape each path and
/// join them with a single space, so pasting a file drops a ready-to-use,
/// space-safe path (multiple files → space-separated args), matching Warp and
/// macOS Terminal.app. gpui's own `ClipboardItem::text()` would instead
/// concatenate the paths with *no* separator and never escape them.
///
/// Otherwise (plain text, or an image with no text) we defer to `text()`.
fn clipboard_paste_text(item: &ClipboardItem) -> Option<String> {
    let escaped: Vec<String> = item
        .entries()
        .iter()
        .filter_map(|e| match e {
            ClipboardEntry::ExternalPaths(paths) => Some(paths.paths()),
            _ => None,
        })
        .flatten()
        .map(|p| shell_escape_path(&p.to_string_lossy()))
        .collect();
    if !escaped.is_empty() {
        return Some(escaped.join(" "));
    }
    item.text()
}

/// The font fallback chain: the user's configured list with the bundled "Hack"
/// pinned to the end. Hack ships inside the binary (`register_bundled_fonts`)
/// and covers the symbols prompt themes lean on — `❯`, `➜`, box drawing, the
/// sharp powerline wedges — with ink that fits a monospace advance. Without
/// this anchor, a custom `font_family` that lacks one of those codepoints
/// falls through the whole configured list into the OS cascade, which happily
/// serves a proportional glyph wider than the cell that `paint_glyphs`'
/// per-cell clip then truncates (issue #17's severed `➜`).
fn fallback_chain(family: &str, configured: &[String]) -> Vec<String> {
    let mut chain = configured.to_vec();
    if family != "Hack" && !chain.iter().any(|f| f == "Hack") {
        chain.push("Hack".to_string());
    }
    chain
}

impl TerminalView {
    pub fn new(
        working_directory: Option<std::path::PathBuf>,
        restore_pane: Option<u64>,
        shell: Option<ShellSpec>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<Self> {
        // Provisional size; corrected on the first prepaint once we can measure.
        // The PTY lives in the daemon now. On session restore (`restore_pane`),
        // re-`attach` to the still-running pane so its process + scrollback come
        // back intact; otherwise `spawn` a fresh pane (with the caller's shell
        // pick, if any). The caller only passes a `restore_pane` it has already
        // confirmed alive, so we trust it here.
        let (terminal, pane_id, shell_spec) = match restore_pane {
            Some(id) => (
                RemoteTerminal::attach(TermSize::new(80, 24), 8, 17, id)?,
                id,
                // An attached pane keeps whatever shell it already runs; the
                // pick that spawned it (if any) isn't persisted.
                None,
            ),
            None => {
                let (terminal, id) = RemoteTerminal::spawn(
                    TermSize::new(80, 24),
                    8,
                    17,
                    working_directory,
                    shell.clone(),
                )?;
                (terminal, id, shell)
            }
        };
        let mut view = Self::with_terminal(terminal, pane_id, window, cx);
        view.shell_spec = shell_spec;
        Ok(view)
    }

    /// Build the view around an already-connected terminal. Split from [`new`]
    /// so tests can hand in a `RemoteTerminal` backed by a plain socketpair
    /// and exercise the event plumbing without a live daemon.
    fn with_terminal(
        terminal: RemoteTerminal,
        pane_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Font comes from user config: a primary face plus fallbacks so glyphs it
        // lacks still render (e.g. a Nerd Font supplies powerline / box separators
        // and an emoji face covers pictographs). Defaults are Menlo + Hasklug Nerd
        // Font Mono + Apple Color Emoji at 13px.
        let config = cx.global::<Config>();
        let font_family = config.font_family.clone();
        let fallbacks = fallback_chain(&font_family, &config.font_fallbacks);
        let font_size = px(config.font_size);
        let line_height_mul = config.line_height;
        let font_features = config.font_features.clone();
        let mut font = gpui::font(font_family);
        font.fallbacks = Some(gpui::FontFallbacks::from_fonts(fallbacks.clone()));
        if let Some(features) = &font_features {
            font.features = features.clone();
        }
        // Optional distinct bold/italic faces, each carrying the same fallback
        // chain so glyph coverage matches the primary face.
        let alt_font = |family: &Option<String>| {
            family.as_ref().map(|f| {
                let mut af = gpui::font(f.clone());
                af.fallbacks = Some(gpui::FontFallbacks::from_fonts(fallbacks.clone()));
                if let Some(features) = &font_features {
                    af.features = features.clone();
                }
                af
            })
        };
        let font_bold = alt_font(&config.font_family_bold);
        let font_italic = alt_font(&config.font_family_italic);

        let focus_handle = cx.focus_handle();

        // Pump backend events → redraws. The reader thread sends one Wakeup per
        // output chunk, and a TUI redrawing at full tilt (Claude Code streaming)
        // produces long bursts of them; drain whatever queued up behind the
        // first event and collapse the Wakeups to one, so a burst costs one
        // update+notify instead of scheduling dozens of no-op round-trips
        // between two frames.
        let events = terminal.events.clone();
        cx.spawn(async move |this, cx| {
            let mut batch = Vec::new();
            while let Ok(ev) = events.recv().await {
                batch.push(ev);
                while let Ok(ev) = events.try_recv() {
                    batch.push(ev);
                }
                let res = this.update(cx, |view, cx| {
                    let mut woke = false;
                    for ev in batch.drain(..) {
                        // A Wakeup only marks the view dirty, so one per batch
                        // is enough; order relative to other events is moot.
                        if matches!(ev, AlacEvent::Wakeup) && std::mem::replace(&mut woke, true) {
                            continue;
                        }
                        view.handle_event(ev, cx);
                    }
                    woke
                });
                let woke = match res {
                    Ok(woke) => woke,
                    Err(_) => break,
                };
                // `notify()` above only dirties windows whose tracked-entity set
                // still contains this view; if one frame drops the view from
                // that set, every later notify is filtered, the window never
                // goes dirty, never redraws, and so never re-tracks the view —
                // grid updates then sit unseen until some input event forces a
                // refresh. Dirty the view's current window directly so PTY
                // output always reaches the screen; painting stays vsync-paced,
                // so a batch costs the same one frame either way. Failure here
                // only means no window right now — never tear down the pump.
                if woke {
                    let _ = this.update_in(cx, |_, window, _| window.refresh());
                }
            }
        })
        .detach();

        // Track focus so the cursor blinks only while focused, resetting the
        // blink phase on focus changes so it's solid the instant focus returns.
        // Focus changes are also reported to the app when it asked for them
        // (mode 1004): vim's autoread, tmux's focus hooks and prompt
        // frameworks' cursor dimming all key off `CSI I`/`CSI O`.
        let focus_subs = vec![
            cx.on_focus_in(&focus_handle, window, |view, _window, cx| {
                view.focused = true;
                view.cursor_visible = true;
                view.report_focus_change(true);
                cx.notify();
            }),
            cx.on_blur(&focus_handle, window, |view, _window, cx| {
                view.focused = false;
                view.report_focus_change(false);
                cx.notify();
            }),
        ];

        // Blink the block cursor. Toggling and the redraw happen only while
        // focused; unfocused we draw a static hollow box and skip the work.
        // The task stops naturally once the view is dropped (update → Err).
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(530))
                    .await;
                if this
                    .update(cx, |view, cx| {
                        // The search field blinks its own caret; here we only drive
                        // the terminal's block cursor.
                        if view.focused {
                            // Honor `cursor_blink`: when off, keep the cursor
                            // solid (force it visible if a prior toggle left it
                            // hidden) instead of flipping it.
                            if cx.global::<Config>().cursor_blink {
                                view.cursor_visible = !view.cursor_visible;
                                cx.notify();
                            } else if !view.cursor_visible {
                                view.cursor_visible = true;
                                cx.notify();
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        // Poll the PTY's foreground process group once a second to notice when a
        // long-running command finishes while the window is in the background,
        // and post a desktop notification. `update_in` gives us the Window so we
        // can check whether it's currently active.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(300))
                    .await;
                if this
                    .update_in(cx, |view, window, cx| view.poll_foreground(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        window.focus(&focus_handle, cx);

        // Rank without a directory bias for now; the first `poll_foreground` learns
        // the cwd and re-ranks (favouring commands run in this directory).
        let history = super::history::load();
        let history_ranked = super::history::rank_by_frecency(
            &history.entries,
            &history.counts,
            &history.cwds,
            None,
        );

        Self {
            terminal,
            pane_id,
            shell_spec: None,
            focus_handle,
            font,
            font_bold,
            font_italic,
            font_features,
            font_size,
            line_height_mul,
            cell_width: px(8.),
            line_height: px(17.),
            selecting: false,
            drag_scroll: None,
            drag_scroll_epoch: 0,
            title: "tty7".to_string(),
            marked_text: String::new(),
            last_mouse_cell: None,
            scroll_debt: 0.,
            scroll_frac: 0.,
            search: None,
            cursor_visible: true,
            focused: true,
            search_focused: false,
            bell_flash: false,
            last_at_prompt: false,
            running_since: None,
            running_title: String::new(),
            cmd: CmdEditor::new(),
            typeahead: Typeahead::new(),
            hold: GapHold::new(),
            history: history.entries,
            history_counts: history.counts,
            history_cwds: history.cwds,
            history_ranked,
            ranked_cwd: None,
            history_nav: None,
            history_stash: String::new(),
            completion: None,
            reverse_search: None,
            editor_selecting: false,
            hovered_link: None,
            _focus_subs: focus_subs,
        }
    }

    /// Called from the element each frame with the measured grid geometry.
    pub fn set_grid_size(
        &mut self,
        cols: usize,
        rows: usize,
        cell_width: Pixels,
        line_height: Pixels,
    ) {
        self.cell_width = cell_width;
        self.line_height = line_height;
        self.terminal.resize(
            TermSize::new(cols, rows),
            cell_width.as_f32().round() as u16,
            line_height.as_f32().round() as u16,
        );
    }

    /// Current working directory of this terminal's foreground process, used so
    /// new tabs / splits can open in the same place. `None` if it can't be read.
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        self.terminal.foreground_cwd()
    }

    /// The shell this pane was explicitly spawned with (new-tab dropdown pick),
    /// so splits can inherit it. `None` → the default shell.
    pub fn shell_spec(&self) -> Option<ShellSpec> {
        self.shell_spec.clone()
    }

    fn handle_event(&mut self, ev: AlacEvent, cx: &mut Context<Self>) {
        // Surface a child-exit/daemon-disconnect noticed by the reader thread into
        // the field the view reads directly (`self.terminal.exited`).
        self.terminal.poll_exited();
        match ev {
            AlacEvent::Wakeup => cx.notify(),
            AlacEvent::Title(title) => {
                self.title = title;
                cx.notify();
            }
            AlacEvent::ResetTitle => {
                self.title = "tty7".to_string();
                cx.notify();
            }
            AlacEvent::PtyWrite(text) => self.terminal.write(text.into_bytes()),
            AlacEvent::ChildExit(_) | AlacEvent::Exit => {
                self.terminal.exited = true;
                self.title = "tty7 — process exited".to_string();
                // A genuine child exit closes the pane (the app subscribes and
                // collapses the split / closes the tab). A daemon disconnect
                // reaches this same arm but must NOT auto-close: the session
                // may still be alive daemon-side, and closing would both hide
                // the failure and kill the pane.
                if self.terminal.child_exited() {
                    cx.emit(ChildExited);
                }
                cx.notify();
            }
            AlacEvent::ClipboardStore(_, text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            AlacEvent::ClipboardLoad(_, fmt) => {
                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    self.terminal.write(fmt(&text).into_bytes());
                }
            }
            AlacEvent::ColorRequest(idx, fmt) => {
                // OSC 10/11/12 query the default foreground/background/cursor as
                // the special indices 256/257/258, which live *outside* the
                // 256-color palette. The old `idx.min(255)` clamped them all to
                // palette[255] (near-white), so apps probing the background to
                // pick a light/dark UI (e.g. Claude Code) saw a "light" terminal
                // and switched to a washed-out light theme. Reply with the real
                // theme colors instead.
                let theme = cx.theme();
                let rgb = match idx {
                    256 => super::palette::hsla_to_rgb(theme.foreground),
                    257 => super::palette::hsla_to_rgb(theme.background),
                    258 => super::palette::hsla_to_rgb(theme.caret),
                    i => self.terminal.palette[i.min(255)],
                };
                self.terminal.write(fmt(rgb).into_bytes());
            }
            AlacEvent::Bell => {
                // Visual bell: a brief flash instead of an audible beep. Turn it
                // on now, then schedule a one-shot task to clear it ~150ms later.
                self.bell_flash = true;
                cx.notify();
                cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(150))
                        .await;
                    let _ = this.update(cx, |view, cx| {
                        view.bell_flash = false;
                        cx.notify();
                    });
                })
                .detach();
            }
            AlacEvent::TextAreaSizeRequest(fmt) => {
                // CSI 14 t: the text area size in pixels. Image-preview TUIs
                // (yazi, ranger's chafa/sixel backends) size their graphics
                // from this reply; ignoring the request leaves them guessing
                // or stalling on a report that never comes.
                let size = self.terminal.size();
                let reply = fmt(alacritty_terminal::event::WindowSize {
                    num_lines: size.rows as u16,
                    num_cols: size.cols as u16,
                    cell_width: self.cell_width.as_f32().round() as u16,
                    cell_height: self.line_height.as_f32().round() as u16,
                });
                self.terminal.write(reply.into_bytes());
            }
            _ => {}
        }
    }

    /// Report a focus change to the application (`CSI I` / `CSI O`) when it
    /// opted into focus events (mode 1004). No-op otherwise.
    fn report_focus_change(&self, focused: bool) {
        let mode = *self.terminal.term.lock().mode();
        if let Some(bytes) = focus_report_bytes(mode, focused) {
            self.terminal.write(bytes);
        }
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal.exited {
            return;
        }
        // macOS Option-key policy (see `input::reshape_option_keystroke`):
        // reshape the chord once, up front, so every consumer below — the ⌘
        // dispatcher, the prompt editor, the raw PTY encoder — sees the same
        // story. Other platforms have no composed-character split to resolve.
        let reshaped = if cfg!(target_os = "macos") {
            super::input::reshape_option_keystroke(
                &ev.keystroke,
                cx.global::<Config>().macos_option_as_alt,
            )
        } else {
            None
        };
        let ks = reshaped.as_ref().unwrap_or(&ev.keystroke);
        let m = &ks.modifiers;

        // While the search field is focused it owns the keyboard — typing, caret
        // movement, selection, Cmd+A and IME are all handled inside the field, and
        // Enter is delivered via its `PressEnter` event. We only intercept Escape
        // to close the bar; any other key that bubbled up here was unhandled, so
        // swallow it rather than leak it to the PTY.
        if self.search.is_some() && self.search_focused {
            if ks.key == "escape" {
                self.close_search(window, cx);
                cx.stop_propagation();
            }
            return;
        }

        // Cmd shortcuts (copy / paste / find / select-all + macOS line editing).
        // Delegated to keep this dispatcher scannable; the outcome decides whether
        // we consume the key, let it bubble to the app shell (new tab / split /
        // switch), or fall through to the editor / PTY paths below.
        if m.platform && !m.control && !m.alt {
            match self.handle_cmd_shortcut(ks, window, cx) {
                CmdKey::Consumed => {
                    cx.stop_propagation();
                    return;
                }
                CmdKey::Bubble => return,
                CmdKey::FallThrough => {}
            }
        }

        // Off macOS there is no reachable Cmd key, so the clipboard trio lives on
        // Ctrl (the Windows/Linux convention). Route only Ctrl+C / Ctrl+V / Ctrl+X
        // to the shared clipboard handler; every other Ctrl chord keeps its shell /
        // readline meaning (Ctrl+Z suspend, Ctrl+R reverse-search, Ctrl+F forward,
        // …). Ctrl+C copies an active selection and otherwise falls through to ^C
        // (SIGINT); Ctrl+X cuts a prompt selection; Ctrl+V pastes.
        if cfg!(not(target_os = "macos"))
            && m.control
            && !m.platform
            && !m.alt
            && matches!(ks.key.as_str(), "c" | "v" | "x")
        {
            match self.handle_cmd_shortcut(ks, window, cx) {
                CmdKey::Consumed => {
                    cx.stop_propagation();
                    return;
                }
                CmdKey::Bubble | CmdKey::FallThrough => {}
            }
        }

        // Off macOS the "secondary" modifier is Ctrl, so Ctrl+1..9 switches tabs at
        // the app shell (mirroring macOS's Cmd+1..9, which bubbles via the platform
        // branch above). Those digit chords have no terminal meaning, so return
        // without consuming the event — letting it bubble to the root `on_key_down`
        // handler — instead of being swallowed by the editor / PTY paths below.
        if cfg!(not(target_os = "macos"))
            && m.control
            && !m.platform
            && !m.alt
            && matches!(
                ks.key.as_str(),
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"
            )
        {
            return;
        }

        // While idle at the prompt, our local command editor owns the keyboard:
        // editing keys act on the in-memory line and Enter ships it to the PTY.
        // Printable text is delivered through the IME path (`commit_text`), so we
        // only handle the non-text keys here and consume everything else (so it
        // never leaks to the PTY as a raw byte).
        if self.input_active() {
            self.handle_editor_key(ks, cx);
            cx.stop_propagation();
            return;
        }

        let kitty = self.kitty_flags();
        if let Some(bytes) = super::input::keystroke_to_bytes(ks, kitty) {
            let plain = !m.control && !m.alt && !m.platform;
            // A plain Backspace is reconstructable gap input: offer it to the
            // hold, so a fast command's typeahead never touches the PTY (see
            // `hold`). Anything else releases the hold first — FIFO order on
            // the wire — and goes raw, kept in step with the typeahead record
            // for the deferred wipe.
            let held = plain
                && ks.key == "backspace"
                && self.gap_holdable()
                && match self.hold.hold_backspace(&bytes) {
                    Verdict::Held(arm) => {
                        if let Some(epoch) = arm {
                            self.arm_hold_timer(epoch, cx);
                        }
                        true
                    }
                    Verdict::Passthrough => false,
                };
            if !held {
                self.release_hold();
                self.terminal.write(bytes);
                self.typeahead.observe(
                    RawInput::Key {
                        key: ks.key.as_str(),
                        plain,
                    },
                    self.on_alt_screen(),
                );
            }
            // Keep the cursor solid while typing (resets the blink phase).
            self.cursor_visible = true;
            // Typing clears the selection and jumps to the prompt.
            let mut term = self.terminal.term.lock();
            term.selection = None;
            term.scroll_display(Scroll::Bottom);
            self.scroll_frac = 0.;
            drop(term);
            cx.notify();
            // Consume so the key isn't also re-sent through the IME path.
            cx.stop_propagation();
        }
    }

    /// Handle a ⌘ shortcut at the terminal surface and report what the dispatcher
    /// should do with the key (see [`CmdKey`]). Covers copy / cut / paste / find /
    /// select-all plus the macOS editor line-editing chords (⌘Z, ⌘←/→, ⌘⌫), all of
    /// which only act at the prompt. Behavior is identical to the inline block it
    /// replaced; only the stop-propagation / return plumbing moved to the caller.
    fn handle_cmd_shortcut(
        &mut self,
        ks: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> CmdKey {
        let m = &ks.modifiers;
        match ks.key.as_str() {
            "c" => {
                // At the prompt, ⌘C copies the editor's selection — but only when
                // the editor actually has one. With no editor selection we must NOT
                // swallow the key: the user may have mouse-selected terminal
                // output/scrollback (which lives in `term.selection`), so fall
                // through to the terminal-selection branch below.
                if self.input_active() {
                    if let Some(text) = self.cmd.selected_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        return CmdKey::Consumed;
                    }
                }
                // Copy the terminal selection, if any; else fall through
                // (Ctrl+C handles SIGINT).
                if self.has_selection() {
                    self.copy_selection(cx);
                    return CmdKey::Consumed;
                }
                CmdKey::FallThrough
            }
            "x" => {
                // Cut: only meaningful in the editor with a selection — copy it
                // out, then delete it. Elsewhere it's a no-op (swallowed).
                if self.input_active() {
                    if let Some(text) = self.cmd.selected_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                        self.cmd.delete_selection();
                        self.completion = None;
                        self.cursor_visible = true;
                        cx.notify();
                    }
                    return CmdKey::Consumed;
                }
                CmdKey::FallThrough
            }
            "v" => {
                if let Some(item) = cx.read_from_clipboard() {
                    if let Some(text) = clipboard_paste_text(&item) {
                        self.paste(text, cx);
                    } else if !self.input_active()
                        && item
                            .entries()
                            .iter()
                            .any(|e| matches!(e, ClipboardEntry::Image(_)))
                    {
                        // Clipboard holds an image (e.g. a screenshot) with no text.
                        // A foreground TUI like Claude Code reads the image from the OS
                        // clipboard itself when it sees Ctrl+V (SYN, 0x16), so forward
                        // that byte instead of trying to send image data — matching how
                        // GUI terminals route Cmd+V image pastes to CLI agents.
                        self.terminal.write(vec![0x16]);
                    }
                }
                CmdKey::Consumed
            }
            "f" => {
                self.open_search(window, cx);
                CmdKey::Consumed
            }
            "a" => {
                // At the prompt, ⌘A selects the whole edited line; otherwise it
                // selects the whole terminal buffer (scrollback included).
                self.select_all_contextual(cx);
                CmdKey::Consumed
            }
            // The following are editor-only (macOS line editing); they're swallowed
            // elsewhere since they have no terminal meaning.
            "z" => {
                if self.input_active() {
                    if m.shift {
                        self.cmd.redo();
                    } else {
                        self.cmd.undo();
                    }
                    self.completion = None;
                    cx.notify();
                }
                CmdKey::Consumed
            }
            "left" => {
                if self.input_active() {
                    self.editor_move_edge(false, m.shift);
                    cx.notify();
                }
                CmdKey::Consumed
            }
            "right" => {
                if self.input_active() {
                    self.editor_move_edge(true, m.shift);
                    cx.notify();
                }
                CmdKey::Consumed
            }
            "backspace" => {
                if self.input_active() {
                    if !self.cmd.delete_selection() {
                        self.cmd.delete_to_start();
                    }
                    self.completion = None;
                    self.cursor_visible = true;
                    cx.notify();
                }
                CmdKey::Consumed
            }
            _ => CmdKey::Bubble,
        }
    }

    /// Handle one keystroke while the local command editor is live at the prompt.
    /// Editing keys and readline-style control combos act on `self.cmd`; Enter
    /// submits; ↑/↓ recall history. Printable text is *not* handled here — it
    /// arrives via the IME path (`commit_text`). Tab is claimed by the `SendTab`
    /// action (reserved for completion), so it never reaches this method.
    fn handle_editor_key(&mut self, ks: &gpui::Keystroke, cx: &mut Context<Self>) {
        let m = &ks.modifiers;
        let key = ks.key.as_str();
        self.cursor_visible = true;

        // A reverse search, when active, owns the keyboard.
        if self.reverse_search.is_some() {
            self.handle_reverse_search_key(ks, cx);
            return;
        }

        // While a completion menu is open it behaves as a picker:
        // ↑/↓ move the highlight, Enter writes the highlighted candidate into
        // the line (a second Enter submits; Cmd+Enter does both in one stroke),
        // Escape just closes — the line keeps any filled prefix. Tab/Shift-Tab
        // (via the SendTab action) fill the common prefix / move the highlight.
        // Typing and Backspace re-filter the menu live; any other key falls
        // through and closes it just below.
        if self.completion.is_some() && !m.control && !m.alt {
            match (m.platform, key) {
                (false, "up") => {
                    self.completion_select(false, cx);
                    return;
                }
                (false, "down") => {
                    self.completion_select(true, cx);
                    return;
                }
                (false, "enter") => {
                    if self
                        .completion
                        .as_ref()
                        .is_some_and(|s| s.selected().is_some())
                    {
                        self.completion_accept(cx);
                    } else {
                        self.completion = None;
                        self.submit_command(cx);
                    }
                    return;
                }
                (true, "enter") => {
                    // Cmd+Enter: accept the highlighted candidate and run it.
                    self.completion_accept(cx);
                    self.submit_command(cx);
                    return;
                }
                (false, "escape") => {
                    self.completion = None;
                    cx.notify();
                    return;
                }
                (false, "backspace") if self.cmd.selection().is_none() && !self.cmd.is_empty() => {
                    self.cmd.backspace();
                    self.completion_refilter();
                    self.cursor_visible = true;
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }

        // Any other editing key closes an open completion menu.
        self.completion = None;

        // Readline-style control combinations, delegated so this dispatcher stays
        // scannable. Every Ctrl chord is swallowed at the prompt (recognized or
        // not), so this always notifies and returns.
        if m.control && !m.platform && !m.alt {
            // Off macOS, Ctrl is the primary modifier, so Ctrl+A is expected to
            // select the whole edited line (text-editor / Windows convention) —
            // there is no reachable Cmd key to carry the macOS `Cmd+A`. macOS keeps
            // the readline `Ctrl+A` = move-to-line-start (its select-all is Cmd+A).
            if cfg!(not(target_os = "macos")) && key == "a" {
                self.cmd.select_all();
                self.completion = None;
                self.cursor_visible = true;
                cx.notify();
                return;
            }
            self.apply_readline_ctrl(key);
            cx.notify();
            return;
        }

        // Readline-style Meta word chords on the edited line: M-b / M-f motions
        // and M-d delete-word, mirroring the Alt+←/→/Delete handling below. On
        // macOS these are reachable only with `macos_option_as_alt` on — with it
        // off the chord composes a character upstream and arrives here altless,
        // through the printable-text arm. Other Alt+letter chords stay swallowed
        // no-ops as before (the local editor can't mirror every zle widget).
        if m.alt && !m.platform && !m.control {
            match key {
                "b" => {
                    self.editor_move_h(false, m.shift, true);
                    cx.notify();
                    return;
                }
                "f" => {
                    self.editor_move_h(true, m.shift, true);
                    cx.notify();
                    return;
                }
                "d" => {
                    if !self.cmd.delete_selection() {
                        self.cmd.delete_word_right();
                    }
                    self.history_nav = None;
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }

        match key {
            "enter" => {
                self.submit_command(cx);
                return;
            }
            "backspace" => {
                // Empty editor: nothing local to delete, but the shell's own
                // line may hold type-ahead the editor never saw (bytes that
                // reached the PTY outside it — e.g. typed into a finishing
                // command). Pass the key through so such strays are always
                // erasable by hand; on a truly empty line it's a shell no-op.
                // An undrained record must mirror the erase (editor active ⇒
                // primary screen, so no alt-screen taint applies).
                if self.cmd.is_empty() {
                    self.terminal.write(vec![0x7f]);
                    self.typeahead.observe(
                        RawInput::Key {
                            key: "backspace",
                            plain: true,
                        },
                        false,
                    );
                    return;
                }
                // backspace() deletes the selection if there is one; only fall
                // back to word-delete when nothing is selected.
                if m.alt && self.cmd.selection().is_none() {
                    self.cmd.delete_word_left();
                } else {
                    self.cmd.backspace();
                }
                self.history_nav = None;
            }
            "delete" => {
                if m.alt {
                    self.cmd.delete_word_right();
                } else {
                    self.cmd.delete();
                }
            }
            "left" => self.editor_move_h(false, m.shift, m.alt),
            "right" => {
                // At end-of-line with a suggestion and no selection, → accepts it.
                if !m.shift && self.cmd.selection().is_none() {
                    if let Some(full) = self.ghost_suggestion() {
                        self.cmd.set(&full);
                        cx.notify();
                        return;
                    }
                }
                self.editor_move_h(true, m.shift, m.alt);
            }
            "home" => self.editor_move_edge(false, m.shift),
            "end" => self.editor_move_edge(true, m.shift),
            "up" => {
                self.history_prev(cx);
                return;
            }
            "down" => {
                self.history_next(cx);
                return;
            }
            "escape" => {
                // Esc carries no local-editor meaning, so pass it straight to the
                // shell — its own zle bindings act on it (vi command mode from
                // `bindkey -v`, `\e`-prefixed widgets, menu-select cancel). Encode
                // through the shared path so Alt-prefixing and the Kitty `CSI 27 u`
                // form stay identical to the raw path; `escape` always encodes, the
                // fallback is just belt-and-braces.
                //
                // Unlike printable text — which the editor mirrors locally and only
                // ships on Enter — a bare control byte leaves nothing on zle's line
                // to reconcile, so it is deliberately NOT fed to `typeahead.observe`:
                // a non-text key taints the record, firing a spurious `^U` on the
                // next flush.
                let bytes = super::input::keystroke_to_bytes(ks, self.kitty_flags())
                    .unwrap_or_else(|| vec![0x1b]);
                self.terminal.write(bytes);
                return;
            }
            // Printable text delivered directly, without an IME round-trip. On
            // macOS printable keys are routed to the IME and arrive via
            // `commit_text`, so they never reach this method. On Linux (where
            // `prefers_ime_for_printable_keys` is false because gpui's IBus path
            // doesn't commit plain ASCII back) they arrive here as ordinary key
            // events carrying `key_char`; feed them through the same commit path
            // the IME would use so the local editor sees the text. Skip control /
            // Cmd chords and any non-printable char (function keys have no
            // `key_char`; Alt combos stay editor no-ops as before).
            _ => {
                if !m.control && !m.platform && !m.alt {
                    if let Some(ch) = ks.key_char.as_deref() {
                        if !ch.is_empty() && ch.chars().all(|c| c >= '\u{20}' && c != '\u{7f}') {
                            self.commit_text(ch, cx);
                            return;
                        }
                    }
                }
            }
        }
        cx.notify();
    }

    /// Apply a readline-style Ctrl chord to the command editor: Ctrl-A/E/B/F
    /// motions (Ctrl-F also accepts the autosuggestion), Ctrl-W/U/K/H deletions
    /// (each removing the selection first if there is one), Ctrl-L clear-screen,
    /// Ctrl-R reverse search, Ctrl-C interrupt, and Ctrl-D EOF/forward-delete.
    /// Unrecognized chords are no-ops (the caller swallows every Ctrl combo at
    /// the prompt regardless).
    fn apply_readline_ctrl(&mut self, key: &str) {
        match key {
            "r" => self.start_reverse_search(),
            "a" => {
                self.cmd.clear_selection();
                self.cmd.move_home();
            }
            "e" => {
                self.cmd.clear_selection();
                self.cmd.move_end();
            }
            "b" => {
                self.cmd.clear_selection();
                self.cmd.move_left();
            }
            "f" => {
                // Accept the autosuggestion if one is showing; else move right.
                if let Some(full) = self.ghost_suggestion() {
                    self.cmd.set(&full);
                } else {
                    self.cmd.clear_selection();
                    self.cmd.move_right();
                }
            }
            // Deletion combos remove the selection first if there is one.
            "w" => {
                if !self.cmd.delete_selection() {
                    self.cmd.delete_word_left();
                }
            }
            "u" => {
                if !self.cmd.delete_selection() {
                    self.cmd.delete_to_start();
                }
            }
            "k" => {
                if !self.cmd.delete_selection() {
                    self.cmd.delete_to_end();
                }
            }
            "h" => self.cmd.backspace(),
            "l" => {
                // Clear screen belongs to the shell/readline layer: send the
                // same form-feed byte the raw terminal path emits for Ctrl+L.
                self.terminal.write(vec![0x0c]);
            }
            "c" => {
                // Interrupt: drop the edited line and let the shell draw a
                // fresh prompt (send ^C, as a real terminal would). zle's own
                // ^C aborts its line, unadopted gap strays included — the
                // typeahead record is moot and must not resurrect them at the
                // next prompt; likewise any still-held gap input is discarded
                // (^C means "throw the line away").
                self.cmd.clear();
                self.history_nav = None;
                let _ = self.typeahead.drain();
                let _ = self.hold.engage();
                self.terminal.write(vec![0x03]);
            }
            "d" => {
                // ^D on an empty line is EOF (exits the shell); otherwise it's
                // a forward-delete. EOF only reads as EOF on an *empty* zle
                // line — unadopted gap strays would turn it into a completion
                // listing, so wipe them first.
                if self.cmd.is_empty() {
                    self.wipe_pending_typeahead();
                    self.terminal.write(vec![0x04]);
                } else {
                    self.cmd.delete();
                }
            }
            _ => {}
        }
    }

    /// Horizontal caret motion in the editor with selection semantics: Shift
    /// extends, a plain move with an active selection collapses to its edge,
    /// otherwise the caret moves (by word when `word`).
    fn editor_move_h(&mut self, right: bool, shift: bool, word: bool) {
        if shift {
            self.cmd.begin_selection();
        } else if let Some((s, e)) = self.cmd.selection() {
            self.cmd.set_cursor(if right { e } else { s });
            self.cmd.clear_selection();
            return;
        }
        match (right, word) {
            (false, false) => self.cmd.move_left(),
            (false, true) => self.cmd.move_word_left(),
            (true, false) => self.cmd.move_right(),
            (true, true) => self.cmd.move_word_right(),
        }
    }

    /// Home/End motion with selection semantics (Shift extends, else collapses).
    fn editor_move_edge(&mut self, end: bool, shift: bool) {
        if shift {
            self.cmd.begin_selection();
        } else {
            self.cmd.clear_selection();
        }
        if end {
            self.cmd.move_end();
        } else {
            self.cmd.move_home();
        }
    }

    fn has_selection(&self) -> bool {
        self.terminal.term.lock().selection.is_some()
    }

    /// Snapshot the Kitty keyboard-protocol flags the app has enabled, read off the
    /// local `Term`'s mode bits (the reader thread keeps them current by advancing
    /// the emulator over all child output). Consulted by the key encoder so TUIs
    /// that opt into the protocol get `CSI u` reports.
    fn kitty_flags(&self) -> super::input::KittyFlags {
        super::input::KittyFlags::from_mode(self.terminal.term.lock().mode())
    }

    /// Bytes for a Tab / Shift-Tab press sent to the PTY. Honors the Kitty keyboard
    /// protocol when a full-screen app enabled it (so `Tab` arrives as `CSI 9 u`,
    /// distinct from `Ctrl+I`); otherwise the legacy HT / back-tab sequences. These
    /// keys reach the PTY through the `SendTab`/`SendBackTab` actions rather than
    /// `on_key_down`, so the Kitty encoding is applied here as well.
    fn tab_bytes(&self, shift: bool) -> Vec<u8> {
        super::input::tab_bytes(shift, self.kitty_flags())
    }

    /// Write a fixed byte sequence to the PTY (for keystrokes delivered as
    /// actions rather than through `on_key_down`, e.g. Tab / Shift-Tab), applying
    /// the same cursor / selection / scroll housekeeping as normal typing.
    fn send_to_pty(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if self.terminal.exited {
            return;
        }
        self.terminal.write(bytes.to_vec());
        self.cursor_visible = true;
        let mut term = self.terminal.term.lock();
        term.selection = None;
        term.scroll_display(Scroll::Bottom);
        self.scroll_frac = 0.;
        drop(term);
        cx.notify();
    }

    /// Select the entire buffer — from the top of scrollback to the last cell —
    /// so Cmd+A then Cmd+C copies everything.
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        let mut term = self.terminal.term.lock();
        let grid = term.grid();
        let start = Point::new(grid.topmost_line(), Column(0));
        let end = Point::new(grid.bottommost_line(), grid.last_column());
        let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
        sel.update(end, Side::Right);
        term.selection = Some(sel);
        drop(term);
        cx.notify();
    }

    /// "Select All" as the user means it in context: at the prompt, select the
    /// edited command line; otherwise select the whole terminal buffer. Shared by
    /// the ⌘A shortcut and the right-click "Select All" item so the two never
    /// drift apart.
    pub fn select_all_contextual(&mut self, cx: &mut Context<Self>) {
        if self.input_active() {
            self.cmd.select_all();
            cx.notify();
        } else {
            self.select_all(cx);
        }
    }

    /// Paste clipboard text. While idle at the prompt it goes into the local
    /// command editor (a single trailing newline is dropped so a copied line
    /// doesn't auto-submit). Otherwise it's written to the PTY, wrapped in
    /// bracketed-paste markers when the app enabled that mode (so shells/editors
    /// treat it as one paste rather than typed-and-executed input).
    pub fn paste(&mut self, text: String, cx: &mut Context<Self>) {
        if self.input_active() {
            let trimmed = text.strip_suffix('\n').unwrap_or(&text);
            self.cmd.insert_str(trimmed);
            self.history_nav = None;
            self.completion = None;
            self.cursor_visible = true;
            cx.notify();
            return;
        }
        // A gap paste rides the same hold as typed text (a clean single-line
        // paste ahead of a fast command lands in the editor, PTY untouched);
        // `write_gap_text` taints the record on embedded newlines — those
        // lines execute as commands zle-side and must not become a seed.
        let bracketed = self
            .terminal
            .term
            .lock()
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        // `paste_bytes` wraps in bracketed markers when the app enabled that
        // mode (the receiver's own guard against a pasted command
        // auto-executing) and strips any ESC so clipboard text can't smuggle
        // its own `ESC[201~` end-marker to break out.
        self.write_gap_text(&text, paste_bytes(&text, bracketed), cx);
    }

    // ---- Mouse tracking (so vim / tmux / zellij get clicks & drags) ----

    /// True when the application has enabled any mouse-reporting mode.
    pub fn mouse_mode(&self) -> bool {
        self.terminal
            .term
            .lock()
            .mode()
            .intersects(TermMode::MOUSE_MODE)
    }

    /// Encode and send a single mouse event to the PTY. `base` is the raw button
    /// code (0/1/2 buttons, 64/65 wheel, 32/33/34 drag-motion); `row`/`col` are
    /// 0-based viewport coordinates.
    fn write_mouse(&self, base: u8, mods: &Modifiers, col: usize, row: usize, pressed: bool) {
        let sgr = self
            .terminal
            .term
            .lock()
            .mode()
            .contains(TermMode::SGR_MOUSE);
        if let Some(msg) = encode_mouse(sgr, base, mods, col, row, pressed) {
            self.terminal.write(msg);
        }
    }

    pub fn mouse_press(&mut self, button: MouseButton, col: usize, row: usize, mods: &Modifiers) {
        let base = match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            _ => return,
        };
        self.last_mouse_cell = Some((col, row));
        self.write_mouse(base, mods, col, row, true);
    }

    pub fn mouse_release(&mut self, button: MouseButton, col: usize, row: usize, mods: &Modifiers) {
        let base = match button {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            _ => return,
        };
        self.write_mouse(base, mods, col, row, false);
    }

    pub fn mouse_drag(&mut self, button: MouseButton, col: usize, row: usize, mods: &Modifiers) {
        // Only report when the cell changed, and only if the app asked for drag
        // or motion tracking.
        if self.last_mouse_cell == Some((col, row)) {
            return;
        }
        let wants = self
            .terminal
            .term
            .lock()
            .mode()
            .intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION);
        if !wants {
            return;
        }
        self.last_mouse_cell = Some((col, row));
        let base = match button {
            MouseButton::Left => 32,
            MouseButton::Middle => 33,
            MouseButton::Right => 34,
            _ => return,
        };
        self.write_mouse(base, mods, col, row, true);
    }

    /// Report button-less mouse motion when the app asked for *all* motion
    /// (mode 1003, any-event tracking) — hover-driven TUIs never see the mouse
    /// otherwise. Drags (a button held) go through [`mouse_drag`] instead.
    /// Deduped per cell like drags, so pixel moves within one cell don't spam
    /// the PTY. Base 35 = the motion flag (32) plus "no button" (3).
    pub fn mouse_motion(&mut self, col: usize, row: usize, mods: &Modifiers) {
        if self.last_mouse_cell == Some((col, row)) {
            return;
        }
        if !self
            .terminal
            .term
            .lock()
            .mode()
            .contains(TermMode::MOUSE_MOTION)
        {
            return;
        }
        self.last_mouse_cell = Some((col, row));
        self.write_mouse(35, mods, col, row, true);
    }

    /// Scroll handling that also honors mouse-wheel reporting and alternate
    /// scroll, falling back to local scrollback otherwise.
    pub fn scroll(&mut self, lines: i32, mods: &Modifiers, cx: &mut Context<Self>) {
        if lines == 0 {
            return;
        }
        let mode = *self.terminal.term.lock().mode();
        match wheel_route(mode, mods.shift, lines > 0) {
            // Mouse-wheel reporting: one report per line, at the last mouse cell.
            WheelRoute::Report { base } => {
                let (col, row) = self.last_mouse_cell.unwrap_or((0, 0));
                for _ in 0..lines.unsigned_abs() {
                    self.write_mouse(base, mods, col, row, true);
                }
            }
            // Alternate scroll: translate the wheel into arrow keys for
            // full-screen apps (less, man) that don't do mouse reporting.
            WheelRoute::Arrows { seq } => {
                let mut out = Vec::with_capacity(seq.len() * lines.unsigned_abs() as usize);
                for _ in 0..lines.unsigned_abs() {
                    out.extend_from_slice(seq);
                }
                self.terminal.write(out);
            }
            // Local scrollback, in whole lines (wheel scrolling goes through
            // `smooth_scroll` instead and keeps a sub-line fraction; a
            // line-quantized jump here must not leave a stale fraction shifting
            // the paint).
            WheelRoute::Scrollback => {
                self.scroll_frac = 0.;
                self.terminal
                    .term
                    .lock()
                    .scroll_display(Scroll::Delta(lines));
                cx.notify();
            }
        }
    }

    // ---- Cmd+F search ----

    pub fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let text = self.terminal.term.lock().selection_to_string();
        if let Some(mut text) = text {
            // Optionally strip trailing whitespace from each line — a block/rect
            // selection or wrapped rows otherwise carry padding spaces.
            if cx.global::<Config>().clipboard_trim_trailing_spaces {
                text = trim_trailing_spaces(&text);
            }
            if !text.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
        }
    }

    /// Read the system clipboard and paste it into the PTY (bracketed-paste
    /// aware). Used by Cmd+V and the right-click "Paste" item.
    pub fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx
            .read_from_clipboard()
            .as_ref()
            .and_then(clipboard_paste_text)
        {
            self.paste(text, cx);
        }
    }

    /// Files dragged in from Finder (etc.) and dropped on the terminal:
    /// shell-escape each path, join with spaces, and insert them like a paste —
    /// with a trailing space so a dropped path is ready to be an argument and
    /// back-to-back drops don't run together. Matches Warp and macOS
    /// Terminal.app (which reuse their paste escaping for drops).
    fn drop_files(&mut self, paths: &ExternalPaths, cx: &mut Context<Self>) {
        let text = paths
            .paths()
            .iter()
            .map(|p| shell_escape_path(&p.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() {
            return;
        }
        self.paste(format!("{text} "), cx);
    }

    /// Clear the terminal (right-click "Clear"), like Cmd+K / the `clear`
    /// command: purge the scrollback history *and* wipe the visible screen.
    /// We drop the history directly, then send Ctrl+L so the shell/TUI repaints
    /// its prompt at the top with the cursor in sync (no desync from poking the
    /// grid behind the program's back).
    pub fn clear_scrollback(&mut self, cx: &mut Context<Self>) {
        self.terminal.term.lock().grid_mut().clear_history();
        self.scroll_frac = 0.;
        self.terminal.write(vec![0x0c_u8]); // Ctrl+L
        cx.notify();
    }

    /// Swap the primary font face (keeping the configured fallbacks). Lets the
    /// settings panel change the font family live; the element re-measures cell
    /// geometry on the next prepaint, so the grid reflows automatically.
    pub fn set_font_family(&mut self, family: String, cx: &mut Context<Self>) {
        let fallbacks = self.font.fallbacks.clone();
        let mut font = gpui::font(family);
        font.fallbacks = fallbacks;
        if let Some(features) = &self.font_features {
            font.features = features.clone();
        }
        self.font = font;
        cx.notify();
    }

    /// Swap the bold face (`None` = synthesize bold from the primary face). The
    /// alternate carries the primary's fallback chain so glyph coverage matches.
    pub fn set_font_family_bold(&mut self, family: Option<String>, cx: &mut Context<Self>) {
        self.font_bold = self.alt_font(family);
        cx.notify();
    }

    /// Swap the italic face (`None` = synthesize italic from the primary face).
    pub fn set_font_family_italic(&mut self, family: Option<String>, cx: &mut Context<Self>) {
        self.font_italic = self.alt_font(family);
        cx.notify();
    }

    /// Apply OpenType features to the live terminal fonts. `None` restores the
    /// terminal-safe default path, where the renderer disables contextual
    /// ligatures while building paint faces.
    pub fn set_font_features(
        &mut self,
        features: Option<gpui::FontFeatures>,
        cx: &mut Context<Self>,
    ) {
        self.font_features = features.clone();
        let apply = |font: &mut Font| {
            font.features = features.clone().unwrap_or_default();
        };
        apply(&mut self.font);
        if let Some(font) = &mut self.font_bold {
            apply(font);
        }
        if let Some(font) = &mut self.font_italic {
            apply(font);
        }
        cx.notify();
    }

    /// Build an alternate face from a family name, reusing the primary's
    /// fallbacks. `None` → `None` (fall back to synthesizing from `self.font`).
    fn alt_font(&self, family: Option<String>) -> Option<Font> {
        family.map(|f| {
            let mut af = gpui::font(f);
            af.fallbacks = self.font.fallbacks.clone();
            if let Some(features) = &self.font_features {
                af.features = features.clone();
            }
            af
        })
    }

    /// Detect command start/finish by watching the PTY's foreground process
    /// group, and post a desktop notification when a long-running command
    /// finishes while the window is in the background. Called ~1×/second.
    fn poll_foreground(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.terminal.exited {
            return;
        }
        let at_prompt = self.terminal.at_prompt();

        // Re-rank history when the working directory changes (a `cd`), so ghost text
        // and completion start favouring commands run in the new directory. Only on
        // a real, known change — an unknown cwd keeps the previous ranking.
        if let Some(cwd) = self.cwd()
            && self.ranked_cwd.as_ref() != Some(&cwd)
        {
            self.rerank_history(Some(&cwd));
        }

        // Redraw when the prompt/running state flips, so the line editor shows or
        // hides promptly even when the shell produced no output to trigger a
        // repaint (e.g. a command that prints nothing). Without this the editor's
        // visibility — computed in `render` — could lag until the next redraw.
        if at_prompt != self.last_at_prompt {
            self.last_at_prompt = at_prompt;
            cx.notify();
        }

        // "Command finished" notification: a foreground command (not at prompt)
        // that ran long and finished while the window was in the background.
        let running = !at_prompt;
        match (self.running_since, running) {
            (None, true) => {
                self.running_since = Some(std::time::Instant::now());
                self.running_title = self.title.clone();
            }
            (Some(start), false) => {
                let elapsed = start.elapsed();
                let title = std::mem::take(&mut self.running_title);
                self.running_since = None;
                // Gate on the configured policy: never / only-when-unfocused /
                // always. The long-command floor still applies regardless.
                let notify = match cx.global::<Config>().notify_on_command_finish {
                    NotifyMode::Never => false,
                    NotifyMode::Unfocused => !window.is_window_active(),
                    NotifyMode::Always => true,
                };
                if elapsed >= LONG_COMMAND && notify {
                    notify_command_finished(&title, elapsed);
                }
            }
            _ => {}
        }
    }

    /// True when the shell sits idle at its prompt: the PTY's foreground process
    /// group is the shell's own (established as the first group we observe), as
    /// opposed to a foreground command having taken over the terminal. `false`
    /// while a command runs or before the group can be read. Reuses the same
    /// `prompt_pgid` baseline that `poll_foreground` learns.
    fn at_shell_prompt(&self) -> bool {
        self.terminal.at_prompt()
    }

    /// The shell cursor's current viewport cell `(row, col)`, accounting for
    /// scrollback offset — the same mapping `element::build_grid` uses to place
    /// the block cursor. `None` only when the cursor is scrolled off the top of
    /// the viewport. Used to anchor the inline line editor right where the shell
    /// prompt ends.
    ///
    /// The cursor's `Hidden` *shape* is deliberately ignored. A full-screen TUI
    /// (e.g. Claude Code) hides the cursor with DECTCEM (`\e[?25l`) and can hand
    /// back to the shell prompt — or exit — before a matching `\e[?25h` reaches
    /// our local grid, leaving the shape stale-`Hidden` while the shell is
    /// already idle at its prompt. These callers only run while `input_active()`
    /// (at the prompt, off the alt screen), where the cursor *position* is valid
    /// even if the shape is momentarily hidden. Treating hidden as `None` here
    /// made `render_input_bar` fall back to `(0, 0)` and paint the caret in the
    /// top-left corner; `element::build_grid` already ignores the shape the same
    /// way when anchoring the IME window.
    fn cursor_cell(&self) -> Option<(usize, usize)> {
        let term = self.terminal.term.lock();
        let content = term.renderable_content();
        let row = content.cursor.point.line.0 + content.display_offset as i32;
        let col = content.cursor.point.column.0;
        (row >= 0).then_some((row as usize, col))
    }

    /// Handle a left click while the command editor is live: if it lands on the
    /// input line, move the caret to the clicked position and report `true` (so
    /// the caller skips starting a terminal text-selection). The line is rendered
    /// starting at the shell's cursor cell, so the clicked char index is the
    /// column offset from there. (Approximate for wide CJK glyphs, which span two
    /// cells — fine for typical ASCII command lines.)
    /// Map a click cell `(col, row)` to a char index in the edited line, accounting
    /// for wrapping: the input occupies `prompt_cols + len` cells laid out grid-row
    /// by grid-row from the prompt cell. With `clamp`, positions before/after the
    /// input snap to `0`/`len` (for drags); without it, they return `None` (so a
    /// click outside the input isn't treated as an editor click).
    fn editor_char_index(&self, col: usize, row: usize, clamp: bool) -> Option<usize> {
        if !self.input_active() {
            return None;
        }
        let (srow, scol) = self.cursor_cell()?;
        if row < srow {
            return clamp.then_some(0);
        }
        let cols = self.terminal.term.lock().columns().max(1);
        let chars: Vec<char> = self.cmd.text().chars().collect();
        wrapped_click_index(&chars, scol, cols, col, row - srow, clamp)
    }

    pub fn editor_click(
        &mut self,
        col: usize,
        row: usize,
        clicks: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(idx) = self.editor_char_index(col, row, false) else {
            return false;
        };
        match clicks {
            1 => {
                self.cmd.set_cursor(idx);
                self.cmd.clear_selection();
                self.editor_selecting = true; // a drag from here extends selection
            }
            2 => self.cmd.select_word_at(idx),
            _ => self.cmd.select_all(),
        }
        self.completion = None;
        self.cursor_visible = true;
        cx.notify();
        true
    }

    /// Extend the editor selection during a left-drag that began on the input.
    /// Returns whether it handled the drag (so the terminal selection is skipped).
    pub fn editor_drag(&mut self, col: usize, row: usize, cx: &mut Context<Self>) -> bool {
        if !self.editor_selecting {
            return false;
        }
        let Some(idx) = self.editor_char_index(col, row, true) else {
            return false;
        };
        self.cmd.extend_to(idx);
        self.cursor_visible = true;
        cx.notify();
        true
    }

    /// Whether the local line editor should be live and focused: idle at a shell
    /// prompt, not on the alternate screen, no search bar open, process alive.
    /// Everywhere else this is `false`, so the raw terminal keeps the keyboard and
    /// behaves exactly as without the editor.
    pub fn input_active(&self) -> bool {
        // Suppress our command editor only while the search field actually holds
        // keyboard focus (it claims Tab / ↑ / ↓ / typing). If search is open but
        // blurred — e.g. the user clicked back into the terminal — the editor must
        // resume, otherwise keys fall through to the raw PTY path and can't be edited.
        if self.terminal.exited || self.search_focused {
            return false;
        }
        if self.on_alt_screen() {
            return false;
        }
        self.at_shell_prompt()
    }

    /// True while the emulator is on the alternate screen — a full-screen TUI
    /// owns the pane, so raw input belongs to that program, not the shell's
    /// next command line.
    fn on_alt_screen(&self) -> bool {
        self.terminal
            .term
            .lock()
            .mode()
            .contains(TermMode::ALT_SCREEN)
    }

    /// Handoff once zle is reading at the new prompt: wipe the type-ahead it
    /// just consumed and adopt it into the editor (see the `typeahead` module
    /// docs for the full failure mode). The `^U` (kill-whole-line — same
    /// binding in zsh emacs/vi-insert, bash and fish) is written *after*
    /// every stray byte, and the TTY queue is FIFO, so zle always reads the
    /// strays first and then the wipe — correct with no timing assumptions.
    /// The seed is *prepended*: the editor engages at `133;D` but this flush
    /// waits for `133;B` (`zle_reading` — a ^U written while precmd hooks
    /// still run in canonical mode is kernel-echoed as literal `^U` junk),
    /// and anything typed in between already sits in the editor,
    /// chronologically *after* the strays. Runs every render with the editor
    /// live; an untouched record drains to `None` and sends nothing.
    fn flush_typeahead(&mut self) {
        let Some(seed) = self.typeahead.drain() else {
            return;
        };
        self.terminal.write(vec![0x15]);
        if !seed.is_empty() {
            self.cmd.prepend_str(&seed);
        }
    }

    /// The editor is about to write bytes the shell will act on (a submitted
    /// line, ^D EOF) while gap typeahead may still sit unadopted on zle's
    /// line (its wipe waits for `zle_reading`). Wipe first — FIFO puts the
    /// ^U ahead of the caller's bytes — and drop the seed: grafting it into
    /// an action the user just chose would run something they never saw.
    fn wipe_pending_typeahead(&mut self) {
        if self.typeahead.drain().is_some() {
            self.terminal.write(vec![0x15]);
        }
    }

    /// True when gap input may be held for the editor: shell integration is
    /// live (a prompt will come and adopt it) and no full-screen TUI owns the
    /// pane. Only consulted on the raw path, so "the editor is disengaged" is
    /// already implied.
    fn gap_holdable(&self) -> bool {
        self.terminal.shell_active() && !self.on_alt_screen()
    }

    /// Write printable gap text (IME commit, paste) toward the shell: offered
    /// to the hold when reconstructable (see `hold`), otherwise released +
    /// written raw and recorded for the deferred wipe. `bytes` is the exact
    /// PTY encoding (paste may be bracketed-wrapped).
    fn write_gap_text(&mut self, text: &str, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if self.gap_holdable() && !text.chars().any(char::is_control) {
            match self.hold.hold_text(text, &bytes) {
                Verdict::Held(arm) => {
                    if let Some(epoch) = arm {
                        self.arm_hold_timer(epoch, cx);
                    }
                    return;
                }
                Verdict::Passthrough => {}
            }
        } else {
            // Unreconstructable (control chars / TUI input): anything held
            // must precede these bytes on the wire.
            self.release_hold();
        }
        self.terminal.write(bytes);
        let alt = self.on_alt_screen();
        self.typeahead.observe(RawInput::Text(text), alt);
    }

    /// Release any held gap input to the PTY (order-preserving) and record it
    /// for the deferred wipe; the rest of this gap is raw passthrough.
    fn release_hold(&mut self) {
        if let Some((net, bytes)) = self.hold.release() {
            self.terminal.write(bytes);
            let alt = self.on_alt_screen();
            self.typeahead.observe(RawInput::Text(&net), alt);
        }
    }

    /// Start the one-shot dump timer for a freshly opened hold window.
    fn arm_hold_timer(&mut self, epoch: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(HOLD_WINDOW).await;
            let _ = this.update(cx, |view, cx| view.dump_hold(epoch, cx));
        })
        .detach();
    }

    /// The hold window lapsed with the editor still disengaged: the command
    /// is long-running (or reading stdin) — release the bytes to the PTY and
    /// record them for the deferred wipe.
    fn dump_hold(&mut self, epoch: u64, cx: &mut Context<Self>) {
        if let Some((net, bytes)) = self.hold.timeout(epoch) {
            self.terminal.write(bytes);
            let alt = self.on_alt_screen();
            self.typeahead.observe(RawInput::Text(&net), alt);
            cx.notify();
        }
    }

    /// Ship the edited command line to the PTY — the whole line plus a carriage
    /// return — record it in history, then clear the editor for the next command.
    fn submit_command(&mut self, cx: &mut Context<Self>) {
        if self.terminal.exited {
            return;
        }
        // A sub-frame race can land Enter before the render that adopts held
        // gap input: fold it in first so the submitted line is what the user
        // actually typed.
        if let Some(net) = self.hold.engage() {
            self.cmd.prepend_str(&net);
        }
        let line = self.cmd.text();
        // Record in history (skip blanks and immediate duplicates for ↑/↓ recall),
        // but always tally the run — count and the directory it ran in — for
        // frecency, then refresh the ranked view for the current directory.
        if !line.trim().is_empty() {
            let cwd = self.cwd();
            *self.history_counts.entry(line.clone()).or_insert(0) += 1;
            if let Some(dir) = cwd.as_ref().and_then(|p| p.to_str()) {
                self.history_cwds
                    .entry(line.clone())
                    .or_default()
                    .insert(dir.to_string());
            }
            if self.history.last().map(String::as_str) != Some(line.as_str()) {
                self.history.push(line.clone());
                super::history::append(&line, cwd.as_deref());
            }
            self.rerank_history(cwd.as_deref());
        }
        self.history_nav = None;
        self.history_stash.clear();
        self.completion = None;

        // Any gap typeahead still waiting for its wipe (the ^U is deferred
        // until zle reads) would prefix the submitted line on zle's side —
        // "ls" strays + "pwd\r" runs `lspwd`. Wipe first: FIFO puts the ^U
        // ahead of the line bytes.
        self.wipe_pending_typeahead();
        let mut bytes = line.into_bytes();
        bytes.push(b'\r');
        self.terminal.write(bytes);
        self.cmd.clear();
        self.cursor_visible = true;
        let mut term = self.terminal.term.lock();
        term.selection = None;
        term.scroll_display(Scroll::Bottom);
        self.scroll_frac = 0.;
        drop(term);
        cx.notify();
    }

    /// Recall the previous (older) history entry into the editor (↑). On the first
    /// step it stashes the in-progress line so ↓ can restore it.
    fn history_prev(&mut self, cx: &mut Context<Self>) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_nav {
            None => {
                self.history_stash = self.cmd.text();
                self.history.len() - 1
            }
            Some(0) => 0, // already at the oldest
            Some(i) => i - 1,
        };
        self.history_nav = Some(next);
        self.cmd.set(&self.history[next]);
        cx.notify();
    }

    /// Move to the next (newer) history entry (↓); stepping past the newest
    /// restores the stashed in-progress line.
    fn history_next(&mut self, cx: &mut Context<Self>) {
        let Some(i) = self.history_nav else {
            return;
        };
        if i + 1 < self.history.len() {
            self.history_nav = Some(i + 1);
            self.cmd.set(&self.history[i + 1]);
        } else {
            // Past the newest entry: back to the line the user was typing.
            self.history_nav = None;
            let stash = std::mem::take(&mut self.history_stash);
            self.cmd.set(&stash);
        }
        cx.notify();
    }

    /// Re-rank `history_ranked` by frecency for `cwd`, so commands previously run
    /// in that directory float to the top of ghost text and completion. Records the
    /// directory used, so `poll_foreground` can skip re-ranking until it changes.
    fn rerank_history(&mut self, cwd: Option<&std::path::Path>) {
        self.history_ranked = super::history::rank_by_frecency(
            &self.history,
            &self.history_counts,
            &self.history_cwds,
            cwd.and_then(|p| p.to_str()),
        );
        self.ranked_cwd = cwd.map(std::path::Path::to_path_buf);
    }

    /// The autosuggestion (ghost text): the most *frecent* history entry that
    /// starts with the current line, when the caret is at the end. Returns the
    /// *full* suggested line; the renderer shows the remainder in muted text and
    /// Right / Ctrl+F accepts it. `None` when the line is empty, the caret isn't at
    /// the end, or nothing matches. Ranking by frecency (not raw recency) means the
    /// command you actually run a lot wins over the last thing you happened to type.
    fn ghost_suggestion(&self) -> Option<String> {
        if self.cmd.is_empty() || self.cmd.cursor() != self.cmd.len() {
            return None;
        }
        let line = self.cmd.text();
        self.history_ranked
            .iter()
            .find(|h| h.len() > line.len() && h.starts_with(&line))
            .cloned()
    }

    /// Begin a Ctrl+R reverse-history search (no-op if one is already active).
    fn start_reverse_search(&mut self) {
        if self.reverse_search.is_none() {
            self.reverse_search = Some(ReverseSearch::new());
        }
    }

    /// Handle a key while a reverse search is active. The search itself owns the
    /// query/match logic (`reverse_search` module); the view just applies the
    /// resulting [`reverse_search::Action`] and repaints.
    fn handle_reverse_search_key(&mut self, ks: &gpui::Keystroke, cx: &mut Context<Self>) {
        // Printable text typed into the query. A CJK input source routes it through
        // the IME (`input_text` → `push_query`), but a plain ASCII input source —
        // and Linux, where `prefers_ime_for_printable_keys` is false — delivers it
        // here as an ordinary key event carrying `key_char`. Without this the search
        // field can only be typed into via an IME: Ctrl+R opens, but ASCII
        // keystrokes vanish. Mirror the editor's `key_char` path (`handle_editor_key`);
        // control / Cmd / Alt chords and non-printable keys (Enter/Backspace/Esc have
        // no printable `key_char`) fall through to the control-key handling below.
        let m = &ks.modifiers;
        if !m.control && !m.platform && !m.alt {
            if let Some(ch) = ks.key_char.as_deref() {
                if !ch.is_empty() && ch.chars().all(|c| c >= '\u{20}' && c != '\u{7f}') {
                    if let Some(rs) = self.reverse_search.as_mut() {
                        rs.push_query(ch, &self.history);
                    }
                    cx.notify();
                    return;
                }
            }
        }
        let Some(rs) = self.reverse_search.as_mut() else {
            return;
        };
        match rs.handle_key(ks, &self.history) {
            reverse_search::Action::Redraw => {}
            reverse_search::Action::Cancel => self.reverse_search = None,
            reverse_search::Action::Accept(line) => {
                self.reverse_search = None;
                if let Some(line) = line {
                    self.cmd.set(&line);
                }
            }
        }
        cx.notify();
    }

    /// Tab completion over our own engine (command names in command
    /// position, filesystem paths elsewhere — history is deliberately absent:
    /// whole-line recall is ghost text's and Ctrl+R's job). A fresh Tab applies a
    /// unique match immediately; multiple matches fill the candidates' longest
    /// common prefix and open the menu as a *picker* with the first row
    /// highlighted — the line isn't touched again until a candidate is accepted.
    /// With the menu open, Tab fills any further common prefix, else moves the
    /// highlight (`forward` reverses for Shift-Tab).
    fn complete_tab(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.completion.is_some() {
            self.completion_tab_step(forward, cx);
            return;
        }

        // Fresh completion.
        let Some(cwd) = self.cwd().or_else(|| std::env::current_dir().ok()) else {
            return;
        };
        let line = self.cmd.text();
        let cursor = self.cmd.cursor();
        let Some(comp) = super::completion::complete(&line, cursor, &cwd) else {
            return;
        };

        if comp.candidates.len() == 1 {
            // Unique match: accept it outright.
            let c = comp.candidates[0].clone();
            self.completion_insert(&c, c.start);
            self.cursor_visible = true;
            cx.notify();
            return;
        }

        // Multiple matches: fill the longest common prefix when it extends the
        // typed word, then show the menu with the first row highlighted. All
        // candidates share the prefix, so the fill never invalidates the set.
        let word_start = comp.candidates[0].start;
        let word_end = comp.candidates[0].end;
        let word: String = line
            .chars()
            .skip(word_start)
            .take(word_end - word_start)
            .collect();
        let s = CompletionSession::new(word_start, word.clone(), comp.candidates);
        if let Some(lcp) = s.common_prefix()
            && lcp.chars().count() > word.chars().count()
        {
            self.apply_candidate(&line, word_start, word_end, &lcp);
        }
        self.completion = Some(s);
        self.cursor_visible = true;
        cx.notify();
    }

    /// Tab / Shift-Tab with the menu open: first try extending the line to the
    /// filtered candidates' common prefix (bash-style fill); when that makes no
    /// progress, move the highlight instead. A fill that pins down a single
    /// candidate accepts it outright.
    fn completion_tab_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        if forward {
            let Some(s) = self.completion.as_ref() else {
                return;
            };
            let (word_start, lcp, lone) = (s.word_start, s.common_prefix(), s.filtered.len() == 1);
            let line = self.cmd.text();
            let cursor = self.cmd.cursor().min(line.chars().count());
            if let Some(lcp) = lcp
                && lcp.chars().count() > cursor.saturating_sub(word_start)
            {
                if lone {
                    self.completion_accept(cx);
                } else {
                    self.apply_candidate(&line, word_start, cursor, &lcp);
                    self.cursor_visible = true;
                    cx.notify();
                }
                return;
            }
        }
        self.completion_select(forward, cx);
    }

    /// Move the completion highlight (Tab cycling and ↑/↓). Visual only — the
    /// editor line changes on accept, not while browsing.
    fn completion_select(&mut self, forward: bool, cx: &mut Context<Self>) {
        if let Some(s) = self.completion.as_mut() {
            s.select(forward);
            self.cursor_visible = true;
            cx.notify();
        }
    }

    /// Accept the highlighted candidate: write it into the line and close the
    /// menu. The command does not run — a second Enter (or Cmd+Enter in one
    /// stroke) submits.
    fn completion_accept(&mut self, cx: &mut Context<Self>) {
        let Some(s) = self.completion.take() else {
            return;
        };
        if let Some(c) = s.selected().cloned() {
            self.completion_insert(&c, s.word_start);
        }
        self.cursor_visible = true;
        cx.notify();
    }

    /// Write `cand` into the editor over chars `[start, caret)` — the accept
    /// action. Directories keep a trailing `/` so a further Tab descends; other
    /// candidates get a trailing space only when the caret is at the end of the
    /// line (mid-line, the existing tail already separates the word).
    fn completion_insert(&mut self, cand: &completion::Candidate, start: usize) {
        let line = self.cmd.text();
        let len = line.chars().count();
        let cursor = self.cmd.cursor().min(len);
        let mut text = cand.text.clone();
        if cand.is_dir() {
            if !text.ends_with('/') {
                text.push('/');
            }
        } else if cursor == len {
            text.push(' ');
        }
        self.apply_candidate(&line, start, cursor, &text);
    }

    /// Re-filter the open menu after an edit at the caret: the live word must
    /// still extend the word the menu opened on and keep at least one candidate,
    /// else the menu closes. Whitespace in the word (a new argument) closes it
    /// too. No-op when no menu is open.
    fn completion_refilter(&mut self) {
        let Some(s) = self.completion.as_mut() else {
            return;
        };
        let chars: Vec<char> = self.cmd.text().chars().collect();
        let cursor = self.cmd.cursor().min(chars.len());
        let keep = cursor >= s.word_start
            && chars[s.word_start..cursor]
                .iter()
                .all(|c| !c.is_whitespace())
            && {
                let word: String = chars[s.word_start..cursor].iter().collect();
                s.refilter(&word)
            };
        if !keep {
            self.completion = None;
        }
    }

    /// Splice `text` into `orig` over the char range `[start, end)` and put the
    /// result into the editor. Delegates to `completion::Replacement` so the
    /// edit is unit-tested there.
    fn apply_candidate(&mut self, orig: &str, start: usize, end: usize, text: &str) {
        let (line, cursor) = completion::Replacement {
            orig: orig.to_string(),
            start,
            end,
            text: text.to_string(),
        }
        .apply();
        self.cmd.set_with_cursor(&line, cursor);
    }

    /// Commit text from the terminal's IME handler. While idle at the prompt this
    /// inserts into our local command editor; while a command runs it writes
    /// straight to the PTY (bare-terminal behavior). Covers both plain typed text
    /// (routed through the IME) and committed CJK characters.
    pub fn input_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.commit_text(text, cx);
    }

    /// See `input_text`. The single text-commit path, split by whether the editor
    /// is live at the prompt.
    pub fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.terminal.exited || text.is_empty() {
            return;
        }
        // While reverse-searching, typed text edits the query, not the line.
        if let Some(rs) = self.reverse_search.as_mut() {
            rs.push_query(text, &self.history);
            self.cursor_visible = true;
            cx.notify();
            return;
        }
        if self.input_active() {
            // Editing the command line locally — insert at the caret. Typing
            // breaks out of history navigation; an open completion menu
            // re-filters to the extended word (and closes once nothing matches).
            self.cmd.insert_str(text);
            self.history_nav = None;
            self.completion_refilter();
            self.cursor_visible = true;
            cx.notify();
            return;
        }
        // Gap typing: offered to the hold first (a fast command's typeahead
        // then lands in the editor without ever echoing), else written raw
        // and kept in step with the typeahead record (see `hold`/`typeahead`).
        self.write_gap_text(text, text.as_bytes().to_vec(), cx);
        // Keep the cursor solid while committing input (resets the blink phase).
        self.cursor_visible = true;
        let mut term = self.terminal.term.lock();
        term.selection = None;
        term.scroll_display(Scroll::Bottom);
        self.scroll_frac = 0.;
        drop(term);
        cx.notify();
    }

    /// Set the IME pre-edit (composing) text to display at the cursor.
    pub fn set_marked_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.marked_text = text;
        cx.notify();
    }

    /// Clear the IME pre-edit state.
    pub fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
        if !self.marked_text.is_empty() {
            self.marked_text.clear();
            cx.notify();
        }
    }

    pub fn on_select_start(
        &mut self,
        col: usize,
        row: usize,
        left: bool,
        clicks: usize,
        cx: &mut Context<Self>,
    ) {
        let mut term = self.terminal.term.lock();
        let display_offset = term.grid().display_offset() as i32;
        let point = Point::new(Line(row as i32 - display_offset), Column(col));
        let side = if left { Side::Left } else { Side::Right };
        let ty = match clicks {
            2 => SelectionType::Semantic, // word
            n if n >= 3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        term.selection = Some(Selection::new(ty, point, side));
        drop(term);
        self.selecting = true;
        cx.notify();
    }

    pub fn on_select_update(&mut self, col: usize, row: usize, left: bool, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        let mut term = self.terminal.term.lock();
        let display_offset = term.grid().display_offset() as i32;
        let point = Point::new(Line(row as i32 - display_offset), Column(col));
        let side = if left { Side::Left } else { Side::Right };
        if let Some(sel) = term.selection.as_mut() {
            sel.update(point, side);
        }
        drop(term);
        cx.notify();
    }

    /// Drive selection auto-scroll from a drag's vertical overshoot past the
    /// pane bounds, in lines (0 while the pointer is inside, positive above
    /// the top edge). Called on every left-drag move: entering the edge zone
    /// arms a repeating task that scrolls the scrollback and keeps extending
    /// the selection at the edge row; later moves just retune its speed and
    /// column, and moving back inside (or releasing) stops it.
    pub fn select_autoscroll(
        &mut self,
        overshoot: f32,
        col: usize,
        left: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.selecting || overshoot == 0. {
            self.drag_scroll = None;
            return;
        }
        let side = if left { Side::Left } else { Side::Right };
        let was_idle = self.drag_scroll.is_none();
        self.drag_scroll = Some(DragScroll {
            overshoot,
            col,
            side,
        });
        if !was_idle {
            // The running task reads the fresh state on its next tick.
            return;
        }
        // First step immediately so a quick flick past the edge still moves,
        // then keep stepping on a timer. The task stops itself once the state
        // clears (pointer back inside, drag ended), a newer task supersedes
        // it (epoch mismatch), or the view is dropped.
        self.drag_scroll_epoch += 1;
        let epoch = self.drag_scroll_epoch;
        self.drag_scroll_tick(epoch, cx);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(50))
                    .await;
                if !matches!(
                    this.update(cx, |view, cx| view.drag_scroll_tick(epoch, cx)),
                    Ok(true)
                ) {
                    break;
                }
            }
        })
        .detach();
    }

    /// One auto-scroll step: scroll by an amount that grows with the
    /// overshoot, then re-anchor the selection's moving end to the edge row
    /// it is pushing past (top row when scrolling up, bottom when down).
    /// Returns whether the task should keep ticking. Scrolling clamps at the
    /// history limits, so pinning the pointer past the edge at the top of
    /// scrollback just idles until it moves.
    fn drag_scroll_tick(&mut self, epoch: u64, cx: &mut Context<Self>) -> bool {
        if epoch != self.drag_scroll_epoch {
            // Superseded by a newer task: the state now belongs to it, so just
            // bow out without clearing anything.
            return false;
        }
        if !self.selecting {
            self.drag_scroll = None;
        }
        let Some(ds) = self.drag_scroll else {
            return false;
        };
        let mut term = self.terminal.term.lock();
        let before = term.grid().display_offset();
        term.scroll_display(Scroll::Delta(drag_scroll_step(ds.overshoot)));
        let offset = term.grid().display_offset();
        let row = if ds.overshoot > 0. {
            0
        } else {
            term.screen_lines().saturating_sub(1)
        };
        let point = Point::new(Line(row as i32 - offset as i32), Column(ds.col));
        if let Some(sel) = term.selection.as_mut() {
            sel.update(point, ds.side);
        }
        drop(term);
        if offset != before {
            self.scroll_frac = 0.;
            cx.notify();
        }
        true
    }

    pub fn on_select_end(&mut self, _cx: &mut Context<Self>) {
        self.selecting = false;
        self.editor_selecting = false;
        self.drag_scroll = None;
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let mult = cx.global::<Config>().mouse_scroll_multiplier;
        let raw = match ev.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => p.y.as_f32() / self.line_height.as_f32(),
        };
        let delta = raw * mult;

        // Mouse-tracking reports and alternate-scroll arrow keys consume whole
        // lines, so those paths accumulate fractional deltas and spend only the
        // whole part: rounding each trackpad event separately either discards
        // them all (slow scrolls stall) or over-counts them (each tiny nudge
        // becomes a full line). Shift forces local scrollback, matching
        // `scroll`'s own routing.
        let quantized = !ev.modifiers.shift && {
            let mode = *self.terminal.term.lock().mode();
            mode.intersects(TermMode::MOUSE_MODE)
                || mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
        };
        if quantized {
            let total = self.scroll_debt + delta;
            let lines = total.trunc() as i32;
            self.scroll_debt = total - lines as f32;
            if lines != 0 {
                self.scroll(lines, &ev.modifiers, cx);
            }
            return;
        }

        // Local scrollback keeps the fraction instead: the view position is
        // continuous and every wheel event moves pixels, not lines.
        self.smooth_scroll(delta, cx);
    }

    /// Scroll the local scrollback by a possibly-fractional number of lines,
    /// pixel-smooth: whole lines go to the emulator's `display_offset`, the
    /// remainder stays in `scroll_frac` and shifts the paint. The position may
    /// come to rest between line boundaries, like a native scroll view.
    fn smooth_scroll(&mut self, delta: f32, cx: &mut Context<Self>) {
        let mut term = self.terminal.term.lock();
        let offset = term.grid().display_offset();
        let max = term.grid().history_size();
        let (jump, frac) = smooth_scroll_step(offset, self.scroll_frac, delta, max);
        if jump != 0 {
            term.scroll_display(Scroll::Delta(jump));
        }
        drop(term);
        if jump != 0 || frac != self.scroll_frac {
            self.scroll_frac = frac;
            cx.notify();
        }
    }

    /// Open the URL under the given cell, if any (OSC 8 hyperlink or a plain URL
    /// detected in the row text). Returns true if a URL was opened.
    pub fn open_link_at(&self, col: usize, row: usize, cx: &mut Context<Self>) -> bool {
        if !cx.global::<Config>().link_url {
            return false;
        }
        let term = self.terminal.term.lock();
        let display_offset = term.grid().display_offset() as i32;
        let line = Line(row as i32 - display_offset);
        let cols = term.columns();
        if col >= cols {
            return false;
        }

        // 1) Explicit OSC 8 hyperlink carried on the cell.
        let cell = &term.grid()[line][Column(col)];
        if let Some(hl) = cell.hyperlink() {
            let uri = hl.uri().to_string();
            drop(term);
            cx.open_url(&uri);
            return true;
        }

        // 2) Fall back to detecting a bare URL in the row's text.
        let mut text = String::with_capacity(cols);
        for c in 0..cols {
            text.push(term.grid()[line][Column(c)].c);
        }
        drop(term);
        if let Some(url) = super::search::url_at(&text, col) {
            cx.open_url(&url);
            return true;
        }
        false
    }

    /// Update the remembered hovered link for the screen cell `(col, row)` and
    /// repaint if it changed. Returns whether a link sits under the cursor, so the
    /// element can switch to a pointing-hand cursor. Cheap on the common case: any
    /// non-URL cell resolves to `None` and bails.
    pub fn hover_link_at(&mut self, col: usize, row: usize, cx: &mut Context<Self>) -> bool {
        // URL detection off → never underline or switch to the pointing hand,
        // and drop any underline a prior hover left behind.
        if !cx.global::<Config>().link_url {
            self.clear_hovered_link(cx);
            return false;
        }
        let next = self.link_span_at(col, row);
        if next != self.hovered_link {
            self.hovered_link = next;
            cx.notify();
        }
        self.hovered_link.is_some()
    }

    /// Forget any hovered link (mouse left the grid, or moved onto plain text),
    /// repainting to drop the underline.
    pub fn clear_hovered_link(&mut self, cx: &mut Context<Self>) {
        if self.hovered_link.take().is_some() {
            cx.notify();
        }
    }

    /// Resolve the link span at screen cell `(col, row)`: an OSC 8 hyperlink (the
    /// contiguous run of cells sharing the same target) or a bare URL token in the
    /// row text. Mirrors [`open_link_at`](Self::open_link_at)'s detection so the
    /// underline always covers exactly what a Cmd+click would open.
    fn link_span_at(&self, col: usize, row: usize) -> Option<HoveredLink> {
        let term = self.terminal.term.lock();
        let display_offset = term.grid().display_offset() as i32;
        let line = Line(row as i32 - display_offset);
        let cols = term.columns();
        if col >= cols {
            return None;
        }

        // 1) Explicit OSC 8 hyperlink: highlight the whole contiguous run carrying
        //    the same URI, which may be wider than the visible link text.
        if let Some(hl) = term.grid()[line][Column(col)].hyperlink() {
            let uri = hl.uri().to_string();
            let same = |c: usize| {
                term.grid()[line][Column(c)]
                    .hyperlink()
                    .is_some_and(|h| h.uri() == uri)
            };
            let mut start = col;
            while start > 0 && same(start - 1) {
                start -= 1;
            }
            let mut end = col;
            while end + 1 < cols && same(end + 1) {
                end += 1;
            }
            return Some(HoveredLink {
                line: line.0,
                start,
                end,
            });
        }

        // 2) Bare URL detected in the row's text.
        let mut text = String::with_capacity(cols);
        for c in 0..cols {
            text.push(term.grid()[line][Column(c)].c);
        }
        drop(term);
        let (start, end, _url) = super::search::url_span_at(&text, col)?;
        Some(HoveredLink {
            line: line.0,
            start,
            end,
        })
    }

    /// The inline command line, anchored right where the shell prompt
    /// ends (the cursor cell) and shown only while `input_active`. It carries the
    /// terminal's own font over a transparent background, with no chrome of its
    /// own, so the typed text reads as a natural continuation of the shell prompt
    /// rather than a separate widget. The terminal's own block cursor is hidden
    /// while the editor is live (see `element::paint`), leaving the field's caret
    /// as the single cursor.
    fn render_input_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let (crow, ccol) = self.cursor_cell().unwrap_or((0, 0));
        let cx_left = px(16.) + self.cell_width * (ccol as f32);
        let cy_top = px(8.) + self.line_height * (crow as f32);

        // Reverse-search mode replaces the line with a `(reverse-i-search)` prompt
        // showing the query and the current match.
        if let Some(rs) = &self.reverse_search {
            let label = format!("(reverse-i-search)`{}': ", rs.query());
            let matched = rs
                .match_index()
                .map(|i| self.history[i].clone())
                .unwrap_or_default();
            return div()
                .absolute()
                .left(cx_left)
                .top(cy_top)
                .right_4()
                .h(self.line_height)
                .flex()
                .items_center()
                .font_family(self.font.family.clone())
                .text_size(self.font_size)
                .child(
                    div()
                        .whitespace_nowrap()
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                )
                .child(
                    div()
                        .whitespace_nowrap()
                        .text_color(cx.theme().foreground)
                        .child(matched),
                );
        }

        let chars: Vec<char> = self.cmd.text().chars().collect();
        let len = chars.len();
        let cursor = self.cmd.cursor();
        let marked = self.marked_text.clone();
        let has_marked = !marked.is_empty();
        let selection = self.cmd.selection();

        let theme = cx.theme();
        let fg = theme.foreground;
        let caret_col = theme.caret;
        let muted = theme.muted_foreground;
        // The theme's dedicated selection color, kept translucent so the colored
        // text still reads through it.
        let mut sel_bg = theme.selection;
        sel_bg.a = 0.55;
        let cell_w = self.cell_width;
        let lh = self.line_height;
        // The blinking bar caret should be as tall as the *text*, not the full
        // line box: `lh` is `font_size × line_height_mul` (e.g. 1.35×), so a
        // full-height bar visibly pokes above/below the glyphs (which the cells
        // centre within `lh`). Size it to roughly the glyph extent and centre it
        // in the cell so it hugs the text like a normal editor caret.
        let caret_h = px((self.font_size.as_f32() * 1.2).min(lh.as_f32()));
        let caret_top = px((lh.as_f32() - caret_h.as_f32()) / 2.0);

        // Per-char syntax color, expanded from the highlighter's spans (which tile
        // the whole line), so each character cell can be colored independently.
        let line: String = chars.iter().collect();
        let mut colors: Vec<gpui::Hsla> = Vec::with_capacity(len);
        for span in highlight::highlight(&line) {
            let c = self.kind_color(span.kind, cx);
            for _ in span.text.chars() {
                colors.push(c);
            }
        }

        // Render the input one fixed-width cell per character. This makes the wrap
        // deterministic (exactly grid-width cells per row), so a click anywhere —
        // including a wrapped continuation line — maps back to a char index (see
        // `editor_char_index`). The caret is an absolutely-positioned bar inside a
        // cell, so it never perturbs cell widths.
        let cursor_on = self.cursor_visible;
        // The editor caret honours the configured `cursor_style`, matching the
        // grid cursor `paint_cursor` draws while a program runs — otherwise the
        // shape setting would appear to do nothing at the (most common) prompt.
        // Bar = a thin vertical line; Block = a translucent fill over the cell (so
        // the glyph still reads through, like the grid block); Underline = a line
        // along the cell's baseline. All are absolutely positioned inside their
        // relative parent cell, so `w_full` spans exactly one (wide-aware) cell.
        let cursor_style = cx.global::<Config>().cursor_style;
        let caret_bar = move || {
            use crate::core::config::CursorStyle;
            let base = div().absolute().left_0().bg(caret_col);
            match cursor_style {
                CursorStyle::Bar => base.top(caret_top).w(px(1.5)).h(caret_h),
                CursorStyle::Block => base.top(px(0.)).w_full().h(lh).bg(caret_col.opacity(0.5)),
                CursorStyle::Underline => {
                    let uh = px(2.);
                    base.top(lh - uh).w_full().h(uh)
                }
            }
        };
        let cell = |color: gpui::Hsla, ch: char, selected: bool, caret: bool, underline: bool| {
            // Wide (CJK / fullwidth / emoji) glyphs occupy two terminal cells, so
            // size the box accordingly — otherwise the glyph is clipped by the
            // next cell and the click→char mapping drifts.
            let w = cell_w * (display_width(ch) as f32);
            let mut d = div()
                .relative()
                .flex_none()
                .w(w)
                .h(lh)
                .flex()
                .items_center()
                .text_color(color);
            if selected {
                d = d.bg(sel_bg);
            }
            if underline {
                d = d.border_b_1().border_color(fg);
            }
            d = d.child(ch.to_string());
            if caret {
                d = d.child(caret_bar());
            }
            d.into_any_element()
        };

        // Leading spacer the width of the shell prompt: the first input row begins
        // right after the prompt, while wrapped rows start at the grid's left edge
        // — matching how a real terminal wraps a long command line.
        let mut children: Vec<gpui::AnyElement> = vec![
            div()
                .flex_none()
                .w(cell_w * (ccol as f32))
                .h(lh)
                .into_any_element(),
        ];

        for i in 0..len {
            // IME pre-edit shows underlined at the caret; the bar caret is hidden
            // while composing.
            if i == cursor && has_marked {
                for mc in marked.chars() {
                    children.push(cell(fg, mc, false, false, true));
                }
            }
            let selected = selection.is_some_and(|(s, e)| i >= s && i < e);
            let caret = selection.is_none() && !has_marked && cursor_on && cursor == i;
            children.push(cell(colors[i], chars[i], selected, caret, false));
        }

        // Ghost autosuggestion remainder (only when caret is at the end, no
        // selection / IME), computed up front so the end-of-line caret can ride on
        // the first ghost cell instead of needing its own (which would push the
        // ghost a full cell to the right).
        let ghost: Option<String> = if selection.is_none() && !has_marked {
            self.ghost_suggestion()
                .map(|full| full.chars().skip(len).collect::<String>())
                .filter(|r| !r.is_empty())
        } else {
            None
        };

        // Caret / pre-edit at end of line.
        if cursor == len {
            if has_marked {
                for mc in marked.chars() {
                    children.push(cell(fg, mc, false, false, true));
                }
            } else if ghost.is_none() {
                // No ghost following: a trailing cell carries the caret (and is the
                // click target for "end of line").
                let mut tail = div().relative().flex_none().w(cell_w).h(lh);
                if selection.is_none() && cursor_on {
                    tail = tail.child(caret_bar());
                }
                children.push(tail.into_any_element());
            }
            // else: the caret rides on the first ghost cell below.
        }

        if let Some(rem) = ghost {
            for (gi, gc) in rem.chars().enumerate() {
                let caret = gi == 0 && cursor == len && cursor_on;
                children.push(cell(muted, gc, false, caret, false));
            }
        }

        div()
            .absolute()
            .left(px(16.))
            .top(cy_top)
            .right_4()
            .min_h(lh)
            .flex()
            .flex_wrap()
            .items_center()
            // Transparent: the text overlays the grid in place, reading as a
            // natural continuation of the shell prompt rather than a separate bar.
            .font_family(self.font.family.clone())
            .text_size(self.font_size)
            .line_height(lh)
            .text_color(fg)
            .children(children)
    }

    /// The floating completion menu, shown below the word while a completion is
    /// active. Renders the re-filtered candidates with the picked row
    /// highlighted; the list is capped with a "+N more" footer so a huge match
    /// set stays compact.
    fn render_completion_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let s = self.completion.as_ref()?;
        // The re-filtered view of the candidates; refilter() closes the session
        // before this can go empty, but guard anyway.
        let items: Vec<&completion::Candidate> = s.filtered.iter().map(|&i| &s.all[i]).collect();
        if items.is_empty() {
            return None;
        }
        let (srow, scol) = self.cursor_cell()?;

        // Decide how many rows to show and whether to drop the menu below the input
        // row or flip it above — based on the room actually available in the grid,
        // so a prompt near the bottom of the window doesn't push the menu off
        // screen. The window-around-the-selection keeps the highlighted candidate
        // visible even when the full list is taller than the space.
        const MAX_ROWS: usize = 10;
        let total_rows = self.terminal.term.lock().screen_lines();
        let (place_above, visible, first) = menu_layout(
            total_rows,
            srow,
            items.len(),
            s.index.unwrap_or(0),
            MAX_ROWS,
        );
        let hidden_above = first;
        let hidden_below = items.len() - first - visible;

        let theme = cx.theme();
        // Each row is forced to exactly `line_height` so the `menu_h` estimate
        // below is exact — critical for upward placement, where an underestimate
        // would let the menu's real bottom edge cover the input line.
        let lh = self.line_height;
        let row = |i: usize| {
            let cand = items[i];
            let selected = s.index == Some(i);
            // Leading icon: the Fig spec's per-entry icon when present (emoji
            // rendered as-is, `fig://icon?type=…` mapped to a bundled glyph),
            // else a per-kind default. Glyphs stay monochrome (muted, like the
            // tab strip); emoji keep their own color.
            let icon_color = if selected {
                theme.foreground
            } else {
                theme.muted_foreground
            };
            let icon = completion_row_icon(cand.icon.as_deref(), cand.kind, icon_color);
            // Directories show their trailing `/` in the menu too.
            let label = if cand.is_dir() && !cand.text.ends_with('/') {
                format!("{}/", cand.text)
            } else {
                cand.text.clone()
            };
            div()
                .h(lh)
                .flex()
                .items_center()
                .gap_1p5()
                .px_2()
                .whitespace_nowrap()
                // Use the app-tuned `list_active` fill (same as the command
                // palette) rather than the stock `accent`: `apply_theme` never
                // overrides `accent`, so in light mode it stays a near-white
                // `neutral-100` that vanishes against the white popover — the
                // selection looked unhighlighted. `list_active` is a per-theme
                // bg/fg blend that reads clearly in both light and dark.
                .when(selected, |d| {
                    d.bg(theme.list_active).text_color(theme.foreground)
                })
                .child(icon)
                .child(div().flex_shrink_0().child(label))
                // Second column: the flag/subcommand description from the command
                // signature — muted, sized to its content. The menu's `max_w` +
                // `overflow_hidden` clip an over-long line; the name never shrinks.
                .when_some(cand.description.clone(), |d, desc| {
                    d.child(div().ml_2().text_color(theme.muted_foreground).child(desc))
                })
                .into_any_element()
        };
        let rows: Vec<gpui::AnyElement> = (first..first + visible).map(row).collect();

        // Menu height (for upward placement) = rows + any overflow footers.
        let footer = |n: usize, label: String| {
            (n > 0).then(|| {
                div()
                    .h(lh)
                    .flex()
                    .items_center()
                    .px_2()
                    .text_color(theme.muted_foreground)
                    .child(label)
                    .into_any_element()
            })
        };
        let footer_lines = (hidden_above > 0) as usize + (hidden_below > 0) as usize;
        let line_count = visible + footer_lines;
        let menu_h = self.line_height * (line_count as f32) + px(10.);

        // A small gap so the menu never sits flush against the input line — in
        // particular, when flipped above it clears the caret instead of covering it.
        let gap = px(6.);
        // Anchor at the command start (the cursor cell), where the line begins.
        let x = px(16.) + self.cell_width * (scol as f32);
        let y = if place_above {
            px(8.) + self.line_height * (srow as f32) - menu_h - gap
        } else {
            px(8.) + self.line_height * ((srow + 1) as f32) + gap
        };

        Some(
            div()
                .absolute()
                .left(x)
                .top(y)
                .flex()
                .flex_col()
                .py_1()
                .min_w(px(120.))
                .max_w(px(480.))
                .overflow_hidden()
                .bg(theme.popover)
                .border_1()
                .border_color(theme.border)
                .rounded(px(6.))
                .font_family(self.font.family.clone())
                .text_size(self.font_size)
                .text_color(theme.popover_foreground)
                .children(footer(hidden_above, format!("↑ {hidden_above} more")))
                .children(rows)
                .children(footer(hidden_below, format!("↓ {hidden_below} more"))),
        )
    }

    /// Map a highlighter token kind to a theme color.
    fn kind_color(&self, kind: TokenKind, cx: &App) -> gpui::Hsla {
        let theme = cx.theme();
        match kind {
            TokenKind::Command => theme.green,
            TokenKind::Flag => theme.cyan,
            TokenKind::Path => theme.blue,
            TokenKind::StringLit => theme.yellow,
            TokenKind::Operator => theme.magenta,
            TokenKind::Comment => theme.muted_foreground,
            TokenKind::Arg | TokenKind::Whitespace => theme.foreground,
        }
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Editor live: adopt anything typed while it was disengaged. Held gap
        // input goes straight in — the PTY never saw those bytes, so nothing
        // needs a wipe. Input that did reach the PTY waits for zle to read
        // (`zle_reading`) so its ^U wipe is consumed silently; the `133;B`
        // that arms the flag arrives as pane output, so a render always
        // follows it (Output → Wakeup → notify). Both prepend: they were
        // typed before any post-engage keys already sitting in the editor.
        if self.input_active() {
            if let Some(net) = self.hold.engage() {
                self.cmd.prepend_str(&net);
            }
            if self.terminal.zle_reading() {
                self.flush_typeahead();
            }
        }
        let entity = cx.entity();
        let search_bar = self
            .search
            .as_ref()
            .map(|s| self.render_search_bar(s, window, cx));

        // The command editor lives on the terminal's own focus handle (no separate
        // input widget to focus), so there's no per-frame focus routing: the
        // terminal keeps focus throughout, and the editor overlay is rendered only
        // while idle at the prompt.
        let input_bar = self.input_active().then(|| self.render_input_bar(cx));
        let completion_menu = self
            .input_active()
            .then(|| self.render_completion_menu(cx))
            .flatten();

        // Captured for the right-click menu: the focus handle routes dispatched
        // actions to this terminal (and lets tab/split ones bubble to the root),
        // and the selection state greys out "Copy" when there's nothing selected.
        let menu_focus = self.focus_handle.clone();
        let has_selection = self.has_selection();

        div()
            .id("terminal-surface")
            .track_focus(&self.focus_handle)
            .key_context("Terminal")
            .size_full()
            .relative()
            .overflow_hidden()
            .px_4()
            .py_2()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseDownEvent, window, cx| {
                    // A focusable child that was clicked (e.g. the search field)
                    // has already claimed focus via gpui's track_focus auto-focus
                    // and called `prevent_default`. Honor that convention and don't
                    // steal focus back — otherwise clicking into the search bar
                    // instantly bounces focus to the terminal and the field can
                    // never be re-entered for editing.
                    if window.default_prevented() {
                        return;
                    }
                    window.focus(&this.focus_handle, cx);
                }),
            )
            // Files dragged from Finder (etc.) onto the terminal insert their
            // shell-escaped paths like a paste. `drag_over` tints the surface so
            // the drop target is obvious while a drag hovers.
            .drag_over::<ExternalPaths>(|s, _, _, cx| s.bg(cx.theme().drag_border.opacity(0.12)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                window.focus(&this.focus_handle, cx);
                this.drop_files(paths, cx);
            }))
            // Context-menu actions handled by this view; tab/split actions in the
            // same menu fall through to `Tty7App`.
            .on_action(cx.listener(|this, _: &CopyText, _w, cx| this.copy_selection(cx)))
            .on_action(cx.listener(|this, _: &PasteText, _w, cx| this.paste_from_clipboard(cx)))
            .on_action(cx.listener(|this, _: &SelectAll, _w, cx| this.select_all_contextual(cx)))
            .on_action(
                cx.listener(|this, _: &FindInTerminal, window, cx| this.open_search(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ClearScrollback, _w, cx| this.clear_scrollback(cx)))
            // Tab / Shift-Tab are claimed here (in the "Terminal" key context) so
            // they reach the shell instead of triggering Root's focus navigation.
            // Tab → HT (0x09); Shift-Tab → CSI Z (back-tab), the standard sequence.
            // While the search field is focused it owns these keys, so propagate.
            .on_action(cx.listener(|this, _: &SendTab, _w, cx| {
                if this.search_focused {
                    cx.propagate();
                } else if this.input_active() {
                    this.complete_tab(true, cx);
                } else {
                    let bytes = this.tab_bytes(false);
                    this.send_to_pty(&bytes, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SendBackTab, _w, cx| {
                if this.search_focused {
                    cx.propagate();
                } else if this.input_active() {
                    this.complete_tab(false, cx);
                } else {
                    let bytes = this.tab_bytes(true);
                    this.send_to_pty(&bytes, cx);
                }
            }))
            .child(TerminalElement::new(entity))
            .children(search_bar)
            .children(input_bar)
            .children(completion_menu)
            // Right-click context menu (gpui-component PopupMenu).
            .context_menu(move |menu, _window, _cx| {
                // Small size = tighter 20px rows; the default 26px felt too airy.
                // A fixed min-width keeps the menu a consistent, intentional size
                // instead of hugging the longest label (which reads ragged).
                // Copy/Paste/Select All/Find are dispatched inline (see
                // `handle_cmd_shortcut`) with no registered `KeyBinding`, so the menu
                // can't auto-derive their hints the way it does for the items below.
                // We render the hint ourselves via `menu_row_with_hint` to keep the
                // whole menu consistent, rather than register real bindings (which
                // would risk the Ctrl+C SIGINT fall-through on Windows/Linux).
                menu.with_size(Size::Small)
                    .min_w(px(220.))
                    .action_context(menu_focus.clone())
                    .menu_element_with_disabled(
                        Box::new(CopyText),
                        !has_selection,
                        menu_row_with_hint("Copy", Some("secondary-c")),
                    )
                    .menu_element(
                        Box::new(PasteText),
                        menu_row_with_hint("Paste", Some("secondary-v")),
                    )
                    .menu_element(
                        Box::new(SelectAll),
                        menu_row_with_hint("Select All", mac_only("secondary-a")),
                    )
                    .separator()
                    .menu_element(
                        Box::new(FindInTerminal),
                        menu_row_with_hint("Find…", mac_only("secondary-f")),
                    )
                    .menu("Clear", Box::new(ClearScrollback))
                    .separator()
                    .menu("Split Right", Box::new(SplitRight))
                    .menu("Split Down", Box::new(SplitDown))
                    .menu("Maximize Pane", Box::new(ToggleMaximizePane))
                    .separator()
                    .menu("New Tab", Box::new(NewTab))
                    .menu("Close Pane", Box::new(CloseActiveTab))
            })
    }
}

/// Build a context-menu row that shows its shortcut right-aligned, matching the
/// hint gpui-component auto-renders for items whose action has a registered
/// keybinding. `key` is `None` when the action has no shortcut on this platform,
/// leaving the row hint-less like a plain item.
fn menu_row_with_hint(
    label: &'static str,
    key: Option<&'static str>,
) -> impl Fn(&mut Window, &mut App) -> gpui::AnyElement {
    move |_window, _cx| {
        let hint = key.map(|k| {
            // Strip Kbd's keycap box (filled bg + border) so it reads as the same
            // quiet muted-foreground hint the auto-rendered items show — see
            // gpui-component's `PopupMenu::render_key_binding`.
            Kbd::new(gpui::Keystroke::parse(k).expect("valid static keystroke"))
                .p_0()
                .flex_nowrap()
                .border_0()
                .bg(gpui::transparent_white())
        });
        h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .justify_between()
            .child(label)
            .children(hint)
            .into_any_element()
    }
}

/// `Some(key)` on macOS, `None` elsewhere. ⌘A (Select All) and ⌘F (Find) are
/// wired only on macOS; on Windows/Linux those chords keep their readline meaning
/// (line-start / forward-char), so the menu must not advertise them there.
#[cfg(target_os = "macos")]
fn mac_only(key: &'static str) -> Option<&'static str> {
    Some(key)
}
#[cfg(not(target_os = "macos"))]
fn mac_only(_key: &'static str) -> Option<&'static str> {
    None
}

/// Approximate terminal display width of a char in cells: 2 for East-Asian
/// wide / fullwidth glyphs and most emoji, 1 otherwise. Mirrors how the grid
/// (alacritty) lays out wide characters, so the editor's per-char cells and
/// click hit-testing line up with the shell's own rendering.
fn display_width(c: char) -> usize {
    let u = c as u32;
    let wide = matches!(u,
        0x1100..=0x115F   // Hangul Jamo
        | 0x2329 | 0x232A
        | 0x2E80..=0x303E // CJK radicals, Kangxi, punctuation
        | 0x3041..=0x33FF // Hiragana, Katakana, CJK symbols
        | 0x3400..=0x4DBF // CJK Ext A
        | 0x4E00..=0x9FFF // CJK Unified
        | 0xA000..=0xA4CF // Yi
        | 0xAC00..=0xD7A3 // Hangul syllables
        | 0xF900..=0xFAFF // CJK compatibility
        | 0xFE10..=0xFE19 | 0xFE30..=0xFE6F // vertical / compat forms
        | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 // fullwidth forms
        | 0x1F300..=0x1FAFF // emoji & pictographs
        | 0x20000..=0x3FFFD // CJK Ext B+
    );
    if wide { 2 } else { 1 }
}

/// Where a wheel tick goes, decided by the modes the app negotiated.
#[derive(Debug, PartialEq)]
enum WheelRoute {
    /// Mouse-wheel reporting: one report per scrolled line (64 up / 65 down).
    Report { base: u8 },
    /// Alternate scroll: the wheel becomes arrow keys (less, man).
    Arrows { seq: &'static [u8] },
    /// Nothing negotiated: scroll the local scrollback.
    Scrollback,
}

/// Route a wheel tick. Shift always bypasses app handling (the standard
/// "scroll the terminal anyway" escape hatch), mouse reporting wins over
/// alternate scroll when both are on, and alternate scroll additionally
/// requires the *alt screen* — an app that set ALTERNATE_SCROLL but has
/// returned to the primary screen must not hijack the wheel from the
/// scrollback.
fn wheel_route(mode: TermMode, shift: bool, up: bool) -> WheelRoute {
    if !shift && mode.intersects(TermMode::MOUSE_MODE) {
        return WheelRoute::Report {
            base: if up { 64 } else { 65 },
        };
    }
    if !shift && mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
        let seq: &'static [u8] = match (up, mode.contains(TermMode::APP_CURSOR)) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        };
        return WheelRoute::Arrows { seq };
    }
    WheelRoute::Scrollback
}

/// One mouse report, encoded for the protocol the app negotiated. SGR (1006)
/// prints decimal 1-based coordinates and keeps the button in the final
/// letter (`M` press / `m` release); X10 packs everything into three bytes,
/// which caps coordinates at 223 (255 − 32 − 1) — events beyond that are
/// dropped (`None`) rather than sent corrupted — and loses the button
/// identity on release (code 3). Modifier bits (shift 4 / alt 8 / ctrl 16)
/// are added to `base` in both encodings.
fn encode_mouse(
    sgr: bool,
    base: u8,
    mods: &Modifiers,
    col: usize,
    row: usize,
    pressed: bool,
) -> Option<Vec<u8>> {
    let mut mod_bits = 0u8;
    if mods.shift {
        mod_bits += 4;
    }
    if mods.alt {
        mod_bits += 8;
    }
    if mods.control {
        mod_bits += 16;
    }

    if sgr {
        let c = if pressed { 'M' } else { 'm' };
        let msg = format!("\x1b[<{};{};{}{}", base + mod_bits, col + 1, row + 1, c);
        Some(msg.into_bytes())
    } else {
        // X10 encoding caps coordinates at 223 (255 - 32).
        if col >= 223 || row >= 223 {
            return None;
        }
        let code = if pressed {
            base + mod_bits
        } else {
            3 + mod_bits
        };
        Some(vec![
            0x1b,
            b'[',
            b'M',
            32 + code,
            (32 + 1 + col) as u8,
            (32 + 1 + row) as u8,
        ])
    }
}

/// The focus-event report for a focus change, when the app enabled focus
/// reporting (mode 1004): `CSI I` on gain, `CSI O` on loss, `None` when the
/// mode is off (the overwhelmingly common case — nothing reaches the PTY).
fn focus_report_bytes(mode: TermMode, focused: bool) -> Option<&'static [u8]> {
    if !mode.contains(TermMode::FOCUS_IN_OUT) {
        return None;
    }
    Some(if focused { b"\x1b[I" } else { b"\x1b[O" })
}

/// A completion row's leading icon, in a fixed-width centered slot so emoji and
/// SVG glyphs share one column. Prefers the Fig spec's `icon` (emoji rendered
/// as text, `fig://icon?type=…` mapped to a bundled glyph), falling back to a
/// per-kind default.
fn completion_row_icon(
    raw: Option<&str>,
    kind: CandidateKind,
    color: gpui::Hsla,
) -> gpui::AnyElement {
    let slot = |child: gpui::AnyElement| {
        div()
            .w(px(16.))
            .flex()
            .justify_center()
            .items_center()
            .child(child)
            .into_any_element()
    };

    if let Some(raw) = raw {
        if let Some(emoji) = fig_icon_emoji(raw) {
            return slot(
                div()
                    .text_size(px(13.))
                    .child(emoji.to_string())
                    .into_any_element(),
            );
        }
        if let Some(name) = fig_icon_glyph(raw) {
            return slot(
                Icon::new(name)
                    .size(px(15.))
                    .text_color(color)
                    .into_any_element(),
            );
        }
    }

    // Per-kind default: a terminal glyph for commands / subcommands / values, a
    // dash for flags, folder / file for paths.
    let name = match kind {
        CandidateKind::Command | CandidateKind::Value => IconName::SquareTerminal,
        CandidateKind::Flag => IconName::Dash,
        CandidateKind::Dir => IconName::Folder,
        CandidateKind::File => IconName::File,
    };
    slot(
        Icon::new(name)
            .size(px(15.))
            .text_color(color)
            .into_any_element(),
    )
}

/// The emoji to render for a Fig `icon`, if it is one: a bare emoji string, or
/// the `badge` of a `fig://template?…`. `None` for a named `fig://icon?type=…`.
fn fig_icon_emoji(raw: &str) -> Option<&str> {
    if raw.is_empty() {
        None
    } else if !raw.starts_with("fig://") {
        Some(raw)
    } else if raw.starts_with("fig://template") {
        fig_query_param(raw, "badge")
    } else {
        None
    }
}

/// Map a `fig://icon?type=X` to one of tty7's bundled glyphs, or `None` to fall
/// back to the per-kind default — we ship no brand glyph for node/docker/npm/….
fn fig_icon_glyph(raw: &str) -> Option<IconName> {
    let ty = raw
        .strip_prefix("fig://icon")
        .and_then(|r| fig_query_param(r, "type"))?;
    match ty {
        "folder" => Some(IconName::Folder),
        "file" => Some(IconName::File),
        "git" => Some(IconName::Github),
        "asterisk" => Some(IconName::Asterisk),
        _ => None,
    }
}

/// Extract `key`'s value from a `fig://…?a=1&b=2` query string.
fn fig_query_param<'a>(raw: &'a str, key: &str) -> Option<&'a str> {
    raw.split_once('?')?.1.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// Place the completion menu and window its rows around the selection — the
/// pure core of [`TerminalView::render_completion_menu`]. `total_rows` is the
/// grid height, `srow` the input row the menu anchors to, `count` the number of
/// candidates (≥ 1), `sel` the selected index, and `max_rows` the display cap.
/// Returns `(place_above, visible, first)`: whether the menu flips above the
/// input row (only when it doesn't fit below), how many candidate rows to show
/// (at least 1, even squeezed against an edge), and the index of the first
/// visible candidate — chosen so `sel` always lies within
/// `first..first + visible`.
fn menu_layout(
    total_rows: usize,
    srow: usize,
    count: usize,
    sel: usize,
    max_rows: usize,
) -> (bool, usize, usize) {
    let want = count.min(max_rows);
    let below = total_rows.saturating_sub(srow + 1);
    let above = srow;
    // The space budget must include the up-to-two "↑/↓ N more" footer lines a
    // *windowed* list renders — they share the menu box with the candidate
    // rows, so sizing on candidates alone let the menu (and, downward, the
    // selected row riding the window's bottom edge) spill off screen.
    let footers = if count > want { 2 } else { 0 };
    let need = want + footers;
    let (place_above, visible) = if below >= need {
        (false, want)
    } else if above >= need {
        (true, want)
    } else {
        // Cramped on both sides: take the larger side and squeeze the
        // candidate rows under it, reserving the footer lines that squeezing
        // (which hides candidates) makes appear. Always show at least one row.
        let squeeze = |room: usize| room.saturating_sub(2).max(1);
        if above > below {
            (true, squeeze(above))
        } else {
            (false, squeeze(below))
        }
    };
    let visible = visible.min(count);
    // Scroll the visible window so the selected candidate stays in view.
    let first = sel
        .saturating_sub(visible.saturating_sub(1))
        .min(count.saturating_sub(visible));
    (place_above, visible, first)
}

/// Map a click cell to a char index in the wrapped input line — the pure core
/// of [`TerminalView::editor_char_index`], simulating the layout exactly as
/// `render_input_bar` produces it: char 0 starts at column `scol` (right after
/// the prompt) of the input's first row, each char advances by its display
/// width, and a char that wouldn't fit wraps whole to column 0 of the next row.
/// `col` is the clicked column and `target` the clicked row minus the input's
/// first row. A hit on a char cell returns its index; a click left of a row's
/// first char snaps to that char; past a row's content snaps to the next row's
/// first char (or the line end). Rows beyond the input return `len` with
/// `clamp` (for drags) and `None` without (so the click isn't an editor click).
fn wrapped_click_index(
    chars: &[char],
    scol: usize,
    cols: usize,
    col: usize,
    target: usize,
    clamp: bool,
) -> Option<usize> {
    let len = chars.len();
    // `positions[i]` is the (row, start-col, width) of char `i`.
    let mut positions: Vec<(usize, usize, usize)> = Vec::with_capacity(len);
    let mut r = 0usize;
    let mut c = scol;
    for &ch in chars {
        let w = display_width(ch).max(1);
        if c + w > cols {
            r += 1;
            c = 0;
        }
        positions.push((r, c, w));
        c += w;
    }
    // The renderer appends a one-cell end-of-line caret slot after the last
    // char; when the content exactly fills its row, that slot wraps to the next
    // row (where the caret is visibly drawn), so clicks there must still count
    // as "this input", not fall past it.
    let end_row = if c >= cols { r + 1 } else { r };
    if target > end_row {
        return clamp.then_some(len);
    }
    // Exact hit on a char cell.
    for (i, &(pr, pc, pw)) in positions.iter().enumerate() {
        if pr == target && col >= pc && col < pc + pw {
            return Some(i);
        }
    }
    // Click on the row but left of its first char.
    if let Some(fi) = positions.iter().position(|&(pr, _, _)| pr == target) {
        if col < positions[fi].1 {
            return Some(fi);
        }
    }
    // Past the row's content → start of the next row, or end of line.
    match positions.iter().position(|&(pr, _, _)| pr > target) {
        Some(ni) => Some(ni),
        None => Some(len),
    }
}

/// Advance the continuous scroll position `offset + frac` (in lines, 0 =
/// bottom, growing into history) by `delta` lines, clamped to `[0, max]`.
/// Returns the whole-line jump to hand to the emulator's `display_offset`
/// and the new sub-line fraction in `[0, 1)`.
fn smooth_scroll_step(offset: usize, frac: f32, delta: f32, max: usize) -> (i32, f32) {
    let pos = (offset as f32 + frac + delta).clamp(0., max as f32);
    let new_offset = pos.floor();
    (new_offset as i32 - offset as i32, pos - new_offset)
}

/// Lines to scroll per auto-scroll tick for a selection drag sitting
/// `overshoot` lines past the pane edge (sign = direction, positive = up into
/// history). At least one line per tick so grazing the edge still crawls;
/// farther out speeds up, capped so a wild fling stays controllable
/// (8 lines/tick at a 50ms cadence ≈ 160 lines/s).
fn drag_scroll_step(overshoot: f32) -> i32 {
    let lines = overshoot.abs().ceil().clamp(1., 8.) as i32;
    if overshoot < 0. { -lines } else { lines }
}

#[cfg(test)]
mod tests {
    use super::{
        WheelRoute, clipboard_paste_text, display_width, drag_scroll_step, encode_mouse,
        fallback_chain, fig_icon_emoji, fig_icon_glyph, focus_report_bytes, menu_layout,
        paste_bytes, shell_escape_path, smooth_scroll_step, trim_trailing_spaces, wheel_route,
        wrapped_click_index,
    };
    use alacritty_terminal::term::TermMode;
    use gpui::{ClipboardEntry, ClipboardItem, ExternalPaths, Modifiers};
    use gpui_component::IconName;
    use std::path::PathBuf;

    /// The bundled Hack always anchors the fallback chain so prompt symbols
    /// (`➜`, `❯`, powerline wedges) never fall through to the OS cascade —
    /// unless the user already covers it as primary or in their own list.
    #[test]
    fn fallback_chain_pins_bundled_hack_last() {
        let configured = vec!["Menlo".to_string(), "Apple Color Emoji".to_string()];

        // A custom primary that may lack the prompt symbols → Hack appended.
        assert_eq!(
            fallback_chain("JetBrains Mono", &configured),
            ["Menlo", "Apple Color Emoji", "Hack"]
        );

        // Hack as the primary face already covers everything it could add.
        assert_eq!(
            fallback_chain("Hack", &configured),
            ["Menlo", "Apple Color Emoji"]
        );

        // A user who lists Hack explicitly keeps their chosen position.
        let with_hack = vec!["Hack".to_string(), "Menlo".to_string()];
        assert_eq!(fallback_chain("SF Mono", &with_hack), ["Hack", "Menlo"]);

        // "Hack Nerd Font" is a different family — the bundled face still lands.
        assert_eq!(
            fallback_chain("Hack Nerd Font", &[]),
            ["Hack"],
            "a Hack-prefixed family name must not suppress the bundled anchor"
        );
    }

    /// The wheel reaches the app only through the modes it negotiated: mouse
    /// reporting first, alternate scroll second, local scrollback otherwise.
    #[test]
    fn wheel_routes_by_negotiated_mode_with_reporting_first() {
        // Any mouse mode → per-line reports, 64 up / 65 down.
        let mouse = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            wheel_route(mouse, false, true),
            WheelRoute::Report { base: 64 }
        );
        assert_eq!(
            wheel_route(mouse, false, false),
            WheelRoute::Report { base: 65 }
        );

        // Alt screen + alternate scroll (less, man) → arrow keys, and the
        // cursor-keys mode picks between CSI and SS3 encodings.
        let alt = TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL;
        assert_eq!(
            wheel_route(alt, false, true),
            WheelRoute::Arrows { seq: b"\x1b[A" }
        );
        assert_eq!(
            wheel_route(alt, false, false),
            WheelRoute::Arrows { seq: b"\x1b[B" }
        );
        assert_eq!(
            wheel_route(alt | TermMode::APP_CURSOR, false, true),
            WheelRoute::Arrows { seq: b"\x1bOA" }
        );
        assert_eq!(
            wheel_route(alt | TermMode::APP_CURSOR, false, false),
            WheelRoute::Arrows { seq: b"\x1bOB" }
        );

        // Both negotiated (vim with mouse on) → reporting wins.
        assert_eq!(
            wheel_route(mouse | alt, false, true),
            WheelRoute::Report { base: 64 }
        );

        // Nothing negotiated → local scrollback.
        assert_eq!(
            wheel_route(TermMode::empty(), false, true),
            WheelRoute::Scrollback
        );
    }

    /// ALTERNATE_SCROLL without the alt screen must NOT hijack the wheel:
    /// after `less` exits back to the primary screen with the mode bit still
    /// set, the wheel has to scroll the terminal's own history again.
    #[test]
    fn wheel_ignores_alternate_scroll_outside_the_alt_screen() {
        assert_eq!(
            wheel_route(TermMode::ALTERNATE_SCROLL, false, true),
            WheelRoute::Scrollback
        );
    }

    /// Shift is the universal "scroll the terminal anyway" escape hatch — it
    /// bypasses both mouse reporting and alternate scroll.
    #[test]
    fn shift_wheel_always_scrolls_the_local_scrollback() {
        let everything = TermMode::MOUSE_MOTION
            | TermMode::ALT_SCREEN
            | TermMode::ALTERNATE_SCROLL
            | TermMode::APP_CURSOR;
        assert_eq!(wheel_route(everything, true, true), WheelRoute::Scrollback);
        assert_eq!(wheel_route(everything, true, false), WheelRoute::Scrollback);
    }

    /// SGR (1006) reports print 1-based decimal coordinates, stack the
    /// modifier bits onto the button code, and carry press/release in the
    /// final letter. A drift in any of these lands clicks one cell off in
    /// vim/tmux.
    #[test]
    fn sgr_mouse_reports_one_based_decimal_with_modifier_bits() {
        let plain = Modifiers::default();
        // Left press at 0-based (col 4, row 8) → "5;9", press = 'M'.
        assert_eq!(
            encode_mouse(true, 0, &plain, 4, 8, true).unwrap(),
            b"\x1b[<0;5;9M".to_vec()
        );
        // Release keeps the button identity (unlike X10) and flips to 'm'.
        assert_eq!(
            encode_mouse(true, 2, &plain, 4, 8, false).unwrap(),
            b"\x1b[<2;5;9m".to_vec()
        );
        // shift 4 + alt 8 + ctrl 16 = 28 on top of the base code.
        let all = Modifiers {
            shift: true,
            alt: true,
            control: true,
            ..Modifiers::default()
        };
        assert_eq!(
            encode_mouse(true, 0, &all, 0, 0, true).unwrap(),
            b"\x1b[<28;1;1M".to_vec()
        );
        // Wheel (64/65) and drag-motion (32+) codes ride the same path.
        assert_eq!(
            encode_mouse(true, 64, &plain, 10, 3, true).unwrap(),
            b"\x1b[<64;11;4M".to_vec()
        );
        assert_eq!(
            encode_mouse(true, 35, &plain, 1, 1, true).unwrap(),
            b"\x1b[<35;2;2M".to_vec()
        );
    }

    /// SGR exists precisely because X10 tops out at 223 — clicks on a wide
    /// terminal past that column must still encode, not drop or wrap.
    #[test]
    fn sgr_mouse_has_no_coordinate_cap() {
        let plain = Modifiers::default();
        assert_eq!(
            encode_mouse(true, 0, &plain, 500, 300, true).unwrap(),
            b"\x1b[<0;501;301M".to_vec()
        );
    }

    /// X10 packs the code and both coordinates into single bytes offset by
    /// 32 (+1 for 1-based), loses the button identity on release (code 3),
    /// and takes the same modifier bits.
    #[test]
    fn x10_mouse_packs_bytes_and_drops_button_on_release() {
        let plain = Modifiers::default();
        assert_eq!(
            encode_mouse(false, 0, &plain, 4, 8, true).unwrap(),
            vec![0x1b, b'[', b'M', 32, 32 + 1 + 4, 32 + 1 + 8]
        );
        // Any button's release encodes as code 3 — X10 can't say which.
        assert_eq!(
            encode_mouse(false, 2, &plain, 4, 8, false).unwrap(),
            vec![0x1b, b'[', b'M', 32 + 3, 32 + 1 + 4, 32 + 1 + 8]
        );
        let ctrl = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        assert_eq!(
            encode_mouse(false, 1, &ctrl, 0, 0, true).unwrap(),
            vec![0x1b, b'[', b'M', 32 + 1 + 16, 33, 33]
        );
    }

    /// X10's byte packing can't express coordinates past 223 (255 − 32); the
    /// event must be dropped whole — a wrapped byte would teleport the click
    /// to the far side of the grid.
    #[test]
    fn x10_mouse_drops_out_of_range_coordinates_whole() {
        let plain = Modifiers::default();
        assert!(encode_mouse(false, 0, &plain, 223, 0, true).is_none());
        assert!(encode_mouse(false, 0, &plain, 0, 223, true).is_none());
        // The last representable cell still encodes, right at byte 255.
        let last = encode_mouse(false, 0, &plain, 222, 222, true).unwrap();
        assert_eq!(&last[4..], &[255, 255]);
    }

    #[test]
    fn fig_icon_emoji_takes_bare_emoji_and_template_badge_only() {
        // A bare emoji renders as-is.
        assert_eq!(fig_icon_emoji("⚙️"), Some("⚙️"));
        // A colored template contributes its badge emoji.
        assert_eq!(
            fig_icon_emoji("fig://template?color=2ecc71&badge=🔥"),
            Some("🔥")
        );
        // A named glyph icon is not an emoji (it maps to an SVG instead).
        assert_eq!(fig_icon_emoji("fig://icon?type=git"), None);
        // A badge-less template has no emoji to show.
        assert_eq!(fig_icon_emoji("fig://template?color=2ecc71"), None);
        assert_eq!(fig_icon_emoji(""), None);
    }

    #[test]
    fn fig_icon_glyph_maps_known_types_and_falls_back_otherwise() {
        // `IconName` is neither `PartialEq` nor `Debug`, so match on the variant.
        assert!(matches!(
            fig_icon_glyph("fig://icon?type=folder"),
            Some(IconName::Folder)
        ));
        assert!(matches!(
            fig_icon_glyph("fig://icon?type=file"),
            Some(IconName::File)
        ));
        assert!(matches!(
            fig_icon_glyph("fig://icon?type=git"),
            Some(IconName::Github)
        ));
        // No bundled brand glyph → fall back to the per-kind default.
        assert!(fig_icon_glyph("fig://icon?type=docker").is_none());
        assert!(fig_icon_glyph("⚙️").is_none());
    }

    #[test]
    fn focus_reports_only_when_the_app_opted_in() {
        // Mode 1004 off (the default): no bytes reach the PTY on focus changes.
        assert_eq!(focus_report_bytes(TermMode::empty(), true), None);
        assert_eq!(focus_report_bytes(TermMode::empty(), false), None);
        // Opted in: CSI I on gain, CSI O on loss — what vim/tmux key off.
        let mode = TermMode::FOCUS_IN_OUT;
        assert_eq!(focus_report_bytes(mode, true), Some(b"\x1b[I".as_slice()));
        assert_eq!(focus_report_bytes(mode, false), Some(b"\x1b[O".as_slice()));
        // Unrelated modes don't leak reports.
        assert_eq!(focus_report_bytes(TermMode::MOUSE_MOTION, true), None);
    }

    #[test]
    fn smooth_scroll_step_accumulates_and_clamps() {
        // Sub-line deltas accumulate in the fraction without moving the grid.
        assert_eq!(smooth_scroll_step(0, 0.0, 0.4, 100), (0, 0.4));
        // Crossing a line boundary hands the whole line to the emulator and
        // keeps the remainder.
        let (jump, frac) = smooth_scroll_step(0, 0.4, 0.8, 100);
        assert_eq!(jump, 1);
        assert!((frac - 0.2).abs() < 1e-4);
        // Scrolling back down borrows from the offset.
        let (jump, frac) = smooth_scroll_step(5, 0.2, -0.5, 100);
        assert_eq!(jump, -1);
        assert!((frac - 0.7).abs() < 1e-4);
        // The bottom clamps to exactly (0, 0): no fraction survives.
        assert_eq!(smooth_scroll_step(3, 0.5, -10.0, 100), (-3, 0.0));
        // The top of history clamps to (max, 0) likewise.
        assert_eq!(smooth_scroll_step(98, 0.0, 7.3, 100), (2, 0.0));
        // No history at all (alt screen / fresh shell): position is pinned.
        assert_eq!(smooth_scroll_step(0, 0.0, 2.5, 0), (0, 0.0));
    }

    /// Selection auto-scroll: grazing the edge crawls one line per tick,
    /// farther out speeds up with the overshoot, a fling caps at 8, and the
    /// sign follows the direction (positive = up into history).
    #[test]
    fn drag_scroll_step_scales_with_overshoot_and_caps() {
        assert_eq!(drag_scroll_step(0.2), 1);
        assert_eq!(drag_scroll_step(-0.2), -1);
        assert_eq!(drag_scroll_step(3.5), 4);
        assert_eq!(drag_scroll_step(-3.5), -4);
        assert_eq!(drag_scroll_step(50.0), 8);
        assert_eq!(drag_scroll_step(-50.0), -8);
    }

    #[test]
    fn trim_trailing_spaces_strips_per_line_and_preserves_structure() {
        // Trailing spaces/tabs go; interior spaces and line count stay.
        assert_eq!(trim_trailing_spaces("a  \nb\t\nc"), "a\nb\nc");
        // A trailing newline round-trips (no line gained or lost).
        assert_eq!(trim_trailing_spaces("a  \n"), "a\n");
        // No trailing newline stays that way.
        assert_eq!(trim_trailing_spaces("a  "), "a");
        // Leading whitespace is untouched.
        assert_eq!(trim_trailing_spaces("  a  "), "  a");
    }

    #[test]
    fn paste_bytes_strips_esc_to_prevent_bracketed_paste_escape() {
        // A benign paste is wrapped verbatim between the bracketed-paste markers.
        assert_eq!(
            paste_bytes("ls -la", true),
            b"\x1b[200~ls -la\x1b[201~".to_vec()
        );

        // Malicious clipboard text carrying its own `ESC[201~` end-marker followed
        // by a newline + command: without stripping ESC this would break out of the
        // paste and run `rm -rf ~` as typed input. The fix strips every ESC so the
        // smuggled end-marker becomes inert.
        let evil = "foo\x1b[201~\nrm -rf ~\n";
        let out = paste_bytes(evil, true);
        let end = b"\x1b[201~";
        // Exactly one end-marker survives — the trusted one we append, not the
        // smuggled one (an unfiltered impl would leave two).
        let markers = out.windows(end.len()).filter(|w| *w == end).count();
        assert_eq!(markers, 1);
        // No raw ESC remains inside the wrapped payload.
        let inner = &out[b"\x1b[200~".len()..out.len() - end.len()];
        assert!(!inner.contains(&0x1b));
        // Visible characters are preserved; only the ESC bytes are dropped.
        assert_eq!(inner, b"foo[201~\nrm -rf ~\n");

        // Without bracketed paste there is no wrapping, so bytes pass through as-is.
        assert_eq!(paste_bytes("a\x1b[201~b", false), b"a\x1b[201~b".to_vec());
    }

    #[test]
    fn paste_bytes_normalizes_newlines_to_cr_without_bracketed_paste() {
        // Regression: a raw-mode app (the only consumer of the non-bracketed
        // PTY path) reads keys, and Enter is CR — pasted `\n`/`\r\n` must
        // arrive as `\r`, matching xterm/alacritty, or apps that bind
        // accept/submit to CR only mis-handle multi-line pastes.
        assert_eq!(paste_bytes("a\nb\r\nc\n", false), b"a\rb\rc\r".to_vec());
        // Under bracketed paste the receiver gets the text verbatim (minus
        // ESC): the markers make line handling the app's own business.
        assert_eq!(
            paste_bytes("a\nb", true),
            b"\x1b[200~a\nb\x1b[201~".to_vec()
        );
    }

    #[test]
    fn shell_escape_path_escapes_spaces_and_metachars() {
        // A plain path is untouched.
        assert_eq!(
            shell_escape_path("/Users/me/notes.txt"),
            "/Users/me/notes.txt"
        );
        // Spaces and shell metacharacters each gain a backslash so the whole
        // path reaches the shell as a single argument.
        assert_eq!(
            shell_escape_path("/Users/me/My File (1).txt"),
            "/Users/me/My\\ File\\ \\(1\\).txt"
        );
        assert_eq!(
            shell_escape_path("/a/$HOME & more"),
            "/a/\\$HOME\\ \\&\\ more"
        );
        // Empty becomes an explicit empty-string literal.
        assert_eq!(shell_escape_path(""), "''");
        // A newline can't be backslash-escaped, so the path is single-quoted.
        assert_eq!(shell_escape_path("a\nb"), "'a\nb'");
    }

    #[test]
    fn clipboard_paste_text_escapes_and_space_joins_files() {
        // Finder-style file copy: paths are escaped and space-joined — not glued
        // together like gpui's `text()` fallback, and not left raw.
        let item = ClipboardItem {
            entries: vec![ClipboardEntry::ExternalPaths(ExternalPaths(
                vec![
                    PathBuf::from("/Users/me/My File.txt"),
                    PathBuf::from("/tmp/b.log"),
                ]
                .into(),
            ))],
        };
        assert_eq!(
            clipboard_paste_text(&item).as_deref(),
            Some("/Users/me/My\\ File.txt /tmp/b.log")
        );

        // Plain text still passes through verbatim.
        let text = ClipboardItem::new_string("echo hi".to_string());
        assert_eq!(clipboard_paste_text(&text).as_deref(), Some("echo hi"));
    }

    #[test]
    fn display_width_ascii_and_control_are_narrow() {
        assert_eq!(display_width('a'), 1);
        assert_eq!(display_width(' '), 1);
        assert_eq!(display_width('~'), 1);
        assert_eq!(display_width('\t'), 1);
    }

    #[test]
    fn display_width_cjk_and_kana_are_wide() {
        assert_eq!(display_width('你'), 2); // CJK Unified
        assert_eq!(display_width('한'), 2); // Hangul syllable
        assert_eq!(display_width('あ'), 2); // Hiragana
        assert_eq!(display_width('　'), 2); // fullwidth space (U+3000)
    }

    #[test]
    fn display_width_emoji_are_wide() {
        assert_eq!(display_width('🚀'), 2); // U+1F680, in emoji range
        assert_eq!(display_width('🎉'), 2);
    }

    #[test]
    fn display_width_latin_accents_stay_narrow() {
        // Accented Latin and common symbols outside the wide ranges are 1 cell.
        assert_eq!(display_width('é'), 1);
        assert_eq!(display_width('©'), 1);
        assert_eq!(display_width('±'), 1);
    }

    /// Shorthand: run `wrapped_click_index` over `text`'s chars.
    fn click(text: &str, scol: usize, cols: usize, col: usize, row: usize) -> Option<usize> {
        let chars: Vec<char> = text.chars().collect();
        wrapped_click_index(&chars, scol, cols, col, row, false)
    }

    #[test]
    fn wrapped_click_index_hits_chars_on_the_first_row() {
        // Prompt ends at column 4; "git" occupies columns 4..7 of row 0.
        assert_eq!(click("git", 4, 80, 4, 0), Some(0));
        assert_eq!(click("git", 4, 80, 6, 0), Some(2));
        // Left of the first char (on the prompt itself) snaps to char 0.
        assert_eq!(click("git", 4, 80, 1, 0), Some(0));
        // Past the row's content → end of line.
        assert_eq!(click("git", 4, 80, 40, 0), Some(3));
    }

    #[test]
    fn wrapped_click_index_maps_wrapped_rows() {
        // 10-column grid, prompt at column 8: "abcdef" lays out as row 0 =
        // "ab" (cols 8..10), row 1 = "cdef" (cols 0..4).
        assert_eq!(click("abcdef", 8, 10, 9, 0), Some(1)); // 'b'
        assert_eq!(click("abcdef", 8, 10, 0, 1), Some(2)); // 'c'
        assert_eq!(click("abcdef", 8, 10, 3, 1), Some(5)); // 'f'
        // A wide char that can't fit the row's last cell wraps whole, leaving a
        // dead cell at the row end; clicking it snaps to the wrapped char —
        // "a你" on a 4-col grid: 'a' at (0,2), dead cell (0,3), 你 at (1,0..2).
        assert_eq!(click("a你", 2, 4, 3, 0), Some(1));
        // Past the last row's content → end of line.
        assert_eq!(click("abcdef", 8, 10, 9, 1), Some(6));
    }

    #[test]
    fn wrapped_click_index_respects_wide_chars() {
        // "你好" after a 2-col prompt: 你 covers cols 2..4, 好 covers 4..6 —
        // either cell of a wide glyph resolves to its char index.
        assert_eq!(click("你好", 2, 80, 2, 0), Some(0));
        assert_eq!(click("你好", 2, 80, 3, 0), Some(0));
        assert_eq!(click("你好", 2, 80, 4, 0), Some(1));
        // A wide char that doesn't fit in the row's last cell wraps whole: on a
        // 5-col grid with the prompt at column 4, 你 moves to row 1 cols 0..2.
        assert_eq!(click("你", 4, 5, 0, 1), Some(0));
        assert_eq!(click("你", 4, 5, 1, 1), Some(0));
    }

    #[test]
    fn wrapped_click_index_rows_past_the_input_need_clamp() {
        let chars: Vec<char> = "ls".chars().collect();
        // A click two rows below a one-row input isn't an editor click…
        assert_eq!(wrapped_click_index(&chars, 4, 80, 3, 2, false), None);
        // …but a drag (clamp) snaps to the end of the line.
        assert_eq!(wrapped_click_index(&chars, 4, 80, 3, 2, true), Some(2));
        // An empty line: any column of the input row maps to index 0.
        assert_eq!(wrapped_click_index(&[], 4, 80, 30, 0, false), Some(0));
        // One row below a one-row input that doesn't fill its row stays None
        // (there is no caret slot down there).
        assert_eq!(wrapped_click_index(&chars, 4, 80, 3, 1, false), None);
    }

    #[test]
    fn wrapped_click_index_covers_the_wrapped_caret_slot() {
        // Regression: "abcdef" after a 4-col prompt exactly fills a 10-col row,
        // so the renderer's end-of-line caret slot wraps to row 1 col 0 — the
        // blinking caret is visibly drawn there. A click on that row must map
        // to the end of the line, not fall off the input (which turned the
        // click into a terminal selection instead of a caret move).
        assert_eq!(click("abcdef", 4, 10, 0, 1), Some(6));
        assert_eq!(click("abcdef", 4, 10, 7, 1), Some(6));
        // Two rows down is still past the input.
        let chars: Vec<char> = "abcdef".chars().collect();
        assert_eq!(wrapped_click_index(&chars, 4, 10, 0, 2, false), None);
    }

    #[test]
    fn menu_layout_prefers_below_and_flips_above_when_cramped() {
        // Plenty of room below: all 5 rows drop under the input row.
        assert_eq!(menu_layout(24, 3, 5, 0, 10), (false, 5, 0));
        // Input near the bottom: not enough room below, plenty above → flip.
        assert_eq!(menu_layout(24, 22, 5, 0, 10), (true, 5, 0));
        // Cramped on both sides: the larger side wins, squeezed to what fits
        // *including* the footer lines squeezing makes appear.
        assert_eq!(menu_layout(6, 4, 10, 0, 10), (true, 2, 0));
        assert_eq!(menu_layout(6, 1, 10, 0, 10), (false, 2, 0));
        // Even a 1-row grid shows at least one candidate row.
        let (_, visible, _) = menu_layout(1, 0, 8, 0, 10);
        assert_eq!(visible, 1);
    }

    #[test]
    fn menu_layout_budgets_the_overflow_footers() {
        // Regression: a windowed list renders up to two "N more" footer lines
        // in the same box. Sizing on candidate rows alone placed a 12-line menu
        // (10 rows + 2 footers) into 10 free rows below — clipping the last two
        // lines, one of which held the *selected* candidate (the window pins the
        // selection to its bottom edge). The budget must count the footers, so
        // this case flips above where all 12 lines fit.
        let (place_above, visible, first) = menu_layout(24, 13, 30, 17, 10);
        assert!(
            place_above,
            "12 needed lines don't fit in the 10 rows below"
        );
        assert_eq!(visible, 10);
        // The selection stays within the visible window.
        assert!((first..first + visible).contains(&17));
    }

    #[test]
    fn menu_layout_caps_rows_and_windows_around_the_selection() {
        // 30 candidates cap at max_rows; selecting deep into the list scrolls
        // the window so the selection sits on its last visible row.
        let (_, visible, first) = menu_layout(40, 0, 30, 17, 10);
        assert_eq!(visible, 10);
        assert!((first..first + visible).contains(&17));
        assert_eq!(first, 8); // sel rides the window's bottom edge
        // Selecting the last candidate clamps the window to the list's tail.
        let (_, visible, first) = menu_layout(40, 0, 30, 29, 10);
        assert_eq!(first, 20);
        assert_eq!(first + visible, 30);
        // A selection inside the first window leaves it unscrolled.
        assert_eq!(menu_layout(40, 0, 30, 3, 10).2, 0);
    }
}

/// gpui-harness tests: a real (headless) App + Window around a `TerminalView`
/// wired to a socketpair, so `handle_event` and the event pump run exactly as
/// in production. The test plays the daemon on the other end of the socket —
/// write `DaemonMsg`s to feed the terminal, read `ClientMsg`s to observe what
/// the view sent back.
#[cfg(all(test, unix))]
mod gpui_tests {
    use super::*;
    use crate::daemon::protocol::{ClientMsg, DaemonMsg};
    use gpui::TestAppContext;
    use std::os::unix::net::UnixStream;

    fn harness(cx: &mut TestAppContext) -> (gpui::WindowHandle<TerminalView>, UnixStream) {
        // The terminal's reader is a real OS thread feeding a real socket, so
        // this test mixes deterministic scheduling with outside I/O — exactly
        // what `allow_parking` exists for.
        cx.executor().allow_parking();
        let (client_side, daemon_side) = UnixStream::pair().unwrap();
        cx.update(|cx| {
            // Same globals `main` installs: the component theme (view code
            // reads it via `cx.theme()`) and the user config.
            gpui_component::init(cx);
            cx.set_global(Config::default());
        });
        let window = cx.add_window(|window, cx| {
            let terminal = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24))
                .expect("socketpair-backed terminal");
            TerminalView::with_terminal(terminal, 1, window, cx)
        });
        (window, daemon_side)
    }

    #[gpui::test]
    fn title_events_drive_the_tab_title(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                assert_eq!(view.title, "tty7");
                view.handle_event(AlacEvent::Title("vim — main.rs".into()), cx);
                assert_eq!(view.title, "vim — main.rs");
                view.handle_event(AlacEvent::ResetTitle, cx);
                assert_eq!(view.title, "tty7");
            })
            .unwrap();
    }

    /// The first frames out of the socket may be `Resize`s — the headless
    /// window really lays the element out, and the first prepaint syncs its
    /// measured geometry. Skip to the next `Input`.
    fn next_input(daemon: &mut UnixStream) -> Vec<u8> {
        loop {
            match ClientMsg::read(daemon).expect("client socket stays open") {
                ClientMsg::Input(bytes) => return bytes,
                _ => continue,
            }
        }
    }

    fn next_input_until_timeout(daemon: &mut UnixStream) -> Option<Vec<u8>> {
        use std::io::ErrorKind;

        daemon
            .set_read_timeout(Some(std::time::Duration::from_millis(250)))
            .unwrap();
        loop {
            match ClientMsg::read(daemon) {
                Ok(ClientMsg::Input(bytes)) => return Some(bytes),
                Ok(_) => continue,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return None;
                }
                Err(e) => panic!("client socket failed before Input: {e}"),
            }
        }
    }

    #[gpui::test]
    fn ctrl_l_at_prompt_reaches_the_shell(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                let ctrl_l = gpui::Keystroke {
                    modifiers: gpui::Modifiers {
                        control: true,
                        ..Default::default()
                    },
                    key: "l".to_string(),
                    key_char: None,
                };
                view.handle_editor_key(&ctrl_l, cx);
            })
            .unwrap();

        assert_eq!(next_input_until_timeout(&mut daemon), Some(vec![0x0c]));
    }

    /// Readline's Meta word chords act on the local prompt editor: M-b / M-f
    /// move by word, M-d deletes the word right of the caret. (On macOS these
    /// chords reach the editor only with `macos_option_as_alt` on — the
    /// `on_key_down` reshape otherwise strips the alt bit; here we drive the
    /// editor dispatcher directly with the post-reshape keystroke.)
    #[gpui::test]
    fn meta_word_chords_edit_the_prompt_line(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                let meta = |key: &str| gpui::Keystroke {
                    modifiers: gpui::Modifiers {
                        alt: true,
                        ..Default::default()
                    },
                    key: key.to_string(),
                    key_char: None,
                };
                view.cmd.set("echo hello");
                // M-b from the end lands at the start of "hello".
                view.handle_editor_key(&meta("b"), cx);
                assert_eq!(view.cmd.cursor(), 5);
                // M-d deletes the word right of the caret.
                view.handle_editor_key(&meta("d"), cx);
                assert_eq!(view.cmd.text(), "echo ");
                // M-b / M-f hop the remaining word: back to its start, then
                // forward to its end.
                view.handle_editor_key(&meta("b"), cx);
                assert_eq!(view.cmd.cursor(), 0);
                view.handle_editor_key(&meta("f"), cx);
                assert_eq!(view.cmd.cursor(), 4);
                // Other Meta letters stay swallowed no-ops (line untouched).
                view.handle_editor_key(&meta("z"), cx);
                assert_eq!(view.cmd.text(), "echo ");
                assert_eq!(view.cmd.cursor(), 4);
            })
            .unwrap();
    }

    /// A `PtyWrite` raised by the VT layer (query replies, bracketed-paste
    /// wrapping…) must come out of the client socket as an `Input` frame —
    /// this is the half of the query round-trip the remote tests can't see.
    #[gpui::test]
    fn pty_write_events_reach_the_daemon_as_input(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.handle_event(AlacEvent::PtyWrite("ping".into()), cx);
            })
            .unwrap();
        assert_eq!(next_input(&mut daemon), b"ping".to_vec());
    }

    #[gpui::test]
    fn child_exit_marks_the_view_exited(cx: &mut TestAppContext) {
        let (window, _daemon) = harness(cx);
        window
            .update(cx, |view, _, cx| {
                view.handle_event(AlacEvent::Exit, cx);
                assert!(view.terminal.exited);
                assert_eq!(view.title, "tty7 — process exited");
            })
            .unwrap();
    }

    /// CSI 14 t (text-area size in pixels) must be answered from the current
    /// grid geometry — image TUIs (yazi, chafa) stall on a report that never
    /// comes.
    #[gpui::test]
    fn text_area_size_request_replies_with_the_current_geometry(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);
        // The window may have re-measured the grid by now — derive the
        // expectation from whatever size the terminal actually has.
        let want = window
            .update(cx, |view, _, cx| {
                let size = view.terminal.size();
                let fmt = std::sync::Arc::new(|ws: alacritty_terminal::event::WindowSize| {
                    format!("{}x{}", ws.num_cols, ws.num_lines)
                });
                view.handle_event(AlacEvent::TextAreaSizeRequest(fmt), cx);
                format!("{}x{}", size.cols, size.rows)
            })
            .unwrap();
        assert_eq!(next_input(&mut daemon), want.into_bytes());
    }

    /// The full ingress chain — daemon frame → reader thread → grid → event
    /// pump → `handle_event(Wakeup)` — inside a real (headless) App. Guards
    /// the pump against the "grid updated but the view never wakes" class of
    /// bug, and the second frame proves the pump survives its own
    /// redraw-scheduling step (a failed window refresh must degrade, never
    /// tear the pump down).
    #[gpui::test]
    fn daemon_output_reaches_the_grid_through_the_event_pump(cx: &mut TestAppContext) {
        let (window, mut daemon) = harness(cx);

        // Bounded poll: the reader is a real OS thread, so give it wall-clock
        // time, then let the foreground pump run between checks.
        let read_row = |cx: &mut TestAppContext, len: usize| -> String {
            window
                .update(cx, |view, _, _| {
                    let term = view.terminal.term.clone();
                    let term = term.lock();
                    let grid = term.grid();
                    (0..len)
                        .map(|c| grid[alacritty_terminal::index::Line(0)][Column(c)].c)
                        .collect()
                })
                .unwrap()
        };
        let wait_for = |cx: &mut TestAppContext, want: &str| {
            let mut got = String::new();
            for _ in 0..400 {
                cx.run_until_parked();
                got = read_row(cx, want.chars().count());
                if got == want {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            got
        };

        DaemonMsg::Output(b"hello".to_vec())
            .encode(&mut daemon)
            .unwrap();
        assert_eq!(wait_for(cx, "hello"), "hello");

        // A second frame still lands: the pump outlived the first round-trip.
        DaemonMsg::Output(b" again".to_vec())
            .encode(&mut daemon)
            .unwrap();
        assert_eq!(wait_for(cx, "hello again"), "hello again");
    }

    /// Reproduces the "orange caret jumps to the top-left corner after Claude
    /// Code exits" bug at the state level, driving the two conditions that must
    /// co-occur to trigger it:
    ///
    ///   1. the shell is idle at its prompt (`DaemonMsg::Prompt` →
    ///      `input_active()`), so the inline editor is live and draws its own
    ///      caret via `render_input_bar`, which anchors at `cursor_cell()`; and
    ///   2. the local grid's cursor *shape* is still `Hidden` — a full-screen
    ///      TUI hid the cursor with DECTCEM (`\e[?25l`) and handed back to the
    ///      prompt before a matching `\e[?25h` landed.
    ///
    /// The cursor's real *position* is a valid cell (the prompt end), but the
    /// stale-hidden shape used to make `cursor_cell()` return `None`, so
    /// `render_input_bar`'s `unwrap_or((0, 0))` painted the caret at cell
    /// `(0, 0)`. The assertions pin all three facts: the editor is active, the
    /// shape genuinely is `Hidden` (the precondition that tripped the old
    /// early-return), and `cursor_cell()` nonetheless reports the real cell.
    #[gpui::test]
    fn hidden_cursor_at_prompt_anchors_the_editor_at_the_real_cell_not_top_left(
        cx: &mut TestAppContext,
    ) {
        use alacritty_terminal::vte::ansi::CursorShape;

        let (window, mut daemon) = harness(cx);

        // Shell reports it is idle at its prompt: this is what flips
        // `input_active()` true and puts the inline editor in charge.
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(0),
        }
        .encode(&mut daemon)
        .unwrap();
        // CUP to row 4 / col 11 (1-based), then hide the cursor as a TUI would
        // on the way out — leaving the shape `Hidden` at a valid position.
        DaemonMsg::Output(b"\x1b[4;11H\x1b[?25l".to_vec())
            .encode(&mut daemon)
            .unwrap();

        // Poll until both the prompt report and the grid bytes have applied.
        let mut state = (false, false, None);
        for _ in 0..400 {
            cx.run_until_parked();
            state = window
                .update(cx, |view, _, _| {
                    let hidden = matches!(
                        view.terminal.term.lock().renderable_content().cursor.shape,
                        CursorShape::Hidden
                    );
                    (view.input_active(), hidden, view.cursor_cell())
                })
                .unwrap();
            if state == (true, true, Some((3, 10))) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let (active, hidden, cell) = state;
        assert!(
            active,
            "shell at its prompt must make the inline editor active"
        );
        assert!(
            hidden,
            "the TUI's `?25l` must leave the cursor shape Hidden"
        );
        assert_eq!(
            cell,
            Some((3, 10)),
            "a Hidden shape must not collapse the editor anchor to the top-left corner"
        );
    }

    /// A genuine child exit (`DaemonMsg::Exited`) must surface as a
    /// `ChildExited` gpui event — the app's cue to close the pane/tab (the
    /// "typing `exit` leaves a dead pane behind" bug). A daemon disconnect
    /// marks the view exited through the same `AlacEvent::Exit` arm but must
    /// emit nothing: auto-closing on a lost connection would silently discard
    /// (and kill) a pane that may still be alive daemon-side.
    #[gpui::test]
    fn child_exit_emits_the_close_event_but_disconnect_does_not(cx: &mut TestAppContext) {
        use std::cell::Cell;
        use std::rc::Rc;

        let subscribe = |window: &gpui::WindowHandle<TerminalView>, cx: &mut TestAppContext| {
            let got = Rc::new(Cell::new(false));
            let seen = got.clone();
            window
                .update(cx, |_, _, cx| {
                    let this = cx.entity();
                    cx.subscribe(&this, move |_, _, _: &ChildExited, _| seen.set(true))
                        .detach();
                })
                .unwrap();
            got
        };
        let wait_exited = |window: &gpui::WindowHandle<TerminalView>, cx: &mut TestAppContext| {
            for _ in 0..400 {
                cx.run_until_parked();
                let exited = window
                    .update(cx, |view, _, _| view.terminal.exited)
                    .unwrap();
                if exited {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("the view never noticed the exit");
        };

        // The child really exits: the daemon says so.
        let (window, mut daemon) = harness(cx);
        let got = subscribe(&window, cx);
        DaemonMsg::Exited { code: Some(0) }
            .encode(&mut daemon)
            .unwrap();
        wait_exited(&window, cx);
        assert!(got.get(), "a genuine child exit must emit ChildExited");

        // The connection just drops.
        let (window, daemon) = harness(cx);
        let got = subscribe(&window, cx);
        drop(daemon);
        wait_exited(&window, cx);
        assert!(!got.get(), "a daemon disconnect must not emit ChildExited");
    }

    /// Regression for the "cursor vanishes after an ssh session dies mid-TUI"
    /// bug. Over ssh, a remote full-screen TUI entered the alt screen and hid
    /// the cursor (`\e[?1049h\e[?25l`). The network then drops: the restore
    /// sequences (`\e[?25h`, `\e[?1049l`) never arrive, ssh exits, and the
    /// *host* shell draws its prompt (reported via OSC 133 → `Prompt`).
    ///
    /// Before the prompt-time scrub in the remote reader (see
    /// `stale_mode_resets`), the grid stayed stranded on the alt screen with
    /// a `Hidden` cursor shape, so *neither* cursor painted:
    /// `element::build_grid` filters hidden grid cursors, and the inline
    /// editor (which would ignore the stale-Hidden shape, see the test above)
    /// never engaged because `input_active()` requires being off the alt
    /// screen — a visible prompt with no cursor anywhere. The prompt report
    /// must instead scrub the residue: off the alt screen, cursor shown,
    /// editor live again.
    #[gpui::test]
    fn ssh_drop_mid_tui_recovers_at_the_next_prompt(cx: &mut TestAppContext) {
        use alacritty_terminal::vte::ansi::CursorShape;

        let (window, mut daemon) = harness(cx);

        // Bytes that arrived over ssh before the drop: the remote TUI enters
        // the alt screen and hides the cursor. The connection dies before any
        // restore sequence is sent.
        DaemonMsg::Output(b"\x1b[?1049h\x1b[?25l".to_vec())
            .encode(&mut daemon)
            .unwrap();
        // ssh exits; the host shell's integration reports a fresh prompt.
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(255), // ssh's exit code after a connection loss
        }
        .encode(&mut daemon)
        .unwrap();

        let mut state = (false, true, true);
        for _ in 0..400 {
            cx.run_until_parked();
            state = window
                .update(cx, |view, _, _| {
                    let hidden = matches!(
                        view.terminal.term.lock().renderable_content().cursor.shape,
                        CursorShape::Hidden
                    );
                    (view.at_shell_prompt(), view.on_alt_screen(), hidden)
                })
                .unwrap();
            if state == (true, false, false) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let (at_prompt, on_alt, hidden) = state;
        assert!(at_prompt, "the host shell is back at its prompt");
        assert!(
            !on_alt,
            "the prompt report must pull the grid off the stranded alt screen"
        );
        assert!(
            !hidden,
            "the prompt report must re-show the DECTCEM-hidden cursor"
        );

        // With the residue scrubbed, the inline editor engages and owns the
        // caret again — the user sees a cursor at the prompt.
        window
            .update(cx, |view, _, _| {
                assert!(
                    view.input_active(),
                    "off the alt screen and at the prompt, the editor is live"
                );
            })
            .unwrap();
    }
}
